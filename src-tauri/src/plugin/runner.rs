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
//! Plugins MUST emit results incrementally as they process requests — never
//! buffer stdin to EOF before producing output. Remote hosts
//! (`lightview-worker`) bound how many input files exist on disk and only
//! download more as results arrive, so a plugin that waits for EOF deadlocks.
//! The host sets `LIGHTVIEW_JOB_TOTAL` (expected request count) in the
//! plugin's environment so pool/instance sizing doesn't need to see the whole
//! request list up front.
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

/// Resolve a plugin by its manifest `name`. The directory usually matches the
/// name (fast path), but it doesn't have to — e.g. `example-auto-tagger/`
/// declares `name: "example-image-tagger"` — so fall back to scanning every
/// subdirectory's manifest, the same match `scan_plugins` announces by.
pub fn find_plugin(plugin_dir: &Path, name: &str) -> Result<(PluginManifest, PathBuf), RunError> {
    let dir = plugin_dir.join(name);
    let manifest_path = dir.join("manifest.json");
    if manifest_path.exists() {
        let manifest =
            PluginManifest::load(&manifest_path).map_err(|e| RunError::Exec(e.to_string()))?;
        if manifest.name == name {
            return Ok((manifest, dir));
        }
    }
    if let Ok(entries) = std::fs::read_dir(plugin_dir) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if let Ok(manifest) = PluginManifest::load(&dir.join("manifest.json")) {
                if manifest.name == name {
                    return Ok((manifest, dir));
                }
            }
        }
    }
    Err(RunError::NotFound(name.to_string()))
}

/// Lines of plugin stderr kept for replay. Enough to carry a Python traceback
/// plus the line that caused it; small enough that a chatty plugin's progress
/// output cannot grow the host's memory over a long job.
const STDERR_TAIL_LINES: usize = 40;

/// Attach a plugin's recent stderr to a failure message, so the reason travels
/// with the job instead of living only in a debug log nobody had enabled.
///
/// Silence is reported explicitly rather than omitted: for a plugin waiting on
/// an EOF that will never come, "wrote nothing to stderr" *is* the diagnosis,
/// and it is the one a reader would otherwise have to infer from an absence.
pub fn describe_failure(error: String, stderr_tail: &[String]) -> String {
    if stderr_tail.is_empty() {
        return format!("{error} (the plugin wrote nothing to stderr)");
    }
    format!(
        "{error}\nplugin stderr (last {} lines):\n{}",
        stderr_tail.len(),
        stderr_tail.join("\n")
    )
}

/// A running plugin subprocess streaming results back to the host.
pub struct RunningPlugin {
    /// Stream of results emitted by the plugin, in whatever order it chooses.
    pub results: mpsc::Receiver<PluginResult>,
    stderr_tail: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    child: Child,
}

impl RunningPlugin {
    /// The last few lines the plugin wrote to stderr.
    ///
    /// Plugin stderr is logged at debug, which is right for a tagger that
    /// narrates every batch — but it means that at the default level the one
    /// channel that explains a failure is invisible, and the most diagnostic
    /// signal of all (the plugin saying *nothing*, because it is waiting for an
    /// EOF that will not come) shows as an empty list here. Callers attach this
    /// to the error they report, so a failed job explains itself without asking
    /// the user to reproduce it at debug level.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }
}

impl RunningPlugin {
    /// Attach the plugin's recent stderr to a failure message. See
    /// [`describe_failure`]; use that directly when the failure is only known
    /// after [`Self::finish`] has consumed the handle.
    pub fn explain(&self, error: String) -> String {
        describe_failure(error, &self.stderr_tail())
    }

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
    // Capacity matches the request count, so try_send never fails and the
    // whole list is buffered up front — same behavior as before the channel
    // variant existed.
    let total = requests.len();
    let (tx, rx) = mpsc::channel(total.max(1));
    for req in requests {
        let _ = tx.try_send(req);
    }
    drop(tx); // closes the stream → stdin EOF after the last request
    run_plugin_stream_channel(manifest, plugin_dir, rx, Some(total)).await
}

/// Like [`run_plugin_stream`], but requests arrive over a channel so the
/// caller can feed them incrementally. Closing the sender ends the stream
/// (stdin EOF). Used by `lightview-worker`, which downloads images while the
/// plugin is already tagging earlier ones — one subprocess (one model load)
/// for an arbitrarily long job, with only a bounded number of files on disk.
///
/// `total_hint` is exported to the plugin as `LIGHTVIEW_JOB_TOTAL` so it can
/// size instance pools without reading its whole request stream first.
pub async fn run_plugin_stream_channel(
    manifest: &PluginManifest,
    plugin_dir: &Path,
    mut requests: mpsc::Receiver<PluginRequest>,
    total_hint: Option<usize>,
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
    if let Some(total) = total_hint {
        cmd.env("LIGHTVIEW_JOB_TOTAL", total.to_string());
    }

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
        while let Some(req) = requests.recv().await {
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

    // Stderr task: log everything the plugin writes there, and keep the tail
    // so a failure can quote it (see `RunningPlugin::stderr_tail`).
    let plugin_name = manifest.name.clone();
    let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(
        std::collections::VecDeque::with_capacity(STDERR_TAIL_LINES),
    ));
    let tail_writer = stderr_tail.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            log::debug!("[{}] {}", plugin_name, line);
            if let Ok(mut q) = tail_writer.lock() {
                if q.len() == STDERR_TAIL_LINES {
                    q.pop_front();
                }
                q.push_back(line);
            }
        }
    });

    Ok(RunningPlugin {
        results: rx,
        stderr_tail,
        child,
    })
}

/// Apply a plugin result to a companion file under the plugin's tag namespace.
/// Takes the prefix/version directly (not the manifest) so remote workers —
/// which have no local manifest — can write through the same path.
pub fn apply_plugin_output(
    companion: &mut CompanionFile,
    tag_prefix: &str,
    version: &str,
    tags: &[String],
    meta: Option<&serde_json::Value>,
) {
    let entry = companion
        .tags
        .plugins
        .entry(tag_prefix.to_string())
        .or_insert_with(|| PluginTagEntry {
            version: version.to_string(),
            tags: Vec::new(),
            extra: std::collections::HashMap::new(),
        });

    entry.version = version.to_string();
    entry.tags = tags.iter().map(|t| t.replace(' ', "_")).collect();

    if let Some(meta) = meta {
        companion
            .meta
            .plugins
            .insert(tag_prefix.to_string(), meta.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::schema::MediaType;

    #[test]
    fn apply_plugin_output_writes_prefix_namespace() {
        let mut companion = CompanionFile::new("img.jpg", MediaType::Image);
        apply_plugin_output(
            &mut companion,
            "wd-tagger",
            "1.2.0",
            &["blue sky".to_string(), "beach".to_string()],
            Some(&serde_json::json!({ "score": 0.9 })),
        );

        let entry = companion.tags.plugins.get("wd-tagger").unwrap();
        assert_eq!(entry.version, "1.2.0");
        // Spaces are normalized to underscores.
        assert_eq!(entry.tags, vec!["blue_sky", "beach"]);
        assert!(companion.meta.plugins.contains_key("wd-tagger"));

        // A later run replaces the tag list rather than appending.
        apply_plugin_output(&mut companion, "wd-tagger", "1.3.0", &["beach".to_string()], None);
        let entry = companion.tags.plugins.get("wd-tagger").unwrap();
        assert_eq!(entry.version, "1.3.0");
        assert_eq!(entry.tags, vec!["beach"]);
    }
}
