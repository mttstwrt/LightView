//! The manifest a plugin ships, and the subset of it the host acts on.
//!
//! Deliberately smaller than the file on disk. Three fields the old schema
//! carried are read and discarded, because they promised things that do not
//! exist: `ExecutionConfig::Wasm` (there is no wasm runtime), `capabilities`
//! (`read_image`, `network_access` — advisory strings with no sandbox behind
//! them), and `ui.context_menu_items` (the menu builds its own list). A stub
//! that errors is worse than an honest absence, and one that silently does
//! nothing is worse still.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The only protocol version this host speaks.
///
/// Version 0 predates the streaming contract, and a version newer than the
/// host is a plugin expecting behaviour that is not here. Both are refused,
/// and refused in **one** place — the scan — so there is no second opinion
/// further down.
pub const API_VERSION: u32 = 1;

/// The default longest edge a plugin is fed, when its manifest omits one.
///
/// 512 is the tier the idle worker warms, so a plugin taking the default does
/// no generation at all over a warmed gallery. A plugin declaring more pays one
/// generation per image — see [`Input::max_edge`].
const DEFAULT_MAX_EDGE: u32 = 512;

/// Frames sampled from a clip, when the manifest omits a count.
const DEFAULT_VIDEO_FRAMES: u32 = 5;

/// Hard ceiling on frames per clip, whatever a manifest asks for.
pub const MAX_VIDEO_FRAMES: u32 = 16;

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub api_version: u32,
    #[serde(default)]
    pub description: String,
    pub execution: Execution,
    /// The namespace this plugin's tags land in: `plugin.<tag_prefix>`.
    pub tag_prefix: String,
    #[serde(default)]
    pub input: Input,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Execution {
    /// Always `"cli"`. Kept as a field so a manifest declaring something else
    /// is refused by name rather than silently treated as a CLI plugin.
    #[serde(rename = "type")]
    pub kind: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Input {
    /// The longest edge the plugin wants its images scaled to.
    ///
    /// The host serves the smallest cached tier at least this big and **rounds
    /// up, never down**: a model handed a smaller image than it trained on has
    /// lost information it cannot recover.
    #[serde(default = "default_max_edge")]
    pub max_edge: u32,
    /// How many stills to sample from a clip. A plugin never sees a video.
    #[serde(default = "default_video_frames")]
    pub video_frames: u32,
}

fn default_max_edge() -> u32 {
    DEFAULT_MAX_EDGE
}

fn default_video_frames() -> u32 {
    DEFAULT_VIDEO_FRAMES
}

impl Default for Input {
    fn default() -> Self {
        Self {
            max_edge: DEFAULT_MAX_EDGE,
            video_frames: DEFAULT_VIDEO_FRAMES,
        }
    }
}

/// An installed plugin: its manifest, and where it lives.
#[derive(Debug, Clone)]
pub struct Installed {
    pub manifest: Manifest,
    pub directory: PathBuf,
}

impl Installed {
    /// Frames to sample from a clip, clamped to the host's ceiling.
    pub fn video_frames(&self) -> u32 {
        self.manifest.input.video_frames.clamp(1, MAX_VIDEO_FRAMES)
    }
}

/// What a client is told about a plugin. No command, no path, no arguments —
/// nothing a request could echo back to select something to run.
#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub api_version: u32,
    pub description: String,
    pub tag_prefix: String,
}

impl From<&Installed> for PluginInfo {
    fn from(installed: &Installed) -> Self {
        let m = &installed.manifest;
        Self {
            name: m.name.clone(),
            display_name: m.display_name.clone(),
            version: m.version.clone(),
            api_version: m.api_version,
            description: m.description.clone(),
            tag_prefix: m.tag_prefix.clone(),
        }
    }
}

/// Every usable plugin under `root`, in name order.
///
/// A directory with no manifest, an unparseable manifest, or a manifest this
/// host cannot speak to is skipped with a log line rather than failing the
/// scan: one broken plugin must not make the others unreachable.
pub fn installed(root: &Path) -> Vec<Installed> {
    let Ok(entries) = std::fs::read_dir(root) else {
        // No plugins directory is the ordinary case, not an error.
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let directory = entry.path();
        let path = directory.join("manifest.json");
        let Ok(body) = std::fs::read(&path) else {
            continue;
        };
        let manifest: Manifest = match serde_json::from_slice(&body) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("ignoring {}: {e}", path.display());
                continue;
            }
        };
        if manifest.api_version != API_VERSION {
            log::warn!(
                "ignoring {}: api_version {} (this host speaks {API_VERSION})",
                path.display(),
                manifest.api_version
            );
            continue;
        }
        if manifest.execution.kind != "cli" {
            log::warn!(
                "ignoring {}: execution type {:?} is not supported",
                path.display(),
                manifest.execution.kind
            );
            continue;
        }
        // The directory name is the identity, not the manifest's `name` field:
        // a manifest can claim any name, and a lookup that trusted it would let
        // one plugin answer to another's name.
        let directory_name = entry.file_name().to_string_lossy().to_string();
        if manifest.name != directory_name {
            log::warn!(
                "ignoring {}: manifest name {:?} does not match its directory {:?}",
                path.display(),
                manifest.name,
                directory_name
            );
            continue;
        }
        out.push(Installed { manifest, directory });
    }
    out.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    out
}

/// Find one installed plugin by name.
///
/// **There is no path join here, and that is the point.** The candidates are
/// exactly what [`installed`] found by scanning, so an absolute path, a `..`
/// segment or any other spelling simply matches nothing — the confinement is
/// structural rather than a check that could be removed.
pub fn find(root: &Path, name: &str) -> Option<Installed> {
    installed(root)
        .into_iter()
        .find(|p| p.manifest.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plant(root: &Path, dir: &str, body: &str) {
        let d = root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("manifest.json"), body).unwrap();
    }

    fn valid(name: &str) -> String {
        format!(
            r#"{{
              "name": "{name}",
              "display_name": "A Tagger",
              "version": "1.0.0",
              "api_version": 1,
              "description": "d",
              "execution": {{ "type": "cli", "command": "python3", "args": [] }},
              "tag_prefix": "{name}"
            }}"#
        )
    }

    #[test]
    fn a_missing_plugins_directory_is_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(installed(&d.path().join("nope")).is_empty());
    }

    #[test]
    fn an_omitted_input_block_takes_the_warmed_tier() {
        // The payoff of tier quantization is conditional on the declared edge,
        // so the default must be the tier the idle worker actually warms.
        let d = tempfile::tempdir().unwrap();
        plant(d.path(), "alpha", &valid("alpha"));
        let found = installed(d.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].manifest.input.max_edge, 512);
        assert_eq!(found[0].video_frames(), 5);
    }

    #[test]
    fn video_frames_are_clamped_to_the_host_ceiling() {
        let d = tempfile::tempdir().unwrap();
        plant(
            d.path(),
            "greedy",
            &valid("greedy").replace(
                r#""tag_prefix": "greedy""#,
                r#""tag_prefix": "greedy", "input": { "video_frames": 500 }"#,
            ),
        );
        assert_eq!(installed(d.path())[0].video_frames(), MAX_VIDEO_FRAMES);
    }

    #[test]
    fn one_broken_manifest_does_not_hide_the_others() {
        let d = tempfile::tempdir().unwrap();
        plant(d.path(), "good", &valid("good"));
        plant(d.path(), "broken", "{ not json");
        plant(d.path(), "old", &valid("old").replace("\"api_version\": 1", "\"api_version\": 0"));
        plant(d.path(), "future", &valid("future").replace("\"api_version\": 1", "\"api_version\": 2"));
        plant(d.path(), "wasm", &valid("wasm").replace("\"type\": \"cli\"", "\"type\": \"wasm\""));

        let names: Vec<String> = installed(d.path())
            .iter()
            .map(|p| p.manifest.name.clone())
            .collect();
        assert_eq!(names, vec!["good"]);
    }

    #[test]
    fn a_manifest_cannot_answer_to_another_plugins_name() {
        // Whoever writes the manifest writes its name field, so the directory
        // is the identity and the field is checked against it.
        let d = tempfile::tempdir().unwrap();
        plant(d.path(), "innocent", &valid("wd-tagger"));
        assert!(installed(d.path()).is_empty());
    }

    #[test]
    fn a_name_that_is_a_path_selects_nothing() {
        // The whole confinement argument, as a test: there is no join, so
        // these are simply names that were not found.
        let d = tempfile::tempdir().unwrap();
        plant(d.path(), "alpha", &valid("alpha"));
        std::fs::create_dir_all(d.path().join("../evil")).ok();

        assert!(find(d.path(), "alpha").is_some());
        assert!(find(d.path(), "/tmp/evil").is_none());
        assert!(find(d.path(), "../evil").is_none());
        assert!(find(d.path(), "").is_none());
    }
}
