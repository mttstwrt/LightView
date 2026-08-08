//! Reading the gallery's files.
//!
//! This was a `FileProvider` trait plus a registry of implementations, sized
//! for the SMB/SFTP/S3 backends named in `ProviderType`. None of those were
//! ever written, and of the trait's seven methods only two were ever called —
//! so the abstraction cost a `dyn` dispatch, an `async_trait` allocation, and
//! five unreachable method bodies per implementation, and bought a seam nothing
//! used. It is now the concrete thing: one struct, two operations.
//!
//! Re-introducing the trait is the right move on the *second* backend, not on
//! the first hypothetical one. The two call shapes to preserve if that happens
//! are `list_dir_recursive` (one recursive scan at gallery open, feeding
//! `media_meta`) and `read_file` (one whole-file read for the viewer).

pub mod local;

use serde::Serialize;

/// One media file found by a scan. Directories are not represented: the scan
/// walks them but only ever emits files.
#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub size: u64,
    /// Modification time, Unix seconds.
    pub mtime: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("File not found: {0}")]
    NotFound(String),
    #[error("Permission denied: {0}")]
    PermissionDenied(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
