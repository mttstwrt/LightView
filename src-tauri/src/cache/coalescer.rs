// ---------------------------------------------------------------------------
// Thumbnail generation coalescer
// ---------------------------------------------------------------------------
//
// Deduplicates concurrent on-miss thumbnail generation requests for the same
// `(path, tier)` key. The first caller becomes the Generator; concurrent
// callers for the same key become Waiters and resume once the Generator
// signals completion. The actual result is persisted to SQLite, so waiters
// only need a wake-up to re-read the cache.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

use crate::cache::thumbnails::ThumbTier;

pub type ThumbKey = (String, ThumbTier);

pub enum Role {
    /// You got the slot — run the generator, then call `release`.
    Generator,
    /// Another caller is generating — await `notified()` then re-read cache.
    Waiter,
}

#[derive(Default)]
pub struct ThumbGenCoalescer {
    inner: Mutex<HashMap<ThumbKey, Arc<Notify>>>,
}

impl ThumbGenCoalescer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim a slot for `key`. If the key is free the caller becomes the
    /// Generator and is expected to run generation then call `release`.
    /// Otherwise the caller becomes a Waiter and should await the returned
    /// `Notify` (enrol via `Notified::enable` before awaiting to avoid
    /// missing a release that races with this call).
    pub fn acquire(&self, key: ThumbKey) -> (Role, Arc<Notify>) {
        let mut map = self.inner.lock().unwrap();
        if let Some(notify) = map.get(&key) {
            (Role::Waiter, notify.clone())
        } else {
            let notify = Arc::new(Notify::new());
            map.insert(key, notify.clone());
            (Role::Generator, notify)
        }
    }

    /// Called by the Generator after generation (success or failure).
    /// Removes the key and wakes all waiters.
    pub fn release(&self, key: &ThumbKey) {
        let mut map = self.inner.lock().unwrap();
        if let Some(notify) = map.remove(key) {
            notify.notify_waiters();
        }
    }
}
