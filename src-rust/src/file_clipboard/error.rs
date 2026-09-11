//! Errors from reading file paths off the system clipboard.

use std::path::PathBuf;

#[derive(Debug)]
pub enum Error {
    /// Caller passed an empty path list.
    Empty,
    /// One of the supplied paths does not exist on disk.
    InvalidPath(PathBuf),
    /// Backend (X11 / Win32 / Cocoa) returned an error.
    Backend(String),
    /// This target OS has no clipboard backend wired up.
    Unsupported,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Empty => write!(f, "no paths supplied"),
            Error::InvalidPath(p) => write!(f, "path does not exist: {}", p.display()),
            Error::Backend(msg) => write!(f, "clipboard backend error: {}", msg),
            Error::Unsupported => write!(f, "file clipboard not supported on this OS"),
        }
    }
}

impl std::error::Error for Error {}
