//! Where LightView keeps machine-local state.
//!
//! Three XDG base directories, not one application directory and not the
//! exe-relative `<exe_dir>/data/` this replaces. The exe-relative layout was a
//! deliberate choice — a copied directory carried its own plugins and
//! certificates — and it is directly incompatible with being installed as an
//! ordinary package: `/usr/bin/lightview` would resolve its state to
//! `/usr/bin/data/`, root-owned and unwritable, broken on first run.
//!
//! Three directories rather than one is *fewer* things to explain, because each
//! is a standard location with an established meaning. "Which of these can I
//! safely delete?" is answered by the path. A user emptying `~/.cache`, or
//! systemd-tmpfiles sweeping it, is safe by construction rather than by a
//! warning in a document.
//!
//! ```text
//! $XDG_CACHE_HOME/lightview/galleries/<sha256-of-canonical-root>/
//!                                   cache.db    derived, disposable, budgeted
//!                                   lock        the one-writer flock
//!                                   instance.json  pid + live launch URL
//!                                   last_opened    the cross-gallery LRU key
//! $XDG_DATA_HOME/lightview/         tls/ devices.db recent.json plugins/<name>/
//! $XDG_CONFIG_HOME/lightview/       server.toml
//! ```
//!
//! Per-*gallery* durable state does not live here at all — it lives in the
//! gallery's own `.lightview/`, which is what lets a gallery move between
//! machines intact.
//!
//! [`Dirs`] is a value, constructed once in `main` and carried in application
//! state, rather than a set of free functions reading a global. That is what
//! makes two galleries with different `--data-dir` overrides coexist in one
//! test process — which section 6's two-tagging-machines test needs.

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

    /// Recently opened galleries, for the opener. Local mode only.
    pub fn recent_json(&self) -> PathBuf {
        self.data.join("recent.json")
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
