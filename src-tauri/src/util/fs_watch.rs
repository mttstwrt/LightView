use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::mpsc;

/// A filesystem watcher that monitors a gallery directory for changes.
pub struct FsWatcher {
    _watcher: RecommendedWatcher,
    receiver: mpsc::Receiver<Result<Event, notify::Error>>,
}

impl FsWatcher {
    /// Start watching a directory for changes (non-recursive by default).
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

    /// Poll for pending filesystem events (non-blocking).
    pub fn poll_events(&self) -> Vec<Event> {
        let mut events = Vec::new();
        while let Ok(result) = self.receiver.try_recv() {
            if let Ok(event) = result {
                events.push(event);
            }
        }
        events
    }
}
