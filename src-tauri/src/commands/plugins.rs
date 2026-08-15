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
use crate::plugin::manifest::PluginManifest;
use crate::plugin::runner;
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

/// Apply a single plugin result to disk and the tag index.
/// Returns (tags_added, success, error_message).
async fn apply_result(
    result: &runner::PluginResult,
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

    let requests = vec![runner::PluginRequest {
        action,
        path: media_path.clone(),
    }];

    let mut running = runner::run_plugin_stream(&manifest, &plugin_path, requests)
        .await
        .map_err(|e| e.to_string())?;

    let result = running
        .results
        .recv()
        .await
        .ok_or_else(|| "Plugin produced no output".to_string())?;
    let _ = running.finish().await;

    let (tags_added, ok, err) = apply_result(&result, &manifest, &state.cache_db).await;

    // Refresh autocomplete since tag counts may have changed.
    let db = state.cache_db.lock().await;
    if let Some(db) = db.as_ref() {
        let _ = db.rebuild_tag_counts();
        if let Ok(counts) = db.query_all_tag_counts() {
            state.autocomplete.refresh(counts).await;
        }
    }

    Ok(PluginRunResult {
        path: result.path,
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
/// Spawns one plugin subprocess, streams every path to it as NDJSON, and
/// forwards each streamed result back to the frontend as a `plugin:progress`
/// event. The plugin itself decides how to batch and parallelise. The host
/// just applies results to companion files in arrival order.
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

    log::info!("Plugin batch: {} files for {}", media_paths.len(), manifest.name);

    let cancelled = state.plugin_cancelled.clone();
    let cache_db = state.cache_db.clone();
    let autocomplete = state.autocomplete.clone();

    tauri::async_runtime::spawn(async move {
        let total = media_paths.len();
        let requests: Vec<runner::PluginRequest> = media_paths
            .into_iter()
            .map(|path| runner::PluginRequest {
                action: action.clone(),
                path,
            })
            .collect();

        let mut running = match runner::run_plugin_stream(&manifest, &plugin_path, requests).await {
            Ok(r) => r,
            Err(e) => {
                log::error!("Failed to start plugin '{}': {}", manifest.name, e);
                let _ = app_handle.emit(
                    "plugin:done",
                    PluginDoneEvent {
                        succeeded: 0,
                        failed: total,
                        cancelled: false,
                    },
                );
                return;
            }
        };

        let mut completed: usize = 0;
        let mut succeeded: usize = 0;
        let mut failed: usize = 0;
        let mut was_cancelled = false;

        // Poll the cancellation flag on a timer so cancellation is responsive
        // even when the plugin is mid-batch and producing no output.
        let mut tick = tokio::time::interval(Duration::from_millis(200));

        loop {
            tokio::select! {
                maybe = running.results.recv() => {
                    let Some(result) = maybe else { break; };

                    completed += 1;
                    let (tags_added, ok, err) =
                        apply_result(&result, &manifest, &cache_db).await;
                    if ok {
                        succeeded += 1;
                    } else {
                        failed += 1;
                    }

                    let _ = app_handle.emit(
                        "plugin:progress",
                        PluginProgressEvent {
                            completed,
                            total,
                            path: result.path.clone(),
                            tags_added,
                            success: ok,
                            error: err,
                        },
                    );
                }
                _ = tick.tick() => {
                    if cancelled.load(Ordering::Relaxed) {
                        was_cancelled = true;
                        break;
                    }
                }
            }
        }

        if was_cancelled {
            running.kill().await;
        } else {
            let _ = running.finish().await;
        }

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
