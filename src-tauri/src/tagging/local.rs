//! In-process tagging executor: the server host registers itself as a worker
//! in the tagging registry and runs jobs with its own installed plugins.
//!
//! This is the "run it on the server" option next to `lightview-worker`. It
//! goes through the exact same queue — announce, claim, progress heartbeat,
//! cancel back-channel — but skips the whole HTTP leg: files are local, so
//! the plugin reads them in place, and tag writes call
//! `apply_plugin_tags_impl` directly. It only ever runs plugins installed
//! under the server's own `data_dir()/plugins`; remote devices still can't
//! ship code here. Opt-in by installing plugins server-side — with an empty
//! plugins dir the executor never registers and the web UI never offers it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::commands::plugins::PluginTagWrite;
use crate::plugin::input::{
    plan_parts, spawn_local_producer, InputPolicy, LocalPart, MergedItem, PartTracker,
};
use crate::plugin::runner;
use crate::plugin::PluginInfo;
use crate::AppState;

use super::ClaimedJob;

/// Reserved worker id for the in-process executor. `worker_announce` over
/// HTTP rejects it so a remote device can't impersonate the server.
pub const LOCAL_WORKER_ID: &str = "local-server";

const POLL_SECS: u64 = 3;
/// Plugins dir re-scan cadence — installing a plugin server-side shows up in
/// the web UI within this window, no restart needed.
const SCAN_SECS: u64 = 30;
const HEARTBEAT_SECS: u64 = 10;
const APPLY_BATCH: usize = 32;

/// Spawn the executor loop once per process; later calls are no-ops. Called
/// wherever the tagging queue becomes reachable (headless `serve`, desktop
/// `enable_remote_access`).
pub fn ensure_started(state: &AppState) {
    if state.local_tagger_started.swap(true, Ordering::SeqCst) {
        return;
    }
    let state = state.clone();
    tokio::spawn(run(state));
}

async fn run(state: AppState) {
    let worker_name = sysinfo::System::host_name()
        .map(|h| format!("Server ({h})"))
        .unwrap_or_else(|| "Server".to_string());

    let mut plugins: Vec<PluginInfo> = Vec::new();
    let mut last_scan: Option<std::time::Instant> = None;

    loop {
        if last_scan.is_none_or(|t| t.elapsed().as_secs() >= SCAN_SECS) {
            plugins = crate::plugin::scan_plugins(&crate::plugin::default_dir());
            last_scan = Some(std::time::Instant::now());
        }
        if plugins.is_empty() {
            // Nothing to offer; stay out of the registry (a previous entry
            // ages out via the worker TTL) and watch for installs.
            tokio::time::sleep(Duration::from_secs(SCAN_SECS)).await;
            continue;
        }

        // Announce every iteration — it doubles as the TTL keep-alive and the
        // registry dedupes SSE broadcasts when nothing changed.
        if let Err(e) = super::announce_worker(
            &state,
            LOCAL_WORKER_ID.to_string(),
            worker_name.clone(),
            plugins.clone(),
            Some(env!("CARGO_PKG_VERSION").to_string()),
            true,
        )
        .await
        {
            log::warn!("local tagger announce failed: {e}");
        }

        let names: Vec<String> = plugins.iter().map(|p| p.name.clone()).collect();
        match super::claim_job(&state, LOCAL_WORKER_ID.to_string(), names).await {
            Ok(Some(claimed)) => {
                execute_job(&state, claimed).await;
                // The finished job may have unblocked another; claim again
                // immediately.
            }
            Ok(None) => tokio::time::sleep(Duration::from_secs(POLL_SECS)).await,
            Err(e) => {
                log::warn!("local tagger claim failed: {e}");
                tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
            }
        }
    }
}

enum Outcome {
    Finished,
    Cancelled,
    Failed(String),
}

async fn execute_job(state: &AppState, claimed: ClaimedJob) {
    let job = claimed.job;
    log::info!(
        "local tagger claimed job {} — {} on {} images",
        job.id,
        job.display_name,
        claimed.paths.len()
    );

    let outcome = drive_plugin(state, &job.id, &job.plugin_name, claimed.paths).await;
    if let Outcome::Failed(error) = outcome {
        log::error!("local tagging job {} failed: {error}", job.id);
        let _ = super::fail_job(state, job.id, LOCAL_WORKER_ID.to_string(), error).await;
    }
}

async fn drive_plugin(
    state: &AppState,
    job_id: &str,
    plugin_name: &str,
    paths: Vec<String>,
) -> Outcome {
    let (manifest, plugin_dir) =
        match runner::find_plugin(&crate::plugin::default_dir(), plugin_name) {
            Ok(found) => found,
            Err(e) => return Outcome::Failed(format!("plugin '{plugin_name}': {e}")),
        };

    let temp_dir = std::env::temp_dir()
        .join("lightview-local-tagger")
        .join(job_id);
    if let Err(e) = tokio::fs::create_dir_all(&temp_dir).await {
        return Outcome::Failed(format!("cannot create temp dir: {e}"));
    }
    let outcome = drive_prepared(state, job_id, &manifest, &plugin_dir, paths, &temp_dir).await;
    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    outcome
}

async fn drive_prepared(
    state: &AppState,
    job_id: &str,
    manifest: &crate::plugin::manifest::PluginManifest,
    plugin_dir: &std::path::Path,
    paths: Vec<String>,
    temp_dir: &std::path::Path,
) -> Outcome {
    // Files are local, but "local" no longer means "hand over the original".
    // A 448-pixel model asked for 448 pixels, and a video has to become frames
    // for the plugin to see anything at all — the same preparation a remote
    // worker gets from the server, so a plugin cannot behave differently
    // depending on where the job happened to run.
    let policy = InputPolicy::for_manifest(manifest);
    let request_total: usize = paths
        .iter()
        .map(|p| plan_parts(std::path::Path::new(p), &policy).len())
        .sum();

    let (req_tx, req_rx) = mpsc::channel::<runner::PluginRequest>(8);
    let mut running =
        match runner::run_plugin_stream_channel(manifest, plugin_dir, req_rx, Some(request_total))
            .await
        {
            Ok(r) => r,
            Err(e) => return Outcome::Failed(format!("failed to start plugin: {e}")),
        };

    let tracker: Arc<tokio::sync::Mutex<PartTracker<LocalPart>>> =
        Arc::new(tokio::sync::Mutex::new(PartTracker::new()));
    let prepare_failed = Arc::new(AtomicUsize::new(0));
    let producer = spawn_local_producer(
        state.thumb_pool.clone(),
        paths,
        policy,
        temp_dir.to_path_buf(),
        req_tx,
        tracker.clone(),
        prepare_failed.clone(),
    );

    let mut succeeded: usize = 0;
    let mut failed: usize = 0;
    let mut batch: Vec<PluginTagWrite> = Vec::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    heartbeat.reset(); // don't fire immediately
    let mut last_result = tokio::time::Instant::now();

    let outcome = loop {
        tokio::select! {
            maybe = running.results.recv() => {
                let Some(result) = maybe else {
                    let mut t = tracker.lock().await;
                    let orphaned = t.drain_unanswered();
                    let done = t.take_completed();
                    drop(t);
                    for part in orphaned {
                        part.release().await;
                    }
                    collect(done, manifest, &mut batch, &mut failed);
                    if !batch.is_empty() {
                        match push_batch(state, job_id, &mut batch, &mut succeeded, &mut failed).await {
                            Ok(true) => break Outcome::Cancelled,
                            Ok(false) => {}
                            Err(e) => break Outcome::Failed(e),
                        }
                    }
                    break Outcome::Finished;
                };

                let mut t = tracker.lock().await;
                let Some(part) = t.result(result) else {
                    drop(t);
                    log::warn!("local tagging job {job_id}: plugin result for unknown path");
                    continue;
                };
                let done = t.take_completed();
                drop(t);

                last_result = tokio::time::Instant::now();
                part.release().await;
                collect(done, manifest, &mut batch, &mut failed);

                if batch.len() >= APPLY_BATCH {
                    match push_batch(state, job_id, &mut batch, &mut succeeded, &mut failed).await {
                        Ok(true) => break Outcome::Cancelled,
                        Ok(false) => {}
                        Err(e) => break Outcome::Failed(e),
                    }
                }
            }

            _ = heartbeat.tick() => {
                // Flush partial work first (bounds tag latency and preserves
                // progress if the job gets cancelled), then heartbeat — the
                // update refreshes the stall timer and picks up cancellation.
                if !batch.is_empty() {
                    match push_batch(state, job_id, &mut batch, &mut succeeded, &mut failed).await {
                        Ok(true) => break Outcome::Cancelled,
                        Ok(false) => {}
                        Err(e) => break Outcome::Failed(e),
                    }
                    continue;
                }

                // Same reclaim the remote worker does. It matters less here —
                // nothing blocks on a network round trip — but a plugin that
                // skips a request would otherwise lose that file its tags with
                // no error anywhere, and this is where the file is reported.
                let idle = last_result.elapsed();
                let mut t = tracker.lock().await;
                let stale = t.take_stale(idle);
                let done = t.take_completed();
                drop(t);
                for (name, part) in stale {
                    log::warn!(
                        "local tagging job {job_id}: plugin '{}' never answered '{name}'",
                        manifest.name,
                    );
                    part.release().await;
                }
                collect(done, manifest, &mut batch, &mut failed);

                let total_failed = failed + prepare_failed.load(Ordering::Relaxed);
                match super::update_job(
                    state,
                    job_id.to_string(),
                    LOCAL_WORKER_ID.to_string(),
                    succeeded,
                    total_failed,
                )
                .await
                {
                    Ok(u) if u.cancelled => break Outcome::Cancelled,
                    Ok(_) => {}
                    Err(e) => log::warn!("local tagger heartbeat failed: {e}"),
                }
            }
        }
    };

    producer.abort();
    tracker.lock().await.drain_unanswered();
    failed += prepare_failed.load(Ordering::Relaxed);

    match outcome {
        Outcome::Finished => {
            // A clean stream that produced results is a success even if the
            // process exits nonzero; a silent death with zero results is not.
            // Capture the stderr tail first — `finish` consumes the handle.
            let stderr_tail = running.stderr_tail();
            if let Err(e) = running.finish().await {
                if succeeded == 0 {
                    return Outcome::Failed(runner::describe_failure(
                        format!("plugin failed: {e}"),
                        &stderr_tail,
                    ));
                }
                log::warn!("plugin exited abnormally after {succeeded} results: {e}");
            }
            log::info!("local tagging job {job_id} done: {succeeded} tagged, {failed} failed");
            let _ = super::complete_job(
                state,
                job_id.to_string(),
                LOCAL_WORKER_ID.to_string(),
                succeeded,
                failed,
            )
            .await;
            Outcome::Finished
        }
        Outcome::Cancelled => {
            log::info!("local tagging job {job_id} cancelled");
            running.kill().await;
            Outcome::Cancelled
        }
        Outcome::Failed(e) => {
            let explained = running.explain(e);
            running.kill().await;
            Outcome::Failed(explained)
        }
    }
}

/// Turn finished media items into tag writes, counting the ones that produced
/// nothing. One item is one unit of progress, whether it took one request or
/// five frames' worth.
fn collect(
    done: Vec<MergedItem>,
    manifest: &crate::plugin::manifest::PluginManifest,
    batch: &mut Vec<PluginTagWrite>,
    failed: &mut usize,
) {
    for item in done {
        if let Some(err) = item.error {
            log::debug!("{}: {err}", item.path);
            *failed += 1;
            continue;
        }
        batch.push(PluginTagWrite {
            path: item.path,
            tag_prefix: manifest.tag_prefix.clone(),
            version: manifest.version.clone(),
            tags: item.tags,
            meta: item.meta,
        });
    }
}

/// Apply one batch of tag writes and report progress. Returns whether the job
/// was cancelled.
async fn push_batch(
    state: &AppState,
    job_id: &str,
    batch: &mut Vec<PluginTagWrite>,
    succeeded: &mut usize,
    failed: &mut usize,
) -> Result<bool, String> {
    let entries = std::mem::take(batch);
    let applied = crate::commands::plugins::apply_plugin_tags_impl(state, entries)
        .await
        .map_err(|e| format!("apply_plugin_tags failed: {e}"))?;
    *succeeded += applied.succeeded.len();
    for f in &applied.failed {
        log::warn!("{}: tag write rejected: {}", f.path, f.error);
    }
    *failed += applied.failed.len();

    let update = super::update_job(
        state,
        job_id.to_string(),
        LOCAL_WORKER_ID.to_string(),
        *succeeded,
        *failed,
    )
    .await?;
    Ok(update.cancelled)
}
