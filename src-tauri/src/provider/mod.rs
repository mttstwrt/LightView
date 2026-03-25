pub mod local;

use async_trait::async_trait;
use bytes::Bytes;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

use crate::companion::schema::CompanionFile;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u64,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GalleryInfo {
    pub path: String,
    pub total_media: usize,
    pub provider_type: ProviderType,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderType {
    Local,
    Smb,
    Sftp,
    S3,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("File not found: {0}")]
    NotFound(String),
    #[error("Permission denied: {0}")]
    PermissionDenied(String),
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Parse error: {0}")]
    Parse(String),
}

// ---------------------------------------------------------------------------
// FileProvider trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait FileProvider: Send + Sync {
    /// List all entries in a directory.
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, ProviderError>;

    /// Read raw file bytes.
    async fn read_file(&self, path: &str) -> Result<Bytes, ProviderError>;

    /// Read and parse a companion file for the given media path.
    async fn read_companion(&self, media_path: &str) -> Result<Option<CompanionFile>, ProviderError>;

    /// Write a companion file atomically.
    async fn write_companion(
        &self,
        media_path: &str,
        data: &CompanionFile,
    ) -> Result<(), ProviderError>;

    /// Check if a file exists.
    async fn exists(&self, path: &str) -> Result<bool, ProviderError>;

    /// Get file modification time (unix timestamp) for cache invalidation.
    async fn mtime(&self, path: &str) -> Result<u64, ProviderError>;

    /// Provider type identifier.
    fn provider_type(&self) -> ProviderType;
}

// ---------------------------------------------------------------------------
// Provider registry
// ---------------------------------------------------------------------------

/// Manages multiple file providers, keyed by a user-defined label or path.
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn FileProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    /// Register a provider under a key (typically the gallery path or a label).
    pub fn register(&mut self, key: String, provider: Arc<dyn FileProvider>) {
        self.providers.insert(key, provider);
    }

    /// Get a provider by key.
    pub fn get(&self, key: &str) -> Option<Arc<dyn FileProvider>> {
        self.providers.get(key).cloned()
    }

    /// Remove a provider by key.
    pub fn remove(&mut self, key: &str) -> Option<Arc<dyn FileProvider>> {
        self.providers.remove(key)
    }
}
