//! macOS backend (placeholder).
//!
//! Planned implementation: build an `NSArray<NSURL>` of `fileURLWithPath:`
//! entries and call `NSPasteboard generalPasteboard | clearContents |
//! writeObjects:` via objc2-app-kit. Add the `objc2`, `objc2-app-kit`, and
//! `objc2-foundation` dependencies under
//! `[target.'cfg(target_os = "macos")'.dependencies]`.

use std::path::Path;

use crate::file_clipboard::{Error, Op};

pub fn write_files(_paths: &[&Path], _op: Op) -> Result<(), Error> {
    Err(Error::Backend(
        "macOS backend not yet implemented".to_string(),
    ))
}
