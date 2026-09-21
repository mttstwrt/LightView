//! Portable "copy files to OS clipboard" module.
//!
//! Drop this directory into any Rust project and call [`write_files`] to put
//! a list of file paths on the native clipboard such that pasting into the
//! system file manager performs a real file paste.
//!
//! ## Required Cargo deps to port this module
//!
//! ```toml
//! [target.'cfg(target_os = "linux")'.dependencies]
//! x11-clipboard = "0.9"
//!
//! [target.'cfg(target_os = "windows")'.dependencies]
//! clipboard-win = "5"
//!
//! [target.'cfg(target_os = "macos")'.dependencies]
//! objc2 = "0.5"
//! objc2-app-kit = "0.2"
//! objc2-foundation = "0.2"
//! ```
//!
//! No other project code is referenced — this module is self-contained.
//!
//! ## Its precondition is gone
//!
//! The X11 backend owns the selection on a background thread for the life of
//! the process, and a Wayland session without XWayland fails at
//! `Clipboard::new()`. That was "fine in practice" only because the host forced
//! `GDK_BACKEND=x11` for WebKit — a variable that is deleted along with WebKit,
//! on a process that may now have no display at all. So the failure is
//! reported rather than assumed away: [`available`] answers it once, the
//! capabilities command carries the answer to the client, and the frontend
//! hides the action instead of offering a button that returns an error.
//!
//! It is an `Owner` command regardless, so it is never offered remotely.

use std::path::Path;

mod error;
pub use error::Error;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "macos")]
mod macos;

/// Whether the file list should be pasted as a copy or a move.
///
/// On platforms whose file managers do not honour the cut/copy distinction
/// over the clipboard (currently: Linux without GNOME-style markers), `Cut`
/// degrades to `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Copy,
    Cut,
}

/// Whether this process can put files on a clipboard at all.
///
/// Answered by doing the thing that fails — constructing the backend — because
/// guessing from `XDG_SESSION_TYPE` gets XWayland wrong in both directions. The
/// connection is cached, so the probe is paid once and [`write_files`] reuses
/// it.
pub fn available() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::available()
    }
    #[cfg(not(target_os = "linux"))]
    {
        cfg!(any(target_os = "windows", target_os = "macos"))
    }
}

/// Place `paths` on the OS clipboard as a file list.
///
/// Blocking. Safe to call from any thread; on Linux the X11 connection is
/// cached internally so the selection survives across calls.
pub fn write_files(paths: &[&Path], op: Op) -> Result<(), Error> {
    if paths.is_empty() {
        return Err(Error::Empty);
    }
    for p in paths {
        if !p.exists() {
            return Err(Error::InvalidPath(p.to_path_buf()));
        }
    }

    #[cfg(target_os = "linux")]
    {
        linux::write_files(paths, op)
    }
    #[cfg(target_os = "windows")]
    {
        windows::write_files(paths, op)
    }
    #[cfg(target_os = "macos")]
    {
        macos::write_files(paths, op)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = op;
        Err(Error::Unsupported)
    }
}

/// Percent-encode a single path component for embedding in a `file://` URI.
///
/// Encodes everything outside the RFC 3986 unreserved set, except `/` which is
/// left as a path separator.
pub(crate) fn encode_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b;
        let unreserved = c.is_ascii_alphanumeric()
            || matches!(c, b'-' | b'.' | b'_' | b'~' | b'/');
        if unreserved {
            out.push(c as char);
        } else {
            out.push('%');
            out.push_str(&format!("{:02X}", c));
        }
    }
    out
}

/// Build a CRLF-separated `text/uri-list` payload from absolute paths.
pub(crate) fn build_uri_list(paths: &[&Path]) -> String {
    let mut out = String::new();
    for p in paths {
        if !out.is_empty() {
            out.push_str("\r\n");
        }
        out.push_str("file://");
        out.push_str(&encode_path(p));
    }
    out
}
