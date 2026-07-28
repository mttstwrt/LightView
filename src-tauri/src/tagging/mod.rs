//! Remote-tagging job queue + worker registry.
//!
//! Tagging jobs are enqueued by web clients and executed by "workers":
//! usually a `lightview-worker` on a capable paired machine (it claims the
//! job, downloads the image bytes over HTTP, runs the plugin locally, and
//! pushes tags back via `apply_plugin_tags`), and optionally the server host
//! itself via the in-process executor in [`local`] — it registers in the same
//! registry when plugins are installed server-side and runs them directly on
//! the local files. No plugin ever executes in this process *on behalf of a
//! remote device*; remote devices only ever submit plugin names and tag data.
//! See `docs/workerTagging.md`.
//!
//! Everything is in-memory: tag writes are idempotent and filter-target jobs
//! resolve their path list at *claim* time (`NOT has::plugin.<prefix>` skips
//! already-tagged files), so a lost queue entry costs one re-enqueue click.
//!
//! `TaggingState` methods are synchronous and take `now` explicitly so the
//! transition logic is unit-testable; the `async fn`s at the bottom are the
//! `AppState` glue used by the `/api/invoke` dispatch, and they broadcast a
//! `TaggingSseEvent` on every observable change for the `/api/events` stream.

pub mod local;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::plugin::PluginInfo;
use crate::AppState;

/// A worker that hasn't announced or claimed/updated for this long is gone.
/// Workers announce every ~15s and poll claims every ~3s.
pub const WORKER_TTL_SECS: i64 = 45;
/// A running job whose worker hasn't updated for this long is requeued.
/// Workers heartbeat via `update_tagging_job` at least every ~10s.
pub const JOB_STALL_SECS: i64 = 90;
/// A running job that keeps heartbeating but whose counts haven't moved for
/// this long is failed.
///
/// The heartbeat proves the *worker process* is alive, not that the job is
/// progressing — it refreshes `updated_at` unconditionally — so without this a
/// worker that is alive but wedged holds a job in `Running` forever, with no
/// error anywhere. The worker's own watchdog (`NO_RESULT_STALL_SECS`) should
/// catch that first; this is the backstop for the cases it can't see. Well
/// clear of a tagger's first-run model download, which legitimately produces
/// nothing for minutes.
pub const JOB_NO_PROGRESS_SECS: i64 = 30 * 60;
/// Finished (done/failed/cancelled) jobs retained for the web UI's job list.
pub const MAX_FINISHED_JOBS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(self, JobState::Done | JobState::Failed | JobState::Cancelled)
    }
}

/// What a job tags: an explicit path list (context-menu selection) or a filter
/// query resolved at claim time ("tag all untagged").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobTarget {
    Paths(Vec<String>),
    Filter(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaggingJob {
    pub id: String,
    pub plugin_name: String,
    pub tag_prefix: String,
    pub display_name: String,
    pub target: JobTarget,
    /// When set, only this worker may claim the job — how the web UI picks
    /// "run on the server" vs. a specific remote worker when several offer
    /// the same plugin. Unpinned jobs go to whoever claims first.
    pub pinned_worker: Option<String>,
    pub state: JobState,
    /// Fixed when the job is claimed (filter targets resolve then); before
    /// that it's the raw path count for path targets, 0 for filter targets.
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub claimed_by: Option<String>,
    pub worker_name: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    /// Last contact from the worker — refreshed by every heartbeat, whether or
    /// not anything was tagged. Drives `JOB_STALL_SECS`.
    pub updated_at: i64,
    /// Last time `completed + failed` actually advanced (reset at claim time).
    /// Drives `JOB_NO_PROGRESS_SECS`.
    pub progressed_at: i64,
}

/// Registry entry for an announced worker.
#[derive(Debug, Clone)]
pub struct WorkerEntry {
    pub worker_id: String,
    pub worker_name: String,
    pub plugins: Vec<PluginInfo>,
    pub last_seen: i64,
    /// True for the in-process executor running on the server host itself.
    pub local: bool,
}

/// Serialized view of a worker for the web UI / SSE.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerStatus {
    pub worker_id: String,
    pub worker_name: String,
    pub plugins: Vec<PluginInfo>,
    pub last_seen: i64,
    /// True for the in-process executor running on the server host itself.
    pub local: bool,
    /// Id of the running job claimed by this worker, if any (derived).
    pub busy_job_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaggingStatus {
    pub workers: Vec<WorkerStatus>,
    pub jobs: Vec<TaggingJob>,
}

/// A claimed job as returned to the worker: the job snapshot plus the resolved
/// server-side absolute paths it should download and tag.
#[derive(Debug, Clone, Serialize)]
pub struct ClaimedJob {
    #[serde(flatten)]
    pub job: TaggingJob,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateResult {
    pub cancelled: bool,
}

/// Broadcast to `/api/events` subscribers; the SSE route maps each variant to
/// its event name (`tagging-job` / `tagging-workers`).
#[derive(Debug, Clone)]
pub enum TaggingSseEvent {
    Job(TaggingJob),
    Workers(Vec<WorkerStatus>),
}

#[derive(Debug, Default)]
pub struct TaggingState {
    /// Insertion order == enqueue order; claims scan front-to-back, so the
    /// oldest queued job wins and requeued jobs keep their place in line.
    pub jobs: Vec<TaggingJob>,
    pub workers: HashMap<String, WorkerEntry>,
}

/// What a lazy reap pass changed (broadcast by the async glue).
#[derive(Debug, Default)]
pub struct ReapOutcome {
    pub workers_changed: bool,
    pub requeued: Vec<TaggingJob>,
    /// Jobs given up on because they stopped making progress.
    pub failed: Vec<TaggingJob>,
}

impl TaggingState {
    /// Drop expired workers and requeue stalled running jobs. Called lazily
    /// from every claim/status read — the worker's poll cadence makes this run
    /// constantly, so there is no background reaper task.
    pub fn reap(&mut self, now: i64) -> ReapOutcome {
        let mut out = ReapOutcome::default();

        let before = self.workers.len();
        self.workers.retain(|_, w| w.last_seen + WORKER_TTL_SECS > now);
        out.workers_changed = self.workers.len() != before;

        for job in &mut self.jobs {
            if job.state != JobState::Running {
                continue;
            }
            if job.updated_at + JOB_STALL_SECS <= now {
                // The worker went silent — put the job back in line for
                // whoever picks it up next.
                job.state = JobState::Queued;
                job.claimed_by = None;
                job.worker_name = None;
                job.total = 0;
                job.completed = 0;
                job.failed = 0;
                job.updated_at = now;
                out.requeued.push(job.clone());
            } else if job.progressed_at + JOB_NO_PROGRESS_SECS <= now {
                // Still heartbeating, but nothing has been tagged for half an
                // hour. Requeueing would just hand the same wedge to the same
                // worker, so fail it visibly instead — tag writes are
                // idempotent and filter targets skip what's already tagged, so
                // re-enqueueing resumes rather than redoes the work.
                job.state = JobState::Failed;
                job.error = Some(format!(
                    "no progress for {} minutes (worker '{}' still responding but tagged \
                     nothing) — check the worker log, then re-enqueue to resume",
                    JOB_NO_PROGRESS_SECS / 60,
                    job.worker_name.as_deref().unwrap_or("unknown"),
                ));
                job.updated_at = now;
                out.failed.push(job.clone());
            }
        }
        if !out.failed.is_empty() {
            self.prune_finished();
        }
        out
    }

    /// Register or refresh a worker. Returns true when the visible registry
    /// changed (new worker, renamed, or different plugin list) — heartbeat
    /// refreshes return false so announces don't spam the SSE stream.
    pub fn announce(
        &mut self,
        worker_id: String,
        worker_name: String,
        plugins: Vec<PluginInfo>,
        local: bool,
        now: i64,
    ) -> bool {
        match self.workers.get_mut(&worker_id) {
            Some(entry) => {
                let changed = entry.worker_name != worker_name || entry.plugins != plugins;
                entry.worker_name = worker_name;
                entry.plugins = plugins;
                entry.local = local;
                entry.last_seen = now;
                changed
            }
            None => {
                self.workers.insert(
                    worker_id.clone(),
                    WorkerEntry {
                        worker_id,
                        worker_name,
                        plugins,
                        last_seen: now,
                        local,
                    },
                );
                true
            }
        }
    }

    /// Find the plugin descriptor among live workers, so enqueue can stamp
    /// the job with `tag_prefix`/`display_name`. With `pinned_worker` set the
    /// plugin must come from that specific worker.
    pub fn resolve_plugin(
        &self,
        plugin_name: &str,
        pinned_worker: Option<&str>,
    ) -> Result<&PluginInfo, String> {
        match pinned_worker {
            Some(worker_id) => {
                let worker = self
                    .workers
                    .get(worker_id)
                    .ok_or_else(|| format!("unknown or disconnected worker '{worker_id}'"))?;
                worker
                    .plugins
                    .iter()
                    .find(|p| p.name == plugin_name)
                    .ok_or_else(|| {
                        format!(
                            "worker '{}' does not provide plugin '{plugin_name}'",
                            worker.worker_name
                        )
                    })
            }
            None => self
                .workers
                .values()
                .flat_map(|w| w.plugins.iter())
                .find(|p| p.name == plugin_name)
                .ok_or_else(|| format!("no connected worker provides plugin '{plugin_name}'")),
        }
    }

    pub fn enqueue(
        &mut self,
        plugin_name: &str,
        target: JobTarget,
        pinned_worker: Option<String>,
        now: i64,
    ) -> Result<TaggingJob, String> {
        let descriptor = self.resolve_plugin(plugin_name, pinned_worker.as_deref())?;

        let total = match &target {
            JobTarget::Paths(paths) if paths.is_empty() => {
                return Err("paths must not be empty".to_string())
            }
            JobTarget::Paths(paths) => paths.len(),
            JobTarget::Filter(query) if query.trim().is_empty() => {
                return Err("filter must not be empty".to_string())
            }
            JobTarget::Filter(_) => 0,
        };

        let job = TaggingJob {
            id: uuid::Uuid::new_v4().to_string(),
            plugin_name: descriptor.name.clone(),
            tag_prefix: descriptor.tag_prefix.clone(),
            display_name: descriptor.display_name.clone(),
            target,
            pinned_worker,
            state: JobState::Queued,
            total,
            completed: 0,
            failed: 0,
            claimed_by: None,
            worker_name: None,
            error: None,
            created_at: now,
            updated_at: now,
            progressed_at: now,
        };
        self.jobs.push(job.clone());
        self.prune_finished();
        Ok(job)
    }

    /// Index of the oldest queued job this worker's plugins can run. Jobs
    /// pinned to a different worker are skipped.
    pub fn next_claimable(&self, worker_id: &str, plugin_names: &[String]) -> Option<usize> {
        self.jobs.iter().position(|j| {
            j.state == JobState::Queued
                && plugin_names.iter().any(|n| *n == j.plugin_name)
                && j.pinned_worker.as_deref().is_none_or(|w| w == worker_id)
        })
    }

    /// Progress update + heartbeat from the worker. `cancelled: true` tells
    /// the worker to abandon the job — either it was cancelled, or the server
    /// no longer recognizes the claim (restart, requeue after a stall).
    pub fn update(
        &mut self,
        job_id: &str,
        worker_id: &str,
        completed: usize,
        failed: usize,
        now: i64,
    ) -> (UpdateResult, Option<TaggingJob>) {
        if let Some(w) = self.workers.get_mut(worker_id) {
            w.last_seen = now;
        }
        let Some(job) = self.jobs.iter_mut().find(|j| j.id == job_id) else {
            return (UpdateResult { cancelled: true }, None);
        };
        if job.state != JobState::Running || job.claimed_by.as_deref() != Some(worker_id) {
            return (UpdateResult { cancelled: true }, None);
        }
        // Only real forward motion counts as progress; a heartbeat that repeats
        // the same counts refreshes liveness but not the no-progress deadline.
        if completed + failed > job.completed + job.failed {
            job.progressed_at = now;
        }
        job.completed = completed;
        job.failed = failed;
        job.updated_at = now;
        (UpdateResult { cancelled: false }, Some(job.clone()))
    }

    /// Terminal transition from the worker. No-ops (Ok(None)) when the claim
    /// is no longer valid — e.g. the job was cancelled while the worker's last
    /// batch was in flight; the cancelled state wins.
    pub fn finish(
        &mut self,
        job_id: &str,
        worker_id: &str,
        outcome: JobState,
        completed: usize,
        failed: usize,
        error: Option<String>,
        now: i64,
    ) -> Option<TaggingJob> {
        debug_assert!(matches!(outcome, JobState::Done | JobState::Failed));
        if let Some(w) = self.workers.get_mut(worker_id) {
            w.last_seen = now;
        }
        let job = self.jobs.iter_mut().find(|j| j.id == job_id)?;
        if job.state != JobState::Running || job.claimed_by.as_deref() != Some(worker_id) {
            return None;
        }
        job.state = outcome;
        job.completed = completed;
        job.failed = failed;
        job.error = error;
        job.updated_at = now;
        let snapshot = job.clone();
        self.prune_finished();
        Some(snapshot)
    }

    /// Cancel a job. Queued jobs die immediately; running jobs flip to
    /// Cancelled and the worker learns via its next `update` call.
    pub fn cancel(&mut self, job_id: &str, now: i64) -> Result<Option<TaggingJob>, String> {
        let job = self
            .jobs
            .iter_mut()
            .find(|j| j.id == job_id)
            .ok_or_else(|| format!("unknown job '{job_id}'"))?;
        if job.state.is_terminal() {
            return Ok(None);
        }
        job.state = JobState::Cancelled;
        job.updated_at = now;
        let snapshot = job.clone();
        self.prune_finished();
        Ok(Some(snapshot))
    }

    /// Keep at most `MAX_FINISHED_JOBS` terminal jobs (oldest dropped first).
    fn prune_finished(&mut self) {
        let finished = self.jobs.iter().filter(|j| j.state.is_terminal()).count();
        let mut to_drop = finished.saturating_sub(MAX_FINISHED_JOBS);
        if to_drop > 0 {
            self.jobs.retain(|j| {
                if to_drop > 0 && j.state.is_terminal() {
                    to_drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    pub fn worker_statuses(&self) -> Vec<WorkerStatus> {
        let mut list: Vec<WorkerStatus> = self
            .workers
            .values()
            .map(|w| WorkerStatus {
                worker_id: w.worker_id.clone(),
                worker_name: w.worker_name.clone(),
                plugins: w.plugins.clone(),
                last_seen: w.last_seen,
                local: w.local,
                busy_job_id: self
                    .jobs
                    .iter()
                    .find(|j| {
                        j.state == JobState::Running
                            && j.claimed_by.as_deref() == Some(&w.worker_id)
                    })
                    .map(|j| j.id.clone()),
            })
            .collect();
        list.sort_by(|a, b| a.worker_name.cmp(&b.worker_name));
        list
    }

    pub fn status(&self) -> TaggingStatus {
        TaggingStatus {
            workers: self.worker_statuses(),
            jobs: self.jobs.clone(),
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// AppState glue (called from http_server::api dispatch)
// ---------------------------------------------------------------------------

fn broadcast(state: &AppState, event: TaggingSseEvent) {
    // Send fails only when no SSE subscriber exists — that's fine.
    let _ = state.tagging_event_tx.send(event);
}

fn broadcast_reap(state: &AppState, tagging: &TaggingState, outcome: ReapOutcome) {
    for job in outcome.requeued.into_iter().chain(outcome.failed) {
        broadcast(state, TaggingSseEvent::Job(job));
    }
    if outcome.workers_changed {
        broadcast(state, TaggingSseEvent::Workers(tagging.worker_statuses()));
    }
}

pub async fn announce_worker(
    state: &AppState,
    worker_id: String,
    worker_name: String,
    plugins: Vec<PluginInfo>,
    local: bool,
) -> Result<(), String> {
    let now = now_secs();
    let mut tagging = state.tagging.lock().await;
    let reaped = tagging.reap(now);
    let changed = tagging.announce(worker_id, worker_name, plugins, local, now);
    // Fold the announce into the reap broadcast so at most one workers
    // snapshot goes out.
    let outcome = ReapOutcome {
        workers_changed: reaped.workers_changed || changed,
        requeued: reaped.requeued,
        failed: reaped.failed,
    };
    broadcast_reap(state, &tagging, outcome);
    Ok(())
}

pub async fn enqueue_job(
    state: &AppState,
    plugin_name: String,
    paths: Option<Vec<String>>,
    filter: Option<String>,
    pinned_worker: Option<String>,
) -> Result<TaggingJob, String> {
    let target = match (paths, filter) {
        (Some(paths), None) => JobTarget::Paths(paths),
        (None, Some(filter)) => JobTarget::Filter(filter),
        _ => return Err("provide exactly one of 'paths' or 'filter'".to_string()),
    };

    let now = now_secs();
    let mut tagging = state.tagging.lock().await;
    let reaped = tagging.reap(now);
    broadcast_reap(state, &tagging, reaped);
    let job = tagging.enqueue(&plugin_name, target, pinned_worker, now)?;
    broadcast(state, TaggingSseEvent::Job(job.clone()));
    Ok(job)
}

/// Claim the oldest queued job this worker can run, resolving its target to a
/// concrete image path list. Filter targets that resolve to nothing complete
/// instantly (idempotent re-enqueue of "tag all untagged") and the scan moves
/// on to the next queued job.
pub async fn claim_job(
    state: &AppState,
    worker_id: String,
    plugin_names: Vec<String>,
) -> Result<Option<ClaimedJob>, String> {
    let now = now_secs();
    let mut tagging = state.tagging.lock().await;
    let reaped = tagging.reap(now);
    broadcast_reap(state, &tagging, reaped);

    let worker_name = match tagging.workers.get_mut(&worker_id) {
        Some(w) => {
            w.last_seen = now; // the claim poll doubles as a heartbeat
            w.worker_name.clone()
        }
        // Forces announce-before-claim; the worker re-announces on this error.
        None => return Err("unknown worker — call worker_announce first".to_string()),
    };

    loop {
        let Some(idx) = tagging.next_claimable(&worker_id, &plugin_names) else {
            return Ok(None);
        };

        let target = tagging.jobs[idx].target.clone();
        // Resolving may hit the cache DB while we hold the tagging lock;
        // nothing acquires these two locks in the other order.
        let paths = match resolve_target(state, &target).await {
            Ok(paths) => paths,
            Err(e) => {
                let job = &mut tagging.jobs[idx];
                job.state = JobState::Failed;
                job.error = Some(e);
                job.updated_at = now;
                broadcast(state, TaggingSseEvent::Job(job.clone()));
                continue;
            }
        };

        let job = &mut tagging.jobs[idx];
        if paths.is_empty() {
            // Nothing left to tag — complete without bothering the worker.
            job.state = JobState::Done;
            job.total = 0;
            job.updated_at = now;
            broadcast(state, TaggingSseEvent::Job(job.clone()));
            continue;
        }

        job.state = JobState::Running;
        job.claimed_by = Some(worker_id.clone());
        job.worker_name = Some(worker_name.clone());
        job.total = paths.len();
        job.completed = 0;
        job.failed = 0;
        job.updated_at = now;
        job.progressed_at = now;
        let snapshot = job.clone();
        broadcast(state, TaggingSseEvent::Job(snapshot.clone()));
        return Ok(Some(ClaimedJob {
            job: snapshot,
            paths,
        }));
    }
}

/// Resolve a job target to the image paths the worker should tag. Videos and
/// GIFs are excluded — the remote loop shouldn't ship them, and the image
/// taggers can't use them anyway.
async fn resolve_target(state: &AppState, target: &JobTarget) -> Result<Vec<String>, String> {
    let candidates = match target {
        JobTarget::Paths(paths) => paths.clone(),
        JobTarget::Filter(query) => {
            crate::commands::filter::apply_filter_impl(state, query.clone()).await?
        }
    };

    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    let mut stmt = db
        .conn()
        .prepare("SELECT path FROM media_meta WHERE media_type = 'image'")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let images: std::collections::HashSet<String> =
        rows.filter_map(|r| r.ok()).collect();

    Ok(candidates
        .into_iter()
        .filter(|p| images.contains(p))
        .collect())
}

pub async fn update_job(
    state: &AppState,
    job_id: String,
    worker_id: String,
    completed: usize,
    failed: usize,
) -> Result<UpdateResult, String> {
    let now = now_secs();
    let mut tagging = state.tagging.lock().await;
    let (result, changed) = tagging.update(&job_id, &worker_id, completed, failed, now);
    if let Some(job) = changed {
        broadcast(state, TaggingSseEvent::Job(job));
    }
    Ok(result)
}

pub async fn complete_job(
    state: &AppState,
    job_id: String,
    worker_id: String,
    succeeded: usize,
    failed: usize,
) -> Result<(), String> {
    let now = now_secs();
    let mut tagging = state.tagging.lock().await;
    if let Some(job) = tagging.finish(
        &job_id,
        &worker_id,
        JobState::Done,
        succeeded,
        failed,
        None,
        now,
    ) {
        broadcast(state, TaggingSseEvent::Job(job));
    }
    Ok(())
}

pub async fn fail_job(
    state: &AppState,
    job_id: String,
    worker_id: String,
    error: String,
) -> Result<(), String> {
    let now = now_secs();
    let mut tagging = state.tagging.lock().await;
    // Preserve whatever progress counts the last update reported.
    let (completed, failed) = tagging
        .jobs
        .iter()
        .find(|j| j.id == job_id)
        .map(|j| (j.completed, j.failed))
        .unwrap_or((0, 0));
    if let Some(job) = tagging.finish(
        &job_id,
        &worker_id,
        JobState::Failed,
        completed,
        failed,
        Some(error),
        now,
    ) {
        broadcast(state, TaggingSseEvent::Job(job));
    }
    Ok(())
}

pub async fn cancel_job(state: &AppState, job_id: String) -> Result<(), String> {
    let now = now_secs();
    let mut tagging = state.tagging.lock().await;
    if let Some(job) = tagging.cancel(&job_id, now)? {
        broadcast(state, TaggingSseEvent::Job(job));
    }
    Ok(())
}

pub async fn get_status(state: &AppState) -> Result<TaggingStatus, String> {
    let now = now_secs();
    let mut tagging = state.tagging.lock().await;
    let reaped = tagging.reap(now);
    broadcast_reap(state, &tagging, reaped);
    Ok(tagging.status())
}

// ---------------------------------------------------------------------------
// Tests (pure state machine — no AppState / DB)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(name: &str) -> PluginInfo {
        PluginInfo {
            name: name.to_string(),
            display_name: format!("{name} (display)"),
            version: "1.0.0".to_string(),
            description: String::new(),
            tag_prefix: name.to_string(),
        }
    }

    fn state_with_worker(now: i64) -> TaggingState {
        let mut s = TaggingState::default();
        s.announce("w1".into(), "worker-one".into(), vec![plugin("tagger")], false, now);
        s
    }

    fn enqueue_paths(s: &mut TaggingState, now: i64) -> TaggingJob {
        s.enqueue(
            "tagger",
            JobTarget::Paths(vec!["/g/a.jpg".into(), "/g/b.jpg".into()]),
            None,
            now,
        )
        .unwrap()
    }

    /// Claim bookkeeping normally done by the async glue (which needs a DB to
    /// resolve targets) — tests drive the same field transitions directly.
    fn mark_running(s: &mut TaggingState, job_id: &str, worker_id: &str, total: usize, now: i64) {
        let job = s.jobs.iter_mut().find(|j| j.id == job_id).unwrap();
        job.state = JobState::Running;
        job.claimed_by = Some(worker_id.to_string());
        job.total = total;
        job.updated_at = now;
        job.progressed_at = now;
    }

    #[test]
    fn enqueue_requires_a_live_worker_with_the_plugin() {
        let mut s = TaggingState::default();
        let err = s
            .enqueue("tagger", JobTarget::Filter("x".into()), None, 100)
            .unwrap_err();
        assert!(err.contains("no connected worker"));

        let mut s = state_with_worker(100);
        assert!(s.enqueue("other-plugin", JobTarget::Filter("x".into()), None, 100).is_err());
        let job = s.enqueue("tagger", JobTarget::Filter("x".into()), None, 100).unwrap();
        assert_eq!(job.state, JobState::Queued);
        assert_eq!(job.tag_prefix, "tagger");
        assert_eq!(job.display_name, "tagger (display)");
    }

    #[test]
    fn claim_order_is_fifo_and_respects_plugin_match() {
        let mut s = state_with_worker(100);
        s.announce("w2".into(), "worker-two".into(), vec![plugin("other")], false, 100);
        let a = s.enqueue("tagger", JobTarget::Filter("q1".into()), None, 100).unwrap();
        let b = s.enqueue("other", JobTarget::Filter("q2".into()), None, 101).unwrap();
        let c = s.enqueue("tagger", JobTarget::Filter("q3".into()), None, 102).unwrap();

        // Worker offering only "other" skips over the older "tagger" job.
        let idx = s.next_claimable("w2", &["other".into()]).unwrap();
        assert_eq!(s.jobs[idx].id, b.id);

        // Worker offering "tagger" gets the oldest queued tagger job.
        let idx = s.next_claimable("w1", &["tagger".into()]).unwrap();
        assert_eq!(s.jobs[idx].id, a.id);
        mark_running(&mut s, &a.id, "w1", 5, 103);
        let idx = s.next_claimable("w1", &["tagger".into()]).unwrap();
        assert_eq!(s.jobs[idx].id, c.id);
    }

    #[test]
    fn update_reports_progress_and_cancellation() {
        let mut s = state_with_worker(100);
        let job = enqueue_paths(&mut s, 100);
        mark_running(&mut s, &job.id, "w1", 2, 100);

        let (r, changed) = s.update(&job.id, "w1", 1, 0, 110);
        assert!(!r.cancelled);
        assert_eq!(changed.unwrap().completed, 1);

        // Wrong worker → treated as a dead claim.
        let (r, changed) = s.update(&job.id, "w2", 2, 0, 111);
        assert!(r.cancelled);
        assert!(changed.is_none());

        // Unknown job (e.g. server restarted) → cancelled.
        let (r, _) = s.update("nope", "w1", 0, 0, 112);
        assert!(r.cancelled);

        // Cancelled mid-run → worker learns on next update.
        s.cancel(&job.id, 113).unwrap();
        let (r, _) = s.update(&job.id, "w1", 2, 0, 114);
        assert!(r.cancelled);
    }

    #[test]
    fn cancel_semantics_queued_vs_running_vs_terminal() {
        let mut s = state_with_worker(100);
        let queued = enqueue_paths(&mut s, 100);
        let cancelled = s.cancel(&queued.id, 101).unwrap().unwrap();
        assert_eq!(cancelled.state, JobState::Cancelled);
        // Terminal cancel is a no-op, not an error.
        assert!(s.cancel(&queued.id, 102).unwrap().is_none());
        assert!(s.cancel("missing", 103).is_err());
    }

    #[test]
    fn finish_is_ignored_when_the_claim_is_stale() {
        let mut s = state_with_worker(100);
        let job = enqueue_paths(&mut s, 100);
        mark_running(&mut s, &job.id, "w1", 2, 100);
        s.cancel(&job.id, 105).unwrap();

        // Worker completes after cancellation raced it — cancelled wins.
        assert!(s
            .finish(&job.id, "w1", JobState::Done, 2, 0, None, 106)
            .is_none());
        assert_eq!(s.jobs[0].state, JobState::Cancelled);
    }

    #[test]
    fn stalled_running_jobs_are_requeued() {
        let mut s = state_with_worker(100);
        let job = enqueue_paths(&mut s, 100);
        mark_running(&mut s, &job.id, "w1", 2, 100);

        // Not yet stalled.
        let out = s.reap(100 + JOB_STALL_SECS - 1);
        assert!(out.requeued.is_empty());

        let out = s.reap(100 + JOB_STALL_SECS);
        assert_eq!(out.requeued.len(), 1);
        let j = &s.jobs[0];
        assert_eq!(j.state, JobState::Queued);
        assert!(j.claimed_by.is_none());
        assert_eq!((j.total, j.completed, j.failed), (0, 0, 0));
    }

    /// A worker can stay perfectly responsive while getting nothing done — the
    /// heartbeat refreshes `updated_at` either way, so `JOB_STALL_SECS` never
    /// fires and only the no-progress deadline ends the job.
    #[test]
    fn heartbeating_jobs_that_never_progress_are_failed() {
        let mut s = state_with_worker(100);
        let job = enqueue_paths(&mut s, 100);
        mark_running(&mut s, &job.id, "w1", 2, 100);

        // Heartbeat every 10s, always reporting the same counts.
        let mut t = 100;
        while t + 10 < 100 + JOB_NO_PROGRESS_SECS {
            t += 10;
            let (res, _) = s.update(&job.id, "w1", 0, 0, t);
            assert!(!res.cancelled, "liveness heartbeat kept the claim at t={t}");
            assert!(s.reap(t).failed.is_empty(), "failed early at t={t}");
            assert_eq!(s.jobs[0].state, JobState::Running);
        }

        let out = s.reap(100 + JOB_NO_PROGRESS_SECS);
        assert_eq!(out.failed.len(), 1);
        assert!(out.requeued.is_empty(), "a wedge should fail, not requeue");
        let j = &s.jobs[0];
        assert_eq!(j.state, JobState::Failed);
        assert!(j.error.as_deref().unwrap().contains("no progress"));

        // The worker's next heartbeat now tells it to give up.
        assert!(s.update(&job.id, "w1", 0, 0, t + 10).0.cancelled);
    }

    /// Real progress pushes the deadline out, so a slow-but-working job runs
    /// as long as it needs to.
    #[test]
    fn progress_resets_the_no_progress_deadline() {
        let mut s = state_with_worker(100);
        let job = enqueue_paths(&mut s, 100);
        mark_running(&mut s, &job.id, "w1", 100, 100);

        let mut completed = 0;
        let mut t = 100;
        // One tagged file every 20 minutes, for three times the deadline.
        for _ in 0..3 {
            t += JOB_NO_PROGRESS_SECS - 60;
            completed += 1;
            s.update(&job.id, "w1", completed, 0, t);
            assert!(s.reap(t).failed.is_empty());
            assert_eq!(s.jobs[0].state, JobState::Running);
        }
        assert_eq!(s.jobs[0].completed, 3);
    }

    #[test]
    fn workers_expire_by_ttl_and_announce_dedupes_broadcasts() {
        let mut s = TaggingState::default();
        assert!(s.announce("w1".into(), "n".into(), vec![plugin("t")], false, 100));
        // Pure heartbeat: nothing visible changed.
        assert!(!s.announce("w1".into(), "n".into(), vec![plugin("t")], false, 110));
        // Plugin list changed.
        assert!(s.announce("w1".into(), "n".into(), vec![], false, 120));

        let out = s.reap(120 + WORKER_TTL_SECS - 1);
        assert!(!out.workers_changed);
        let out = s.reap(120 + WORKER_TTL_SECS);
        assert!(out.workers_changed);
        assert!(s.workers.is_empty());
    }

    #[test]
    fn finished_jobs_are_capped() {
        let mut s = state_with_worker(100);
        for i in 0..(MAX_FINISHED_JOBS + 10) {
            let job = enqueue_paths(&mut s, 100 + i as i64);
            s.cancel(&job.id, 100 + i as i64).unwrap();
        }
        let finished = s.jobs.iter().filter(|j| j.state.is_terminal()).count();
        assert_eq!(finished, MAX_FINISHED_JOBS);
        // Queued/running jobs are never pruned.
        let live = enqueue_paths(&mut s, 500);
        assert!(s.jobs.iter().any(|j| j.id == live.id));
    }

    #[test]
    fn pinned_jobs_only_claimable_by_their_worker() {
        let mut s = state_with_worker(100);
        s.announce("w2".into(), "worker-two".into(), vec![plugin("tagger")], false, 100);
        let pinned = s
            .enqueue(
                "tagger",
                JobTarget::Filter("q".into()),
                Some("w2".to_string()),
                100,
            )
            .unwrap();
        assert_eq!(pinned.pinned_worker.as_deref(), Some("w2"));

        // w1 offers the plugin but the job is pinned to w2.
        assert!(s.next_claimable("w1", &["tagger".into()]).is_none());
        let idx = s.next_claimable("w2", &["tagger".into()]).unwrap();
        assert_eq!(s.jobs[idx].id, pinned.id);
    }

    #[test]
    fn enqueue_validates_the_pinned_worker() {
        let mut s = state_with_worker(100);
        // Unknown worker id.
        let err = s
            .enqueue(
                "tagger",
                JobTarget::Filter("q".into()),
                Some("nope".to_string()),
                100,
            )
            .unwrap_err();
        assert!(err.contains("unknown or disconnected worker"));
        // Live worker that doesn't offer the plugin.
        s.announce("w2".into(), "worker-two".into(), vec![plugin("other")], false, 100);
        let err = s
            .enqueue(
                "tagger",
                JobTarget::Filter("q".into()),
                Some("w2".to_string()),
                100,
            )
            .unwrap_err();
        assert!(err.contains("does not provide"));
    }

    #[test]
    fn local_flag_surfaces_in_worker_status() {
        let mut s = TaggingState::default();
        s.announce("local-server".into(), "Server".into(), vec![plugin("t")], true, 100);
        s.announce("w1".into(), "remote".into(), vec![plugin("t")], false, 100);
        let statuses = s.worker_statuses();
        let local = statuses.iter().find(|w| w.worker_id == "local-server").unwrap();
        let remote = statuses.iter().find(|w| w.worker_id == "w1").unwrap();
        assert!(local.local);
        assert!(!remote.local);
    }

    #[test]
    fn busy_job_id_is_derived() {
        let mut s = state_with_worker(100);
        let job = enqueue_paths(&mut s, 100);
        assert!(s.worker_statuses()[0].busy_job_id.is_none());
        mark_running(&mut s, &job.id, "w1", 2, 100);
        assert_eq!(s.worker_statuses()[0].busy_job_id.as_deref(), Some(job.id.as_str()));
    }
}
