//! Whether anyone still has this gallery open, and whether it is safe to stop.
//!
//! `lightview <dir>` is started by a click — "Open with LightView" in a file
//! manager — that nobody associates with a process lifetime. Closing the window
//! left it serving nothing, holding the gallery lock, its watcher, its idle
//! worker and its thread pool, until the user went looking for it.
//!
//! **The signal already existed: the SSE stream at `GET /api/events`.** A
//! browser tears that connection down when the tab closes, and the fifteen-
//! second keep-alive means a connection whose peer is gone is noticed within
//! one interval even when no events flow. A count of live streams is a count of
//! open windows. Nothing is polled and no second channel exists.
//!
//! **Presence is not activity, and the difference is the whole design.** There
//! is already an idle signal — [`crate::pipeline::serve::Activity`], the time
//! of the last user-driven request — and reusing it here would kill a session
//! under someone reading a page, because a tab parked on a grid makes no
//! requests for hours. `Activity`'s own doc warns against the mirror of this
//! mistake: reading the subscriber count as activity. Both directions are now
//! named where someone would reach for the wrong one.
//!
//! # Why this lives in `util` and not in `server`
//!
//! Two different layers report into it. The HTTP layer counts windows; the
//! background services count durable work in flight, because HTTP graceful
//! shutdown covers requests and the writes that actually matter here are not
//! requests — the open-time enrichment pass and the hourly companion sweep run
//! detached and write sidecars for minutes after the first screen is painted.
//! A `server::` type would mean [`crate::services::gallery`] importing from the
//! adapter above it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

// `tokio::time::Instant`, not `std::time::Instant`: the grace period is
// measured against the same clock the sleep below uses. With the std clock the
// two disagree under a paused runtime, which makes the policy untestable —
// and a policy about waiting that cannot be tested without waiting five real
// minutes is one nobody will test.
use tokio::time::Instant;

/// How often the watchdog looks. Cheap: three atomic loads.
const TICK: Duration = Duration::from_secs(5);

/// How long every window must have been gone before the process stops.
///
/// **Five minutes, not thirty seconds, because of tab discard.** Chrome and
/// Safari drop backgrounded tabs under memory pressure, closing their sockets
/// while the tab stays in the strip and reloads when it is focused again. At
/// thirty seconds "the last window closed" and "the last window was
/// backgrounded" are the same observation. Waiting longer costs nothing: the
/// process is idle, and a `lightview <dir>` inside the window attaches to it
/// rather than starting a second one, which is the outcome wanted anyway.
///
/// The other direction is safe for a reason specific to the loopback bind: a
/// suspended laptop does not drop a TCP connection whose endpoints are both on
/// the same machine, so a closed lid is not a closed window.
pub const GRACE: Duration = Duration::from_secs(5 * 60);

/// Open windows, and durable work in flight.
#[derive(Debug, Default)]
pub struct Presence {
    windows: AtomicUsize,
    /// Whether a window has *ever* been open. Until one has, the watchdog is
    /// not armed — a browser that is slow to start would otherwise be raced by
    /// the process it is being started for.
    seen_any: AtomicBool,
    busy: AtomicUsize,
}

impl Presence {
    /// Count one open window for as long as the returned guard lives.
    ///
    /// Held by the SSE stream itself rather than by the handler that built it,
    /// so it is dropped when the response body is dropped — which is what
    /// "the client went away" looks like from here.
    pub fn window(self: &Arc<Self>) -> Window {
        self.windows.fetch_add(1, Ordering::Relaxed);
        self.seen_any.store(true, Ordering::Relaxed);
        Window(Arc::clone(self))
    }

    /// Mark durable work in flight for as long as the returned guard lives.
    ///
    /// Take one around anything that writes a companion. Sidecars are the only
    /// durable data there is, and a `modify_companion` interrupted between its
    /// lock and its rename leaves a temp file in the user's gallery — the one
    /// tree the design promises is safe to `rsync`.
    pub fn busy(self: &Arc<Self>) -> Busy {
        self.busy.fetch_add(1, Ordering::Relaxed);
        Busy(Arc::clone(self))
    }

    pub fn windows(&self) -> usize {
        self.windows.load(Ordering::Relaxed)
    }

    pub fn seen_any(&self) -> bool {
        self.seen_any.load(Ordering::Relaxed)
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed) > 0
    }
}

/// One open window. See [`Presence::window`].
pub struct Window(Arc<Presence>);

impl Drop for Window {
    fn drop(&mut self) {
        self.0.windows.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Durable work in flight. See [`Presence::busy`].
pub struct Busy(Arc<Presence>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.busy.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Resolves once every window has been closed for [`GRACE`], and no durable
/// work is in flight.
///
/// **Local mode only.** A `--serve` deployment must outlive every client; a
/// phone locking its screen is not a shutdown request, and the caller there
/// passes a future that never resolves instead of calling this.
///
/// The zero is re-checked each tick rather than armed once, so a window opening
/// inside the window simply disarms it and there is no cancellation to get
/// wrong.
pub async fn last_window_closed(presence: Arc<Presence>) {
    let mut zero_since: Option<Instant> = None;
    loop {
        tokio::time::sleep(TICK).await;
        if !presence.seen_any() {
            continue;
        }
        if presence.windows() > 0 || presence.is_busy() {
            zero_since = None;
            continue;
        }
        let since = *zero_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= GRACE {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_is_counted_for_as_long_as_its_guard_lives() {
        let p = Arc::new(Presence::default());
        assert_eq!(p.windows(), 0);
        assert!(!p.seen_any(), "nothing has connected yet");

        let first = p.window();
        let second = p.window();
        assert_eq!(p.windows(), 2);
        assert!(p.seen_any());

        drop(first);
        assert_eq!(p.windows(), 1, "one tab of two closing is not the last one");
        drop(second);
        assert_eq!(p.windows(), 0);
        assert!(p.seen_any(), "having been open is not forgotten");
    }

    #[test]
    fn durable_work_counts_separately_from_windows() {
        let p = Arc::new(Presence::default());
        let work = p.busy();
        assert!(p.is_busy());
        assert_eq!(p.windows(), 0, "background work is not a window");
        assert!(!p.seen_any(), "and it does not arm the watchdog");
        drop(work);
        assert!(!p.is_busy());
    }

    /// Step the paused clock one tick at a time, letting the watchdog run
    /// between steps.
    ///
    /// Not one long `advance`: the watchdog measures a *continuous* zero by
    /// observing it on successive ticks, and a single jump is one observation
    /// however far it moves the clock — it would set `zero_since` to the far
    /// side of the jump and start counting from there. Stepping is how real
    /// time reaches the loop.
    async fn ticks_until_done(watch: &tokio::task::JoinHandle<()>, ticks: usize) -> bool {
        for _ in 0..ticks {
            tokio::time::advance(TICK).await;
            tokio::task::yield_now().await;
            if watch.is_finished() {
                return true;
            }
        }
        false
    }

    /// Comfortably past the grace period, in ticks.
    const PAST_GRACE: usize = (GRACE.as_secs() / TICK.as_secs()) as usize + 4;

    /// The watchdog must not fire before a browser has ever connected, or it
    /// races the browser it was started for.
    #[tokio::test(start_paused = true)]
    async fn nothing_exits_before_the_first_window_opens() {
        let p = Arc::new(Presence::default());
        let watch = tokio::spawn(last_window_closed(Arc::clone(&p)));
        assert!(
            !ticks_until_done(&watch, PAST_GRACE * 3).await,
            "exited without a client ever arriving"
        );
        watch.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn the_last_window_closing_ends_the_session() {
        let p = Arc::new(Presence::default());
        let window = p.window();
        let watch = tokio::spawn(last_window_closed(Arc::clone(&p)));

        assert!(
            !ticks_until_done(&watch, PAST_GRACE).await,
            "exited with a window still open"
        );

        drop(window);
        assert!(
            ticks_until_done(&watch, PAST_GRACE).await,
            "the last window closed and the session did not end"
        );
    }

    /// A reload closes the stream and opens a new one. The grace period is what
    /// keeps that from reading as the end of the session.
    #[tokio::test(start_paused = true)]
    async fn a_reload_inside_the_grace_period_does_not_end_it() {
        let p = Arc::new(Presence::default());
        let window = p.window();
        let watch = tokio::spawn(last_window_closed(Arc::clone(&p)));

        drop(window);
        assert!(
            !ticks_until_done(&watch, PAST_GRACE / 2).await,
            "a reload was mistaken for a close"
        );
        let reconnected = p.window();
        assert!(
            !ticks_until_done(&watch, PAST_GRACE).await,
            "the reconnected window was not noticed"
        );

        drop(reconnected);
        assert!(ticks_until_done(&watch, PAST_GRACE).await);
    }

    /// Enrichment runs detached for minutes on a first open. Exiting under it
    /// is what leaves a `.tmp` in someone's gallery.
    #[tokio::test(start_paused = true)]
    async fn durable_work_outlasts_the_last_window() {
        let p = Arc::new(Presence::default());
        let window = p.window();
        let writing = p.busy();
        let watch = tokio::spawn(last_window_closed(Arc::clone(&p)));

        drop(window);
        assert!(
            !ticks_until_done(&watch, PAST_GRACE * 2).await,
            "exited mid companion write"
        );

        drop(writing);
        assert!(ticks_until_done(&watch, PAST_GRACE).await);
    }
}
