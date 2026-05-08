//! Plugin runner: streaming NDJSON protocol.
//!
//! The host spawns the plugin as a single subprocess, writes one NDJSON
//! request per line on stdin, then closes stdin. The plugin streams NDJSON
//! results back on stdout — the host applies each as it arrives. The plugin
//! owns its own batching, threading, and any model lifecycle.
//!
//! Protocol:
//!   Request line:  {"action": "tag", "path": "/abs/path/img.jpg"}
//!   Result line:   {"path": "/abs/path/img.jpg", "tags": [...], "meta": {...}}
//!                  {"path": "...", "error": "..."}
//!
//! Cancellation: the host kills the child process. Plugins should be tolerant
//! of mid-batch SIGTERM.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::companion::schema::{CompanionFile, PluginTagEntry};
use crate::plugin::manifest::{ExecutionConfig, PluginManifest};

#[derive(Debug, Clone, Serialize)]
pub struct PluginRequest {
    pub action: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginResult {
    pub path: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("Plugin not found: {0}")]
    NotFound(String),
    #[error("Plugin execution failed: {0}")]
    Exec(String),
    #[error("WASM plugins are not yet supported")]
    WasmNotSupported,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

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

/// A running plugin subprocess streaming results back to the host.
pub struct RunningPlugin {
    /// Stream of results emitted by the plugin, in whatever order it chooses.
    pub results: mpsc::Receiver<PluginResult>,
    child: Child,
}

impl RunningPlugin {
    /// Forcibly terminate the plugin process. Used for cancellation.
    pub async fn kill(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    /// Wait for the plugin to exit naturally.
    pub async fn finish(mut self) -> Result<(), RunError> {
        let status = self.child.wait().await?;
        if !status.success() {
            return Err(RunError::Exec(format!("Plugin exited with {}", status)));
        }
        Ok(())
    }
}

/// Spawn the plugin and start streaming `requests` to its stdin.
///
/// Returns immediately — results arrive asynchronously via `RunningPlugin::results`.
/// The host writes all requests then closes stdin; the plugin is free to batch
/// or parallelise internally and emit results in any order.
pub async fn run_plugin_stream(
    manifest: &PluginManifest,
    plugin_dir: &Path,
    requests: Vec<PluginRequest>,
) -> Result<RunningPlugin, RunError> {
    let (command, args) = match &manifest.execution {
        ExecutionConfig::Cli { command, args } => (command, args),
        ExecutionConfig::Wasm { .. } => return Err(RunError::WasmNotSupported),
    };

    let dir_str = plugin_dir.display().to_string();
    let resolved_command = command.replace("{plugin_dir}", &dir_str);
    let resolved_args: Vec<String> = args
        .iter()
        .map(|a| a.replace("{plugin_dir}", &dir_str))
        .collect();

    let mut cmd = Command::new(&resolved_command);
    cmd.args(&resolved_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(plugin_dir)
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| RunError::Exec(format!("Failed to spawn '{}': {}", command, e)))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| RunError::Exec("Failed to capture stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RunError::Exec("Failed to capture stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| RunError::Exec("Failed to capture stderr".into()))?;

    // Writer task: serialize each request as NDJSON, write, then drop stdin to send EOF.
    tokio::spawn(async move {
        let mut stdin = stdin;
        for req in requests {
            let mut line = match serde_json::to_string(&req) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("Failed to serialize plugin request: {}", e);
                    return;
                }
            };
            line.push('\n');
            if stdin.write_all(line.as_bytes()).await.is_err() {
                // Plugin closed stdin or died; nothing more we can do.
                return;
            }
        }
    });

    // Reader task: parse NDJSON results from stdout, forward over the channel.
    let (tx, rx) = mpsc::channel::<PluginResult>(64);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<PluginResult>(line) {
                Ok(result) => {
                    if tx.send(result).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Plugin produced invalid NDJSON: {} — line: {}",
                        e,
                        &line[..line.len().min(200)]
                    );
                }
            }
        }
    });

    // Stderr task: log everything the plugin writes there.
    let plugin_name = manifest.name.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            log::debug!("[{}] {}", plugin_name, line);
        }
    });

    Ok(RunningPlugin { results: rx, child })
}

/// Apply a plugin result to a companion file under the plugin's tag namespace.
pub fn apply_plugin_output(
    companion: &mut CompanionFile,
    manifest: &PluginManifest,
    tags: &[String],
    meta: Option<&serde_json::Value>,
) {
    let entry = companion
        .tags
        .plugins
        .entry(manifest.tag_prefix.clone())
        .or_insert_with(|| PluginTagEntry {
            version: manifest.version.clone(),
            tags: Vec::new(),
            extra: std::collections::HashMap::new(),
        });

    entry.version = manifest.version.clone();
    entry.tags = tags.iter().map(|t| t.replace(' ', "_")).collect();

    if let Some(meta) = meta {
        companion
            .meta
            .plugins
            .insert(manifest.tag_prefix.clone(), meta.clone());
    }
}
