//! Reading a cached tier, and generating it on a miss — once, no matter how
//! many callers ask at the same time.
//!
//! This is the hot path. A scrolling grid asks for a few hundred thumbnails a
//! second, aborts most of those requests before they finish, and asks again a
//! moment later. Three properties keep that from falling over, and each one
//! replaces a failure that actually happened:
//!
//! - **Reads go through the read-only pool**, never the writer, so a scroll is
//!   not queued behind the index pass.
//! - **Generation is coalesced**, and the slot is an RAII guard so a cancelled
//!   request releases it. See [`crate::cache::coalescer`].
//! - **Access marks buffer in memory** and are drained immediately before an
//!   eviction pass. The read path holds a read-only connection, so it cannot
//!   stamp `accessed_at` itself; draining after an eviction instead of before
//!   would evict exactly what the user is looking at.
//!
//! `RelPath` in, `GalleryPath` only inside the generate branch: a request
//! answered from the database never pays for a `realpath` walk, and the branch
//! that opens an arbitrary file cannot compile without the check.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use crate::cache::coalescer::{Acquired, ThumbGenCoalescer};
use crate::cache::db::{CacheDb, CacheError};
use crate::cache::{meta, tiers};
use crate::cache::tiers::ThumbTier;
use crate::path::{RelPath, Root};
use crate::pipeline::thumbnailer::{self, ThumbError};

/// How many attempts a waiter makes before giving up and reporting a miss.
///
/// A generator whose request future was cancelled wakes its waiters having
/// produced nothing; the woken waiter re-checks, finds the slot free and
/// becomes the generator. Bounding it means a persistently failing source
/// degrades to a miss instead of spinning.
const MAX_ATTEMPTS: usize = 3;

/// Result of a tier lookup.
pub enum Outcome {
    Hit(Vec<u8>),
    Miss,
}

/// Buffered `accessed_at` stamps, keyed by tier.
///
/// Only the bounded tiers are worth recording — the unbounded ones are never
/// evicted, so their access times are write-only data.
#[derive(Default)]
pub struct AccessMarks {
    inner: Mutex<HashMap<ThumbTier, Vec<RelPath>>>,
}

impl AccessMarks {
    fn record(&self, tier: ThumbTier, path: &RelPath) {
        if !tier.bounded() {
            return;
        }
        self.inner
            .lock()
            .expect("access marks poisoned")
            .entry(tier)
            .or_default()
            .push(path.clone());
    }

    fn drain(&self) -> HashMap<ThumbTier, Vec<RelPath>> {
        std::mem::take(&mut *self.inner.lock().expect("access marks poisoned"))
    }
}

/// When a user-driven thumbnail request last landed.
///
/// **This is the whole idle signal.** It used to be one half of a pair with
/// "no web client is subscribed to filesystem events", which worked only
/// because the desktop user was a different kind of client. With one runtime
/// the local user *is* a subscriber, so that counter is true whenever anyone
/// has the gallery open in a browser — and the backfill would never run at
/// all, taking perceptual hashing, and therefore duplicate detection, with it.
/// The subscriber count no longer means anything and must not be consulted.
#[derive(Debug)]
pub struct Activity {
    last_request: AtomicI64,
}

impl Default for Activity {
    fn default() -> Self {
        Self {
            last_request: AtomicI64::new(0),
        }
    }
}

impl Activity {
    pub fn mark(&self) {
        self.last_request.store(now(), Ordering::Relaxed);
    }

    /// Whether nothing user-driven has landed for `quiet_secs`.
    pub fn idle_for(&self, quiet_secs: i64) -> bool {
        now() - self.last_request.load(Ordering::Relaxed) >= quiet_secs
    }
}

/// Everything a thumbnail request needs, for one open gallery.
pub struct ThumbService {
    pub db: Arc<CacheDb>,
    pub root: Root,
    coalescer: Arc<ThumbGenCoalescer>,
    /// The one bounded CPU pool every decode in the system runs on. Speculative
    /// work lands here too, which is why its callers gate it rather than
    /// escaping to a second pool.
    pool: Arc<rayon::ThreadPool>,
    /// Per-tier byte budget for the two bounded tiers.
    budget_bytes: i64,
    marks: AccessMarks,
    pub activity: Activity,
}

impl ThumbService {
    pub fn new(
        db: Arc<CacheDb>,
        root: Root,
        pool: Arc<rayon::ThreadPool>,
        budget_bytes: i64,
    ) -> Self {
        Self {
            db,
            root,
            coalescer: Arc::new(ThumbGenCoalescer::new()),
            pool,
            budget_bytes,
            marks: AccessMarks::default(),
            activity: Activity::default(),
        }
    }

    /// Read a cached tier. No generation, no canonicalize, no writer.
    pub async fn read_cached(&self, tier: ThumbTier, path: &RelPath) -> Outcome {
        let conn = self.db.read().await;
        match tiers::get(&conn, tier, path) {
            Ok(Some(row)) => {
                self.marks.record(tier, path);
                Outcome::Hit(row.bytes)
            }
            Ok(None) => Outcome::Miss,
            Err(e) => {
                log::warn!("thumb cache read failed for {path} ({tier:?}): {e}");
                Outcome::Miss
            }
        }
    }

    /// Full lookup-then-generate.
    ///
    /// `user_driven` marks the activity clock: a grid request does, the idle
    /// backfill does not, and confusing the two is what would stop the
    /// backfill ever deciding it was idle.
    pub async fn get_or_generate(
        &self,
        tier: ThumbTier,
        path: &RelPath,
        user_driven: bool,
    ) -> Outcome {
        if user_driven {
            self.activity.mark();
        }

        for _ in 0..MAX_ATTEMPTS {
            match self.read_cached(tier, path).await {
                Outcome::Miss => {}
                hit => return hit,
            }

            match self.coalescer.acquire((path.clone(), tier)) {
                Acquired::Generator(_guard) => {
                    // `_guard` releases the slot and wakes waiters when
                    // dropped — on success, on error, and on cancellation at
                    // any await point.
                    return match self.generate_and_store(tier, path).await {
                        Ok(bytes) => Outcome::Hit(bytes),
                        Err(e) => {
                            log::warn!("generate-on-miss failed for {path} ({tier:?}): {e}");
                            Outcome::Miss
                        }
                    };
                }
                Acquired::Waiter(notify) => {
                    let listener = notify.notified();
                    tokio::pin!(listener);
                    // Enrol before re-checking the cache, or a notify that
                    // races the generator's release is lost and this caller
                    // waits for a wake that already happened.
                    listener.as_mut().enable();

                    if let Outcome::Hit(bytes) = self.read_cached(tier, path).await {
                        return Outcome::Hit(bytes);
                    }
                    listener.await;
                    // Round again: a completed generation hits the re-read; a
                    // cancelled one misses and this caller takes the slot.
                }
            }
        }
        self.read_cached(tier, path).await
    }

    /// Decode, resize, encode, store — the only place a tier row is written.
    ///
    /// The decode happens on the rayon pool with no lock held; the writer is
    /// taken afterwards, for statements only.
    pub async fn generate_and_store(
        &self,
        tier: ThumbTier,
        path: &RelPath,
    ) -> Result<Vec<u8>, GenerateError> {
        // The one canonicalize on this path, in the branch that is about to
        // open an arbitrary file.
        let gallery_path = self.root.resolve(path)?;
        let edge = tier.edge();
        let filter = thumbnailer::filter_for_size(edge);

        let result = on_pool(&self.pool, move || {
            thumbnailer::generate_for_path_fit(gallery_path.as_path(), filter, edge)
        })
        .await
        .ok_or(GenerateError::PoolGone)??;

        // The ThumbHash comes from the pixels already in hand, from the tier
        // the grid asks for first. Deriving it later would mean a second decode
        // and a first paint with no placeholders.
        let thumbhash = if tier == ThumbTier::J {
            thumbnailer::compute_thumbhash(&result.rgba, result.width, result.height).ok()
        } else {
            None
        };

        let bytes = result.data.clone();
        {
            let conn = self.db.writer().await;
            tiers::put(&conn, tier, path, &result.data)?;
            if let Some(hash) = thumbhash {
                meta::set_thumbhash(&conn, path, &hash)?;
            }
            // A placeholder must never write dimensions: a 0x0 fills the
            // `width IS NULL` gap that guards the column and hands the grid a
            // degenerate aspect ratio.
            if result.src_width > 0 && result.src_height > 0 {
                meta::set_probed(
                    &conn,
                    path,
                    &meta::ProbedMedia {
                        width: Some(result.src_width),
                        height: Some(result.src_height),
                        ..Default::default()
                    },
                )?;
            }
            self.enforce_budget(&conn)?;
        }
        Ok(bytes)
    }

    /// Drain buffered access marks and evict past the hysteresis point.
    ///
    /// Both write paths call this, not only the batch one — a budget enforced
    /// on one of two writers is not a budget.
    fn enforce_budget(&self, conn: &rusqlite::Connection) -> Result<(), CacheError> {
        let drained = self.marks.drain();
        for (tier, paths) in &drained {
            tiers::touch(conn, *tier, paths)?;
        }
        for tier in ThumbTier::ALL.into_iter().filter(|t| t.bounded()) {
            tiers::enforce_budget(conn, tier, self.budget_bytes)?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GenerateError {
    #[error(transparent)]
    Path(#[from] crate::path::PathError),
    #[error(transparent)]
    Thumb(#[from] ThumbError),
    #[error(transparent)]
    Cache(#[from] CacheError),
    #[error("the thumbnail pool went away")]
    PoolGone,
}

/// Run CPU work on the bounded pool and await its result.
async fn on_pool<T: Send + 'static>(
    pool: &rayon::ThreadPool,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    pool.spawn(move || {
        // The receiver is gone when the request future was cancelled, which is
        // ordinary: the work is finished and nobody wants it.
        let _ = tx.send(f());
    });
    rx.await.ok()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_is_the_only_idle_signal() {
        let a = Activity::default();
        // A process that has never served a request is idle by construction,
        // which is what lets the backfill start on a cold gallery.
        assert!(a.idle_for(60));
        a.mark();
        assert!(!a.idle_for(60));
        assert!(a.idle_for(0));
    }

    #[test]
    fn only_bounded_tiers_accumulate_access_marks() {
        let marks = AccessMarks::default();
        let p = RelPath::new("a.jpg").unwrap();
        for tier in ThumbTier::ALL {
            marks.record(tier, &p);
        }
        let drained = marks.drain();
        assert_eq!(drained.len(), 2);
        assert!(drained.contains_key(&ThumbTier::Jm));
        assert!(drained.contains_key(&ThumbTier::Jh));
        assert!(marks.drain().is_empty(), "drain must be a take, not a copy");
    }
}
