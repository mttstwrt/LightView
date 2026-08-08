//! Atomically replacing a companion file.
//!
//! Serialize to a uniquely-named temp file **in the target directory**, then
//! rename over the destination. Same directory means same filesystem, which is
//! what makes the rename atomic: a concurrent reader sees either the old file
//! or the new one, never a partial write. That matters because batch tagging
//! writes companions while the indexer may be walking the same tree.
//!
//! `modified` is stamped here rather than by the caller, so every write carries
//! an accurate timestamp regardless of which path produced it.

use std::path::Path;

use crate::companion::reader::{companion_path, CompanionLocation};
use crate::companion::schema::CompanionFile;

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Write a companion file atomically using the default location.
pub fn write_companion(media_path: &Path, data: &mut CompanionFile) -> Result<(), WriteError> {
    write_companion_at(media_path, data, CompanionLocation::default())
}

/// Write a companion file atomically to the specified location.
pub fn write_companion_at(
    media_path: &Path,
    data: &mut CompanionFile,
    location: CompanionLocation,
) -> Result<(), WriteError> {
    let target = companion_path(media_path, location);
    let dir = target
        .parent()
        .unwrap_or_else(|| Path::new("."));

    // Ensure the directory exists (needed for .lightview/companions/ path)
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }

    // Update the modified timestamp
    data.modified = chrono::Utc::now().to_rfc3339();

    // Serialize to pretty JSON
    let json = serde_json::to_string_pretty(data)?;

    // Write to a temp file in the same directory (ensures same filesystem for rename)
    let temp_name = format!(
        ".lightview-tmp-{}.json",
        uuid::Uuid::new_v4().as_simple()
    );
    let temp_path = dir.join(&temp_name);

    std::fs::write(&temp_path, &json)?;

    // Atomic rename
    std::fs::rename(&temp_path, &target).map_err(|e| {
        // Clean up temp file on rename failure
        let _ = std::fs::remove_file(&temp_path);
        WriteError::Io(e)
    })?;

    Ok(())
}

/// Ensure a companion file exists for the given media file.
/// If it doesn't exist, create one with defaults.
pub fn ensure_companion(
    media_path: &Path,
    media_type: crate::companion::schema::MediaType,
) -> Result<CompanionFile, WriteError> {
    ensure_companion_at(media_path, media_type, CompanionLocation::default())
}

/// Ensure a companion file exists at the specified location.
pub fn ensure_companion_at(
    media_path: &Path,
    media_type: crate::companion::schema::MediaType,
    location: CompanionLocation,
) -> Result<CompanionFile, WriteError> {
    let target = companion_path(media_path, location);
    if target.exists() {
        // Read existing
        let contents = std::fs::read_to_string(&target)?;
        let companion: CompanionFile = serde_json::from_str(&contents)
            .map_err(|e| WriteError::Json(e))?;
        return Ok(companion);
    }

    // Also check fallback location
    let fallback = match location {
        CompanionLocation::LightviewFolder => CompanionLocation::Alongside,
        CompanionLocation::Alongside => CompanionLocation::LightviewFolder,
    };
    let fallback_path = companion_path(media_path, fallback);
    if fallback_path.exists() {
        let contents = std::fs::read_to_string(&fallback_path)?;
        let companion: CompanionFile = serde_json::from_str(&contents)
            .map_err(|e| WriteError::Json(e))?;
        return Ok(companion);
    }

    // Create new
    let filename = media_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let mut companion = CompanionFile::new(filename, media_type);
    write_companion_at(media_path, &mut companion, location)?;
    Ok(companion)
}
