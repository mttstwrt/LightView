//! Idle backfill worker: generates missing standard-tier thumbnails, then
//! perceptual hashes, while nobody is using the gallery.
//!
//! Purpose: on a headless server (weak CPU, e.g. an N100) thumbnails and
//! phashes are otherwise computed on demand — first browse is slow and the
//! duplicate finder only covers what has been browsed. This worker grinds
//! through the backlog during quiet periods so both are pre-warmed.
//!
//! "Idle" means: no web client is connected (`fs_change_tx` has no SSE
//! subscribers) AND no user-driven thumbnail request landed recently
//! (`last_thumb_activity`, touched by the desktop protocol handler and the
//! frontend batch commands). Both signals are re-checked between small work
//! units, so the worker yields within one batch of a user showing up.
//!
//! *Which* tiers get warmed comes from the gallery's enabled views — see
//! [`crate::views`]. Disabling a view stops its generation; it never deletes
//! rows already cached.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;

use crate::cache::thumbnails::ThumbTier;
use crate::thumb_serve::{self, ThumbOutcome};
use crate::AppState;

/// Poll cadence while waiting for idleness or new work.
const POLL_SECS: u64 = 5;
/// Sleep once the backlog is empty (fs-watch may add files at any time).
const DRAINED_SECS: u64 = 60;
/// How long thumbnail activity must be quiet before working.
const ACTIVITY_QUIET_MS: u64 = 60_000;
/// Missing-thumbnail paths fetched per DB query.
const THUMB_BATCH: usize = 8;
/// Phashes computed per DB-lock acquisition.
const PHASH_BATCH: usize = 64;

/// Tiers the worker pre-warms, derived from the gallery's enabled views
/// ([`crate::views::prewarm_tiers`]). Standard ("m") backs the square grid;
/// Justified ("j") backs the justified layout's base zoom — the phone web
/// client's default view. Justified is aspect-preserving, so it cannot be
/// derived from the square-cropped Standard bytes; it needs its own source
/// decode, which is exactly what idle time is for. Work cycles rotate
/// between the tiers (rather than draining one first) so both views' newest
/// items warm early on a large cold backlog.
///
/// Re-read each cycle rather than captured at start: a view toggled in
/// settings takes effect at the next poll instead of at the next gallery open,
/// and the read is one indexed `gallery_meta` lookup every [`POLL_SECS`].
async fn prewarm_tiers(state: &AppState) -> Vec<ThumbTier> {
    let db = state.cache_db.lock().await;
    match db.as_ref() {
        Some(db) => crate::views::prewarm_tiers(db.conn()),
        None => Vec::new(),
    }
}

/// Paths whose generation failed, grouped by tier. See the field comment in
/// [`start_idle_worker`] for why it isn't a flat set of pairs.
type FailedPaths = HashMap<ThumbTier, HashSet<String>>;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn is_idle(state: &AppState) -> bool {
    if state.fs_change_tx.receiver_count() > 0 {
        return false;
    }
    let last = state.last_thumb_activity.load(Ordering::Relaxed);
    now_ms().saturating_sub(last) >= ACTIVITY_QUIET_MS
}

/// Start (or restart) the idle worker for the currently open gallery. Bumping
/// `idle_generation` makes any previous worker exit at its next check, so a
/// gallery switch never leaves two workers running.
pub fn start_idle_worker(state: &AppState) {
    let my_gen = state.idle_generation.fetch_add(1, Ordering::SeqCst) + 1;
    let state = state.clone();

    tauri::async_runtime::spawn(async move {
        log::info!("Idle backfill worker started (gen {my_gen})");
        // Paths that failed generation once, per tier — skipped instead of
        // retried (and re-logged) forever. Reset on restart. Keyed tier-first
        // so the candidate filter below can probe with a borrowed `&str`; a
        // flat `HashSet<(String, ThumbTier)>` forced a String clone per
        // candidate on every poll just to ask the question.
        let mut failed: FailedPaths = HashMap::new();
        // Round-robin position so a long backlog in one tier doesn't starve
        // the other — each work cycle takes one batch from the next tier
        // with pending work.
        let mut tier_cursor = 0usize;

        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(POLL_SECS)).await;
            if state.idle_generation.load(Ordering::SeqCst) != my_gen {
                break;
            }
            if !is_idle(&state) {
                continue;
            }

            let tiers = prewarm_tiers(&state).await;
            let mut worked = false;
            for offset in 0..tiers.len() {
                let tier = tiers[(tier_cursor + offset) % tiers.len()];
                if backfill_thumbnails(&state, tier, &mut failed).await {
                    tier_cursor = (tier_cursor + offset + 1) % tiers.len();
                    worked = true;
                    break;
                }
            }
            let worked = worked || backfill_phashes(&state).await;

            if !worked {
                // Backlog drained — check back occasionally for new files.
                tokio::time::sleep(tokio::time::Duration::from_secs(DRAINED_SECS)).await;
            }
        }
        log::info!("Idle backfill worker exited (gen {my_gen})");
    });
}

/// Generate one tier's thumbnails for a small batch of unthumbnailed media.
/// Returns whether any work was attempted (i.e. the backlog isn't empty).
///
/// The backlog walks newest-first (`date_taken` falls back to file mtime at
/// index time, `date_added` breaks ties) — the same order the default
/// date-descending sort presents, so the first screen a phone opens onto is
/// the first thing warmed rather than whatever the table scan happened upon.
async fn backfill_thumbnails(
    state: &AppState,
    tier: ThumbTier,
    failed: &mut FailedPaths,
) -> bool {
    let failed_here = failed.entry(tier).or_default();
    let batch: Vec<String> = {
        let db = state.cache_db.lock().await;
        let Some(db) = db.as_ref() else { return false };
        // The SQL string is per-tier but each is stable, so prepare_cached
        // still keys on a small fixed set of statements.
        let sql = format!(
            "SELECT m.path FROM media_meta m
             LEFT JOIN {} t ON t.path = m.path
             WHERE t.path IS NULL
             ORDER BY m.date_taken DESC NULLS LAST, m.date_added DESC
             LIMIT ?1",
            tier.table()
        );
        let mut stmt = match db.conn().prepare_cached(&sql) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("Idle worker: missing-thumbnail query failed ({tier:?}): {e}");
                return false;
            }
        };
        // Over-fetch by the failed count so a backlog of known failures
        // doesn't mask fresh work forever.
        let limit = THUMB_BATCH + failed_here.len();
        match stmt.query_map([limit], |row| row.get::<_, String>(0)) {
            Ok(rows) => rows
                .filter_map(|r| r.ok())
                .filter(|p| !failed_here.contains(p.as_str()))
                .take(THUMB_BATCH)
                .collect(),
            Err(e) => {
                log::warn!("Idle worker: missing-thumbnail query failed ({tier:?}): {e}");
                return false;
            }
        }
    };

    if batch.is_empty() {
        return false;
    }

    for path in batch {
        if !is_idle(state) {
            return true; // someone showed up mid-batch — yield now
        }
        match thumb_serve::get_or_generate(state, tier, path.clone()).await {
            ThumbOutcome::Hit { .. } => {}
            ThumbOutcome::Miss => {
                log::debug!("Idle worker: thumbnail generation failed for {path} ({tier:?})");
                failed_here.insert(path);
            }
            ThumbOutcome::NoGallery => return false,
        }
    }
    true
}

/// Compute one batch of missing phashes. Returns whether anything was computed.
async fn backfill_phashes(state: &AppState) -> bool {
    let db = state.cache_db.lock().await;
    let Some(db) = db.as_ref() else { return false };
    match db.compute_phashes_batch(PHASH_BATCH) {
        Ok(n) => n > 0,
        Err(e) => {
            log::warn!("Idle worker: phash batch failed: {e}");
            false
        }
    }
}
