//! Running an external plugin as a subprocess.
//!
//! [`manifest`] is the `manifest.json` a plugin directory must contain;
//! [`runner`] spawns the process and speaks the NDJSON protocol over its
//! stdin/stdout. Process isolation is the point — a plugin that segfaults,
//! wedges, or allocates without bound costs the host one child, and brings its
//! own interpreter and native dependencies with it.
//!
//! Plugins **must stream**: consume requests as they arrive and emit each
//! result as soon as it is ready, never read stdin to EOF first. A remote host
//! keeps a bounded number of downloaded files on disk and fetches more only as
//! results come back, so an EOF-buffering plugin deadlocks any job past that
//! bound. `LIGHTVIEW_JOB_TOTAL` exists for plugins that legitimately need the
//! count up front. That rule is no longer convention only: it is what
//! [`PLUGIN_API_VERSION`] 1 means, and [`check_api_version`] refuses a plugin
//! that does not claim it on the host where it would hang.
//!
//! Scope, stated plainly: there is one verb. `action` is plumbed but never
//! varies from `"tag"`, and capabilities in the manifest are advisory — there
//! is no sandbox, so a plugin runs with the host's privileges. That is why
//! plugin execution is absent from the remote `/api/invoke` allowlist. See
//! `docs/plugins/README.md` and
//! `docs/decisions/0006-plugins-are-ndjson-subprocesses.md`.

pub mod input;
pub mod install;
pub mod manifest;
pub mod runner;

use std::path::Path;

use serde::{Deserialize, Serialize};

use manifest::PluginManifest;

/// The plugin protocol version this host speaks.
///
/// A plugin declares the version it was written against in its manifest, and
/// the host checks the two before running anything. What each value means:
///
/// - **0** — no declaration. The manifest predates the versioned protocol, and
///   therefore predates the requirement that a plugin stream its results. Such
///   a plugin may read stdin to EOF before doing any work, which is fine on a
///   host that writes every request up front and correct-by-deadlock on one
///   that releases requests as results come back. See [`check_api_version`].
/// - **1** — current. The plugin consumes requests as they arrive, emits
///   exactly one result per request (an `error` result counts), and reads
///   `LIGHTVIEW_JOB_TOTAL` instead of sizing itself off the request list.
///   Videos never reach it: the host samples frames and merges the results.
///
/// Bump this only when a change would break a plugin that follows the current
/// rules, and say what changed in [`docs/plugins/README.md`]. The version
/// exists because plugins are *copied* into an install directory that nothing
/// migrates — across a repository boundary, drift is the normal case rather
/// than a mistake.
pub const PLUGIN_API_VERSION: u32 = 1;

/// How a host feeds requests to a plugin. The declared `api_version` means
/// different things to the two, which is why the check takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestDelivery {
    /// Every request is written up front and stdin closed immediately — the
    /// desktop batch command and the in-process executor. A plugin that reads
    /// to EOF before starting work still completes here, which is exactly why
    /// a broken plugin can look healthy on the desktop for years.
    UpFront,
    /// Requests are released only as results come back (`lightview-worker`),
    /// so a plugin that waits for EOF never starts and the job hangs.
    Incremental,
}

/// Can this host run this plugin? `Err` carries a message written for whoever
/// has to fix it.
///
/// The pre-streaming case is the one that pays for the whole mechanism: those
/// plugins install cleanly, work on the desktop, tag the first 64 images of a
/// remote job and then hang with no error and no VRAM ever allocated. Refusing
/// them at load turns a day of debugging into one line at startup.
pub fn check_api_version(m: &PluginManifest, delivery: RequestDelivery) -> Result<(), String> {
    if m.api_version > PLUGIN_API_VERSION {
        return Err(format!(
            "plugin '{}' declares api_version {} but this host speaks {} — update LightView",
            m.name, m.api_version, PLUGIN_API_VERSION
        ));
    }
    if m.api_version == 0 && delivery == RequestDelivery::Incremental {
        return Err(format!(
            "plugin '{}' declares no api_version, so it predates the streaming protocol and \
             would deadlock this host once 64 images are in flight. Update the plugin to \
             stream its results and add \"api_version\": {} to its manifest.json \
             (see docs/plugins/README.md).",
            m.name, PLUGIN_API_VERSION
        ));
    }
    Ok(())
}

/// Summary of an installed plugin, as listed to the frontend and announced by
/// remote tagging workers over `worker_announce`. Field names are the wire
/// format (snake_case) that `list_plugins` has always returned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    pub display_name: String,
    pub version: String,
    /// Protocol version the plugin declares. Travels with the announce so a
    /// server can see what its workers are actually running — the question
    /// that was unanswerable when a worker binary could be rebuilt while the
    /// plugin beside it stayed whatever it was the day it was copied.
    #[serde(default)]
    pub api_version: u32,
    pub description: String,
    pub tag_prefix: String,
}

impl From<&PluginManifest> for PluginInfo {
    fn from(m: &PluginManifest) -> Self {
        PluginInfo {
            name: m.name.clone(),
            display_name: m.display_name.clone(),
            version: m.version.clone(),
            api_version: m.api_version,
            description: m.description.clone(),
            tag_prefix: m.tag_prefix.clone(),
        }
    }
}

/// The host's default plugins directory (`<exe_dir>/data/plugins`).
pub fn default_dir() -> std::path::PathBuf {
    crate::util::paths::data_dir().join("plugins")
}

/// Scan a plugins directory (`<dir>/*/manifest.json`) for installed plugins.
/// Shared by the desktop `list_plugins` command and the `lightview-worker`
/// binary, which runs the same plugin layout on a different machine.
///
/// Lists everything that parses, compatible or not — the caller knows how it
/// delivers requests, and a plugin the *worker* must refuse is still one the
/// desktop can run.
pub fn scan_plugins(dir: &Path) -> Vec<PluginInfo> {
    let mut plugins = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return plugins;
    };
    for entry in entries.flatten() {
        let manifest_path = entry.path().join("manifest.json");
        if manifest_path.exists() {
            if let Ok(manifest) = PluginManifest::load(&manifest_path) {
                plugins.push(PluginInfo::from(&manifest));
            }
        }
    }
    plugins
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifest::{Capability, ExecutionConfig, PluginInputConfig};

    fn manifest(api_version: u32) -> PluginManifest {
        PluginManifest {
            name: "t".into(),
            display_name: "T".into(),
            version: "1.0.0".into(),
            api_version,
            description: String::new(),
            author: None,
            license: None,
            execution: ExecutionConfig::Cli {
                command: "python3".into(),
                args: vec![],
            },
            capabilities: vec![Capability::ReadImage],
            tag_prefix: "t".into(),
            input: PluginInputConfig::default(),
            ui: None,
        }
    }

    /// The asymmetry is the whole point: an undeclared plugin is one the
    /// desktop has always run fine and the worker has always hung on.
    #[test]
    fn an_undeclared_plugin_is_refused_only_where_it_would_hang() {
        let m = manifest(0);
        assert!(check_api_version(&m, RequestDelivery::UpFront).is_ok());
        let err = check_api_version(&m, RequestDelivery::Incremental).unwrap_err();
        assert!(err.contains("api_version"), "{err}");
    }

    #[test]
    fn a_current_plugin_runs_everywhere() {
        let m = manifest(PLUGIN_API_VERSION);
        assert!(check_api_version(&m, RequestDelivery::UpFront).is_ok());
        assert!(check_api_version(&m, RequestDelivery::Incremental).is_ok());
    }

    /// A plugin from a newer host may speak a protocol we cannot answer, so
    /// neither delivery mode is safe — the fix is upgrading LightView.
    #[test]
    fn a_plugin_from_the_future_is_refused_everywhere() {
        let m = manifest(PLUGIN_API_VERSION + 1);
        for delivery in [RequestDelivery::UpFront, RequestDelivery::Incremental] {
            let err = check_api_version(&m, delivery).unwrap_err();
            assert!(err.contains("update LightView"), "{err}");
        }
    }
}
