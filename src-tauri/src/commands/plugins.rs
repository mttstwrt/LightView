use crate::plugin::manifest::PluginManifest;
use crate::AppState;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub description: String,
}

/// List all discovered plugins.
#[tauri::command]
pub async fn list_plugins(
    _state: tauri::State<'_, AppState>,
) -> Result<Vec<PluginInfo>, String> {
    let plugin_dir = dirs_config_path().join("plugins");
    if !plugin_dir.exists() {
        return Ok(Vec::new());
    }

    let mut plugins = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&plugin_dir) {
        for entry in entries.flatten() {
            let manifest_path = entry.path().join("manifest.json");
            if manifest_path.exists() {
                if let Ok(manifest) = PluginManifest::load(&manifest_path) {
                    plugins.push(PluginInfo {
                        name: manifest.name,
                        display_name: manifest.display_name,
                        version: manifest.version,
                        description: manifest.description,
                    });
                }
            }
        }
    }

    Ok(plugins)
}

/// Run a plugin on a single media file.
#[tauri::command]
pub async fn run_plugin(
    _state: tauri::State<'_, AppState>,
    plugin_name: String,
    media_path: String,
    action: String,
) -> Result<String, String> {
    // TODO: Implement CLI plugin execution
    // See project overview §8.2 for the execution flow
    Err(format!(
        "Plugin execution not yet implemented: {} on {} (action: {})",
        plugin_name, media_path, action
    ))
}

/// Run a plugin on a batch of media files.
#[tauri::command]
pub async fn run_plugin_batch(
    _state: tauri::State<'_, AppState>,
    plugin_name: String,
    media_paths: Vec<String>,
    action: String,
) -> Result<Vec<String>, String> {
    // TODO: Implement batch plugin execution with transactional index updates
    Err(format!(
        "Batch plugin execution not yet implemented: {} on {} files (action: {})",
        plugin_name,
        media_paths.len(),
        action
    ))
}

fn dirs_config_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.config"))
        .join("gallery-app")
}
