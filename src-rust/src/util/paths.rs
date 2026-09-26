//! Where LightView keeps machine-local state, and the rule that decides what
//! is machine-local: **everything durable lives in the gallery; everything
//! derived lives here.**
//!
//! ```text
//! <gallery>/                                    the user's photos, untouched
//!   .lightview/
//!     companions/<name>.lightview.json          per directory — tags, rating, notes
//!     companions/.lock                          the fcntl lock for that directory
//!     trash/<epoch_ms>_<seq>/<relative path>    one delete is one directory
//!     settings.toml                             default filter, trash retention
//!
//! $XDG_CACHE_HOME/lightview/galleries/<sha256-of-canonical-root>/
//!                                   cache.db    derived, disposable, budgeted
//!                                   lock        the one-writer flock
//!                                   instance.json  pid + live launch URL
//!                                   last_opened    the cross-gallery LRU key
//! $XDG_DATA_HOME/lightview/         tls/ devices.db plugins/<name>/
//! $XDG_CONFIG_HOME/lightview/       server.toml
//! ```
//!
//! **The derived cache is outside the gallery** because the gallery is the one
//! tree a person greps, rsyncs, backs up and syncs, and a SQLite database inside
//! it breaks all four — a WAL on a network mount worst of all. Losing the cache
//! costs time and nothing else, which is what lets a `format_version` bump
//! delete and rebuild rather than migrate. Keying it on the SHA-256 of the
//! *canonical* root makes a symlinked path and its target one gallery, not two.
//! The cost is that a desktop tagging a NAS gallery over a mount builds its own
//! cache for it, because it cannot read the server's; the ceiling in
//! [`crate::cache::store`] is what bounds that.
//!
//! Callers must uphold two rules. **Never write a derived byte into the
//! gallery** — it holds originals, companions, the trash and `settings.toml`,
//! and nothing else. **Resolve against the canonical root**
//! ([`crate::state::Gallery::root`]), never the path the user typed: the
//! database keys, the watcher's `strip_prefix` and this directory's name agree
//! only because they derive from one value.
//!
//! Three XDG base directories rather than one application directory, because
//! each is a standard location with an established meaning: "which of these can
//! I safely delete?" is answered by the path, and a user emptying `~/.cache`, or
//! systemd-tmpfiles sweeping it, is safe by construction. An exe-relative
//! `<exe_dir>/data/` would resolve an installed `/usr/bin/lightview` to a
//! root-owned, unwritable `/usr/bin/data/`.
//!
//! [`Dirs`] is a value, constructed once in `main` and carried in application
//! state, rather than a set of free functions reading a global. That is what
//! lets two galleries with different `--data-dir` overrides coexist in one test
//! process, as the two-tagging-machines test needs.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The three machine-local roots, resolved once at startup.
#[derive(Debug, Clone)]
pub struct Dirs {
    cache: PathBuf,
    data: PathBuf,
    config: PathBuf,
}

impl Dirs {
    /// Resolve from the environment: `XDG_CACHE_HOME`, `XDG_DATA_HOME` and
    /// `XDG_CONFIG_HOME`, each falling back to its specified default under
    /// `$HOME`.
    pub fn from_env() -> Self {
        Self {
            cache: xdg("XDG_CACHE_HOME", ".cache").join("lightview"),
            data: xdg("XDG_DATA_HOME", ".local/share").join("lightview"),
            config: xdg("XDG_CONFIG_HOME", ".config").join("lightview"),
        }
    }

    /// Put all three under one root, as `--data-dir <path>` does.
    ///
    /// One line in a compose file replaces the volume mount the exe-relative
    /// layout needed, and one flag gives a test its own private machine.
    pub fn under(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            cache: root.join("cache"),
            data: root.join("data"),
            config: root.join("config"),
        }
    }

    /// `$XDG_CACHE_HOME/lightview` — derived, disposable, budgeted.
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// `$XDG_DATA_HOME/lightview` — pairings, TLS material, installed plugins.
    pub fn data(&self) -> &Path {
        &self.data
    }

    /// `$XDG_CONFIG_HOME/lightview` — `server.toml` and nothing else.
    pub fn config(&self) -> &Path {
        &self.config
    }

    /// The directory holding every gallery's derived cache. The cross-gallery
    /// ceiling is measured over exactly this subtree.
    pub fn galleries(&self) -> PathBuf {
        self.cache.join("galleries")
    }

    /// This gallery's derived-cache directory.
    ///
    /// Keyed by a hash of the **canonical** root, so the same gallery reached
    /// through a symlink and through its real path is one cache rather than
    /// two. The accepted cost, stated in the design: moving a gallery
    /// re-thumbnails it, and a share mounted at different paths on two machines
    /// gets two caches.
    pub fn gallery_cache(&self, canonical_root: &Path) -> PathBuf {
        self.galleries().join(gallery_key(canonical_root))
    }

    /// Installed plugin code. A job carries a plugin *name*; only an actual
    /// child of this directory can ever be selected.
    pub fn plugins(&self) -> PathBuf {
        self.data.join("plugins")
    }

    /// The self-signed certificate and its private key.
    pub fn tls(&self) -> PathBuf {
        self.data.join("tls")
    }

    /// Every pairing this account holds. Per account rather than per gallery:
    /// a phone paired to this machine is paired to every gallery it serves.
    pub fn devices_db(&self) -> PathBuf {
        self.data.join("devices.db")
    }


    /// `server.toml`, read at startup and on change.
    pub fn server_toml(&self) -> PathBuf {
        self.config.join("server.toml")
    }

    /// Create the three roots. Called once at startup; the gallery
    /// subdirectory is created when a gallery is opened.
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.cache)?;
        std::fs::create_dir_all(&self.data)?;
        std::fs::create_dir_all(&self.config)?;
        Ok(())
    }
}

/// The cache key for a gallery: the hex SHA-256 of its canonical root.
///
/// Public because the cache directory's name is user-visible through
/// `lightview cache`, and because a test that wants to plant a cache needs to
/// be able to name one.
pub fn gallery_key(canonical_root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_root.as_os_str().as_encoded_bytes());
    format!("{:x}", hasher.finalize())
}

/// `$XDG_<name>` if it is set and absolute, else `$HOME/<fallback>`.
///
/// The absoluteness check is the specification's, not caution: a relative value
/// is required to be ignored, and honouring one would put state wherever the
/// process happened to be started.
fn xdg(var: &str, fallback: &str) -> PathBuf {
    if let Some(v) = std::env::var_os(var) {
        let p = PathBuf::from(v);
        if p.is_absolute() {
            return p;
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    home.join(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_override_covers_all_three() {
        let d = Dirs::under("/tmp/lv-test");
        assert_eq!(d.cache(), Path::new("/tmp/lv-test/cache"));
        assert_eq!(d.data(), Path::new("/tmp/lv-test/data"));
        assert_eq!(d.config(), Path::new("/tmp/lv-test/config"));
    }

    #[test]
    fn gallery_key_is_stable_and_distinct() {
        let a = gallery_key(Path::new("/mnt/nas/photos"));
        let b = gallery_key(Path::new("/mnt/nas/photos"));
        let c = gallery_key(Path::new("/mnt/nas/photos2"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn relative_xdg_value_is_ignored() {
        // The spec requires it, and honouring one would scatter state wherever
        // the process was started from.
        unsafe {
            std::env::set_var("LV_TEST_XDG", "relative/path");
            std::env::set_var("HOME", "/home/someone");
        }
        assert_eq!(
            xdg("LV_TEST_XDG", ".cache"),
            PathBuf::from("/home/someone/.cache")
        );
        unsafe {
            std::env::remove_var("LV_TEST_XDG");
        }
    }
}
