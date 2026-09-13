//! The companion file's on-disk shape.
//!
//! Every field here is a wire format: it is written to the user's gallery and
//! read back by other LightView installations, so changing a name or a type is
//! a breaking change that needs a [`crate::companion::migration`] entry and a
//! `CURRENT_SCHEMA_VERSION` bump. It is also the only durable data in the
//! system — everything else is reconstructable from the photos and these files
//! — which makes it the largest commitment in the design.
//!
//! Two attributes carry most of the compatibility weight, and neither is
//! decoration:
//!
//! **`#[serde(default)]` on every field** is what makes an old sidecar without
//! `set`, and a new one without `auto`, parse rather than fail. Not the schema
//! version — the version says what to migrate, and a file that will not
//! deserialize never reaches the migration.
//!
//! **`#[serde(flatten)] extra`** on the two collections is what stops the next
//! write from erasing what the struct no longer models. Removing the `auto`
//! field means the first rating change would otherwise silently delete a user's
//! `auto` tags from the one file that cannot be regenerated. Dropping `auto`
//! from the *index* is a decision; dropping it from the *file* is data loss,
//! and this is the line between them.
//!
//! `Location` is decimal degrees, WGS-84, with altitude in metres above sea
//! level when present.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Current schema version. Increment on breaking changes.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// The extension appended to media files for companion data.
pub const COMPANION_EXTENSION: &str = ".lightview.json";

// ---------------------------------------------------------------------------
// Top-level companion file
// ---------------------------------------------------------------------------

/// The full contents of a `.lightview.json` sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanionFile {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub file_hash: String,
    #[serde(default = "default_media_type")]
    pub media_type: MediaType,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub modified: String,
    #[serde(default)]
    pub tags: TagCollection,
    #[serde(default)]
    pub meta: MetaCollection,
}

fn default_media_type() -> MediaType {
    MediaType::Image
}

impl CompanionFile {
    /// A fresh companion for a media file.
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

    /// Every tag this file contributes to the index, as `(namespace, tag)`.
    ///
    /// `tags.auto` is deliberately **not** enumerated. Old sidecars may carry
    /// it and it round-trips untouched through `TagCollection::extra`, but the
    /// namespace no longer exists in the query language and folding it into
    /// `user::` would silently promote machine output to user intent — the one
    /// boundary this format exists to keep.
    pub fn all_tags(&self) -> Vec<(String, String)> {
        let mut result = Vec::new();
        for tag in &self.tags.user {
            result.push(("user".to_string(), tag.clone()));
        }
        for tag in &self.tags.set {
            result.push(("set".to_string(), tag.clone()));
        }
        for (plugin_name, entry) in &self.tags.plugins {
            for tag in &entry.tags {
                result.push((format!("plugin.{}", plugin_name), tag.clone()));
            }
        }
        result
    }

    /// Tags in one namespace only.
    pub fn tags_in_namespace(&self, namespace: &str) -> Vec<String> {
        match namespace {
            "user" => self.tags.user.clone(),
            "set" => self.tags.set.clone(),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Image,
    Video,
    Gif,
}

impl MediaType {
    /// Infer media type from a file extension.
    ///
    /// This is also the upload allowlist. It admits no `.json`, `.svg` or
    /// `.html`, so a paired device cannot land a companion file, anything
    /// inside `.lightview/`, or a script-bearing type served from the gallery's
    /// own origin. Adding an extension here widens that, so weigh it as a
    /// security change rather than a format one.
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

/// The three tag homes, and one bucket for anything a future build adds.
///
/// `user` is never written by anything but the user. `set` is a sibling of it,
/// not a plugin bucket, and that placement is load-bearing: a plugin bucket is
/// versioned and replaced wholesale on a re-run, which is right for geocoded
/// place names and exactly wrong for a set, which is user-owned and must
/// survive re-tagging. A plugin writes only under its own key, so re-running a
/// tagger replaces that tagger's output and touches nothing else.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TagCollection {
    #[serde(default)]
    pub user: Vec<String>,
    #[serde(default)]
    pub set: Vec<String>,
    #[serde(default)]
    pub plugins: HashMap<String, PluginTagEntry>,
    /// Keys this build does not model — `auto` from an older one, or anything a
    /// newer one adds — round-tripped untouched.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginTagEntry {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Additional plugin-specific data (confidence scores, and so on).
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MetaCollection {
    #[serde(default)]
    pub core: Option<CoreMeta>,
    #[serde(default)]
    pub plugins: HashMap<String, serde_json::Value>,
    /// As on [`TagCollection`]: unmodelled keys survive a write.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// The fields the application itself owns.
///
/// `date_added` and `last_viewed` are here because otherwise requirement 11 —
/// "delete everything else and reopen, and nothing is lost but time" — is
/// false. Both are sort fields and neither used to exist outside the derived
/// database, so three operations the design blesses destroyed them permanently
/// and silently: a `format_version` bump, `lightview cache --prune`, and the
/// scan-prune. After any of them "Date added" collapsed to a single instant for
/// the whole library and "Last viewed" was empty.
///
/// RFC 3339 strings rather than epoch integers, to match `date_rated` and
/// because this file is meant to be readable with `grep`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CoreMeta {
    #[serde(default)]
    pub rating: Option<u8>,
    #[serde(default)]
    pub date_rated: Option<String>,
    #[serde(default)]
    pub color_label: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub media: Option<MediaInfo>,
    #[serde(default)]
    pub location: Option<Location>,
    /// When this file entered the gallery. Mirrored from the database at index
    /// time **only when absent here** — the other direction would have the
    /// first machine to open a gallery with a cold cache stamp its own `now`
    /// onto every file, which is precisely the loss the mirroring prevents.
    #[serde(default)]
    pub date_added: Option<String>,
    #[serde(default)]
    pub last_viewed: Option<String>,
}

/// GPS coordinates extracted from EXIF (or a video container). Decimal degrees,
/// WGS-84; altitude in metres above sea level when present.
///
/// `PartialEq` is derived so `MediaInfo` can keep its own — comparing two
/// coordinates is exact float equality, which is what "did the probe return the
/// same thing?" means here. It is not a proximity test.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Location {
    pub lat: f64,
    pub lon: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alt: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    #[serde(default)]
    pub codec: Option<String>,
    #[serde(default)]
    pub has_audio: Option<bool>,
    #[serde(default)]
    pub fps: Option<f64>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_type_from_extension() {
        assert_eq!(MediaType::from_extension("jpg"), Some(MediaType::Image));
        assert_eq!(MediaType::from_extension("JPG"), Some(MediaType::Image));
        assert_eq!(MediaType::from_extension("mp4"), Some(MediaType::Video));
        assert_eq!(MediaType::from_extension("gif"), Some(MediaType::Gif));
        assert_eq!(MediaType::from_extension("txt"), None);
        // The upload allowlist half: none of these may ever land in a gallery.
        for hostile in ["json", "svg", "html", "js"] {
            assert_eq!(MediaType::from_extension(hostile), None);
        }
    }

    #[test]
    fn companion_roundtrip() {
        let companion = CompanionFile::new("test.jpg", MediaType::Image);
        let json = serde_json::to_string_pretty(&companion).unwrap();
        let parsed: CompanionFile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.file, "test.jpg");
        assert_eq!(parsed.media_type, MediaType::Image);
        assert_eq!(parsed.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn an_old_sidecar_without_set_parses() {
        // The claim "an old file reads correctly in the new build" is true
        // because of `#[serde(default)]` and false without it.
        let old = r#"{
            "schema_version": 1,
            "file": "a.jpg",
            "file_hash": "",
            "media_type": "image",
            "created": "2024-01-01T00:00:00Z",
            "modified": "2024-01-01T00:00:00Z",
            "tags": { "user": ["vacation"], "auto": ["indoor"], "plugins": {} },
            "meta": { "core": { "rating": 4 }, "plugins": {} }
        }"#;
        let parsed: CompanionFile = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.tags.user, vec!["vacation"]);
        assert!(parsed.tags.set.is_empty());
        assert_eq!(parsed.meta.core.unwrap().rating, Some(4));
    }

    #[test]
    fn auto_tags_survive_a_write_but_leave_the_index() {
        let old = r#"{
            "schema_version": 1, "file": "a.jpg", "file_hash": "",
            "media_type": "image", "created": "", "modified": "",
            "tags": { "user": ["v"], "auto": ["indoor", "night"], "plugins": {} },
            "meta": { "core": null, "plugins": {} }
        }"#;
        let mut parsed: CompanionFile = serde_json::from_str(old).unwrap();

        // Not indexed: the namespace no longer exists in the query language.
        let namespaces: Vec<_> = parsed.all_tags().into_iter().map(|(n, _)| n).collect();
        assert!(!namespaces.contains(&"auto".to_string()));

        // Not destroyed: the next rating change must not erase them.
        parsed.meta.core = Some(CoreMeta {
            rating: Some(5),
            ..Default::default()
        });
        let written = serde_json::to_string(&parsed).unwrap();
        let reread: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(
            reread["tags"]["auto"],
            serde_json::json!(["indoor", "night"])
        );
    }

    #[test]
    fn all_tags_covers_user_set_and_plugins() {
        let mut companion = CompanionFile::new("test.jpg", MediaType::Image);
        companion.tags.user = vec!["vacation".into(), "family".into()];
        companion.tags.set = vec!["kellys-comic".into()];
        companion.tags.plugins.insert(
            "face-recognition".into(),
            PluginTagEntry {
                version: "1.0.0".into(),
                tags: vec!["person:alice".into()],
                extra: HashMap::new(),
            },
        );

        let all = companion.all_tags();
        assert_eq!(all.len(), 4);
        assert!(all.contains(&("user".into(), "vacation".into())));
        assert!(all.contains(&("set".into(), "kellys-comic".into())));
        assert!(all.contains(&(
            "plugin.face-recognition".into(),
            "person:alice".into()
        )));
    }
}
