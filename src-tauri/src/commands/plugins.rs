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

#[derive(Debug, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub description: String,
    pub tag_prefix: String,
}

#[derive(Debug, Serialize)]
pub struct PluginRunResult {
    pub path: String,
    pub tags_added: Vec<String>,
    pub success: bool,
    pub error: Option<String>,
}

fn plugin_dir() -> std::path::PathBuf {
    crate::util::paths::data_dir().join("plugins")
}

#[tauri::command]
pub async fn list_plugins(_state: tauri::State<'_, AppState>) -> Result<Vec<PluginInfo>, String> {
    let dir = plugin_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut plugins = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let manifest_path = entry.path().join("manifest.json");
            if manifest_path.exists() {
                if let Ok(manifest) = PluginManifest::load(&manifest_path) {
                    plugins.push(PluginInfo {
                        name: manifest.name,
                        display_name: manifest.display_name,
                        version: manifest.version,
                        description: manifest.description,
                        tag_prefix: manifest.tag_prefix,
                    });
                }
            }
        }
    }

    Ok(plugins)
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

    runner::apply_plugin_output(&mut companion, manifest, &result.tags, result.meta.as_ref());

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

/// Install a plugin from a directory path.
/// Copies the plugin directory (or a single Python file with auto-generated manifest)
/// into the plugins config directory.
#[tauri::command]
pub async fn install_plugin(
    _state: tauri::State<'_, AppState>,
    path: String,
) -> Result<PluginInfo, String> {
    let source = Path::new(&path);
    if !source.exists() {
        return Err(format!("Path does not exist: {}", path));
    }

    let dest_dir = plugin_dir();
    std::fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;

    if source.is_dir() {
        let manifest_path = source.join("manifest.json");
        if !manifest_path.exists() {
            return Err("Plugin directory must contain a manifest.json".to_string());
        }
        let manifest = PluginManifest::load(&manifest_path).map_err(|e| e.to_string())?;
        let target = dest_dir.join(&manifest.name);

        copy_dir_filtered(source, &target, &|name: &str| {
            name == ".venv" || name == "venv" || name == "__pycache__"
        })
        .map_err(|e| e.to_string())?;

        rewrite_manifest_python(&target.join("manifest.json"), source)?;

        Ok(PluginInfo {
            name: manifest.name,
            display_name: manifest.display_name,
            version: manifest.version,
            description: manifest.description,
            tag_prefix: manifest.tag_prefix,
        })
    } else if source.extension().and_then(|e| e.to_str()) == Some("py") {
        let stem = source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin");
        let plugin_name = stem.replace('_', "-");
        let target = dest_dir.join(&plugin_name);
        std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;

        let dest_script = target.join(source.file_name().unwrap());
        std::fs::copy(source, &dest_script).map_err(|e| e.to_string())?;

        let source_dir = source.parent().unwrap_or(Path::new("."));
        let req_source = source_dir.join("requirements.txt");
        if req_source.exists() {
            let _ = std::fs::copy(&req_source, target.join("requirements.txt"));
        }

        let python_command = detect_venv_python(source_dir)
            .unwrap_or_else(|| "python3".to_string());

        let script_name = source
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("plugin.py");
        let manifest_json = serde_json::json!({
            "name": plugin_name,
            "display_name": plugin_name,
            "version": "1.0.0",
            "description": format!("Auto-installed plugin from {}", script_name),
            "execution": {
                "type": "cli",
                "command": python_command,
                "args": [format!("{{plugin_dir}}/{}", script_name)],
            },
            "capabilities": ["read_image"],
            "tag_prefix": plugin_name,
        });
        let manifest_str =
            serde_json::to_string_pretty(&manifest_json).map_err(|e| e.to_string())?;
        std::fs::write(target.join("manifest.json"), &manifest_str)
            .map_err(|e| e.to_string())?;

        Ok(PluginInfo {
            name: plugin_name.clone(),
            display_name: plugin_name.clone(),
            version: "1.0.0".to_string(),
            description: format!("Auto-installed plugin from {}", script_name),
            tag_prefix: plugin_name,
        })
    } else {
        Err("Path must be a plugin directory (with manifest.json) or a .py file".to_string())
    }
}

/// Look for a Python venv in `dir` and return the absolute path to its interpreter.
/// NOTE: Don't canonicalize — Python venvs work via symlink location.
fn detect_venv_python(dir: &Path) -> Option<String> {
    let abs_dir = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(dir)
    };
    for venv_name in &[".venv", "venv"] {
        let python = abs_dir.join(venv_name).join("bin").join("python");
        if python.exists() {
            return Some(python.display().to_string());
        }
    }
    None
}

fn copy_dir_filtered(
    src: &Path,
    dst: &Path,
    skip: &dyn Fn(&str) -> bool,
) -> std::io::Result<()> {
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if skip(&name_str) {
            continue;
        }
        let file_type = entry.file_type()?;
        let dest_path = dst.join(&name);
        if file_type.is_dir() {
            copy_dir_filtered(&entry.path(), &dest_path, skip)?;
        } else if file_type.is_symlink() {
            let link_target = std::fs::read_link(entry.path())?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link_target, &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}

fn rewrite_manifest_python(manifest_path: &Path, source_dir: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(manifest_path).map_err(|e| e.to_string())?;
    let mut doc: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| e.to_string())?;

    let needs_rewrite = doc
        .pointer("/execution/command")
        .and_then(|v| v.as_str())
        .map(|cmd| cmd.contains(".venv") || cmd.contains("venv"))
        .unwrap_or(false);

    if needs_rewrite {
        if let Some(abs_python) = detect_venv_python(source_dir) {
            if let Some(exec) = doc.get_mut("execution").and_then(|e| e.as_object_mut()) {
                exec.insert(
                    "command".to_string(),
                    serde_json::Value::String(abs_python),
                );
            }
            let out = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
            std::fs::write(manifest_path, out).map_err(|e| e.to_string())?;
        }
    }

    Ok(())
}
