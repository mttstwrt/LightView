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

use std::collections::HashSet;
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
        // Paths that failed generation once — skip them instead of retrying
        // (and re-logging) forever. Reset on restart.
        let mut failed: HashSet<String> = HashSet::new();

        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(POLL_SECS)).await;
            if state.idle_generation.load(Ordering::SeqCst) != my_gen {
                break;
            }
            if !is_idle(&state) {
                continue;
            }

            let worked = backfill_thumbnails(&state, &mut failed).await
                || backfill_phashes(&state).await;

            if !worked {
                // Backlog drained — check back occasionally for new files.
                tokio::time::sleep(tokio::time::Duration::from_secs(DRAINED_SECS)).await;
            }
        }
        log::info!("Idle backfill worker exited (gen {my_gen})");
    });
}

/// Generate standard-tier thumbnails for a small batch of unthumbnailed media.
/// Returns whether any work was attempted (i.e. the backlog isn't empty).
async fn backfill_thumbnails(state: &AppState, failed: &mut HashSet<String>) -> bool {
    let batch: Vec<String> = {
        let db = state.cache_db.lock().await;
        let Some(db) = db.as_ref() else { return false };
        let mut stmt = match db.conn().prepare_cached(
            "SELECT m.path FROM media_meta m
             LEFT JOIN thumbnails t ON t.path = m.path
             WHERE t.path IS NULL LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("Idle worker: missing-thumbnail query failed: {e}");
                return false;
            }
        };
        // Over-fetch by the failed count so a backlog of known failures
        // doesn't mask fresh work forever.
        let limit = THUMB_BATCH + failed.len();
        match stmt.query_map([limit], |row| row.get::<_, String>(0)) {
            Ok(rows) => rows
                .filter_map(|r| r.ok())
                .filter(|p| !failed.contains(p))
                .take(THUMB_BATCH)
                .collect(),
            Err(e) => {
                log::warn!("Idle worker: missing-thumbnail query failed: {e}");
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
        match thumb_serve::get_or_generate(state, ThumbTier::Standard, path.clone()).await {
            ThumbOutcome::Hit { .. } => {}
            ThumbOutcome::Miss => {
                log::debug!("Idle worker: thumbnail generation failed for {path}");
                failed.insert(path);
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
