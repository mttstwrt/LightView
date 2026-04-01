//! Resident plugin daemon — keeps a plugin subprocess alive across requests.
//!
//! Instead of spawning a new process per image (paying Python startup + ONNX model
//! loading each time), the daemon starts the plugin once with `--daemon` and
//! communicates via newline-delimited JSON on stdin/stdout.
//!
//! Protocol:
//!   Request:  `{"id": "<uuid>", "path": "/media/img.jpg"}\n`
//!   Response: `{"id": "<uuid>", "tags": [...], "meta": {...}}\n`

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::plugin::manifest::{ExecutionConfig, PluginManifest};
use crate::plugin::runner::{PluginOutput, RunError};

/// A request sent to the daemon process.
#[derive(Debug, Serialize)]
struct DaemonRequest {
    id: String,
    path: String,
}

/// A response received from the daemon process.
#[derive(Debug, Deserialize)]
struct DaemonResponse {
    id: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    meta: Option<serde_json::Value>,
}

/// Internal message sent from callers to the daemon writer task.
struct PendingRequest {
    id: String,
    path: String,
    reply: oneshot::Sender<Result<PluginOutput, RunError>>,
}

/// Manages a long-running plugin subprocess.
///
/// Concurrency model: requests are serialized through the daemon (one at a time)
/// because the Python worker runs a single inference thread to avoid VRAM
/// contention (Phase 2.3). The Rust side queues incoming requests via an mpsc
/// channel and dispatches them sequentially.
pub struct PluginDaemon {
    /// Channel to send requests to the writer task.
    tx: mpsc::Sender<PendingRequest>,
    /// Handle to the spawned background tasks (for cleanup).
    _writer_handle: tokio::task::JoinHandle<()>,
    _reader_handle: tokio::task::JoinHandle<()>,
}

impl PluginDaemon {
    /// Start a daemon for the given plugin.
    ///
    /// Spawns the plugin process with `--daemon`, waits for it to signal
    /// readiness on stderr, then returns a handle for sending requests.
    pub async fn start(
        manifest: &PluginManifest,
        plugin_dir: &Path,
        extra_env: &[(String, String)],
    ) -> Result<Self, RunError> {
        let (command, args) = match &manifest.execution {
            ExecutionConfig::Cli {
                command, args, ..
            } => (command, args),
            ExecutionConfig::Wasm { .. } => return Err(RunError::WasmNotSupported),
        };

        let dir_str = plugin_dir.display().to_string();
        let resolved_command = command.replace("{plugin_dir}", &dir_str);
        let mut resolved_args: Vec<String> = args
            .iter()
            .map(|a| a.replace("{plugin_dir}", &dir_str))
            .collect();
        resolved_args.push("--daemon".to_string());

        let mut cmd = Command::new(&resolved_command);
        cmd.args(&resolved_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(plugin_dir);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(|e| {
            RunError::Exec(format!("Failed to spawn daemon '{}': {}", command, e))
        })?;

        let stderr = child.stderr.take().ok_or_else(|| {
            RunError::Exec("Failed to capture daemon stderr".to_string())
        })?;

        // Wait for DAEMON_READY signal on stderr (with timeout).
        let mut stderr_reader = BufReader::new(stderr).lines();
        let ready = tokio::time::timeout(
            std::time::Duration::from_secs(120), // Model loading can take a while
            async {
                while let Ok(Some(line)) = stderr_reader.next_line().await {
                    if line.trim() == "DAEMON_READY" {
                        return Ok(());
                    }
                    // Log other stderr output
                    log::debug!("Plugin daemon stderr: {}", line);
                }
                Err(RunError::Exec(
                    "Daemon process exited before signaling DAEMON_READY".to_string(),
                ))
            },
        )
        .await
        .map_err(|_| RunError::Timeout(120))?;
        ready?;

        log::info!("Plugin daemon ready: {}", manifest.name);

        let stdin = child.stdin.take().ok_or_else(|| {
            RunError::Exec("Failed to capture daemon stdin".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            RunError::Exec("Failed to capture daemon stdout".to_string())
        })?;

        // Pending response map: id → oneshot sender
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Result<PluginOutput, RunError>>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Channel for incoming requests (bounded to prevent unbounded queuing)
        let (tx, mut rx) = mpsc::channel::<PendingRequest>(256);

        // Writer task: reads from the channel, writes NDJSON to stdin
        let pending_w = pending.clone();
        let writer_handle = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(req) = rx.recv().await {
                // Register the pending response before writing
                pending_w.lock().await.insert(req.id.clone(), req.reply);

                let request = DaemonRequest {
                    id: req.id,
                    path: req.path,
                };
                let mut line = serde_json::to_string(&request).unwrap();
                line.push('\n');

                if let Err(e) = stdin.write_all(line.as_bytes()).await {
                    log::error!("Failed to write to daemon stdin: {}", e);
                    break;
                }
                if let Err(e) = stdin.flush().await {
                    log::error!("Failed to flush daemon stdin: {}", e);
                    break;
                }
            }
        });

        // Reader task: reads NDJSON from stdout, dispatches to pending senders
        let pending_r = pending.clone();
        let reader_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }

                match serde_json::from_str::<DaemonResponse>(&line) {
                    Ok(resp) => {
                        let output = PluginOutput {
                            tags: resp.tags,
                            meta: resp.meta,
                        };
                        if let Some(id) = &resp.id {
                            let mut map = pending_r.lock().await;
                            if let Some(sender) = map.remove(id) {
                                let _ = sender.send(Ok(output));
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("Invalid daemon response: {} — line: {}", e, &line[..line.len().min(200)]);
                    }
                }
            }

            // Process has exited — fail all pending requests
            let mut map = pending_r.lock().await;
            for (_, sender) in map.drain() {
                let _ = sender.send(Err(RunError::Exec(
                    "Daemon process exited unexpectedly".to_string(),
                )));
            }
        });

        // Spawn a task to log remaining stderr output
        tokio::spawn(async move {
            while let Ok(Some(line)) = stderr_reader.next_line().await {
                log::debug!("Plugin daemon stderr: {}", line);
            }
        });

        Ok(Self {
            tx,
            _writer_handle: writer_handle,
            _reader_handle: reader_handle,
        })
    }

    /// Send a tagging request to the daemon and wait for the response.
    pub async fn request(&self, path: &str) -> Result<PluginOutput, RunError> {
        let id = uuid::Uuid::new_v4().to_string();
        let (reply_tx, reply_rx) = oneshot::channel();

        self.tx
            .send(PendingRequest {
                id,
                path: path.to_string(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| RunError::Exec("Daemon channel closed".to_string()))?;

        // Wait for the response with a per-request timeout
        tokio::time::timeout(std::time::Duration::from_secs(120), reply_rx)
            .await
            .map_err(|_| RunError::Timeout(120))?
            .map_err(|_| RunError::Exec("Daemon response channel dropped".to_string()))?
    }
}
