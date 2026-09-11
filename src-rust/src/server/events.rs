//! Change notification: one broadcast channel, relayed as SSE.
//!
//! There were two channels, for one reason: the filesystem channel's subscriber
//! count doubled as the "is anyone watching?" signal for the idle worker, and
//! tagging traffic must not make the server think a user is present. That
//! signal is abolished — [`crate::pipeline::serve::Activity`] is the whole of
//! it now — so the reason is gone and the second channel with it.
//!
//! **But the two channels carried different lag contracts, and merging them
//! naively collapses both into the expensive one.** On `RecvError::Lagged` the
//! filesystem relay used to emit an empty payload that the client read as
//! "refetch everything", while the tagging relay simply continued, safe because
//! every tagging event is a full snapshot the client re-syncs anyway. With one
//! channel the receiver cannot tell which domain it missed, so the safe answer
//! would be the filesystem answer — and tagging is by far the heavier producer.
//! Lag would become common exactly where it was rare, and each occurrence would
//! cost every connected client a full-library payload.
//!
//! So: **one channel, typed lag recovery.** Every event names the domain it
//! belongs to; on `Lagged` the relay emits a single [`Event::Resync`] naming the
//! domains that may have been missed, and the client re-fetches only those.
//!
//! And **job progress is throttled to at most one message a second**. A
//! progress bar does not need 32-file granularity, and it is the only
//! high-rate producer on the shared channel.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::broadcast;

use crate::path::RelPath;

/// How many events a slow subscriber may fall behind before it is lagged.
///
/// Generous, because the recovery is a re-fetch: a phone that backgrounds for a
/// moment should come back to its events, not to a resync.
const CAPACITY: usize = 512;

/// At most one job-progress broadcast per second.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// What a client would have to re-fetch after missing events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Domain {
    /// The item list: additions, removals, metadata.
    Items,
    /// Tags and the autocomplete vocabulary.
    Tags,
    /// Plugin run state.
    Jobs,
}

/// One notification, as it reaches a client.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Event {
    /// Files appeared or vanished. **Carries what changed**, so a client
    /// splices rather than re-fetching: one phone upload used to cost every
    /// connected client a full-library payload.
    FsChanged {
        added: Vec<RelPath>,
        removed: Vec<RelPath>,
    },
    /// One file's metadata changed — a rating, a colour label, a tag write.
    ItemChanged { path: RelPath },
    /// Tags were re-indexed from companions; the vocabulary may have moved.
    TagsIndexed,
    /// A plugin run advanced. Throttled; see [`Events::job_progress`].
    JobProgress {
        plugin: String,
        done: u32,
        total: u32,
    },
    /// A plugin run ended, successfully or not.
    JobFinished { plugin: String, error: Option<String> },
    /// **You may have missed something in these domains.** Re-fetch exactly
    /// them, and nothing else.
    Resync { domains: Vec<Domain> },
}

impl Event {
    /// Which domain a missed event would have belonged to. Used to build the
    /// `Resync` payload without the receiver having to guess.
    pub fn domain(&self) -> Domain {
        match self {
            Event::FsChanged { .. } | Event::ItemChanged { .. } => Domain::Items,
            Event::TagsIndexed => Domain::Tags,
            Event::JobProgress { .. } | Event::JobFinished { .. } => Domain::Jobs,
            // A resync that is itself lagged is still a resync.
            Event::Resync { .. } => Domain::Items,
        }
    }
}

/// The publisher. One per open gallery.
pub struct Events {
    tx: broadcast::Sender<Event>,
    last_progress: Mutex<Option<Instant>>,
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

impl Events {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self {
            tx,
            last_progress: Mutex::new(None),
        }
    }

    /// Publish. Errors only when nobody is subscribed, which is ordinary.
    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// Publish job progress, at most once a second.
    ///
    /// A terminal message is never dropped — only the intermediate ticks are.
    pub fn job_progress(&self, plugin: &str, done: u32, total: u32) {
        let mut last = self.last_progress.lock().expect("progress clock poisoned");
        let now = Instant::now();
        let due = last.is_none_or(|t| now.duration_since(t) >= PROGRESS_INTERVAL);
        // The last file is always reported, or a run appears to stall at 99%.
        if !due && done < total {
            return;
        }
        *last = Some(now);
        drop(last);
        self.send(Event::JobProgress {
            plugin: plugin.to_string(),
            done,
            total,
        });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

/// Drive one subscriber, turning lag into a typed resync.
///
/// Returns `None` when the channel has closed. A late subscriber sees only
/// events from subscription onward — a reconnecting phone re-fetches boot
/// state rather than replaying history, which is what its `onopen` is for.
pub async fn next_for_client(rx: &mut broadcast::Receiver<Event>) -> Option<Event> {
    match rx.recv().await {
        Ok(event) => Some(event),
        Err(broadcast::error::RecvError::Closed) => None,
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            // We do not know *which* events were dropped, only how many, so the
            // honest answer names every domain. That is the expensive answer,
            // which is why the throttle above exists: the domain that produces
            // enough traffic to cause lag is the one that does not need
            // per-message delivery.
            log::warn!("an SSE subscriber lagged by {skipped} events; sending a resync");
            Some(Event::Resync {
                domains: vec![Domain::Items, Domain::Tags, Domain::Jobs],
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_subscriber_receives_what_is_published() {
        let events = Events::new();
        let mut rx = events.subscribe();
        events.send(Event::TagsIndexed);
        assert!(matches!(
            next_for_client(&mut rx).await,
            Some(Event::TagsIndexed)
        ));
    }

    #[tokio::test]
    async fn lag_becomes_one_resync_rather_than_a_flood() {
        let events = Events::new();
        let mut rx = events.subscribe();
        for _ in 0..(CAPACITY + 10) {
            events.send(Event::TagsIndexed);
        }
        match next_for_client(&mut rx).await {
            Some(Event::Resync { domains }) => assert_eq!(domains.len(), 3),
            other => panic!("expected a resync, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn job_progress_is_throttled_but_never_drops_the_last_one() {
        let events = Events::new();
        let mut rx = events.subscribe();

        for done in 1..=5u32 {
            events.job_progress("wd-tagger", done, 10);
        }
        // One tick got through; the rest were inside the interval.
        assert!(matches!(
            rx.try_recv(),
            Ok(Event::JobProgress { done: 1, .. })
        ));
        assert!(rx.try_recv().is_err(), "the throttle let a second tick past");

        // The terminal one is never throttled, or a run appears to stall.
        events.job_progress("wd-tagger", 10, 10);
        assert!(matches!(
            rx.try_recv(),
            Ok(Event::JobProgress { done: 10, total: 10, .. })
        ));
    }

    #[test]
    fn every_event_names_a_domain_a_client_can_refetch() {
        // The resync payload is built from these, so an event whose domain is
        // wrong sends the client to re-fetch the wrong thing.
        let p = RelPath::new("a.jpg").unwrap();
        assert_eq!(
            Event::FsChanged { added: vec![p.clone()], removed: vec![] }.domain(),
            Domain::Items
        );
        assert_eq!(Event::ItemChanged { path: p }.domain(), Domain::Items);
        assert_eq!(Event::TagsIndexed.domain(), Domain::Tags);
        assert_eq!(
            Event::JobFinished { plugin: "x".into(), error: None }.domain(),
            Domain::Jobs
        );
    }
}
