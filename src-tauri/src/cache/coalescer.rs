// ---------------------------------------------------------------------------
// Thumbnail generation coalescer
// ---------------------------------------------------------------------------
//
// Deduplicates concurrent on-miss thumbnail generation requests for the same
// `(path, tier)` key. The first caller becomes the Generator; concurrent
// callers for the same key become Waiters and resume once the Generator
// signals completion. The actual result is persisted to SQLite, so waiters
// only need a wake-up to re-read the cache.
//
// The generator's slot is held by an RAII guard: releasing (and waking
// waiters) happens on Drop, so it also happens when the generating future is
// *cancelled* — an HTTP request future is dropped whenever the browser aborts
// the fetch, which the grids' virtual scrolling does constantly. The previous
// explicit-release design leaked the key on cancellation, permanently hanging
// every later request for that thumbnail (and, once a few piled up, the
// browser's whole per-origin connection budget — the "server stops
// responding" symptom).

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
