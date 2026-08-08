//! The `manifest.json` a plugin directory declares itself with.
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
    pub version: String,
    pub description: String,
    pub author: Option<String>,
    pub license: Option<String>,
    pub execution: ExecutionConfig,
    pub capabilities: Vec<Capability>,
    pub tag_prefix: String,
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
