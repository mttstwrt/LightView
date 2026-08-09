//! Worker configuration, persisted at `<exe_dir>/data/worker.toml` — the same
//! portable exe-relative layout the plugins directory uses, so a copied worker
//! directory is self-contained.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// Base URL of the LightView server, no trailing slash (e.g.
    /// `https://192.168.1.10:8787`).
    pub server_url: String,
    /// The `<cookie-name>=<id>.<secret>` cookie pair obtained at pairing. The
    /// name is the server's per-gallery `lv_device_<suffix>` (older servers
    /// issue a bare `lv_device`), stored verbatim and replayed as-is.
    pub cookie: String,
    /// Hex SHA-256 of the server's TLS certificate (TOFU pin). The server cert
    /// is self-signed, so this — not a CA — is what authenticates the server.
    pub cert_sha256: String,
    /// Stable identity this worker announces/claims with (uuid, minted at pair).
    pub worker_id: String,
    pub worker_name: String,
    /// Longest edge for `?fit=` media downloads; 0 = full-resolution bytes.
    #[serde(default = "default_fit_edge")]
    pub fit_edge: u32,
    /// Idle claim-poll interval in seconds.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
}

fn default_fit_edge() -> u32 {
    1024
}

fn default_poll_secs() -> u64 {
    3
}

pub fn config_path() -> PathBuf {
    lightview_lib::util::paths::data_dir().join("worker.toml")
}

pub fn load() -> Result<WorkerConfig, String> {
    let path = config_path();
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {} ({e}) — run `lightview-worker pair` first", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("invalid config {}: {e}", path.display()))
}

pub fn save(config: &WorkerConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = toml::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    // The file holds the device cookie — keep it private to the user.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}
