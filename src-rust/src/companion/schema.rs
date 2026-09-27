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
//! **A set entry is `name` or `name::N`.** `N` is this file's position in the
//! set: where the grid shows it when the filter is exactly that set. It is a
//! suffix on the string rather than a field of its own because membership and
//! position are one fact — a rename, a removal or a merge rewrites the one
//! string, and cannot leave a position behind for a set the file has left. Only
//! this file sees the suffix: the index splits it off ([`split_set_entry`]), so
//! everything else still knows a set by its name.
//!
//! That changes what an existing field means, and `CURRENT_SCHEMA_VERSION` is
//! deliberately **not** bumped for it. A build refuses a sidecar newer than
//! itself, so a bump would make an older build refuse every sidecar this one
//! rewrites. Without it an older build degrades instead of failing: it takes
//! `comic::3` for a set of its own and its remove and rename miss such an
//! entry, but it reads the file and round-trips the string untouched.
//!
//! `Location` is decimal degrees, WGS-84, with altitude in metres above sea
//! level when present.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

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

/// The media type an old companion without the field is read as.
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

    /// Every tag this file contributes to the index, as `(namespace, tag,
    /// position)`, in list order.
    ///
    /// A set entry contributes its bare name, with its position beside it; every
    /// other tag has no position. The order matters: the index keeps the first
    /// row per name, and [`TagCollection::one_entry_per_set`] keeps the first
    /// entry — the same one.
    ///
    /// `tags.auto` is deliberately **not** enumerated. Old sidecars may carry
    /// it and it round-trips untouched through `TagCollection::extra`, but the
    /// namespace no longer exists in the query language and folding it into
    /// `user::` would silently promote machine output to user intent — the one
    /// boundary this format exists to keep.
    pub fn all_tags(&self) -> Vec<(String, String, Option<u32>)> {
        let mut result = Vec::new();
        for tag in &self.tags.user {
            result.push(("user".to_string(), tag.clone(), None));
        }
        for entry in &self.tags.set {
            let (name, position) = split_set_entry(entry);
            result.push(("set".to_string(), name.to_string(), position));
        }
        for (plugin_name, entry) in &self.tags.plugins {
            for tag in &entry.tags {
                result.push((format!("plugin.{}", plugin_name), tag.clone(), None));
            }
        }
        result
    }

}

/// Split a `tags.set` entry into the set's name and this file's position in it.
///
/// **The position is the digits after the last `::`, and nothing else is.**
/// `a::b::3` is set `a::b` at position 3; `a::b` and `comic::x` are names with
/// no position. Reading from the right is what lets a name contain `::` at all,
/// and it is also why a name may not *end* in `::<digits>`: it would read back
/// as a shorter name plus a position, so the tag service refuses such a name
/// on the way in.
pub fn split_set_entry(entry: &str) -> (&str, Option<u32>) {
    if let Some(at) = entry.rfind("::") {
        let (name, digits) = (&entry[..at], &entry[at + 2..]);
        let all_digits = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
        if !name.is_empty() && all_digits {
            // Out of `u32` range reads as part of the name rather than failing:
            // no writer produces it, so it can only be a name someone chose.
            if let Ok(position) = digits.parse() {
                return (name, Some(position));
            }
        }
    }
    (entry, None)
}

/// The `tags.set` entry that [`split_set_entry`] reads back as `(name,
/// position)`.
pub fn set_entry(name: &str, position: Option<u32>) -> String {
    match position {
        Some(position) => format!("{name}::{position}"),
        None => name.to_string(),
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

    /// The media type as the companion and `media_meta` spell it.
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

impl TagCollection {
    /// Keep the first entry for each set name and drop the rest.
    ///
    /// A file holds one place in a set. This drops exactly what the reader
    /// already ignores — [`CompanionFile::all_tags`] is in list order and the
    /// index keeps the first row per name — so it never changes what a file
    /// means; it only stops a dead second entry being written back.
    pub fn one_entry_per_set(&mut self) {
        let mut seen = HashSet::new();
        self.set
            .retain(|entry| seen.insert(split_set_entry(entry).0.to_string()));
    }
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
/// **Nothing in LightView writes this**, and that is deliberate rather than an
/// oversight. It is part of the sidecar format, so a plugin or another tool may
/// fill it in and must find it intact on the next round trip. A probed duration
/// does not belong here either: the companion is the one thing a cache rebuild
/// cannot regenerate, and a duration is recoverable from the file itself.
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
        let namespaces: Vec<_> = parsed.all_tags().into_iter().map(|(n, _, _)| n).collect();
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
        assert!(all.contains(&("user".into(), "vacation".into(), None)));
        assert!(all.contains(&("set".into(), "kellys-comic".into(), None)));
        assert!(all.contains(&(
            "plugin.face-recognition".into(),
            "person:alice".into(),
            None
        )));
    }

    #[test]
    fn a_set_entry_splits_at_the_last_double_colon_followed_by_digits() {
        assert_eq!(split_set_entry("comic"), ("comic", None));
        assert_eq!(split_set_entry("comic::3"), ("comic", Some(3)));
        // A name may contain `::`; only trailing digits are a position.
        assert_eq!(split_set_entry("a::b::3"), ("a::b", Some(3)));
        assert_eq!(split_set_entry("a::b"), ("a::b", None));
        assert_eq!(split_set_entry("comic::x"), ("comic::x", None));
        assert_eq!(split_set_entry("comic::"), ("comic::", None));
        assert_eq!(split_set_entry("::3"), ("::3", None));
        assert_eq!(split_set_entry("comic::-3"), ("comic::-3", None));
        assert_eq!(split_set_entry("comic::99999999999"), ("comic::99999999999", None));
    }

    #[test]
    fn a_set_entry_round_trips() {
        for (name, position) in [("comic", None), ("comic", Some(1)), ("a::b", Some(12))] {
            assert_eq!(split_set_entry(&set_entry(name, position)), (name, position));
        }
    }

    #[test]
    fn the_index_sees_a_set_by_its_name_with_the_position_beside_it() {
        let mut c = CompanionFile::new("a.jpg", MediaType::Image);
        c.tags.set = vec!["comic::3".into(), "burst".into()];
        let all = c.all_tags();
        assert!(all.contains(&("set".into(), "comic".into(), Some(3))));
        assert!(all.contains(&("set".into(), "burst".into(), None)));
    }

    #[test]
    fn one_entry_per_set_keeps_the_first() {
        let mut tags = TagCollection {
            set: vec![
                "comic::3".into(),
                "burst".into(),
                "comic::7".into(),
                "comic".into(),
            ],
            ..Default::default()
        };
        tags.one_entry_per_set();
        assert_eq!(tags.set, vec!["comic::3", "burst"]);
    }
}
