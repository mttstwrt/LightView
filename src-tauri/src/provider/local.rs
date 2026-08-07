//! Local-filesystem access for an open gallery.

use bytes::Bytes;
use std::path::{Path, PathBuf};

use crate::companion::schema::MediaType;
use crate::provider::{FileEntry, ProviderError};

/// Rooted at the open gallery directory. Paths handed to it may be absolute
/// (the common case — every cached path is) or relative to that root.
pub struct LocalProvider {
    root: PathBuf,
}

impl LocalProvider {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.root.join(path)
        }
    }

    /// Recursively discover the media files under `path`.
    ///
    /// Skips every dot-directory, which covers `.lightview` (our own cache and
    /// companion storage) along with the usual `.git`/`.Trash` noise, and skips
    /// anything whose extension is not a known media type — so the result is
    /// exactly the set `media_meta` should contain.
    ///
    /// Runs once per gallery open and is the gating step before the grid can
    /// render, so it does the minimum per entry: one `metadata()` call, no
    /// canonicalization, no companion reads.
    pub fn list_dir_recursive(&self, path: &str) -> Result<Vec<FileEntry>, ProviderError> {
        let dir = self.resolve(path);
        let mut entries = Vec::new();

        for entry in walkdir::WalkDir::new(&dir)
            .into_iter()
            .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.'))
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if entry.file_type().is_dir() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();

            if name.ends_with(crate::companion::schema::COMPANION_EXTENSION) {
                continue;
            }

            let ext = entry.path()
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if MediaType::from_extension(ext).is_none() {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };

            let mtime = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            entries.push(FileEntry {
                name,
                path: entry.path().to_string_lossy().to_string(),
                size: metadata.len(),
                mtime,
            });
        }

        entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(entries)
    }

    /// Read a whole file into memory.
    ///
    /// Deliberately `std::fs::read` rather than a memory map: the caller wants
    /// every byte (it is about to decode the image), so a map would only add a
    /// copy out of the mapped pages. The thumbnail path, which often touches
    /// only part of a stream, does map — see `pipeline::thumbnailer`.
    pub fn read_file(&self, path: &str) -> Result<Bytes, ProviderError> {
        let full = self.resolve(path);
        let data = std::fs::read(&full).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ProviderError::NotFound(full.display().to_string()),
            std::io::ErrorKind::PermissionDenied => {
                ProviderError::PermissionDenied(full.display().to_string())
            }
            _ => ProviderError::Io(e),
        })?;
        Ok(Bytes::from(data))
    }
}
