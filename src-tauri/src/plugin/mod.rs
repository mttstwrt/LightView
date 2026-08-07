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
//! count up front.
//!
//! Scope, stated plainly: there is one verb. `action` is plumbed but never
//! varies from `"tag"`, and capabilities in the manifest are advisory — there
//! is no sandbox, so a plugin runs with the host's privileges. That is why
//! plugin execution is absent from the remote `/api/invoke` allowlist. See
//! `docs/plugins/README.md` and
//! `docs/decisions/0006-plugins-are-ndjson-subprocesses.md`.

pub mod manifest;
pub mod runner;

use std::path::Path;

use serde::{Deserialize, Serialize};

use manifest::PluginManifest;

/// Summary of an installed plugin, as listed to the frontend and announced by
/// remote tagging workers over `worker_announce`. Field names are the wire
/// format (snake_case) that `list_plugins` has always returned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub description: String,
    pub tag_prefix: String,
}

impl From<&PluginManifest> for PluginInfo {
    fn from(m: &PluginManifest) -> Self {
        PluginInfo {
            name: m.name.clone(),
            display_name: m.display_name.clone(),
            version: m.version.clone(),
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
