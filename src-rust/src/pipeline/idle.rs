//! The background worker that fills in what nobody has asked for yet.
//!
//! Three backlogs, in one loop, all of them cheap to abandon:
//!
//! 1. **`j` and `js` thumbnails**, newest-first — the order the default
//!    date-descending sort presents, so the first screen is warm first. `js` is
//!    warmed alongside `j` because the three panels that use it address
//!    *arbitrary*, typically old files, and would otherwise fire up to 120
//!    simultaneous generate-on-miss requests at exactly the part of the library
//!    a newest-first backfill reaches last.
//! 2. **Perceptual hashes**, decoded from the cached `j` bytes — no source
//!    decode, and the decode happens here rather than under the writer lock.
//!    The loop this replaces held the writer across 64 image decodes per
//!    acquisition, which blocked every thumbnail the grid was waiting on.
//! 3. **ThumbHashes** for rows that somehow have a `j` tier and no placeholder.
//!
//! **"Is anyone looking" is the activity timestamp and nothing else.** The
//! signal it replaces was `fs_change_tx.receiver_count() > 0` — *no web client
//! is subscribed* — which worked only because the desktop user was a separate
//! kind of client, detected separately. With one runtime the local user **is**
//! an SSE subscriber, so that counter is true whenever anybody has the gallery
//! open in a browser, and this worker would never run at all: no perceptual
//! hashes, and therefore no duplicate detection, ever. See
//! [`crate::pipeline::serve::Activity`].
//!
//! Every unit re-checks idleness before starting, so a user who touches the
//! grid gets the pool back within one batch rather than at the end of a sweep.

use std::sync::Arc;
use std::time::Duration;

use crate::cache::tiers::ThumbTier;
use crate::cache::{duplicates, meta};
use crate::path::RelPath;
use crate::pipeline::serve::ThumbService;
use crate::pipeline::thumbnailer;

/// How long the gallery must be quiet before the worker does anything.
const QUIET_SECS: i64 = 60;
/// How long to wait before looking again, whether or not it did work.
const POLL: Duration = Duration::from_secs(5);
/// Thumbnails per unit. Small enough that a returning user waits for at most
/// this many decodes.
const THUMB_BATCH: usize = 16;
/// Hashes per unit. Cheaper per item — a WebP decode of a 512px image — so the
/// unit can be larger.
const HASH_BATCH: usize = 64;

/// Run the backlog loop until the handle is dropped.
pub fn spawn(thumbs: Arc<ThumbService>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL).await;
            if !thumbs.activity.idle_for(QUIET_SECS) {
                continue;
            }
            match run_one_unit(&thumbs).await {
                Ok(true) => {}
                Ok(false) => {
                    // Nothing left to do; the poll interval is the whole
                    // backoff. There is no completion state to track, because
                    // a new file makes the backlog non-empty again.
                }
                Err(e) => log::warn!("idle backfill unit failed: {e}"),
            }
        }
    })
}

/// One unit of work. Returns whether it did anything.
async fn run_one_unit(thumbs: &ThumbService) -> Result<bool, crate::cache::db::CacheError> {
    if warm_thumbnails(thumbs).await? {
        return Ok(true);
    }
    if compute_hashes(thumbs).await? {
        return Ok(true);
    }
    Ok(false)
}

/// Generate the two unbounded tiers for the newest files that lack them.
async fn warm_thumbnails(thumbs: &ThumbService) -> Result<bool, crate::cache::db::CacheError> {
    let mut did_work = false;
    for tier in [ThumbTier::J, ThumbTier::Js] {
        let pending = missing(thumbs, tier, THUMB_BATCH).await?;
        for path in pending {
            // Re-check between work units: a user who touches the grid gets the
            // bounded pool back now, not at the end of the batch.
            if !thumbs.activity.idle_for(QUIET_SECS) {
                return Ok(did_work);
            }
            // Not user-driven — marking activity here would make the worker
            // permanently believe someone was looking.
            thumbs.get_or_generate(tier, &path, false).await;
            did_work = true;
        }
        if did_work {
            return Ok(true);
        }
    }
    Ok(did_work)
}

/// Rows in `media_meta` with no row in `tier`, newest first.
///
/// A `LEFT JOIN` on the tier's primary key rather than `NOT IN`: it reads the
/// index and never touches a thumbnail blob.
async fn missing(
    thumbs: &ThumbService,
    tier: ThumbTier,
    limit: usize,
) -> Result<Vec<RelPath>, crate::cache::db::CacheError> {
    let conn = thumbs.db.read().await;
    let sql = format!(
        "SELECT m.path FROM media_meta m
         LEFT JOIN {} t ON t.path = m.path
         WHERE t.path IS NULL
         ORDER BY {} DESC, m.path DESC
         LIMIT ?1",
        tier.table(),
        crate::sort::sorter::SORT_DATE
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map([limit], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(RelPath::new(&row?)?);
    }
    Ok(out)
}

/// Hash a batch of cached `j` rows, and fill in any missing ThumbHash while the
/// pixels are decoded anyway.
async fn compute_hashes(thumbs: &ThumbService) -> Result<bool, crate::cache::db::CacheError> {
    let pending = {
        let conn = thumbs.db.read().await;
        duplicates::unhashed(&conn, HASH_BATCH)?
    };
    if pending.is_empty() {
        return Ok(false);
    }

    // Decoding outside every lock is the point of splitting this in two.
    let hashed: Vec<(RelPath, Option<u64>, Option<Vec<u8>>)> = tokio::task::spawn_blocking(move || {
        pending
            .into_iter()
            .map(|(path, bytes)| match thumbnailer::decode_thumb_bytes_to_rgba(&bytes) {
                Ok((rgba, w, h)) => {
                    let hash = duplicates::dhash_rgba(&rgba, w, h);
                    let thumbhash = thumbnailer::compute_thumbhash(&rgba, w, h).ok();
                    (path, hash, thumbhash)
                }
                Err(e) => {
                    // NULL, never a sentinel: a flat image legitimately hashes
                    // to 0, so conflating "could not decode" with "hashed to
                    // zero" makes `WHERE phash IS NOT NULL` pass every row and
                    // the finder union the whole library into one group.
                    log::warn!("could not decode a cached j tier for hashing: {e}");
                    (path, None, None)
                }
            })
            .collect()
    })
    .await
    .unwrap_or_default();

    let conn = thumbs.db.writer().await;
    let phashes: Vec<(RelPath, Option<u64>)> =
        hashed.iter().map(|(p, h, _)| (p.clone(), *h)).collect();
    duplicates::set_phashes(&conn, &phashes)?;
    for (path, _, thumbhash) in &hashed {
        if let Some(hash) = thumbhash {
            meta::set_thumbhash(&conn, path, hash)?;
        }
    }
    Ok(true)
}
