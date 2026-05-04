use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Current schema version. Increment on breaking changes.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// The extension appended to media files for companion data.
pub const COMPANION_EXTENSION: &str = ".lightview.json";

// ---------------------------------------------------------------------------
// Top-level companion file
// ---------------------------------------------------------------------------

/// Represents the full contents of a `.lightview.json` sidecar file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanionFile {
    pub schema_version: u32,
    pub file: String,
    pub file_hash: String,
    pub media_type: MediaType,
    pub created: String,
    pub modified: String,
    pub tags: TagCollection,
    pub meta: MetaCollection,
}

impl CompanionFile {
    /// Create a new companion file with defaults for the given media file.
    pub fn new(filename: &str, media_type: MediaType) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            file: filename.to_string(),
            file_hash: String::new(),
            media_type,
            created: now.clone(),
            modified: now,
            tags: TagCollection::default(),
            meta: MetaCollection::default(),
        }
    }

    /// Get all tags across all namespaces as (namespace, tag) pairs.
    pub fn all_tags(&self) -> Vec<(String, String)> {
        let mut result = Vec::new();
        for tag in &self.tags.user {
            result.push(("user".to_string(), tag.clone()));
        }
        for tag in &self.tags.auto {
            result.push(("auto".to_string(), tag.clone()));
        }
        for (plugin_name, entry) in &self.tags.plugins {
            for tag in &entry.tags {
                result.push((format!("plugin.{}", plugin_name), tag.clone()));
            }
        }
        result
    }

    /// Get tags from a specific namespace only.
    pub fn tags_in_namespace(&self, namespace: &str) -> Vec<String> {
        match namespace {
            "user" => self.tags.user.clone(),
            "auto" => self.tags.auto.clone(),
            ns if ns.starts_with("plugin.") => {
                let plugin_name = &ns["plugin.".len()..];
                self.tags
                    .plugins
                    .get(plugin_name)
                    .map(|e| e.tags.clone())
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Media type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Image,
    Video,
    Gif,
}

impl MediaType {
    /// Infer media type from file extension.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_lowercase().as_str() {
            "jpg" | "jpeg" | "png" | "webp" | "bmp" | "tiff" | "tif" | "heic" | "heif"
            | "avif" | "raw" | "cr2" | "nef" | "arw" | "dng" => Some(MediaType::Image),
            "gif" => Some(MediaType::Gif),
            "mp4" | "mov" | "avi" | "mkv" | "webm" | "m4v" | "wmv" | "flv" => {
                Some(MediaType::Video)
            }
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            MediaType::Image => "image",
            MediaType::Video => "video",
            MediaType::Gif => "gif",
        }
    }
}

impl std::fmt::Display for MediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TagCollection {
    pub user: Vec<String>,
    pub auto: Vec<String>,
    pub plugins: HashMap<String, PluginTagEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginTagEntry {
    pub version: String,
    pub tags: Vec<String>,
    /// Additional plugin-specific data (confidence scores, etc.)
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MetaCollection {
    pub core: Option<CoreMeta>,
    pub plugins: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CoreMeta {
    pub rating: Option<u8>,
    pub date_rated: Option<String>,
    pub color_label: Option<String>,
    pub notes: Option<String>,
    pub media: Option<MediaInfo>,
    pub location: Option<Location>,
}

/// GPS coordinates extracted from EXIF (or video container) metadata.
/// Decimal degrees, WGS-84. Altitude in metres above sea level when present.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Location {
    pub lat: f64,
    pub lon: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alt: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    pub duration_seconds: Option<f64>,
    pub codec: Option<String>,
    pub has_audio: Option<bool>,
    pub fps: Option<f64>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_media_type_from_extension() {
        assert_eq!(MediaType::from_extension("jpg"), Some(MediaType::Image));
        assert_eq!(MediaType::from_extension("JPG"), Some(MediaType::Image));
        assert_eq!(MediaType::from_extension("mp4"), Some(MediaType::Video));
        assert_eq!(MediaType::from_extension("gif"), Some(MediaType::Gif));
        assert_eq!(MediaType::from_extension("txt"), None);
    }

    #[test]
    fn test_companion_roundtrip() {
        let companion = CompanionFile::new("test.jpg", MediaType::Image);
        let json = serde_json::to_string_pretty(&companion).unwrap();
        let parsed: CompanionFile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.file, "test.jpg");
        assert_eq!(parsed.media_type, MediaType::Image);
        assert_eq!(parsed.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn test_all_tags() {
        let mut companion = CompanionFile::new("test.jpg", MediaType::Image);
        companion.tags.user = vec!["vacation".to_string(), "family".to_string()];
        companion.tags.auto = vec!["outdoor".to_string()];
        companion.tags.plugins.insert(
            "face-recognition".to_string(),
            PluginTagEntry {
                version: "1.0.0".to_string(),
                tags: vec!["person:alice".to_string()],
                extra: HashMap::new(),
            },
        );

        let all = companion.all_tags();
        assert_eq!(all.len(), 4);
        assert!(all.contains(&("user".to_string(), "vacation".to_string())));
        assert!(all.contains(&(
            "plugin.face-recognition".to_string(),
            "person:alice".to_string()
        )));
    }
}
