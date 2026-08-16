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
//! is `plugin::input::is_stale` — without it a single unanswered request
//! permanently consumed a slot, and 64 of them stopped the job dead.
//!
//! What the plugin actually receives is `plugin::input`'s decision, not this
//! module's: stills arrive scaled to the edge the manifest asked for, and a
//! video arrives as several extracted frames whose results are merged back into
//! one. The server does the extraction, so the worker needs no ffmpeg and a
//! clip costs a few hundred KB instead of the whole file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lightview_lib::plugin::input::{plan_parts, InputPolicy, MergedItem, Part, PartTracker};
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
/// `plugin::input::is_stale`, which is what guarantees every permit comes back.
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
/// is the outer backstop; `plugin::input::is_stale` is what keeps an ordinary
/// job moving.
///
/// Only results that matched a pending entry refresh this timer. Counting
/// unmatched ones — a plugin echoing back a path we cannot key on — kept it
/// permanently fresh in exactly the case it exists to catch.
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


/// What one in-flight request holds until its result comes back: the temp file
/// to delete and the disk slot to return.
struct RemotePart {
    temp_path: PathBuf,
    _permit: OwnedSemaphorePermit,
}

impl RemotePart {
    async fn release(self) {
        let _ = tokio::fs::remove_file(&self.temp_path).await;
    }
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

/// Progress counters, in *media items* rather than requests — one clip is one
/// item however many frames it took.
#[derive(Default)]
struct Counts {
    succeeded: usize,
    failed: usize,
}

async fn drive_plugin(
    client: &ServerClient,
    config: &WorkerConfig,
    plugin: &LocalPlugin,
    job: &ClaimedJob,
    temp_dir: &Path,
) -> Outcome {
    // The worker refuses api_version 0 at startup, so this is always
    // `Prepared` — but reading it from the manifest rather than assuming keeps
    // the three drivers agreeing on one source of truth.
    let policy = InputPolicy::for_manifest(&plugin.manifest);

    let (req_tx, req_rx) = mpsc::channel::<PluginRequest>(8);
    // The total the plugin is told is requests, not items: it sizes buffers
    // with it, and a five-frame clip really is five inferences.
    let request_total = job.paths.iter().map(|p| plan_parts(Path::new(p), &policy).len()).sum();
    let mut running = match runner::run_plugin_stream_channel(
        &plugin.manifest,
        &plugin.dir,
        req_rx,
        Some(request_total),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return Outcome::Failed(format!("failed to start plugin: {e}")),
    };

    let tracker: Arc<Mutex<PartTracker<RemotePart>>> = Arc::new(Mutex::new(PartTracker::new()));
    let downloader = spawn_downloader(
        client,
        config,
        job,
        temp_dir,
        req_tx,
        tracker.clone(),
        policy.clone(),
    );

    let mut counts = Counts::default();
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
                    // Plugin exited (stdin EOF after the last download). Give
                    // up on anything it never answered so those files are
                    // reported rather than silently dropped.
                    let mut t = tracker.lock().await;
                    let orphaned = t.drain_unanswered();
                    let done = t.take_completed();
                    drop(t);
                    for part in orphaned {
                        part.release().await;
                    }
                    collect(done, plugin, &mut batch, &mut counts);
                    if !batch.is_empty() {
                        match push_batch(client, config, job, &mut batch, &mut counts).await {
                            Ok(true) => break Outcome::Cancelled,
                            Ok(false) => {}
                            Err(e) => break Outcome::Failed(e),
                        }
                    }
                    break Outcome::Finished;
                };

                let mut t = tracker.lock().await;
                let Some(part) = t.result(result) else {
                    // Not a result we can attribute to a file, so it must not
                    // refresh `last_result` — an unmatched result is a symptom
                    // of the wedge the timer exists to catch, not evidence of
                    // progress. The file it was meant for ages out instead.
                    drop(t);
                    log::warn!("job {}: plugin result for unknown path", job.id);
                    continue;
                };
                let done = t.take_completed();
                drop(t);

                last_result = tokio::time::Instant::now();
                part.release().await;
                collect(done, plugin, &mut batch, &mut counts);

                if batch.len() >= APPLY_BATCH {
                    match push_batch(client, config, job, &mut batch, &mut counts).await {
                        Ok(true) => break Outcome::Cancelled,
                        Ok(false) => {}
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
                    match push_batch(client, config, job, &mut batch, &mut counts).await {
                        Ok(true) => break Outcome::Cancelled,
                        Ok(false) => {}
                        Err(e) => break Outcome::Failed(e),
                    }
                    continue; // push_batch already reported progress
                }

                // Reclaim requests the plugin is never going to answer. Each
                // one costs a failed image and returns its disk slot, so the
                // window slides on instead of closing permanently — this is
                // what lets a several-thousand-image job survive a plugin that
                // silently drops the occasional request.
                let idle = last_result.elapsed();
                let mut t = tracker.lock().await;
                let stale = t.take_stale(idle);
                let done = t.take_completed();
                let in_flight = t.in_flight_count();
                drop(t);
                for (name, part) in stale {
                    log::warn!(
                        "job {}: plugin '{}' never answered request '{name}' — counting it \
                         failed and releasing its slot (plugins must emit exactly one result \
                         per request; see docs/plugins/README.md)",
                        job.id,
                        job.plugin_name,
                    );
                    part.release().await;
                }
                collect(done, plugin, &mut batch, &mut counts);

                // Nothing matched for the whole stall window means the plugin
                // is not working at all — a stdin-to-EOF buffering plugin, or
                // one that died without closing stdout. Reclaiming slots cannot
                // help that, so give up rather than march the rest of the job
                // through timeouts one window at a time.
                if idle >= Duration::from_secs(NO_RESULT_STALL_SECS) && in_flight > 0 {
                    break Outcome::Failed(format!(
                        "plugin '{}' produced no usable result for {}s while holding \
                         {in_flight} request(s) — the job cannot progress. Plugins must \
                         stream results and answer every request \
                         (see docs/plugins/README.md).",
                        job.plugin_name,
                        idle.as_secs(),
                    ));
                }

                let update: Result<UpdateResult, _> = client.invoke(
                    "update_tagging_job",
                    json!({
                        "jobId": job.id,
                        "workerId": config.worker_id,
                        "completed": counts.succeeded,
                        "failed": counts.failed,
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
    // Drop any permits/temp paths still held before removing the temp dir.
    tracker.lock().await.drain_unanswered();

    match outcome {
        Outcome::Finished => {
            // Plugin exit status only matters if it died without doing the
            // work — a clean stream that tagged everything is a success even
            // if the process exits nonzero. Capture the stderr tail first:
            // `finish` consumes the handle, and a plugin that died on startup
            // has said why only there.
            let stderr_tail = running.stderr_tail();
            if let Err(e) = running.finish().await {
                if counts.succeeded == 0 {
                    return Outcome::Failed(runner::describe_failure(
                        format!("plugin failed: {e}"),
                        &stderr_tail,
                    ));
                }
                log::warn!(
                    "plugin exited abnormally after {} results: {e}",
                    counts.succeeded
                );
            }
            log::info!(
                "job {} done: {} tagged, {} failed",
                job.id,
                counts.succeeded,
                counts.failed
            );
            let _ = client
                .invoke::<serde_json::Value>(
                    "complete_tagging_job",
                    json!({
                        "jobId": job.id,
                        "workerId": config.worker_id,
                        "succeeded": counts.succeeded,
                        "failed": counts.failed,
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

/// Turn finished media items into tag writes, counting the ones that produced
/// nothing. One item is one unit of job progress, whether it took one request
/// or five frames' worth.
fn collect(
    done: Vec<MergedItem>,
    plugin: &LocalPlugin,
    batch: &mut Vec<serde_json::Value>,
    counts: &mut Counts,
) {
    for item in done {
        if let Some(err) = item.error {
            log::debug!("{}: {err}", item.path);
            counts.failed += 1;
            continue;
        }
        batch.push(json!({
            "path": item.path,
            "tagPrefix": plugin.manifest.tag_prefix,
            "version": plugin.manifest.version,
            "tags": item.tags,
            "meta": item.meta,
        }));
    }
}

/// Push one apply batch and report progress. Returns whether the server says
/// the job is cancelled.
async fn push_batch(
    client: &ServerClient,
    config: &WorkerConfig,
    job: &ClaimedJob,
    batch: &mut Vec<serde_json::Value>,
    counts: &mut Counts,
) -> Result<bool, String> {
    let entries = std::mem::take(batch);
    let applied: ApplyResult = client
        .invoke("apply_plugin_tags", json!({ "entries": entries }))
        .await
        .map_err(|e| format!("apply_plugin_tags failed: {e}"))?;
    counts.succeeded += applied.succeeded.len();
    for f in &applied.failed {
        log::warn!("{}: server rejected tags: {}", f.path, f.error);
    }
    counts.failed += applied.failed.len();

    let update: UpdateResult = client
        .invoke(
            "update_tagging_job",
            json!({
                "jobId": job.id,
                "workerId": config.worker_id,
                "completed": counts.succeeded,
                "failed": counts.failed,
            }),
        )
        .await
        .map_err(|e| format!("update_tagging_job failed: {e}"))?;
    Ok(update.cancelled)
}

/// Stream the job's media to disk and into the plugin's request channel.
///
/// One media file becomes one request for a still and `video_frames` requests
/// for a clip: the server extracts and scales each frame, so the worker needs
/// no ffmpeg and a clip costs a few hundred KB rather than the whole file. The
/// still edge and the frame count both come from the plugin's manifest, so a
/// 448-pixel model never pulls a 60-megapixel original across the network.
///
/// Ends (dropping the sender → plugin stdin EOF) after the last part.
fn spawn_downloader(
    client: &ServerClient,
    config: &WorkerConfig,
    job: &ClaimedJob,
    temp_dir: &Path,
    req_tx: mpsc::Sender<PluginRequest>,
    tracker: Arc<Mutex<PartTracker<RemotePart>>>,
    policy: InputPolicy,
) -> tokio::task::JoinHandle<()> {
    let client = client.clone();
    // A plugin that states the edge it wants wins over the worker's own
    // setting: the config value is a fallback for plugins that say nothing,
    // not a cap on what a plugin may ask for.
    let fit_edge = match policy.max_edge() {
        0 => config.fit_edge,
        edge => edge,
    };
    let paths = job.paths.clone();
    let temp_dir = temp_dir.to_path_buf();

    tokio::spawn(async move {
        let disk_slots = Arc::new(Semaphore::new(MAX_PENDING_FILES));
        let mut seq: usize = 0;
        for server_path in paths {
            let parts = plan_parts(Path::new(&server_path), &policy);
            let item = tracker
                .lock()
                .await
                .begin_item(server_path.clone(), parts.len());

            for part in parts {
                seq += 1;
                // Wait for a free slot — result handling releases permits as it
                // deletes processed temp files.
                let Ok(permit) = disk_slots.clone().acquire_owned().await else {
                    return;
                };
                let frame = match part {
                    Part::Whole => None,
                    Part::Frame { index, count } => Some((index, count)),
                };
                match client
                    .download_media(&server_path, fit_edge, frame, &temp_dir, seq)
                    .await
                {
                    Ok(dest) => {
                        let request_path = dest.to_string_lossy().to_string();
                        tracker.lock().await.sent(
                            item,
                            &request_path,
                            RemotePart {
                                temp_path: dest,
                                _permit: permit,
                            },
                        );
                        let request = PluginRequest {
                            action: "tag".to_string(),
                            path: request_path,
                        };
                        if req_tx.send(request).await.is_err() {
                            // Plugin died; the result loop will surface it.
                            return;
                        }
                    }
                    Err(e) => {
                        log::warn!("{server_path}: download failed: {e}");
                        tracker.lock().await.part_failed(item);
                        drop(permit);
                    }
                }
            }
        }
    })
}
