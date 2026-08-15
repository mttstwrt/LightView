//! The worker's claim/execute loop.
//!
//! One job at a time: claim → spawn ONE plugin subprocess for the whole job
//! (so ML models load once) → a downloader task streams image bytes from the
//! server into a temp dir and feeds the paths to the plugin, bounded by a
//! semaphore on files-on-disk → results are mapped back to server paths and
//! pushed in batches via `apply_plugin_tags`, with `update_tagging_job` as
//! progress + heartbeat + cancellation back-channel.
//!
//! The files-on-disk bound is a sliding window, not a job size: a job of any
//! length streams through it as permits recycle. What guarantees they recycle
//! is [`is_stale`] — without it a single unanswered request permanently
//! consumed a slot, and 64 of them stopped the job dead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lightview_lib::plugin::manifest::PluginManifest;
use lightview_lib::plugin::runner::{self, PluginRequest};
use lightview_lib::plugin::PluginInfo;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore};

use crate::config::WorkerConfig;
use crate::http::ServerClient;

/// Max downloaded-but-not-yet-tagged files on disk. Backpressure for the
/// downloader; the plugin's own stdin buffering is unbounded, so the request
/// channel alone wouldn't bound disk usage.
///
/// This is a *window*, not a job size limit: permits are recycled as results
/// land, so a job of any length streams through it. What used to make it read
/// like a limit is that a permit could leak permanently — see
/// [`is_stale`], which is what guarantees every permit comes back.
const MAX_PENDING_FILES: usize = 64;
/// Tag writes per `apply_plugin_tags` batch.
const APPLY_BATCH: usize = 32;
/// Heartbeat cadence while a job runs (server requeues after 90s of silence;
/// this also bounds how long a cancel takes to reach the worker).
const HEARTBEAT_SECS: u64 = 10;
/// Announce cadence (server worker TTL is 45s).
const ANNOUNCE_SECS: u64 = 15;
/// How long the plugin may go without producing a single *matched* result
/// before the job is failed instead of left hanging.
///
/// Generous because a tagger's first run downloads and loads its model before
/// the first result, and from here that is indistinguishable from a wedge. This
/// is the outer backstop; [`is_stale`] is what keeps an ordinary job moving.
///
/// Only results that matched a pending entry refresh this timer. Counting
/// unmatched ones — a plugin echoing back a path we cannot key on — kept it
/// permanently fresh in exactly the case it exists to catch.
const NO_RESULT_STALL_SECS: u64 = 20 * 60;
/// A downloaded file is abandoned once the plugin has answered this many
/// *other* requests since it was sent.
///
/// Deliberately a count rather than a clock: a slow tagger legitimately leaves
/// a file queued for a long time, so any wall-clock per-file deadline is either
/// too tight for a CPU-only model or too loose to be useful. At most
/// `MAX_PENDING_FILES` requests are ever outstanding, so watching twice that
/// many *other* files get answered means this one was skipped — no plugin
/// reorders across a window it cannot see.
const STALE_AFTER_RESULTS: u64 = (MAX_PENDING_FILES * 2) as u64;
/// A file outstanding this long while the plugin sits idle is abandoned too.
///
/// The count rule above cannot fire at the tail of a job: if the plugin skips
/// the last few requests, no further results arrive to count. This clears them
/// so the downloader's sender drops, the plugin sees EOF, and the job finishes
/// instead of waiting out `NO_RESULT_STALL_SECS`. Only applies once the plugin
/// has produced at least one result, so a first-run model load is never
/// mistaken for it.
const IDLE_RECLAIM_SECS: u64 = 5 * 60;

/// Job snapshot as returned by `claim_tagging_job` (camelCase, flattened with
/// the resolved path list).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimedJob {
    pub id: String,
    pub plugin_name: String,
    // (The wire also carries tag_prefix; the worker tags with its local
    // manifest's prefix/version instead, which is what actually ran.)
    pub display_name: String,
    pub total: usize,
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateResult {
    pub cancelled: bool,
}

#[derive(Debug, Deserialize)]
pub struct ApplyResult {
    pub succeeded: Vec<String>,
    pub failed: Vec<ApplyFailure>,
}

#[derive(Debug, Deserialize)]
pub struct ApplyFailure {
    pub path: String,
    pub error: String,
}

pub struct LocalPlugin {
    pub manifest: PluginManifest,
    pub dir: PathBuf,
}

/// Announce, then poll for jobs forever. Transport errors back off
/// exponentially; a claim rejected with "unknown worker" (server restarted and
/// lost the registry) triggers a re-announce.
pub async fn run_loop(config: WorkerConfig, plugins: Vec<LocalPlugin>) {
    let client = match ServerClient::new(&config.server_url, &config.cookie, &config.cert_sha256) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return;
        }
    };

    let infos: Vec<PluginInfo> = plugins.iter().map(|p| PluginInfo::from(&p.manifest)).collect();
    let plugin_names: Vec<String> = infos.iter().map(|p| p.name.clone()).collect();

    let mut last_announce: Option<std::time::Instant> = None;
    let mut backoff_secs: u64 = 1;

    loop {
        // Announce doubles as registration and keep-alive; claims refresh the
        // TTL too, so this only needs to run every ANNOUNCE_SECS.
        if last_announce.map_or(true, |t| t.elapsed().as_secs() >= ANNOUNCE_SECS) {
            match announce(&client, &config, &infos).await {
                Ok(()) => last_announce = Some(std::time::Instant::now()),
                Err(e) => {
                    log::warn!("announce failed: {e}");
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(60);
                    continue;
                }
            }
            backoff_secs = 1;
        }

        let claim: Result<Option<ClaimedJob>, _> = client
            .invoke(
                "claim_tagging_job",
                json!({ "workerId": config.worker_id, "pluginNames": plugin_names }),
            )
            .await;

        match claim {
            Ok(Some(job)) => {
                log::info!(
                    "claimed job {} — {} on {} images",
                    job.id,
                    job.display_name,
                    job.total
                );
                let Some(plugin) = plugins.iter().find(|p| p.manifest.name == job.plugin_name)
                else {
                    // Shouldn't happen (we only claim plugins we announced),
                    // but a stale plugins dir could get us here.
                    let _ = client
                        .invoke::<serde_json::Value>(
                            "fail_tagging_job",
                            json!({
                                "jobId": job.id,
                                "workerId": config.worker_id,
                                "error": format!("plugin '{}' not installed on worker", job.plugin_name),
                            }),
                        )
                        .await;
                    continue;
                };
                execute_job(&client, &config, plugin, job).await;
                // A finished job likely changed what's claimable; poll again
                // immediately.
            }
            Ok(None) => {
                tokio::time::sleep(Duration::from_secs(config.poll_secs)).await;
            }
            Err(e) => {
                if e.message.contains("unknown worker") {
                    last_announce = None; // re-announce on the next iteration
                } else {
                    log::warn!("claim failed: {e}");
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(60);
                }
            }
        }
    }
}

async fn announce(
    client: &ServerClient,
    config: &WorkerConfig,
    plugins: &[PluginInfo],
) -> Result<(), crate::http::InvokeError> {
    client
        .invoke::<serde_json::Value>(
            "worker_announce",
            json!({
                "workerId": config.worker_id,
                "workerName": config.worker_name,
                "plugins": plugins,
                // So a server can tell a freshly built worker from the one that
                // has been running since March — the other half of "what is
                // actually running on that machine", the plugin's own
                // api_version being the first.
                "workerVersion": env!("CARGO_PKG_VERSION"),
            }),
        )
        .await
        .map(|_| ())
}

/// A downloaded file waiting for its plugin result. Holds the semaphore permit
/// so disk usage stays bounded; dropped (releasing the permit) once the result
/// lands and the temp file is deleted, or once [`is_stale`] gives up on it.
struct Pending {
    server_path: String,
    temp_path: PathBuf,
    sent_at: tokio::time::Instant,
    /// Total matched results at the moment this file was handed to the plugin.
    /// The difference against the running total is how many *other* files the
    /// plugin has answered while this one waited.
    results_when_sent: u64,
    _permit: OwnedSemaphorePermit,
}

/// Should a downloaded file that has not been answered be given up on?
///
/// This is the whole reason a job of any size can be started and walked away
/// from. A plugin that skips a request, or answers with a path the worker
/// cannot key on, used to strand that file's disk permit for the rest of the
/// job; at `MAX_PENDING_FILES` stranded permits the downloader blocked forever
/// and the job hung — which read as "batches larger than 64 fail". Giving up on
/// the file instead costs one failed image and lets the window slide on, so the
/// bound stays a window rather than becoming a ceiling.
///
/// Pure so the two rules can be tested without a plugin or a clock; see the
/// constants for why each exists.
fn is_stale(
    results_since_sent: u64,
    outstanding: Duration,
    plugin_idle: Duration,
    plugin_has_answered: bool,
) -> bool {
    if results_since_sent >= STALE_AFTER_RESULTS {
        return true;
    }
    let idle_limit = Duration::from_secs(IDLE_RECLAIM_SECS);
    plugin_has_answered && plugin_idle >= idle_limit && outstanding >= idle_limit
}

/// Key a temp file by its name rather than the full path we handed the plugin.
/// Temp names are `<seq>.<ext>`, unique within a job, so this still identifies
/// the file exactly — but it survives a plugin that echoes back a canonicalized
/// or relative form of its input (a symlinked temp dir is enough to change it).
/// An unmatched result would otherwise strand its `Pending` entry, and with it
/// a disk slot, for the rest of the job.
fn pending_key(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

enum Outcome {
    /// Plugin stream ended; counts are final.
    Finished,
    /// Server said the job is cancelled/stale — kill the plugin, say nothing.
    Cancelled,
    /// Local/transport failure worth reporting via fail_tagging_job.
    Failed(String),
}

async fn execute_job(
    client: &ServerClient,
    config: &WorkerConfig,
    plugin: &LocalPlugin,
    job: ClaimedJob,
) {
    let temp_dir = std::env::temp_dir()
        .join("lightview-worker")
        .join(&job.id);
    if let Err(e) = tokio::fs::create_dir_all(&temp_dir).await {
        let _ = client
            .invoke::<serde_json::Value>(
                "fail_tagging_job",
                json!({
                    "jobId": job.id,
                    "workerId": config.worker_id,
                    "error": format!("cannot create temp dir: {e}"),
                }),
            )
            .await;
        return;
    }

    let outcome = drive_plugin(client, config, plugin, &job, &temp_dir).await;

    match outcome {
        Outcome::Finished | Outcome::Cancelled => {}
        Outcome::Failed(error) => {
            log::error!("job {} failed: {error}", job.id);
            let _ = client
                .invoke::<serde_json::Value>(
                    "fail_tagging_job",
                    json!({ "jobId": job.id, "workerId": config.worker_id, "error": error }),
                )
                .await;
        }
    }

    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
}

async fn drive_plugin(
    client: &ServerClient,
    config: &WorkerConfig,
    plugin: &LocalPlugin,
    job: &ClaimedJob,
    temp_dir: &Path,
) -> Outcome {
    let (req_tx, req_rx) = mpsc::channel::<PluginRequest>(8);
    let mut running = match runner::run_plugin_stream_channel(
        &plugin.manifest,
        &plugin.dir,
        req_rx,
        Some(job.total),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return Outcome::Failed(format!("failed to start plugin: {e}")),
    };

    // temp file name (see `pending_key`) → origin server path + disk permit.
    let pending: Arc<Mutex<HashMap<String, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
    // Files that never reached the plugin (download errors).
    let download_failed = Arc::new(AtomicUsize::new(0));
    // Results matched back to a pending entry, shared with the downloader so
    // each new entry records the count it started waiting at (see `is_stale`).
    let results_matched = Arc::new(AtomicU64::new(0));

    let downloader = spawn_downloader(
        client,
        config,
        job,
        temp_dir,
        req_tx,
        pending.clone(),
        download_failed.clone(),
        results_matched.clone(),
    );

    let mut succeeded: usize = 0;
    let mut failed: usize = 0;
    let mut batch: Vec<serde_json::Value> = Vec::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    heartbeat.reset(); // don't fire immediately
    // Plugin liveness, as distinct from worker liveness. Starts at claim so the
    // model-load window counts against it.
    let mut last_result = tokio::time::Instant::now();

    let outcome = loop {
        tokio::select! {
            maybe = running.results.recv() => {
                let Some(result) = maybe else {
                    // Plugin exited (stdin EOF after the last download).
                    if !batch.is_empty() {
                        match push_batch(client, config, job, &mut batch, &mut succeeded, &mut failed, &download_failed).await {
                            Ok(cancelled) if cancelled => break Outcome::Cancelled,
                            Ok(_) => {}
                            Err(e) => break Outcome::Failed(e),
                        }
                    }
                    break Outcome::Finished;
                };

                let Some(item) = pending.lock().await.remove(&pending_key(&result.path)) else {
                    // Not a result we can attribute to a file, so it must not
                    // refresh `last_result` — an unmatched result is a symptom
                    // of the wedge the timer exists to catch, not evidence of
                    // progress. The file it was meant for ages out via
                    // `is_stale`.
                    log::warn!("plugin result for unknown path: {}", result.path);
                    continue;
                };
                last_result = tokio::time::Instant::now();
                results_matched.fetch_add(1, Ordering::Relaxed);
                let _ = tokio::fs::remove_file(&item.temp_path).await;

                if let Some(err) = result.error {
                    log::debug!("{}: plugin error: {err}", item.server_path);
                    failed += 1;
                } else {
                    batch.push(json!({
                        "path": item.server_path,
                        "tagPrefix": plugin.manifest.tag_prefix,
                        "version": plugin.manifest.version,
                        "tags": result.tags,
                        "meta": result.meta,
                    }));
                }

                if batch.len() >= APPLY_BATCH {
                    match push_batch(client, config, job, &mut batch, &mut succeeded, &mut failed, &download_failed).await {
                        Ok(cancelled) if cancelled => break Outcome::Cancelled,
                        Ok(_) => {}
                        Err(e) => break Outcome::Failed(e),
                    }
                }
            }

            _ = heartbeat.tick() => {
                // Flush whatever results have accumulated (bounds tag-write
                // latency for slow plugins and preserves partial work if the
                // job is cancelled), then heartbeat. The update refreshes the
                // stall timer and picks up cancellation — critical during a
                // tagger's first run, which may download models for minutes
                // before producing any result.
                if !batch.is_empty() {
                    match push_batch(client, config, job, &mut batch, &mut succeeded, &mut failed, &download_failed).await {
                        Ok(cancelled) if cancelled => break Outcome::Cancelled,
                        Ok(_) => {}
                        Err(e) => break Outcome::Failed(e),
                    }
                    continue; // push_batch already reported progress
                }
                // Reclaim files the plugin is never going to answer for. Each
                // one costs a failed image and returns its disk slot, so the
                // window slides on instead of closing permanently — this is
                // what lets a several-thousand-image job survive a plugin that
                // silently drops the occasional request.
                let idle = last_result.elapsed();
                let matched = results_matched.load(Ordering::Relaxed);
                let abandoned: Vec<Pending> = {
                    let mut map = pending.lock().await;
                    let keys: Vec<String> = map
                        .iter()
                        .filter(|(_, e)| {
                            is_stale(
                                matched.saturating_sub(e.results_when_sent),
                                e.sent_at.elapsed(),
                                idle,
                                matched > 0,
                            )
                        })
                        .map(|(k, _)| k.clone())
                        .collect();
                    keys.iter().filter_map(|k| map.remove(k)).collect()
                };
                if !abandoned.is_empty() {
                    for item in &abandoned {
                        log::warn!(
                            "{}: plugin '{}' never answered — counting it failed and releasing \
                             its slot (plugins must emit exactly one result per request; see \
                             docs/plugins/README.md)",
                            item.server_path,
                            job.plugin_name,
                        );
                        let _ = tokio::fs::remove_file(&item.temp_path).await;
                    }
                    failed += abandoned.len();
                    // Dropping the entries here releases their permits, which
                    // unblocks the downloader.
                    drop(abandoned);
                }

                // Nothing matched for the whole stall window means the plugin
                // is not working at all — a stdin-to-EOF buffering plugin, or
                // one that died without closing stdout. Reclaiming slots cannot
                // help that, so give up rather than march the rest of the job
                // through timeouts one window at a time.
                if idle >= Duration::from_secs(NO_RESULT_STALL_SECS) {
                    break Outcome::Failed(format!(
                        "plugin '{}' produced no usable result for {}s — the job cannot \
                         progress. Plugins must stream results and answer every request \
                         (see docs/plugins/README.md).",
                        job.plugin_name,
                        idle.as_secs(),
                    ));
                }
                let total_failed = failed + download_failed.load(Ordering::Relaxed);
                let update: Result<UpdateResult, _> = client.invoke(
                    "update_tagging_job",
                    json!({
                        "jobId": job.id,
                        "workerId": config.worker_id,
                        "completed": succeeded,
                        "failed": total_failed,
                    }),
                ).await;
                match update {
                    Ok(u) if u.cancelled => break Outcome::Cancelled,
                    Ok(_) => {}
                    Err(e) => log::warn!("heartbeat failed: {e}"), // transient; retry next tick
                }
            }
        }
    };

    downloader.abort();
    // Drop pending permits/paths before removing the temp dir.
    pending.lock().await.clear();

    match outcome {
        Outcome::Finished => {
            let total_failed = failed + download_failed.load(Ordering::Relaxed);
            // Plugin exit status only matters if it died without doing the
            // work — a clean stream that tagged everything is a success even
            // if the process exits nonzero.
            if let Err(e) = running.finish().await {
                if succeeded == 0 {
                    return Outcome::Failed(format!("plugin failed: {e}"));
                }
                log::warn!("plugin exited abnormally after {succeeded} results: {e}");
            }
            log::info!(
                "job {} done: {} tagged, {} failed",
                job.id,
                succeeded,
                total_failed
            );
            let _ = client
                .invoke::<serde_json::Value>(
                    "complete_tagging_job",
                    json!({
                        "jobId": job.id,
                        "workerId": config.worker_id,
                        "succeeded": succeeded,
                        "failed": total_failed,
                    }),
                )
                .await;
            Outcome::Finished
        }
        Outcome::Cancelled => {
            log::info!("job {} cancelled by server", job.id);
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

/// Push one apply batch and report progress. Returns whether the server says
/// the job is cancelled.
async fn push_batch(
    client: &ServerClient,
    config: &WorkerConfig,
    job: &ClaimedJob,
    batch: &mut Vec<serde_json::Value>,
    succeeded: &mut usize,
    failed: &mut usize,
    download_failed: &AtomicUsize,
) -> Result<bool, String> {
    let entries = std::mem::take(batch);
    let applied: ApplyResult = client
        .invoke("apply_plugin_tags", json!({ "entries": entries }))
        .await
        .map_err(|e| format!("apply_plugin_tags failed: {e}"))?;
    *succeeded += applied.succeeded.len();
    for f in &applied.failed {
        log::warn!("{}: server rejected tags: {}", f.path, f.error);
    }
    *failed += applied.failed.len();

    let update: UpdateResult = client
        .invoke(
            "update_tagging_job",
            json!({
                "jobId": job.id,
                "workerId": config.worker_id,
                "completed": *succeeded,
                "failed": *failed + download_failed.load(Ordering::Relaxed),
            }),
        )
        .await
        .map_err(|e| format!("update_tagging_job failed: {e}"))?;
    Ok(update.cancelled)
}

/// Stream the job's images to disk and into the plugin's request channel.
/// Ends (dropping the sender → plugin stdin EOF) after the last path.
fn spawn_downloader(
    client: &ServerClient,
    config: &WorkerConfig,
    job: &ClaimedJob,
    temp_dir: &Path,
    req_tx: mpsc::Sender<PluginRequest>,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    download_failed: Arc<AtomicUsize>,
    results_matched: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    let client = client.clone();
    let fit_edge = config.fit_edge;
    let paths = job.paths.clone();
    let temp_dir = temp_dir.to_path_buf();

    tokio::spawn(async move {
        let disk_slots = Arc::new(Semaphore::new(MAX_PENDING_FILES));
        for (seq, server_path) in paths.into_iter().enumerate() {
            // Wait for a free slot — result handling releases permits as it
            // deletes processed temp files.
            let Ok(permit) = disk_slots.clone().acquire_owned().await else {
                return;
            };
            match client
                .download_media(&server_path, fit_edge, &temp_dir, seq)
                .await
            {
                Ok(dest) => {
                    let temp_path = dest.to_string_lossy().to_string();
                    let key = pending_key(&temp_path);
                    pending.lock().await.insert(
                        key.clone(),
                        Pending {
                            server_path,
                            temp_path: dest,
                            sent_at: tokio::time::Instant::now(),
                            results_when_sent: results_matched.load(Ordering::Relaxed),
                            _permit: permit,
                        },
                    );
                    let request = PluginRequest {
                        action: "tag".to_string(),
                        path: temp_path,
                    };
                    if req_tx.send(request).await.is_err() {
                        // Plugin died; the result loop will surface it.
                        pending.lock().await.remove(&key);
                        return;
                    }
                }
                Err(e) => {
                    log::warn!("{server_path}: download failed: {e}");
                    download_failed.fetch_add(1, Ordering::Relaxed);
                    drop(permit);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEVER_IDLE: Duration = Duration::from_secs(0);

    /// The common case: a slow plugin working through its queue in order. No
    /// file is ever abandoned, however long it sits — this is the property a
    /// wall-clock per-file deadline could not give us, and the reason a
    /// CPU-only tagger on a long job does not shed images.
    #[test]
    fn a_slow_but_working_plugin_never_loses_a_file() {
        for results_since_sent in 0..STALE_AFTER_RESULTS {
            assert!(!is_stale(
                results_since_sent,
                Duration::from_secs(6 * 60 * 60),
                NEVER_IDLE,
                true
            ));
        }
    }

    /// A skipped request: the plugin keeps answering everything else, so the
    /// count runs away while this file waits.
    #[test]
    fn a_file_the_plugin_overtook_is_abandoned() {
        assert!(is_stale(
            STALE_AFTER_RESULTS,
            Duration::from_secs(1),
            NEVER_IDLE,
            true
        ));
    }

    /// A first-run model download produces nothing for minutes while every
    /// slot fills. Abandoning those files would fail the head of the job for
    /// no reason, so the idle rule stays off until the plugin has answered
    /// something; `NO_RESULT_STALL_SECS` is the only bound until then.
    #[test]
    fn a_model_load_is_not_mistaken_for_a_wedge() {
        let long = Duration::from_secs(IDLE_RECLAIM_SECS * 4);
        assert!(!is_stale(0, long, long, false));
        // Same numbers, except the plugin has proven it can answer.
        assert!(is_stale(0, long, long, true));
    }

    /// The tail of a job, where no further results can arrive to drive the
    /// count rule: the plugin skipped the last few requests and went quiet.
    #[test]
    fn the_idle_rule_needs_both_clocks_past_the_limit() {
        let over = Duration::from_secs(IDLE_RECLAIM_SECS);
        let under = Duration::from_secs(IDLE_RECLAIM_SECS - 1);
        assert!(is_stale(0, over, over, true));
        // Just downloaded — the plugin has not had a chance at it yet.
        assert!(!is_stale(0, under, over, true));
        // Plugin answered something recently, so it is working, not wedged.
        assert!(!is_stale(0, over, under, true));
    }

    /// A plugin echoing back a canonicalized path still identifies the file:
    /// temp names are unique within a job, so the directory can be rewritten
    /// freely.
    #[test]
    fn pending_key_ignores_the_directory() {
        assert_eq!(pending_key("/tmp/lv/000007.webp"), "000007.webp");
        assert_eq!(pending_key("/private/var/tmp/lv/000007.webp"), "000007.webp");
        assert_eq!(pending_key("000007.webp"), "000007.webp");
    }
}
