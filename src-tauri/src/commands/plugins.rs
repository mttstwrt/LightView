//! Installing, listing, and running plugins on the host.
//!
//! Desktop-only by design: running a plugin executes an arbitrary subprocess
//! with the host's privileges, so none of these commands appear in the
//! `/api/invoke` allowlist. A remote client that wants tagging done enqueues a
//! job through `tagging/` instead, which routes it to a worker that opted in.
//!
//! Batch runs are cancellable through `AppState::plugin_cancelled`, checked
//! between images — a plugin holds a model in memory for the length of a run,
//! so the alternative to cooperative cancellation is killing the process and
//! paying the model load again on the next attempt.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tauri::Emitter;

use crate::companion::reader;
use crate::companion::schema::{CompanionFile, MediaType};
use crate::companion::writer;
use crate::plugin::input::{plan_parts, InputPolicy, LocalPart, MergedItem, PartTracker};
use crate::plugin::manifest::PluginManifest;
use crate::plugin::runner;
use crate::plugin::RequestDelivery;
use crate::AppState;

// Canonical definition lives in `plugin` so the worker binary can reuse it;
// re-exported here because the command layer is where callers historically
// found it.
pub use crate::plugin::PluginInfo;

#[derive(Debug, Serialize)]
pub struct PluginRunResult {
    pub path: String,
    pub tags_added: Vec<String>,
    pub success: bool,
    pub error: Option<String>,
}

fn plugin_dir() -> std::path::PathBuf {
    crate::plugin::default_dir()
}

#[tauri::command]
pub async fn list_plugins(_state: tauri::State<'_, AppState>) -> Result<Vec<PluginInfo>, String> {
    Ok(crate::plugin::scan_plugins(&plugin_dir()))
}

fn get_or_create_companion(media_path: &Path) -> Result<CompanionFile, String> {
    match reader::read_companion_optional(media_path).map_err(|e| e.to_string())? {
        Some(c) => Ok(c),
        None => {
            let ext = media_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let media_type = MediaType::from_extension(ext).unwrap_or(MediaType::Image);
            let name = media_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            Ok(CompanionFile::new(name, media_type))
        }
    }
}

/// Apply a single merged item to disk and the tag index.
/// Returns (tags_added, success, error_message).
async fn apply_merged(
    result: &MergedItem,
    manifest: &PluginManifest,
    cache_db: &Arc<tokio::sync::Mutex<Option<crate::cache::db::CacheDb>>>,
) -> (Vec<String>, bool, Option<String>) {
    if let Some(err) = &result.error {
        return (vec![], false, Some(err.clone()));
    }

    let media = Path::new(&result.path);
    let mut companion = match get_or_create_companion(media) {
        Ok(c) => c,
        Err(e) => return (vec![], false, Some(e)),
    };

    runner::apply_plugin_output(
        &mut companion,
        &manifest.tag_prefix,
        &manifest.version,
        &result.tags,
        result.meta.as_ref(),
    );

    if let Err(e) = writer::write_companion(media, &mut companion) {
        return (vec![], false, Some(e.to_string()));
    }

    let db = cache_db.lock().await;
    if let Some(db) = db.as_ref() {
        let _ = db.reindex_tags_for_file(&result.path, &companion);
    }

    (result.tags.clone(), true, None)
}

/// Run a plugin on one file and wait for the answer.
///
/// The batch path's machinery would be overkill for a single item, but the
/// *input* must be identical or the same clip would tag differently depending
/// on whether one or two files were selected. So it prepares the same parts —
/// bounded by `MAX_VIDEO_FRAMES`, hence safe to materialize up front — and
/// merges them the same way.
#[tauri::command]
pub async fn run_plugin(
    state: tauri::State<'_, AppState>,
    plugin_name: String,
    media_path: String,
    action: String,
) -> Result<PluginRunResult, String> {
    let dir = plugin_dir();
    let (manifest, plugin_path) =
        runner::find_plugin(&dir, &plugin_name).map_err(|e| e.to_string())?;
    crate::plugin::check_api_version(&manifest, RequestDelivery::UpFront)?;
    if action != "tag" {
        return Err(format!("unknown plugin action: {action}"));
    }

    let policy = InputPolicy::for_manifest(&manifest);
    let source = Path::new(&media_path);
    let temp_dir = std::env::temp_dir()
        .join("lightview-plugin")
        .join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;

    let mut requests = Vec::new();
    for (i, part) in plan_parts(source, &policy).into_iter().enumerate() {
        let prepared = crate::plugin::input::prepare_on_pool(
            &state.thumb_pool,
            source,
            part,
            &policy,
            &temp_dir,
            &format!("{i:04}"),
        )
        .await?;
        requests.push(runner::PluginRequest {
            action: "tag".to_string(),
            path: prepared
                .unwrap_or_else(|| source.to_path_buf())
                .to_string_lossy()
                .to_string(),
        });
    }
    let expected = requests.len();

    let mut running = runner::run_plugin_stream(&manifest, &plugin_path, requests)
        .await
        .map_err(|e| e.to_string())?;

    let mut parts = Vec::with_capacity(expected);
    while parts.len() < expected {
        match running.results.recv().await {
            Some(r) => parts.push(r),
            None => break,
        }
    }
    let stderr_tail = running.stderr_tail();
    let _ = running.finish().await;
    let _ = std::fs::remove_dir_all(&temp_dir);

    if parts.is_empty() {
        return Err(runner::describe_failure(
            "Plugin produced no output".to_string(),
            &stderr_tail,
        ));
    }
    let merged = crate::plugin::input::merge_parts(&media_path, parts);
    let (tags_added, ok, err) = apply_merged(&merged, &manifest, &state.cache_db).await;

    // Refresh autocomplete since tag counts may have changed.
    let db = state.cache_db.lock().await;
    if let Some(db) = db.as_ref() {
        let _ = db.rebuild_tag_counts();
        if let Ok(counts) = db.query_all_tag_counts() {
            state.autocomplete.refresh(counts).await;
        }
    }

    Ok(PluginRunResult {
        path: merged.path,
        tags_added,
        success: ok,
        error: err,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginProgressEvent {
    pub completed: usize,
    pub total: usize,
    pub path: String,
    pub tags_added: Vec<String>,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginDoneEvent {
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: bool,
}

#[tauri::command]
pub async fn cancel_plugin_batch(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.plugin_cancelled.store(true, Ordering::Relaxed);
    Ok(())
}

/// Run a plugin on a batch of media files.
///
/// Spawns one plugin subprocess and streams requests to it as NDJSON,
/// forwarding each merged result back to the frontend as a `plugin:progress`
/// event. The plugin decides how to batch and parallelise internally; the host
/// decides what it *receives* — see [`crate::plugin::input`], which scales
/// stills to the size the manifest asked for and turns a video into several
/// frames. This used to hand over raw paths and leave videos to the plugin,
/// which is why each tagger carried its own copy of ffmpeg sampling; converging
/// on one policy is the point, so that a plugin cannot behave differently
/// depending on whether the job ran here, on the server, or on a worker.
///
/// Progress is counted in media items, not requests: a five-frame clip is one
/// step of the progress bar.
#[tauri::command]
pub async fn run_plugin_batch(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
    plugin_name: String,
    media_paths: Vec<String>,
    action: String,
) -> Result<(), String> {
    state.plugin_cancelled.store(false, Ordering::Relaxed);

    let dir = plugin_dir();
    let (manifest, plugin_path) =
        runner::find_plugin(&dir, &plugin_name).map_err(|e| e.to_string())?;
    // The desktop writes every request up front, so a pre-streaming plugin runs
    // here as it always has; only a plugin built for a *newer* host is refused.
    crate::plugin::check_api_version(&manifest, RequestDelivery::UpFront)?;
    if action != "tag" {
        return Err(format!("unknown plugin action: {action}"));
    }

    log::info!("Plugin batch: {} files for {}", media_paths.len(), manifest.name);

    let cancelled = state.plugin_cancelled.clone();
    let cache_db = state.cache_db.clone();
    let autocomplete = state.autocomplete.clone();
    let thumb_pool = state.thumb_pool.clone();

    tauri::async_runtime::spawn(async move {
        let total = media_paths.len();
        let policy = InputPolicy::for_manifest(&manifest);
        let request_total: usize = media_paths
            .iter()
            .map(|p| plan_parts(Path::new(p), &policy).len())
            .sum();

        // Temp dir for prepared inputs. Empty unless the plugin asked for
        // scaled stills or there are videos in the batch.
        let temp_dir = std::env::temp_dir()
            .join("lightview-plugin")
            .join(uuid::Uuid::new_v4().to_string());
        if let Err(e) = tokio::fs::create_dir_all(&temp_dir).await {
            log::error!("Plugin batch: cannot create temp dir: {e}");
            let _ = app_handle.emit(
                "plugin:done",
                PluginDoneEvent { succeeded: 0, failed: total, cancelled: false },
            );
            return;
        }

        let (req_tx, req_rx) = tokio::sync::mpsc::channel::<runner::PluginRequest>(8);
        let mut running = match runner::run_plugin_stream_channel(
            &manifest,
            &plugin_path,
            req_rx,
            Some(request_total),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                log::error!("Failed to start plugin '{}': {}", manifest.name, e);
                let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                let _ = app_handle.emit(
                    "plugin:done",
                    PluginDoneEvent { succeeded: 0, failed: total, cancelled: false },
                );
                return;
            }
        };

        let tracker: Arc<tokio::sync::Mutex<PartTracker<LocalPart>>> =
            Arc::new(tokio::sync::Mutex::new(PartTracker::new()));
        let producer = crate::plugin::input::spawn_local_producer(
            thumb_pool,
            media_paths,
            policy,
            temp_dir.clone(),
            req_tx,
            tracker.clone(),
        );

        let mut completed: usize = 0;
        let mut succeeded: usize = 0;
        let mut failed: usize = 0;
        let mut was_cancelled = false;

        // Poll the cancellation flag on a timer so cancellation is responsive
        // even when the plugin is mid-batch and producing no output. The same
        // tick sweeps requests the plugin has abandoned, so a skipped file is
        // reported rather than silently losing its tags.
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        let mut last_result = tokio::time::Instant::now();

        loop {
            let mut finished_items = Vec::new();
            tokio::select! {
                maybe = running.results.recv() => {
                    let Some(result) = maybe else {
                        let mut t = tracker.lock().await;
                        let orphaned = t.drain_unanswered();
                        finished_items = t.take_completed();
                        drop(t);
                        for part in orphaned {
                            part.release().await;
                        }
                        emit_items(
                            finished_items, &manifest, &cache_db, &app_handle,
                            &mut completed, &mut succeeded, &mut failed, total,
                        ).await;
                        break;
                    };

                    let mut t = tracker.lock().await;
                    let Some(part) = t.result(result) else {
                        drop(t);
                        log::warn!("plugin result for unknown path");
                        continue;
                    };
                    finished_items = t.take_completed();
                    drop(t);
                    last_result = tokio::time::Instant::now();
                    part.release().await;
                }
                _ = tick.tick() => {
                    if cancelled.load(Ordering::Relaxed) {
                        was_cancelled = true;
                        break;
                    }
                    let mut t = tracker.lock().await;
                    let stale = t.take_stale(last_result.elapsed());
                    if !stale.is_empty() {
                        finished_items = t.take_completed();
                    }
                    drop(t);
                    for (name, part) in stale {
                        log::warn!("plugin '{}' never answered '{name}'", manifest.name);
                        part.release().await;
                    }
                }
            }
            emit_items(
                finished_items, &manifest, &cache_db, &app_handle,
                &mut completed, &mut succeeded, &mut failed, total,
            ).await;
        }

        producer.abort();
        tracker.lock().await.drain_unanswered();

        if was_cancelled {
            running.kill().await;
        } else {
            let _ = running.finish().await;
        }
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;

        // Rebuild tag counts once at the end.
        let db = cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            let _ = db.rebuild_tag_counts();
            if let Ok(counts) = db.query_all_tag_counts() {
                autocomplete.refresh(counts).await;
            }
        }

        let _ = app_handle.emit(
            "plugin:done",
            PluginDoneEvent {
                succeeded,
                failed,
                cancelled: was_cancelled,
            },
        );
    });

    Ok(())
}

/// Write each finished media item and report it to the frontend.
#[allow(clippy::too_many_arguments)]
async fn emit_items(
    items: Vec<MergedItem>,
    manifest: &PluginManifest,
    cache_db: &Arc<tokio::sync::Mutex<Option<crate::cache::db::CacheDb>>>,
    app_handle: &tauri::AppHandle,
    completed: &mut usize,
    succeeded: &mut usize,
    failed: &mut usize,
    total: usize,
) {
    for item in items {
        *completed += 1;
        let (tags_added, ok, err) = apply_merged(&item, manifest, cache_db).await;
        if ok {
            *succeeded += 1;
        } else {
            *failed += 1;
        }
        let _ = app_handle.emit(
            "plugin:progress",
            PluginProgressEvent {
                completed: *completed,
                total,
                path: item.path,
                tags_added,
                success: ok,
                error: err,
            },
        );
    }
}

/// Install (or update) a plugin from a directory or a bare `.py` script.
///
/// The copy itself lives in `plugin::install` so `lightview-worker` can run the
/// same logic — a worker is the machine where hand-copying a plugin has
/// actually cost debugging time. See that module for what it does beyond a
/// recursive copy.
#[tauri::command]
pub async fn install_plugin(
    _state: tauri::State<'_, AppState>,
    path: String,
) -> Result<PluginInfo, String> {
    crate::plugin::install::install_from_path(Path::new(&path), &plugin_dir())
}

// ---------------------------------------------------------------------------
// Remote tag application (worker-tagging backend)
// ---------------------------------------------------------------------------

/// One plugin-namespace tag write, as pushed by a remote tagging worker (a
/// paired machine that fetched the image over HTTP, ran an ML tagger locally,
/// and reports the results). Same write path as a locally run plugin, so
/// `NOT has::plugin.<prefix>` filters see the file as tagged afterwards.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginTagWrite {
    pub path: String,
    pub tag_prefix: String,
    pub version: String,
    pub tags: Vec<String>,
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct ApplyPluginTagsResult {
    pub succeeded: Vec<String>,
    pub failed: Vec<crate::commands::files::FileOpError>,
}

/// Write plugin-namespace tags for a batch of files: companion sidecar +
/// `tag_index`, exactly like a local plugin run. Paths are confined to the
/// open gallery (canonicalized, so `..`/symlink escapes are rejected) because
/// this is reachable by remote paired devices.
pub async fn apply_plugin_tags_impl(
    state: &AppState,
    entries: Vec<PluginTagWrite>,
) -> Result<ApplyPluginTagsResult, String> {
    let root = state
        .canonical_gallery_root
        .read()
        .await
        .clone()
        .ok_or("No gallery open")?;

    let mut succeeded = Vec::new();
    let mut failed = Vec::new();

    for entry in entries {
        let result = apply_one_tag_write(state, &root, &entry).await;
        match result {
            Ok(()) => succeeded.push(entry.path),
            Err(error) => failed.push(crate::commands::files::FileOpError {
                path: entry.path,
                error,
            }),
        }
    }

    if !succeeded.is_empty() {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            let _ = db.rebuild_tag_counts();
            if let Ok(counts) = db.query_all_tag_counts() {
                state.autocomplete.refresh(counts).await;
            }
        }
    }

    Ok(ApplyPluginTagsResult { succeeded, failed })
}

async fn apply_one_tag_write(
    state: &AppState,
    gallery_root: &Path,
    entry: &PluginTagWrite,
) -> Result<(), String> {
    if entry.tag_prefix.trim().is_empty() {
        return Err("tag_prefix must not be empty".to_string());
    }
    let candidate = tokio::fs::canonicalize(&entry.path)
        .await
        .map_err(|_| "File not found".to_string())?;
    if !candidate.starts_with(gallery_root) {
        return Err("File is outside the current gallery".to_string());
    }

    let media = Path::new(&entry.path);
    let mut companion = get_or_create_companion(media)?;
    runner::apply_plugin_output(
        &mut companion,
        &entry.tag_prefix,
        &entry.version,
        &entry.tags,
        entry.meta.as_ref(),
    );
    writer::write_companion(media, &mut companion).map_err(|e| e.to_string())?;

    let db = state.cache_db.lock().await;
    if let Some(db) = db.as_ref() {
        let _ = db.reindex_tags_for_file(&entry.path, &companion);
    }
    Ok(())
}

#[tauri::command]
pub async fn apply_plugin_tags(
    state: tauri::State<'_, AppState>,
    entries: Vec<PluginTagWrite>,
) -> Result<ApplyPluginTagsResult, String> {
    apply_plugin_tags_impl(&state, entries).await
}
