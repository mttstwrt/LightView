//! The `manifest.json` a plugin directory declares itself with.
//!
//! Two versions live here and they answer different questions. `version` is the
//! plugin's own — it is stamped into every companion file the plugin writes, so
//! a re-tag can be told from a first tag. `api_version` is the *host contract*
//! the plugin was written against; see [`crate::plugin::PLUGIN_API_VERSION`]
//! for what each value promises. Declaring the second is what stops a plugin
//! from installing cleanly and then deadlocking, which is the failure that
//! motivated it.
//!
//! `tag_prefix` is mandatory and is the plugin's namespace in every companion
//! file it writes, so renaming it orphans that plugin's previous output rather
//! than replacing it.
//!
//! `ExecutionConfig::Wasm` parses but is rejected at run time. It is a
//! placeholder for the sandboxing story, not a half-built feature — and the
//! `capabilities` list is advisory for the same reason: nothing enforces it
//! yet.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub display_name: String,
    /// The plugin's own version, written into `tags.plugins[prefix].version`.
    pub version: String,
    /// Host protocol version this plugin was built against. Absent means `0`:
    /// written before the protocol was versioned, and therefore before
    /// streaming was required. See [`crate::plugin::check_api_version`].
    #[serde(default)]
    pub api_version: u32,
    pub description: String,
    pub author: Option<String>,
    pub license: Option<String>,
    pub execution: ExecutionConfig,
    pub capabilities: Vec<Capability>,
    pub tag_prefix: String,
    /// What the plugin wants its input to look like. Absent gets the defaults,
    /// which reproduce the behaviour from before the field existed.
    #[serde(default)]
    pub input: PluginInputConfig,
    pub ui: Option<PluginUiConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ExecutionConfig {
    #[serde(rename = "cli")]
    Cli {
        command: String,
        args: Vec<String>,
    },
    #[serde(rename = "wasm")]
    Wasm {
        module_path: String,
        memory_limit_mb: Option<u32>,
    },
}

/// What the plugin wants handed to it, rather than what happens to be on disk.
///
/// Both fields exist because the host, not the plugin, is the only place that
/// can act on them cheaply: it already has decoders, a bounded thumbnail pool
/// and (remotely) control over how many bytes cross the network. A 448-pixel
/// model that receives a 60-megapixel original pays for a full decode in Python
/// and then throws almost all of it away.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInputConfig {
    /// Longest edge, in pixels, to scale stills to before the plugin sees them.
    /// `0` means "hand over the original file untouched".
    ///
    /// Never upscales: a source already smaller than this is passed through, so
    /// a plugin cannot use this to demand more detail than the file has.
    #[serde(default)]
    pub max_edge: u32,
    /// How many frames to sample from a video, evenly spaced across the middle
    /// 90% of its duration. The plugin receives them as ordinary still requests
    /// and never learns it was a video; the host merges the results.
    #[serde(default = "default_video_frames")]
    pub video_frames: u32,
}

/// Mirrors the `VIDEO_FRAME_SAMPLES = 5` the bundled taggers used to sample
/// with themselves, so moving extraction into the host changed no output.
fn default_video_frames() -> u32 {
    5
}

impl Default for PluginInputConfig {
    fn default() -> Self {
        PluginInputConfig {
            max_edge: 0,
            video_frames: default_video_frames(),
        }
    }
}

/// What the plugin needs from the host app.
/// These are declarations of intent — the app enforces them, not the plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Plugin will receive the media file path and can read the image bytes.
    ReadImage,
    /// Plugin may make network requests (informational — not sandboxed yet).
    NetworkAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginUiConfig {
    pub settings_schema: Option<serde_json::Value>,
    pub context_menu_items: Vec<ContextMenuItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextMenuItem {
    pub label: String,
    pub action: String,
    pub icon: Option<String>,
}

impl PluginManifest {
    pub fn load(path: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        let manifest: Self = serde_json::from_str(&contents)?;
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field added since the first release is optional, because manifests
    /// live in install directories nothing migrates — a plugin copied a year
    /// ago must still parse.
    #[test]
    fn a_manifest_without_the_newer_fields_still_parses() {
        let json = r#"{
            "name": "old", "display_name": "Old", "version": "1.0.0",
            "description": "", "author": null, "license": null,
            "execution": { "type": "cli", "command": "python3", "args": [] },
            "capabilities": ["read_image"], "tag_prefix": "old", "ui": null
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        // Absent api_version is 0, which is what marks it pre-streaming.
        assert_eq!(m.api_version, 0);
        assert_eq!(m.input.max_edge, 0);
        assert_eq!(m.input.video_frames, 5);
    }

    #[test]
    fn input_config_is_read_when_present() {
        let json = r#"{
            "name": "n", "display_name": "N", "version": "1.0.0",
            "api_version": 1, "description": "", "author": null, "license": null,
            "execution": { "type": "cli", "command": "python3", "args": [] },
            "capabilities": [], "tag_prefix": "n",
            "input": { "max_edge": 448, "video_frames": 3 }, "ui": null
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.api_version, 1);
        assert_eq!(m.input.max_edge, 448);
        assert_eq!(m.input.video_frames, 3);
    }
}
