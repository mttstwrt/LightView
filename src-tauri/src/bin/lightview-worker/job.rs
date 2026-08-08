//! The worker's claim/execute loop.
//!
//! One job at a time: claim → spawn ONE plugin subprocess for the whole job
//! (so ML models load once) → a downloader task streams image bytes from the
//! server into a temp dir and feeds the paths to the plugin, bounded by a
//! semaphore on files-on-disk → results are mapped back to server paths and
//! pushed in batches via `apply_plugin_tags`, with `update_tagging_job` as
//! progress + heartbeat + cancellation back-channel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
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
const MAX_PENDING_FILES: usize = 64;
/// Tag writes per `apply_plugin_tags` batch.
const APPLY_BATCH: usize = 32;
/// Heartbeat cadence while a job runs (server requeues after 90s of silence;
/// this also bounds how long a cancel takes to reach the worker).
const HEARTBEAT_SECS: u64 = 10;
/// Announce cadence (server worker TTL is 45s).
const ANNOUNCE_SECS: u64 = 15;
/// How long the plugin may sit on downloaded files without producing a single
/// result before the job is failed instead of left hanging.
///
/// Every unanswered file holds one of the `MAX_PENDING_FILES` disk slots; once
/// they are all held the downloader blocks on the semaphore, the plugin stops
/// receiving stdin lines, and no result ever arrives — while the heartbeat
/// below keeps the server's stall timer fresh, so the job would otherwise show
/// "running" forever with no error. Generous because a tagger's first run
/// downloads and loads its model before the first result, which looks exactly
/// the same from here.
const NO_RESULT_STALL_SECS: u64 = 20 * 60;

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
            }),
        )
        .await
        .map(|_| ())
}

/// A downloaded file waiting for its plugin result. Holds the semaphore permit
/// so disk usage stays bounded; dropped (releasing the permit) once the result
/// lands and the temp file is deleted.
struct Pending {
    server_path: String,
    temp_path: PathBuf,
    _permit: OwnedSemaphorePermit,
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

    let downloader = spawn_downloader(
        client,
        config,
        job,
        temp_dir,
        req_tx,
        pending.clone(),
        download_failed.clone(),
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

                last_result = tokio::time::Instant::now();

                let Some(item) = pending.lock().await.remove(&pending_key(&result.path)) else {
                    log::warn!("plugin result for unknown path: {}", result.path);
                    continue;
                };
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
                // Files delivered to the plugin with nothing coming back is the
                // signature of a wedge: a plugin that buffers stdin to EOF
                // instead of streaming, or one that silently drops requests
                // (each dropped request strands a disk slot, and once all
                // MAX_PENDING_FILES are stranded the downloader blocks for
                // good). It is indistinguishable from a very slow first model
                // load, so warn early but only fail once it is truly hopeless —
                // the alternative, left as-is, is a job that runs forever.
                let pending_now = pending.lock().await.len();
                if pending_now > 0 {
                    let idle = last_result.elapsed();
                    if idle >= Duration::from_secs(NO_RESULT_STALL_SECS) {
                        break Outcome::Failed(format!(
                            "plugin '{}' returned no result for {}s while holding {pending_now} \
                             downloaded file(s) — the job cannot progress. Plugins must stream \
                             results and answer every request (see docs/plugins/README.md).",
                            job.plugin_name,
                            idle.as_secs(),
                        ));
                    }
                    if pending_now >= MAX_PENDING_FILES {
                        log::warn!(
                            "job {}: all {MAX_PENDING_FILES} download slots held with no result \
                             for {}s — plugin '{}' is not answering requests; failing the job \
                             at {NO_RESULT_STALL_SECS}s",
                            job.id,
                            idle.as_secs(),
                            job.plugin_name,
                        );
                    }
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
            running.kill().await;
            Outcome::Failed(e)
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
