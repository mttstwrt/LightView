//! The executor: one subprocess, streaming NDJSON, and the rules that decide a
//! plugin has stopped answering.
//!
//! **One executor, in process, and no queue between machines.** A run is a
//! local operation — read bytes, feed the subprocess, write companions, report
//! progress — driven from the UI on a loopback bind and from `lightview tag` on
//! the command line. Those are the same code path with a different progress
//! sink, which is why the sink is a closure and not a trait with one
//! implementation.
//!
//! ## The protocol
//!
//! One JSON object per line, both ways:
//!
//! - request `{"action":"tag","path":"/abs/path.webp"}`
//! - result `{"path":"…","tags":[…],"meta":{…}}` or `{"path":"…","error":"…"}`
//!
//! **Exactly one result per request**, including an error result for anything
//! the plugin cannot process, and **plugins must emit each result as soon as it
//! is ready** rather than buffering stdin to EOF. A plugin that waits for EOF
//! deadlocks any job larger than the pending window by construction — which is
//! precisely the bug this codebase shipped for a year. `LIGHTVIEW_JOB_TOTAL`
//! carries the expected request count so a plugin never needs the read-to-EOF
//! sizing pattern that caused it.
//!
//! ## Why the staleness rules are counts, not clocks
//!
//! A tagger's first run legitimately produces nothing for minutes while it
//! downloads and loads a model, so **a live subprocess is not a progressing
//! one** and liveness is worthless as a signal. Every rule below therefore
//! counts *results*, and the two clocks that remain are backstops which only
//! start once the plugin has answered something — so a model download is never
//! mistaken for a wedge.
//!
//! A run that stops progressing is failed and reported, and **never retried**:
//! retrying hands the same wedge to the same plugin.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::plugin::input::{Part, PartResult};
use crate::plugin::manifest::Installed;

/// Requests in flight at once.
///
/// Also the number of temp files on disk, which is what it really bounds: the
/// plugin reads from a directory this executor fills, and an unbounded window
/// means an unbounded temp directory for a library-sized run.
pub const MAX_PENDING: usize = 32;

/// Abandon a request once the plugin has answered this many *others*.
///
/// A count rather than a clock, so a slow CPU tagger never sheds images just
/// for being slow: what marks a request as lost is the plugin moving past it,
/// not time passing.
const STALE_AFTER_RESULTS: usize = 128;

/// Clears a job's tail, where no further results arrive to drive the count
/// above. Starts only once the plugin has answered something.
const IDLE_RECLAIM: Duration = Duration::from_secs(5 * 60);

/// Outer backstop, refreshed **only by results that matched** a pending
/// request — an unmatched line must not keep a wedged run alive.
const NO_RESULT_STALL: Duration = Duration::from_secs(20 * 60);

/// A compile-time invariant: a single clip must never fill the window by
/// itself, or a run over one video deadlocks on its own frames.
const _: () = assert!(
    (crate::plugin::manifest::MAX_VIDEO_FRAMES as usize) * 2 <= MAX_PENDING,
    "a clip's frames must fit in the pending window twice over",
);

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("could not start {0}: {1}")]
    Spawn(String, std::io::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0} stopped answering")]
    Stalled(String),
}

/// What a plugin wrote on one line of stdout.
#[derive(Debug, Deserialize)]
struct Line {
    path: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    meta: Option<Value>,
    #[serde(default)]
    error: Option<String>,
}

/// A running plugin, fed one part at a time.
///
/// Deliberately not an iterator or a stream: the caller interleaves *producing*
/// parts (which costs a decode) with *consuming* results, and the window is
/// what couples the two. A stream would either buffer the whole plan or hide
/// the coupling behind a combinator.
pub struct Session {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    /// Temp file name → the part it belongs to.
    pending: HashMap<String, Pending>,
    /// Results seen so far, for the staleness count.
    answered: usize,
    /// When the last *matched* result arrived. `None` until the first one, so
    /// a model download is not a stall.
    last_match: Option<Instant>,
    name: String,
}

struct Pending {
    /// How many results had arrived when this request went out. The difference
    /// against `answered` is how far the plugin has moved past it.
    issued_at: usize,
    path: PathBuf,
}

/// One answer, paired with the part it belongs to.
pub struct Answer {
    pub name: String,
    pub result: PartResult,
}

impl Session {
    /// Spawn the plugin. `total` is the request count it will be sent, which
    /// travels in `LIGHTVIEW_JOB_TOTAL`.
    pub fn start(plugin: &Installed, total: usize) -> Result<Self, RunError> {
        let dir = plugin.directory.to_string_lossy().to_string();
        let expand = |s: &str| s.replace("{plugin_dir}", &dir);

        let mut command = Command::new(expand(&plugin.manifest.execution.command));
        for arg in &plugin.manifest.execution.args {
            command.arg(expand(arg));
        }
        let mut child = command
            .env("LIGHTVIEW_JOB_TOTAL", total.to_string())
            .current_dir(&plugin.directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited, so a plugin's own diagnostics reach whoever started
            // the run — a tagger printing "downloading model…" is the only
            // explanation a five-minute silence has.
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| RunError::Spawn(plugin.manifest.name.clone(), e))?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            pending: HashMap::new(),
            answered: 0,
            last_match: None,
            name: plugin.manifest.name.clone(),
        })
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    pub fn has_room(&self) -> bool {
        self.pending.len() < MAX_PENDING
    }

    /// Send one request.
    pub async fn send(&mut self, part: &Part) -> Result<(), RunError> {
        let line = serde_json::json!({ "action": "tag", "path": part.path });
        self.stdin.write_all(line.to_string().as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        self.pending.insert(
            part.name.clone(),
            Pending {
                issued_at: self.answered,
                path: part.path.clone(),
            },
        );
        Ok(())
    }

    /// Tell the plugin no more requests are coming. Must be called once the
    /// plan is exhausted, or a well-behaved plugin waits on stdin forever.
    pub async fn finish_sending(&mut self) -> Result<(), RunError> {
        self.stdin.shutdown().await?;
        Ok(())
    }

    /// Wait for the next answer.
    ///
    /// Returns `Ok(None)` when the plugin has closed stdout and nothing is
    /// outstanding. Requests the plugin has moved past are reaped here and
    /// returned as errors, so a caller that only ever reads answers still
    /// accounts for every part exactly once.
    pub async fn next_answer(&mut self) -> Result<Option<Answer>, RunError> {
        loop {
            if let Some(stale) = self.take_stale() {
                return Ok(Some(stale));
            }
            if self.pending.is_empty() {
                return Ok(None);
            }

            // `None` until the first result: no clock runs, because a first
            // run is legitimately silent for as long as the model takes to
            // download and load.
            let deadline = self.last_match.map(|last| {
                (last + IDLE_RECLAIM.min(NO_RESULT_STALL))
                    .saturating_duration_since(Instant::now())
            });

            let line = match deadline {
                None => self.stdout.next_line().await?,
                Some(remaining) => {
                    match tokio::time::timeout(remaining, self.stdout.next_line()).await {
                        Ok(line) => line?,
                        // The plugin answered once and then went quiet for the
                        // whole window. Everything still outstanding is lost.
                        Err(_) => return Err(RunError::Stalled(self.name.clone())),
                    }
                }
            };

            let Some(line) = line else {
                // stdout closed with work outstanding: the plugin exited or
                // crashed. Report what is left rather than hanging.
                if self.pending.is_empty() {
                    return Ok(None);
                }
                let (name, _) = self.pending.iter().next().map(|(k, v)| (k.clone(), v.path.clone())).unwrap();
                self.pending.remove(&name);
                return Ok(Some(Answer {
                    name,
                    result: PartResult {
                        error: Some(format!("{} exited before answering", self.name)),
                        ..Default::default()
                    },
                }));
            };

            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let parsed: Line = match serde_json::from_str(line) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("{}: unparseable result line ({e}): {line}", self.name);
                    continue;
                }
            };

            self.answered += 1;
            // Keyed on the file *name*: a plugin that canonicalizes its input
            // under a symlinked TMPDIR echoes back a different path for the
            // same file, and matching on the whole string would drop every
            // result it produced.
            let key = std::path::Path::new(&parsed.path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| parsed.path.clone());

            if self.pending.remove(&key).is_none() {
                // Logged rather than dropped silently: an unmatched result is
                // either a plugin bug or a host bug, and both are invisible
                // otherwise. It deliberately does *not* refresh the clock.
                log::warn!("{}: result for an unknown request: {}", self.name, parsed.path);
                continue;
            }
            self.last_match = Some(Instant::now());

            return Ok(Some(Answer {
                name: key,
                result: PartResult {
                    tags: parsed.tags,
                    meta: parsed.meta,
                    error: parsed.error,
                },
            }));
        }
    }

    /// Reap one request the plugin has demonstrably moved past.
    fn take_stale(&mut self) -> Option<Answer> {
        let stale = self.pending.iter().find_map(|(name, p)| {
            (self.answered.saturating_sub(p.issued_at) > STALE_AFTER_RESULTS)
                .then(|| name.clone())
        })?;
        self.pending.remove(&stale);
        Some(Answer {
            name: stale,
            result: PartResult {
                error: Some(format!(
                    "{} answered {STALE_AFTER_RESULTS} later requests without this one",
                    self.name
                )),
                ..Default::default()
            },
        })
    }

    /// Stop the plugin. Always called, including on the error paths — a
    /// subprocess outliving its run is how a wedged tagger keeps a GPU busy
    /// after the UI says the job failed.
    pub async fn shutdown(mut self) {
        let _ = self.child.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::Manifest;

    /// A plugin written in `sh`, so the test exercises the real subprocess,
    /// the real pipes and the real NDJSON rather than a mock of them.
    fn script_plugin(dir: &std::path::Path, name: &str, script: &str) -> Installed {
        let plugin_dir = dir.join(name);
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let script_path = plugin_dir.join("run.sh");
        std::fs::write(&script_path, script).unwrap();
        let manifest: Manifest = serde_json::from_str(&format!(
            r#"{{
              "name": "{name}",
              "display_name": "{name}",
              "version": "1.0.0",
              "api_version": 1,
              "execution": {{
                "type": "cli",
                "command": "sh",
                "args": ["{{plugin_dir}}/run.sh"]
              }},
              "tag_prefix": "{name}"
            }}"#
        ))
        .unwrap();
        Installed { manifest, directory: plugin_dir }
    }

    fn part(dir: &std::path::Path, name: &str) -> Part {
        let path = dir.join(name);
        std::fs::write(&path, b"x").unwrap();
        Part { name: name.to_string(), path }
    }

    #[tokio::test]
    async fn a_streaming_plugin_answers_every_request() {
        let d = tempfile::tempdir().unwrap();
        // Echoes one result per request, as it arrives.
        let plugin = script_plugin(
            d.path(),
            "echo",
            r#"while IFS= read -r line; do
                 p=$(printf '%s' "$line" | sed 's/.*"path":"\([^"]*\)".*/\1/')
                 printf '{"path":"%s","tags":["seen"]}\n' "$p"
               done"#,
        );

        let mut session = Session::start(&plugin, 3).unwrap();
        let parts: Vec<Part> = (0..3).map(|i| part(d.path(), &format!("p{i}.webp"))).collect();
        for p in &parts {
            session.send(p).await.unwrap();
        }
        session.finish_sending().await.unwrap();

        let mut answered = Vec::new();
        while let Some(a) = session.next_answer().await.unwrap() {
            assert_eq!(a.result.tags, vec!["seen"]);
            answered.push(a.name);
        }
        answered.sort();
        assert_eq!(answered, vec!["p0.webp", "p1.webp", "p2.webp"]);
        session.shutdown().await;
    }

    #[tokio::test]
    async fn a_result_is_matched_on_the_file_name_not_the_path() {
        // A plugin that rewrites the directory — what a canonicalized symlinked
        // TMPDIR does — must not lose every result.
        let d = tempfile::tempdir().unwrap();
        let plugin = script_plugin(
            d.path(),
            "rewriter",
            r#"while IFS= read -r line; do
                 p=$(printf '%s' "$line" | sed 's/.*"path":"\([^"]*\)".*/\1/')
                 printf '{"path":"/somewhere/else/%s","tags":["ok"]}\n' "$(basename "$p")"
               done"#,
        );
        let mut session = Session::start(&plugin, 1).unwrap();
        let p = part(d.path(), "only.webp");
        session.send(&p).await.unwrap();
        session.finish_sending().await.unwrap();

        let a = session.next_answer().await.unwrap().expect("an answer");
        assert_eq!(a.name, "only.webp");
        assert_eq!(a.result.tags, vec!["ok"]);
        session.shutdown().await;
    }

    #[tokio::test]
    async fn a_plugin_that_exits_early_does_not_hang_the_run() {
        // Two outcomes are correct here and which one happens is a race with
        // the subprocess: the write lands in the pipe buffer and the request is
        // answered with "exited before answering", or the pipe is already
        // closed and the write itself fails with EPIPE. What must never happen
        // is a hang — so the assertion is that the run *ends*, either way.
        let d = tempfile::tempdir().unwrap();
        let plugin = script_plugin(d.path(), "quitter", "exit 0");
        let mut session = Session::start(&plugin, 2).unwrap();

        let ended = match session.send(&part(d.path(), "a.webp")).await {
            Err(_) => true,
            Ok(()) => {
                session.finish_sending().await.ok();
                let answer = session.next_answer().await.unwrap().expect("an answer");
                assert!(answer.result.error.is_some());
                session.next_answer().await.unwrap().is_none()
            }
        };
        assert!(ended, "the run must end rather than wait on a dead plugin");
        session.shutdown().await;
    }

    #[tokio::test]
    async fn an_error_result_is_an_answer_rather_than_a_failure() {
        let d = tempfile::tempdir().unwrap();
        let plugin = script_plugin(
            d.path(),
            "refuser",
            r#"while IFS= read -r line; do
                 p=$(printf '%s' "$line" | sed 's/.*"path":"\([^"]*\)".*/\1/')
                 printf '{"path":"%s","error":"cannot read"}\n' "$p"
               done"#,
        );
        let mut session = Session::start(&plugin, 1).unwrap();
        session.send(&part(d.path(), "a.webp")).await.unwrap();
        session.finish_sending().await.unwrap();

        let a = session.next_answer().await.unwrap().expect("an answer");
        assert_eq!(a.result.error.as_deref(), Some("cannot read"));
        session.shutdown().await;
    }

    #[tokio::test]
    async fn a_run_larger_than_the_window_does_not_deadlock() {
        // The failure the streaming contract exists to prevent, as a test: a
        // job several windows deep, against a plugin that answers as it goes.
        let d = tempfile::tempdir().unwrap();
        let plugin = script_plugin(
            d.path(),
            "streamer",
            r#"while IFS= read -r line; do
                 p=$(printf '%s' "$line" | sed 's/.*"path":"\([^"]*\)".*/\1/')
                 printf '{"path":"%s","tags":["t"]}\n' "$p"
               done"#,
        );
        let total = MAX_PENDING * 3;
        let mut session = Session::start(&plugin, total).unwrap();
        let parts: Vec<Part> = (0..total).map(|i| part(d.path(), &format!("p{i}.webp"))).collect();

        let mut next = 0;
        let mut answered = 0;
        while answered < total {
            while session.has_room() && next < total {
                session.send(&parts[next]).await.unwrap();
                next += 1;
            }
            if next == total {
                session.finish_sending().await.ok();
            }
            match session.next_answer().await.unwrap() {
                Some(_) => answered += 1,
                None => break,
            }
        }
        assert_eq!(answered, total);
        session.shutdown().await;
    }
}
