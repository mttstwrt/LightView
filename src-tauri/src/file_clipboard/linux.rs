//! Linux/X11 backend.
//!
//! Publishes the file list as `text/uri-list` on the `CLIPBOARD` selection
//! using the `x11-clipboard` crate, which owns the selection on a background
//! thread for the lifetime of the process. We cache one `Clipboard` instance
//! globally so successive calls reuse the same X11 connection — creating a
//! second `Clipboard` would replace the first selection owner before paste.
//!
//! ## Caveats
//! - `Op::Cut` currently degrades to `Op::Copy`. Honouring cut requires
//!   advertising `x-special/gnome-copied-files` (Nautilus) or
//!   `x-kde-cutselection` (Dolphin) alongside `text/uri-list`, which needs
//!   multi-target SELECTION_REQUEST handling not exposed by the high-level
//!   `Clipboard::store` API. Add when needed.
//! - Wayland-only sessions (`XDG_SESSION_TYPE=wayland`) without XWayland will
//!   fail at `Clipboard::new()`. The host app currently forces
//!   `GDK_BACKEND=x11`, so this is fine in practice; for a Wayland-native
//!   build, swap this backend for `wl-clipboard-rs`.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use x11_clipboard::Clipboard;
use x11rb::protocol::xproto::ConnectionExt;

use crate::file_clipboard::{build_uri_list, Error, Op};

fn clipboard() -> Result<&'static Mutex<Clipboard>, Error> {
    static CB: OnceLock<Mutex<Clipboard>> = OnceLock::new();
    if let Some(cb) = CB.get() {
        return Ok(cb);
    }
    let cb = Clipboard::new().map_err(|e| Error::Backend(e.to_string()))?;
    Ok(CB.get_or_init(|| Mutex::new(cb)))
}

pub fn write_files(paths: &[&Path], _op: Op) -> Result<(), Error> {
    let payload = build_uri_list(paths);

    let cb_lock = clipboard()?;
    let cb = cb_lock.lock().map_err(|e| Error::Backend(e.to_string()))?;

    let atoms = &cb.setter.atoms;
    let selection = atoms.clipboard;
    let target = cb
        .setter
        .connection
        .intern_atom(false, b"text/uri-list")
        .map_err(|e| Error::Backend(e.to_string()))?
        .reply()
        .map_err(|e| Error::Backend(e.to_string()))?
        .atom;

    cb.store(selection, target, payload.as_bytes())
        .map_err(|e| Error::Backend(e.to_string()))?;

    Ok(())
}
