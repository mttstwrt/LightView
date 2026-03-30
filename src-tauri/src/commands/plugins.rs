use std::path::Path;
use std::sync::atomic::Ordering;

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

/// Result of running a plugin on a single file.
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

/// List all discovered plugins.
#[tauri::command]
pub async fn list_plugins(
    _state: tauri::State<'_, AppState>,
) -> Result<Vec<PluginInfo>, String> {
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

/// Read or create a companion file for the given media path.
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

/// Run a plugin on a single media file.
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

    let media = Path::new(&media_path);

    let output = runner::run_cli_plugin(&manifest, &plugin_path, &media_path, &action, &[])
        .await
        .map_err(|e| e.to_string())?;

    // App handles all companion file I/O
    let mut companion = get_or_create_companion(media)?;
    runner::apply_plugin_output(&mut companion, &manifest, &output);
    writer::write_companion(media, &mut companion).map_err(|e| e.to_string())?;

    // Update the tag index and rebuild counts so autocomplete picks up plugin tags
    let db = state.cache_db.lock().await;
    if let Some(db) = db.as_ref() {
        let _ = db.reindex_tags_for_file(&media_path, &companion);
        let _ = db.rebuild_tag_counts();
        if let Ok(counts) = db.query_all_tag_counts() {
            state.autocomplete.refresh(counts).await;
        }
    }

    let tags_added = output.tags.clone();
    Ok(PluginRunResult {
        path: media_path,
        tags_added,
        success: true,
        error: None,
    })
}

/// Default maximum number of plugin subprocesses to run concurrently in a batch.
const DEFAULT_PLUGIN_BATCH_CONCURRENCY: usize = 4;

/// Per-file progress event emitted to the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct PluginProgressEvent {
    pub completed: usize,
    pub total: usize,
    pub path: String,
    pub tags_added: Vec<String>,
    pub success: bool,
    pub error: Option<String>,
}

/// Final summary event emitted when the batch completes.
#[derive(Debug, Clone, Serialize)]
pub struct PluginDoneEvent {
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: bool,
}

/// Cancel any running plugin batch.
#[tauri::command]
pub async fn cancel_plugin_batch(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.plugin_cancelled.store(true, Ordering::Relaxed);
    Ok(())
}

/// Run a plugin on a batch of media files with configurable concurrency.
///
/// Returns immediately and runs the batch in the background. Progress is
/// reported via `plugin:progress` events and completion via `plugin:done`.
/// Events emitted from within a synchronous `#[tauri::command]` handler are
/// buffered until the command returns, so we must spawn the work as a
/// separate task to get real-time delivery.
#[tauri::command]
pub async fn run_plugin_batch(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
    plugin_name: String,
    media_paths: Vec<String>,
    action: String,
    max_concurrent: Option<usize>,
    onnx_threads: Option<usize>,
) -> Result<(), String> {
    // Reset cancellation flag at the start of a new batch.
    state.plugin_cancelled.store(false, Ordering::Relaxed);

    let concurrency = max_concurrent
        .unwrap_or(DEFAULT_PLUGIN_BATCH_CONCURRENCY)
        .max(1);

    log::info!(
        "Plugin batch: {} files, concurrency={} (raw={:?}), onnx_threads={:?}",
        media_paths.len(),
        concurrency,
        max_concurrent,
        onnx_threads,
    );

    let extra_env: Vec<(String, String)> = match onnx_threads {
        Some(n) => vec![("ONNX_THREADS".to_string(), n.max(1).to_string())],
        None => vec![],
    };

    let dir = plugin_dir();
    let (manifest, plugin_path) =
        runner::find_plugin(&dir, &plugin_name).map_err(|e| e.to_string())?;

    let cancelled = state.plugin_cancelled.clone();
    let cache_db = state.cache_db.clone();
    let autocomplete = state.autocomplete.clone();

    // Spawn the batch as a separate async task so events are delivered in real-time.
    tauri::async_runtime::spawn(async move {
        use futures::stream::{self, StreamExt};

        let total = media_paths.len();
        let mut completed: usize = 0;
        let mut succeeded: usize = 0;
        let mut failed: usize = 0;
        let mut was_cancelled = false;

        let mut result_stream = stream::iter(media_paths.into_iter().map(|media_path| {
            let manifest = &manifest;
            let plugin_path = &plugin_path;
            let action = &action;
            let extra_env = &extra_env;
            async move {
                let res =
                    runner::run_cli_plugin(manifest, plugin_path, &media_path, action, extra_env)
                        .await
                        .map_err(|e| e.to_string());
                (media_path, res)
            }
        }))
        .buffer_unordered(concurrency);

        while let Some((media_path, res)) = result_stream.next().await {
            if cancelled.load(Ordering::Relaxed) {
                was_cancelled = true;
                break;
            }

            completed += 1;

            let (progress_path, progress_tags, progress_ok, progress_err) = match res {
                Ok(output) => {
                    let media = Path::new(&media_path);

                    let companion_result = get_or_create_companion(media)
                        .and_then(|mut companion| {
                            runner::apply_plugin_output(&mut companion, &manifest, &output);
                            writer::write_companion(media, &mut companion)
                                .map_err(|e| e.to_string())?;
                            Ok((output.tags.clone(), companion))
                        });

                    match companion_result {
                        Ok((tags, companion)) => {
                            let db = cache_db.lock().await;
                            if let Some(db) = db.as_ref() {
                                let _ = db.reindex_tags_for_file(&media_path, &companion);
                            }
                            succeeded += 1;
                            (media_path, tags, true, None)
                        }
                        Err(e) => {
                            failed += 1;
                            (media_path, vec![], false, Some(e))
                        }
                    }
                }
                Err(e) => {
                    failed += 1;
                    (media_path, vec![], false, Some(e))
                }
            };

            let _ = app_handle.emit(
                "plugin:progress",
                PluginProgressEvent {
                    completed,
                    total,
                    path: progress_path,
                    tags_added: progress_tags,
                    success: progress_ok,
                    error: progress_err,
                },
            );
        }

        // Rebuild tag counts once after all files processed
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
        // Install from a plugin directory (must have manifest.json)
        let manifest_path = source.join("manifest.json");
        if !manifest_path.exists() {
            return Err("Plugin directory must contain a manifest.json".to_string());
        }
        let manifest = PluginManifest::load(&manifest_path).map_err(|e| e.to_string())?;
        let target = dest_dir.join(&manifest.name);

        // Copy the directory but skip venv directories (they contain symlinks
        // and hardcoded paths that break when relocated).
        copy_dir_filtered(source, &target, &|name: &str| {
            name == ".venv" || name == "venv" || name == "__pycache__"
        })
        .map_err(|e| e.to_string())?;

        // If the manifest references a venv-relative python ({plugin_dir}/.venv/...),
        // resolve it to the absolute path of the *source* venv's interpreter so the
        // installed plugin can still run without a copied venv.
        rewrite_manifest_python(&target.join("manifest.json"), source)?;

        Ok(PluginInfo {
            name: manifest.name,
            display_name: manifest.display_name,
            version: manifest.version,
            description: manifest.description,
            tag_prefix: manifest.tag_prefix,
        })
    } else if source.extension().and_then(|e| e.to_str()) == Some("py") {
        // Install a single Python file — auto-generate a manifest.
        // If a venv exists next to the source file, use its Python interpreter
        // so that plugin dependencies are available at runtime.
        let stem = source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin");
        let plugin_name = stem.replace('_', "-");
        let target = dest_dir.join(&plugin_name);
        std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;

        // Copy the Python file
        let dest_script = target.join(source.file_name().unwrap());
        std::fs::copy(source, &dest_script).map_err(|e| e.to_string())?;

        // Copy requirements.txt if present next to the source
        let source_dir = source.parent().unwrap_or(Path::new("."));
        let req_source = source_dir.join("requirements.txt");
        if req_source.exists() {
            let _ = std::fs::copy(&req_source, target.join("requirements.txt"));
        }

        // Detect a venv next to the source file and use its absolute Python path
        let python_command = detect_venv_python(source_dir)
            .unwrap_or_else(|| "python3".to_string());

        // Generate manifest
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
                "timeout_seconds": 120
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
/// Checks `.venv/bin/python` and `venv/bin/python`.
///
/// NOTE: We must NOT canonicalize/resolve symlinks here. Python venvs work by
/// having `bin/python` be a symlink inside the venv directory — Python uses the
/// symlink's location to discover `lib/pythonX.Y/site-packages`. Resolving to
/// the system Python binary would lose the venv's package context.
fn detect_venv_python(dir: &Path) -> Option<String> {
    // Make sure `dir` is absolute so the resulting path works from any cwd.
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

/// Recursively copy a directory, skipping entries whose name matches `skip`.
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
            // Reproduce symlinks instead of following them
            let link_target = std::fs::read_link(entry.path())?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link_target, &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}

/// Rewrite a copied manifest's command to use the absolute path of the source
/// venv's Python interpreter, since venvs are not copied.
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
