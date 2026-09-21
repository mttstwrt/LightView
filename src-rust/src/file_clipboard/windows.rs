//! Windows backend (placeholder).
//!
//! Planned implementation: `clipboard-win::Clipboard::new_attempts(10)?` then
//! `clipboard_win::raw::set_file_list(&paths)`. Add the `clipboard-win = "5"`
//! dependency under `[target.'cfg(target_os = "windows")'.dependencies]`.

use std::path::Path;

use crate::file_clipboard::{Error, Op};

pub fn write_files(_paths: &[&Path], _op: Op) -> Result<(), Error> {
    Err(Error::Backend(
        "Windows backend not yet implemented".to_string(),
    ))
}
