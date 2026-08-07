//! Locating and parsing a companion file.
//!
//! Reads try the requested [`CompanionLocation`] and then fall back to the
//! other one, so a gallery whose preference changed keeps resolving its older
//! sidecars without a migration pass. The fallback is a read affordance only —
//! writes go where they are told, or the two locations would slowly diverge.
//!
//! [`crate::companion::migration::migrate`] runs on every parse, which is what
//! makes adding a schema version a change to one function rather than an audit
//! of every caller.

use std::path::Path;

use crate::companion::schema::{CompanionFile, COMPANION_EXTENSION, CURRENT_SCHEMA_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Unsupported schema version: {0} (current: {1})")]
    UnsupportedVersion(u32, u32),
    #[error("Companion file not found for: {0}")]
    NotFound(String),
}

/// Where companion files are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompanionLocation {
    /// Store in the `.lightview/companions/` directory tree (default).
    #[default]
    LightviewFolder,
    /// Store alongside the media file (e.g. `photo.jpg.lightview.json`).
    Alongside,
}

/// Compute the companion file path for a given media file and storage location.
pub fn companion_path(media_path: &Path, location: CompanionLocation) -> std::path::PathBuf {
    match location {
        CompanionLocation::Alongside => {
            let mut p = media_path.as_os_str().to_owned();
            p.push(COMPANION_EXTENSION);
            std::path::PathBuf::from(p)
        }
        CompanionLocation::LightviewFolder => {
            // Store in .lightview/companions/ relative to the media file's parent dir.
            // The filename becomes <original_name>.lightview.json inside .lightview/companions/.
            let parent = media_path.parent().unwrap_or_else(|| Path::new("."));
            let filename = media_path
                .file_name()
                .map(|n| {
                    let mut s = n.to_owned();
                    s.push(COMPANION_EXTENSION);
                    s
                })
                .unwrap_or_else(|| std::ffi::OsString::from("unknown.lightview.json"));
            parent
                .join(".lightview")
                .join("companions")
                .join(filename)
        }
    }
}

/// Read and parse a companion file from disk.
/// Tries the specified location first, then falls back to the other location.
pub fn read_companion(media_path: &Path) -> Result<CompanionFile, ReadError> {
    read_companion_at(media_path, CompanionLocation::default())
}

/// Read a companion file from a specific location, with fallback to the other location.
pub fn read_companion_at(
    media_path: &Path,
    primary: CompanionLocation,
) -> Result<CompanionFile, ReadError> {
    let primary_path = companion_path(media_path, primary);
    if primary_path.exists() {
        let contents = std::fs::read_to_string(&primary_path)?;
        return parse_companion(&contents);
    }

    // Try the other location as fallback
    let fallback = match primary {
        CompanionLocation::LightviewFolder => CompanionLocation::Alongside,
        CompanionLocation::Alongside => CompanionLocation::LightviewFolder,
    };
    let fallback_path = companion_path(media_path, fallback);
    if fallback_path.exists() {
        let contents = std::fs::read_to_string(&fallback_path)?;
        return parse_companion(&contents);
    }

    Err(ReadError::NotFound(media_path.display().to_string()))
}

/// Read a companion file if it exists, returning None if not found.
pub fn read_companion_optional(media_path: &Path) -> Result<Option<CompanionFile>, ReadError> {
    read_companion_optional_at(media_path, CompanionLocation::default())
}

/// Read a companion file if it exists at the specified location (with fallback).
pub fn read_companion_optional_at(
    media_path: &Path,
    location: CompanionLocation,
) -> Result<Option<CompanionFile>, ReadError> {
    match read_companion_at(media_path, location) {
        Ok(c) => Ok(Some(c)),
        Err(ReadError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Parse companion JSON string into a CompanionFile, validating the schema version.
pub fn parse_companion(json: &str) -> Result<CompanionFile, ReadError> {
    let companion: CompanionFile = serde_json::from_str(json)?;

    if companion.schema_version > CURRENT_SCHEMA_VERSION {
        return Err(ReadError::UnsupportedVersion(
            companion.schema_version,
            CURRENT_SCHEMA_VERSION,
        ));
    }

    // Future: if companion.schema_version < CURRENT_SCHEMA_VERSION, run migrations

    Ok(companion)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_companion_path_alongside() {
        let p = companion_path(Path::new("/photos/sunset.jpg"), CompanionLocation::Alongside);
        assert_eq!(p, std::path::PathBuf::from("/photos/sunset.jpg.lightview.json"));
    }

    #[test]
    fn test_companion_path_lightview_folder() {
        let p = companion_path(
            Path::new("/photos/sunset.jpg"),
            CompanionLocation::LightviewFolder,
        );
        assert_eq!(
            p,
            std::path::PathBuf::from("/photos/.lightview/companions/sunset.jpg.lightview.json")
        );
    }

    #[test]
    fn test_parse_valid_json() {
        let json = r#"{
            "schema_version": 1,
            "file": "test.jpg",
            "file_hash": "sha256:abc",
            "media_type": "image",
            "created": "2026-01-01T00:00:00Z",
            "modified": "2026-01-01T00:00:00Z",
            "tags": { "user": ["test"], "auto": [], "plugins": {} },
            "meta": { "core": null, "plugins": {} }
        }"#;
        let result = parse_companion(json);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().tags.user, vec!["test"]);
    }

    #[test]
    fn test_parse_future_version_rejected() {
        let json = r#"{
            "schema_version": 999,
            "file": "test.jpg",
            "file_hash": "",
            "media_type": "image",
            "created": "",
            "modified": "",
            "tags": { "user": [], "auto": [], "plugins": {} },
            "meta": { "plugins": {} }
        }"#;
        let result = parse_companion(json);
        assert!(matches!(result, Err(ReadError::UnsupportedVersion(999, _))));
    }
}
