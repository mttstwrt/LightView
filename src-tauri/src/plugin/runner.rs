use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::companion::schema::{CompanionFile, PluginTagEntry};
use crate::plugin::manifest::{ExecutionConfig, PluginManifest};

/// Input sent to a plugin process via stdin.
/// The plugin only receives the image path — the app handles all
/// companion file reading/writing and batching.
#[derive(Debug, Serialize)]
pub struct PluginInput {
    pub action: String,
    pub media_path: String,
}

/// Output expected from a plugin process on stdout.
#[derive(Debug, Deserialize)]
pub struct PluginOutput {
    pub tags: Vec<String>,
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("Plugin not found: {0}")]
    NotFound(String),
    #[error("Plugin execution failed: {0}")]
    Exec(String),
    #[error("Plugin timed out after {0}s")]
    Timeout(u64),
    #[error("Plugin produced invalid output: {0}")]
    InvalidOutput(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("WASM plugins are not yet supported")]
    WasmNotSupported,
}

/// Locate a plugin's manifest by name from the plugin directory.
pub fn find_plugin(plugin_dir: &Path, name: &str) -> Result<(PluginManifest, PathBuf), RunError> {
    let dir = plugin_dir.join(name);
    let manifest_path = dir.join("manifest.json");
    if !manifest_path.exists() {
        return Err(RunError::NotFound(name.to_string()));
    }
    let manifest =
        PluginManifest::load(&manifest_path).map_err(|e| RunError::Exec(e.to_string()))?;
    Ok((manifest, dir))
}

/// Run a CLI plugin on a single media file.
/// The plugin receives only the action and media path — it returns tags
/// and optional metadata. The app is responsible for writing results
/// to the companion file and updating the index.
pub async fn run_cli_plugin(
    manifest: &PluginManifest,
    plugin_dir: &Path,
    media_path: &str,
    action: &str,
) -> Result<PluginOutput, RunError> {
    let (command, args, timeout_secs) = match &manifest.execution {
        ExecutionConfig::Cli {
            command,
            args,
            timeout_seconds,
        } => (command, args, timeout_seconds.unwrap_or(60)),
        ExecutionConfig::Wasm { .. } => return Err(RunError::WasmNotSupported),
    };

    let input = PluginInput {
        action: action.to_string(),
        media_path: media_path.to_string(),
    };
    let input_json = serde_json::to_string(&input).map_err(|e| RunError::Exec(e.to_string()))?;

    // Resolve {plugin_dir} placeholder in command and args.
    let resolved_command = command.replace("{plugin_dir}", &plugin_dir.display().to_string());
    let resolved_args: Vec<String> = args
        .iter()
        .map(|a| a.replace("{plugin_dir}", &plugin_dir.display().to_string()))
        .collect();

    let mut child = Command::new(&resolved_command)
        .args(&resolved_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(plugin_dir)
        .spawn()
        .map_err(|e| RunError::Exec(format!("Failed to spawn '{}': {}", command, e)))?;

    // Write input to stdin
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input_json.as_bytes())
            .await
            .map_err(|e| RunError::Exec(format!("Failed to write to stdin: {}", e)))?;
        // Drop stdin to signal EOF
    }

    // Wait with timeout
    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
        .await
        .map_err(|_| RunError::Timeout(timeout_secs))?
        .map_err(|e| RunError::Exec(format!("Process error: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RunError::Exec(format!(
            "Plugin exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let plugin_output: PluginOutput = serde_json::from_str(stdout.trim()).map_err(|e| {
        RunError::InvalidOutput(format!(
            "{}: stdout was: {}",
            e,
            &stdout[..stdout.len().min(500)]
        ))
    })?;

    Ok(plugin_output)
}

/// Apply plugin output to a companion file.
/// Writes tags under the plugin's namespace and stores any metadata.
pub fn apply_plugin_output(
    companion: &mut CompanionFile,
    manifest: &PluginManifest,
    output: &PluginOutput,
) {
    // Write tags under the plugin namespace
    let entry = companion
        .tags
        .plugins
        .entry(manifest.tag_prefix.clone())
        .or_insert_with(|| PluginTagEntry {
            version: manifest.version.clone(),
            tags: Vec::new(),
            extra: std::collections::HashMap::new(),
        });

    // Replace tags entirely (plugin is authoritative for its namespace)
    entry.version = manifest.version.clone();
    entry.tags = output.tags.iter().map(|t| t.replace(' ', "_")).collect();

    // Store plugin metadata if provided
    if let Some(meta) = &output.meta {
        companion
            .meta
            .plugins
            .insert(manifest.tag_prefix.clone(), meta.clone());
    }
}
