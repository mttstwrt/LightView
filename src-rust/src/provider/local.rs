//! Local-filesystem access for an open gallery.

use bytes::Bytes;
use std::path::Path;

use crate::companion::schema::MediaType;
use crate::path::{GalleryPath, RelPath, Root};
use crate::provider::{FileEntry, ProviderError};

/// Rooted at the open gallery's canonical directory.
pub struct LocalProvider {
    root: Root,
}

impl LocalProvider {
    pub fn new(root: Root) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Root {
        &self.root
    }

    /// Recursively discover the media files under the gallery root.
    ///
    /// Skips every dot-directory, which covers `.lightview` (trash and
    /// companions) along with the usual `.git`/`.Trash` noise, and skips
    /// anything whose extension is not a known media type — so the result is
    /// exactly the set `media_meta` should contain.
    ///
    /// Runs once per gallery open and is the gating step before the grid can
    /// render, so it does the minimum per entry: one `metadata()` call, no
    /// canonicalization, no companion reads.
    ///
    /// **Walk errors are propagated, not swallowed.** The caller deletes every
    /// path-keyed row for a path missing from the result, so an `Ok` that means
    /// "I gave up halfway" is a prune authority it must not have: a NAS not yet
    /// mounted at boot leaves an empty mountpoint that scans to zero, and a
    /// transient `EIO` mid-walk prunes partially with no log line. Both wipe
    /// `date_added` and `last_viewed`, which are not recoverable from the
    /// photos.
    pub fn list_dir_recursive(&self) -> Result<Vec<FileEntry>, ProviderError> {
        let dir = self.root.as_path();
        let mut entries = Vec::new();

        // `filter_entry` is evaluated on the root too, so a gallery whose own
        // directory name starts with a dot — `~/.photos` — would scan to zero
        // with the predicate applied naively. Compare against the root instead
        // of the name alone.
        let walker = walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_entry(|e| {
                e.path() == dir || !e.file_name().to_string_lossy().starts_with('.')
            });

        for entry in walker {
            let entry = entry.map_err(|source| ProviderError::ScanFailed {
                path: dir.display().to_string(),
                source,
            })?;

            if entry.file_type().is_dir() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();

            if name.ends_with(crate::companion::schema::COMPANION_EXTENSION) {
                continue;
            }

            let ext = entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if MediaType::from_extension(ext).is_none() {
                continue;
            }

            let metadata = entry.metadata().map_err(|source| ProviderError::ScanFailed {
                path: entry.path().display().to_string(),
                source,
            })?;

            // The walk starts at the canonical root, so every entry is under
            // it; a path that fails this is a bug rather than an input.
            let path = match self.root.relativize(entry.path()) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("scan produced a path outside the gallery: {e}");
                    continue;
                }
            };

            let mtime = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            entries.push(FileEntry {
                name,
                path,
                size: metadata.len(),
                mtime,
            });
        }

        entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(entries)
    }

    /// Resolve a wire path to something that can be opened.
    pub fn resolve(&self, rel: &RelPath) -> Result<GalleryPath, crate::path::PathError> {
        self.root.resolve(rel)
    }

    /// Read a whole file into memory.
    ///
    /// Deliberately `std::fs::read` rather than a memory map: the caller wants
    /// every byte (it is about to decode the image), so a map would only add a
    /// copy out of the mapped pages. The thumbnail path, which often touches
    /// only part of a stream, does map — see `pipeline::thumbnailer`.
    pub fn read_file(&self, path: &GalleryPath) -> Result<Bytes, ProviderError> {
        let full: &Path = path.as_path();
        let data = std::fs::read(full).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ProviderError::NotFound(full.display().to_string()),
            std::io::ErrorKind::PermissionDenied => {
                ProviderError::PermissionDenied(full.display().to_string())
            }
            _ => ProviderError::Io(e),
        })?;
        Ok(Bytes::from(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gallery() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("2026/january")).unwrap();
        std::fs::write(d.path().join("2026/january/a.jpg"), b"x").unwrap();
        std::fs::write(d.path().join("b.png"), b"x").unwrap();
        std::fs::write(d.path().join("notes.txt"), b"x").unwrap();
        std::fs::create_dir_all(d.path().join(".lightview/companions")).unwrap();
        std::fs::write(
            d.path().join(".lightview/companions/a.jpg.lightview.json"),
            b"{}",
        )
        .unwrap();
        d
    }

    #[test]
    fn scan_returns_relative_media_paths_only() {
        let d = gallery();
        let p = LocalProvider::new(Root::open(d.path()).unwrap());
        let mut found: Vec<_> = p
            .list_dir_recursive()
            .unwrap()
            .into_iter()
            .map(|e| e.path.as_str().to_string())
            .collect();
        found.sort();
        assert_eq!(found, vec!["2026/january/a.jpg", "b.png"]);
    }

    #[test]
    fn a_dot_prefixed_gallery_root_still_scans() {
        // filter_entry runs on the root as well, so `~/.photos` would otherwise
        // scan to zero — and a zero scan is what authorizes a prune.
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join(".photos");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.jpg"), b"x").unwrap();

        let p = LocalProvider::new(Root::open(&root).unwrap());
        assert_eq!(p.list_dir_recursive().unwrap().len(), 1);
    }
}
