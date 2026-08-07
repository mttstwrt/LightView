//! Where LightView keeps process-level state.
//!
//! Exe-relative (`<exe_dir>/data/`), not the platform's user-data directory, so
//! a portable install carries its plugins, TLS material, and recent-gallery
//! list with it. Per-*gallery* state does not live here at all — it lives in
//! the gallery's own `.lightview/` directory, which is what lets a gallery move
//! between machines intact.

use std::path::PathBuf;

/// Return the portable `data/` directory next to the running executable.
///
/// Layout:
///   <exe_dir>/data/plugins/
///   <exe_dir>/data/recent.json
pub fn data_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("failed to locate running executable");
    exe.parent()
        .expect("executable has no parent directory")
        .join("data")
}
