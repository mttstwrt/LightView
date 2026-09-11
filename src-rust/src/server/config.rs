//! `server.toml` — how this account serves, read at startup and on change.
//!
//! **Configuration is a file; commands are actions.** A headless deployment
//! configures itself by editing a file, which is what a headless deployment
//! expects, and it means there is no web UI that can change how the host
//! serves. Two things stay commands anyway, for reasons rather than taste:
//! `lightview pair` mints a code, which is an action; and `lightview password`
//! writes the hash below, because "configuration is a file" cannot mean asking
//! a person to hand-compute an argon2id PHC string.
//!
//! **Trash retention is deliberately not here.** It lives in the gallery's own
//! `.lightview/settings.toml`, and putting it in this file would delete files:
//! a NAS gallery served by the container at 365 days, opened once on a desktop
//! whose `server.toml` does not exist, would take the 30-day default and
//! `remove_dir_all` eleven months of the *shared* trash with no warning. See
//! [`crate::services::settings`].
//!
//! **There is no remote-delete flag.** One existed and gated the whole trash
//! group plus merge; move-to-trash is something a remote client *gets*, not
//! something it might get, so a flag could only narrow `Device` into a third
//! trust state. Two consequences worth naming rather than discovering:
//! `purge_trash` and `merge_duplicates` become unreachable remotely, and a
//! gallery can no longer be served with deletion turned off.

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Default LAN port. Overridable in the file and by `--port`, which is what
/// lets two `--serve` processes on one account coexist.
const DEFAULT_PORT: u16 = 8443;

/// How long a paired device may go without re-entering the password, when one
/// is set. Six hours, carried over.
const DEFAULT_INACTIVITY_HOURS: u64 = 6;

/// Total bytes under `galleries/` before opening a gallery evicts the coldest.
const DEFAULT_CACHE_CEILING_GB: u64 = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Bind address for `--serve`. `0.0.0.0` unless someone has a reason.
    pub bind: IpAddr,
    pub port: u16,
    /// Extra SANs the TLS certificate must carry beyond loopback and the
    /// auto-detected LAN address.
    ///
    /// Interface detection sees only the interface this process routes
    /// through — inside a container that is the bridge address, never the host
    /// address clients dial. Getting it wrong fails quietly: desktop browsers
    /// survive on a click-through exception that iOS drops readily.
    pub tls_sans: Vec<String>,
    /// argon2id PHC string, or empty for no password. Written by
    /// `lightview password`, never by hand.
    pub password_hash: String,
    /// Hours a paired device may idle before the password is challenged again.
    pub inactivity_hours: u64,
    /// Whether paired devices may upload.
    pub uploads_enabled: bool,
    /// Where uploads land, relative to the gallery root.
    pub upload_dir: String,
    /// Cross-gallery derived-cache ceiling, in gigabytes.
    pub cache_ceiling_gb: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: DEFAULT_PORT,
            tls_sans: Vec::new(),
            password_hash: String::new(),
            inactivity_hours: DEFAULT_INACTIVITY_HOURS,
            uploads_enabled: true,
            upload_dir: "Uploads".to_string(),
            cache_ceiling_gb: DEFAULT_CACHE_CEILING_GB,
        }
    }
}

impl ServerConfig {
    /// Read the file, or the defaults if it does not exist.
    ///
    /// A malformed file is an error rather than a silent fallback: serving on
    /// the wrong port, or with no password because a typo made the field
    /// unparseable, is worse than refusing to start.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| ConfigError::Parse {
                path: path.display().to_string(),
                source: e,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }

    /// Write it back, atomically. Used by `lightview password` and nothing
    /// else — every other field is edited by hand.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string_pretty(self).map_err(ConfigError::Serialize)?;
        crate::util::fs_atomic::write_durable(path, text.as_bytes())?;
        Ok(())
    }

    pub fn cache_ceiling_bytes(&self) -> u64 {
        self.cache_ceiling_gb.saturating_mul(1024 * 1024 * 1024)
    }

    pub fn inactivity_secs(&self) -> i64 {
        (self.inactivity_hours * 3600) as i64
    }

    pub fn has_password(&self) -> bool {
        !self.password_hash.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not parse {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("could not serialize configuration: {0}")]
    Serialize(#[source] toml::ser::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_the_defaults() {
        let d = tempfile::tempdir().unwrap();
        let c = ServerConfig::load(&d.path().join("nope.toml")).unwrap();
        assert_eq!(c.port, DEFAULT_PORT);
        assert!(!c.has_password());
    }

    #[test]
    fn a_partial_file_keeps_the_defaults_for_what_it_omits() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("server.toml");
        std::fs::write(&p, "port = 9000\n").unwrap();
        let c = ServerConfig::load(&p).unwrap();
        assert_eq!(c.port, 9000);
        assert_eq!(c.upload_dir, "Uploads");
        assert_eq!(c.inactivity_hours, DEFAULT_INACTIVITY_HOURS);
    }

    #[test]
    fn a_malformed_file_refuses_rather_than_falling_back() {
        // Falling back would serve with no password because of a typo.
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("server.toml");
        std::fs::write(&p, "port = \"not a number\"\n").unwrap();
        assert!(matches!(
            ServerConfig::load(&p),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn round_trips_through_the_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("server.toml");
        let mut c = ServerConfig::default();
        c.password_hash = "$argon2id$v=19$m=19456,t=2,p=1$abc$def".into();
        c.tls_sans = vec!["nas.local".into(), "192.168.1.10".into()];
        c.save(&p).unwrap();

        let back = ServerConfig::load(&p).unwrap();
        assert_eq!(back.password_hash, c.password_hash);
        assert_eq!(back.tls_sans, c.tls_sans);
        assert!(back.has_password());
    }
}
