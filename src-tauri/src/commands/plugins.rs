use std::path::Path;

use serde::Serialize;

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
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.config"))
        .join("gallery-app")
        .join("plugins")
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

    let output = runner::run_cli_plugin(&manifest, &plugin_path, &media_path, &action)
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

/// Run a plugin on a batch of media files.
#[tauri::command]
pub async fn run_plugin_batch(
    state: tauri::State<'_, AppState>,
    plugin_name: String,
    media_paths: Vec<String>,
    action: String,
) -> Result<Vec<PluginRunResult>, String> {
    let dir = plugin_dir();
    let (manifest, plugin_path) =
        runner::find_plugin(&dir, &plugin_name).map_err(|e| e.to_string())?;

    let mut results = Vec::with_capacity(media_paths.len());

    for media_path in &media_paths {
        let media = Path::new(media_path);

        match runner::run_cli_plugin(&manifest, &plugin_path, media_path, &action).await {
            Ok(output) => {
                // App handles all companion file I/O
                let mut companion = match get_or_create_companion(media) {
                    Ok(c) => c,
                    Err(e) => {
                        results.push(PluginRunResult {
                            path: media_path.clone(),
                            tags_added: vec![],
                            success: false,
                            error: Some(e),
                        });
                        continue;
                    }
                };
                runner::apply_plugin_output(&mut companion, &manifest, &output);

                if let Err(e) = writer::write_companion(media, &mut companion) {
                    results.push(PluginRunResult {
                        path: media_path.clone(),
                        tags_added: vec![],
                        success: false,
                        error: Some(e.to_string()),
                    });
                    continue;
                }

                // Update tag index
                let db = state.cache_db.lock().await;
                if let Some(db) = db.as_ref() {
                    let _ = db.reindex_tags_for_file(media_path, &companion);
                }

                let tags_added = output.tags.clone();
                results.push(PluginRunResult {
                    path: media_path.clone(),
                    tags_added,
                    success: true,
                    error: None,
                });
            }
            Err(e) => {
                results.push(PluginRunResult {
                    path: media_path.clone(),
                    tags_added: vec![],
                    success: false,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    // Rebuild tag counts and refresh autocomplete once after all files processed
    let db = state.cache_db.lock().await;
    if let Some(db) = db.as_ref() {
        let _ = db.rebuild_tag_counts();
        if let Ok(counts) = db.query_all_tag_counts() {
            state.autocomplete.refresh(counts).await;
        }
    }

    Ok(results)
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

        // Copy the entire directory
        copy_dir_recursive(source, &target).map_err(|e| e.to_string())?;

        Ok(PluginInfo {
            name: manifest.name,
            display_name: manifest.display_name,
            version: manifest.version,
            description: manifest.description,
            tag_prefix: manifest.tag_prefix,
        })
    } else if source.extension().and_then(|e| e.to_str()) == Some("py") {
        // Install a single Python file — auto-generate a manifest
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
                "command": "python3",
                "args": [format!("{{plugin_dir}}/{}", script_name)],
                "timeout_seconds": 60
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

/// Recursively copy a directory.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}
