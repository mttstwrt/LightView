//! Deduplicates concurrent on-miss thumbnail generation for the same
//! `(path, tier)`.
//!
//! The first caller becomes the generator; concurrent callers for the same key
//! wait on a `Notify` and re-read the cache once woken, since the result is
//! persisted to SQLite rather than passed between them.
//!
//! **The generator's slot is an RAII guard**, so releasing it — and waking the
//! waiters — also happens when the generating future is *cancelled*. That is
//! not a corner case: an HTTP request future is dropped whenever the browser
//! aborts a fetch, which the grid's virtual scrolling does constantly. The
//! explicit-release design this replaces leaked the key on cancellation,
//! permanently hanging every later request for that thumbnail and, once a few
//! piled up, the browser's whole per-origin connection budget. That was the
//! "server stops responding until restart" symptom.
//!
//! Waiters must enrol in the wake queue **before** re-checking the cache, or
//! they can miss a notify that races the generator's release; the serve path
//! also bounds its retries, so a persistently failing source degrades to a miss
//! rather than spinning.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

use crate::cache::tiers::ThumbTier;
use crate::path::RelPath;

pub type ThumbKey = (RelPath, ThumbTier);

/// Outcome of [`ThumbGenCoalescer::acquire`].
pub enum Acquired {
    /// You got the slot — run the generator while holding the guard. The slot
    /// is released and all waiters woken when the guard drops, on every exit
    /// path: success, error, or cancellation mid-await.
    Generator(GenerationGuard),
    /// Another caller is generating. Enrol via `Notified::enable` before
    /// re-checking the cache, then await.
    Waiter(Arc<Notify>),
}

/// RAII slot for a generating caller; see [`Acquired::Generator`].
pub struct GenerationGuard {
    coalescer: Arc<ThumbGenCoalescer>,
    key: ThumbKey,
}

impl Drop for GenerationGuard {
    fn drop(&mut self) {
        self.coalescer.release(&self.key);
    }
}

#[derive(Default)]
pub struct ThumbGenCoalescer {
    inner: Mutex<HashMap<ThumbKey, Arc<Notify>>>,
}

impl ThumbGenCoalescer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim the generation slot for `key`, or enrol as a waiter on whoever
    /// holds it.
    pub fn acquire(self: &Arc<Self>, key: ThumbKey) -> Acquired {
        let mut map = self.inner.lock().expect("coalescer mutex poisoned");
        if let Some(notify) = map.get(&key) {
            Acquired::Waiter(notify.clone())
        } else {
            map.insert(key.clone(), Arc::new(Notify::new()));
            Acquired::Generator(GenerationGuard {
                coalescer: self.clone(),
                key,
            })
        }
    }

    fn release(&self, key: &ThumbKey) {
        let mut map = self.inner.lock().expect("coalescer mutex poisoned");
        if let Some(notify) = map.remove(key) {
            notify.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(p: &str) -> ThumbKey {
        (RelPath::new(p).unwrap(), ThumbTier::J)
    }

    #[test]
    fn the_second_caller_waits_and_the_slot_frees_on_drop() {
        let c = Arc::new(ThumbGenCoalescer::new());
        let guard = match c.acquire(key("a.jpg")) {
            Acquired::Generator(g) => g,
            Acquired::Waiter(_) => panic!("first caller should generate"),
        };
        assert!(matches!(c.acquire(key("a.jpg")), Acquired::Waiter(_)));
        // A different key is never contended.
        assert!(matches!(c.acquire(key("b.jpg")), Acquired::Generator(_)));

        drop(guard);
        assert!(matches!(c.acquire(key("a.jpg")), Acquired::Generator(_)));
    }

    #[tokio::test]
    async fn a_cancelled_generator_still_releases_its_slot() {
        // The failure being pinned: a dropped request future used to leak the
        // key, hanging every later request for that thumbnail forever.
        let c = Arc::new(ThumbGenCoalescer::new());
        let c2 = c.clone();

        let task = tokio::spawn(async move {
            let _guard = match c2.acquire(key("a.jpg")) {
                Acquired::Generator(g) => g,
                Acquired::Waiter(_) => panic!("should generate"),
            };
            std::future::pending::<()>().await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(matches!(c.acquire(key("a.jpg")), Acquired::Waiter(_)));

        task.abort();
        let _ = task.await;
        assert!(matches!(c.acquire(key("a.jpg")), Acquired::Generator(_)));
    }
}
