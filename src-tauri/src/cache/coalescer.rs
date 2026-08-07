//! Deduplicates concurrent on-miss thumbnail generation for the same
//! `(path, tier)`.
//!
//! The first caller becomes the generator; concurrent callers for the same key
//! wait on a `Notify` and re-read the cache once woken, since the result is
//! persisted to SQLite rather than passed between them.
//!
//! The generator's slot is held by an RAII guard, so releasing it — and waking
//! the waiters — also happens when the generating future is *cancelled*. That
//! is not a corner case: an HTTP request future is dropped whenever the browser
//! aborts a fetch, which the grids' virtual scrolling does constantly. The
//! previous explicit-release design leaked the key on cancellation, permanently
//! hanging every later request for that thumbnail and, once a few piled up, the
//! browser's whole per-origin connection budget. That was the "server stops
//! responding until restart" symptom.
//!
//! Waiters must enrol in the wake queue before re-checking the cache, or they
//! can miss a notify that races the generator's release; `thumb_serve` also
//! bounds its retries, so a persistently failing source degrades to a miss
//! rather than spinning.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

use crate::cache::thumbnails::ThumbTier;

pub type ThumbKey = (String, ThumbTier);

/// Outcome of [`ThumbGenCoalescer::acquire`].
pub enum Acquired {
    /// You got the slot — run the generator while holding the guard. The slot
    /// is released and all waiters are woken when the guard drops, on every
    /// exit path (success, error, or cancellation mid-await).
    Generator(GenerationGuard),
    /// Another caller is generating — await `notified()` (enrol via
    /// `Notified::enable` before re-checking the cache to avoid missing a
    /// release that races with `acquire`), then re-read the cache.
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
        let mut map = self.inner.lock().unwrap();
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

    /// Remove the key and wake all waiters. Called from the guard's Drop.
    fn release(&self, key: &ThumbKey) {
        let mut map = self.inner.lock().unwrap();
        if let Some(notify) = map.remove(key) {
            notify.notify_waiters();
        }
    }
}
