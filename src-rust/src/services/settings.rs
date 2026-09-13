//! `.lightview/settings.toml` — the gallery's own two settings.
//!
//! Durable, and it holds **exactly two keys**.
//!
//! The **default filter** is user intent. An earlier design put it in the
//! derived database, where a `format_version` bump would have deleted it; it is
//! written by a `Device` command, because under `--serve` the phone is the only
//! UI there is.
//!
//! **Trash retention** is per gallery because it travels with the folder, and
//! it is set only by editing the file. It is the one setting in the system that
//! deletes data, so no command writes it at any trust level. Moving it to a
//! per-machine config file would mean a NAS gallery served at 365 days, opened
//! once on a desktop with no config, silently losing eleven months of the
//! *shared* trash to a 30-day default.
//!
//! **Display preferences are not here, for any client.** A file inside the
//! gallery is per-*gallery*, not per-client, so two desktops mounting one share
//! would fight over thumbnail size — which is the exact thing a per-client
//! preference exists to prevent. They live in the browser's own storage, for
//! every client including the local one, and no local-versus-remote branch
//! survives into the frontend.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How long a trashed file is kept. Thirty days, and a gallery that wants
/// another number says so in its own file.
const DEFAULT_TRASH_RETENTION_DAYS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GallerySettings {
    /// Applied when the gallery is opened, by any client.
    pub default_filter: String,
    /// Days before `auto_purge` removes a trash entry. Hand-edited only.
    pub trash_retention_days: u64,
}

impl Default for GallerySettings {
    fn default() -> Self {
        Self {
            default_filter: String::new(),
            trash_retention_days: DEFAULT_TRASH_RETENTION_DAYS,
        }
    }
}

/// Where the file lives, given a gallery root.
pub fn settings_path(root: &Path) -> PathBuf {
    root.join(".lightview").join("settings.toml")
}

impl GallerySettings {
    /// Read the gallery's settings, or the defaults.
    ///
    /// **A malformed file falls back to the defaults with a warning**, which is
    /// the opposite of `server.toml`'s behaviour and is deliberate. This file
    /// is hand-edited, it lives in the durable tree, and refusing to open a
    /// gallery because someone fat-fingered a retention value would be a worse
    /// failure than opening it with the default. The dangerous direction —
    /// deleting more than intended — is covered by the default being the
    /// *shortest* of the plausible values only in the sense that it is the
    /// documented one; `auto_purge` is the thing that must be careful.
    pub fn load(root: &Path) -> Self {
        let path = settings_path(root);
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("{} is malformed, using defaults: {e}", path.display());
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                log::warn!("could not read {}: {e}", path.display());
                Self::default()
            }
        }
    }

    /// Write the default filter, preserving whatever retention the file holds.
    ///
    /// The read-modify-write matters: this is called by a `Device` command, and
    /// serializing the whole struct from a value that was loaded at open would
    /// stamp a stale retention over a hand edit made since.
    pub fn set_default_filter(root: &Path, filter: &str) -> std::io::Result<Self> {
        let mut current = Self::load(root);
        current.default_filter = filter.to_string();
        let text = toml::to_string_pretty(&current)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::util::fs_atomic::write_durable(&settings_path(root), text.as_bytes())?;
        Ok(current)
    }

    pub fn trash_retention_secs(&self) -> i64 {
        (self.trash_retention_days * 24 * 3600) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_the_defaults() {
        let d = tempfile::tempdir().unwrap();
        let s = GallerySettings::load(d.path());
        assert_eq!(s, GallerySettings::default());
        assert_eq!(s.trash_retention_days, 30);
    }

    #[test]
    fn a_malformed_file_does_not_stop_the_gallery_opening() {
        let d = tempfile::tempdir().unwrap();
        let p = settings_path(d.path());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "trash_retention_days = \"forever\"\n").unwrap();
        assert_eq!(GallerySettings::load(d.path()), GallerySettings::default());
    }

    #[test]
    fn writing_the_filter_preserves_a_hand_edited_retention() {
        // The failure this prevents: a command serializing a struct loaded at
        // open, stamping a stale retention over an edit made since.
        let d = tempfile::tempdir().unwrap();
        let p = settings_path(d.path());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "trash_retention_days = 365\n").unwrap();

        GallerySettings::set_default_filter(d.path(), "rating>=4").unwrap();

        let s = GallerySettings::load(d.path());
        assert_eq!(s.default_filter, "rating>=4");
        assert_eq!(s.trash_retention_days, 365, "a hand edit was overwritten");
    }

    #[test]
    fn retention_converts_to_seconds() {
        let s = GallerySettings {
            trash_retention_days: 2,
            ..Default::default()
        };
        assert_eq!(s.trash_retention_secs(), 172_800);
    }
}
