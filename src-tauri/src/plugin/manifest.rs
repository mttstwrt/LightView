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
        timeout_seconds: Option<u64>,
    },
    #[serde(rename = "wasm")]
    Wasm {
        module_path: String,
        memory_limit_mb: Option<u32>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    ReadCompanion,
    WriteCompanion,
    ReadImage,
    NetworkAccess,
    BatchProcess,
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
    /// Load a manifest from a JSON file path.
    pub fn load(path: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        let manifest: Self = serde_json::from_str(&contents)?;
        Ok(manifest)
    }
}
