//! A thin, non-blocking wrapper over `notify` for the open gallery.
//!
//! Deliberately only a transport. [`FsWatcher::poll`] drains whatever the
//! watcher thread has queued and returns immediately; the caller polls it on a
//! timer and decides what a burst of events means. That split matters because
//! the interesting logic is all policy: a single user action — a copy, an
//! export, an unzip — produces a storm of notify events, and the consumer
//! coalesces them into one refresh rather than re-indexing per event.
//!
//! **Errors are part of the stream, not noise to be dropped.** An earlier
//! implementation did `if let Ok(event)` here, and inotify's two most
//! consequential conditions arrive through exactly that channel: watch-limit
//! exhaustion and queue overflow, which make the watcher go *partially* deaf —
//! some subtrees stop ingesting and nothing says so — and the unmount signal,
//! which is a NAS dropping out from under a running server. So `poll` returns
//! the results and the caller is obliged to look at them.
//!
//! Only changes made *after* the watcher starts are seen; a restart re-indexes
//! from scratch, which is why the watcher must be armed before the readiness
//! gate opens.

use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::mpsc;

/// A filesystem watcher over a gallery directory.
pub struct FsWatcher {
    _watcher: RecommendedWatcher,
    receiver: mpsc::Receiver<Result<Event, notify::Error>>,
}

impl FsWatcher {
    /// Start watching a directory.
    ///
    /// `path` must be the **canonical** root. The database keys on paths
    /// relative to the canonical root, so a watcher armed on the user-supplied
    /// one fails `strip_prefix` on every event — which looks exactly like "not
    /// in this gallery", and means a symlinked gallery ingests nothing until a
    /// restart.
    pub fn new(path: &Path, recursive: bool) -> Result<Self, notify::Error> {
        let (tx, rx) = mpsc::channel();

        let mut watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.send(res);
            },
            Config::default(),
        )?;

        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };

        watcher.watch(path, mode)?;

        Ok(Self {
            _watcher: watcher,
            receiver: rx,
        })
    }

    /// Drain pending results without blocking.
    ///
    /// Returns the raw `Result`s: an `Err` is a watcher-level condition the
    /// caller has to act on, not an event it can skip.
    pub fn poll(&self) -> Vec<Result<Event, notify::Error>> {
        let mut out = Vec::new();
        while let Ok(result) = self.receiver.try_recv() {
            out.push(result);
        }
        out
    }
}
