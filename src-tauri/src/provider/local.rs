use async_trait::async_trait;
use bytes::Bytes;
use memmap2::Mmap;
use std::path::{Path, PathBuf};

use crate::companion::reader;
use crate::companion::schema::{CompanionFile, MediaType, COMPANION_EXTENSION};
use crate::provider::{FileEntry, FileProvider, ProviderError, ProviderType};

/// File provider for the local filesystem.
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
}

#[async_trait]
impl FileProvider for LocalProvider {
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, ProviderError> {
        let dir = self.resolve(path);
        let mut entries = Vec::new();

        let read_dir = std::fs::read_dir(&dir).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProviderError::NotFound(dir.display().to_string())
            } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                ProviderError::PermissionDenied(dir.display().to_string())
            } else {
                ProviderError::Io(e)
            }
        })?;

        for entry in read_dir {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();

            // Skip hidden files and companion files
            if name.starts_with('.') || name.ends_with(COMPANION_EXTENSION) {
                continue;
            }

            let metadata = entry.metadata()?;
            let is_dir = metadata.is_dir();

            // Skip non-media files (unless directory)
            if !is_dir {
                let ext = Path::new(&name)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");
                if MediaType::from_extension(ext).is_none() {
                    continue;
                }
            }

            let mtime = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let mime_type = if is_dir {
                None
            } else {
                Some(mime_from_ext(
                    Path::new(&name)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or(""),
                ))
            };

            entries.push(FileEntry {
                name,
                path: entry.path().to_string_lossy().to_string(),
                is_dir,
                size: metadata.len(),
                mtime,
                mime_type,
            });
        }

        // Sort: directories first, then by name
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        Ok(entries)
    }

    async fn read_file(&self, path: &str) -> Result<Bytes, ProviderError> {
        let full = self.resolve(path);
        let file = std::fs::File::open(&full).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProviderError::NotFound(full.display().to_string())
            } else {
                ProviderError::Io(e)
            }
        })?;
        // SAFETY: Read-only mmap, file held open for the duration.
        let mmap = unsafe { Mmap::map(&file) }.map_err(ProviderError::Io)?;
        Ok(Bytes::copy_from_slice(&mmap))
    }

    async fn read_companion(
        &self,
        media_path: &str,
    ) -> Result<Option<CompanionFile>, ProviderError> {
        let full = self.resolve(media_path);
        match reader::read_companion_optional(&full) {
            Ok(c) => Ok(c),
            Err(e) => Err(ProviderError::Parse(e.to_string())),
        }
    }

    async fn write_companion(
        &self,
        media_path: &str,
        data: &CompanionFile,
    ) -> Result<(), ProviderError> {
        let full = self.resolve(media_path);
        let mut data = data.clone();
        crate::companion::writer::write_companion(&full, &mut data)
            .map_err(|e| ProviderError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        Ok(())
    }

    async fn exists(&self, path: &str) -> Result<bool, ProviderError> {
        Ok(self.resolve(path).exists())
    }

    async fn mtime(&self, path: &str) -> Result<u64, ProviderError> {
        let full = self.resolve(path);
        let metadata = std::fs::metadata(&full)?;
        Ok(metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0))
    }

    fn provider_type(&self) -> ProviderType {
        ProviderType::Local
    }
}

fn mime_from_ext(ext: &str) -> String {
    match ext.to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tiff" | "tif" => "image/tiff",
        "heic" | "heif" => "image/heif",
        "avif" => "image/avif",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
    .to_string()
}
