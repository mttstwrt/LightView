//! Server configuration: bind address, auth mode, TLS, static root.
//!
//! Two constructors, and they are the only two shapes that exist:
//! [`HttpConfig::local_only`] for the desktop's loopback media server and
//! [`HttpConfig::remote`] for the LAN interface. Loopback stays plain HTTP
//! because the desktop webview will not accept a self-signed certificate and
//! `127.0.0.1` already counts as a secure context; remote is always HTTPS
//! because browsers gate the async clipboard and friends behind one.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub bind: IpAddr,
    pub port: u16,
    pub auth: AuthMode,
    /// Serve the SPA at `/`. False for the desktop's loopback media server,
    /// which exists only for `<video>` Range requests — the webview loads the
    /// app from Tauri, not over HTTP.
    pub serve_spa: bool,
    /// Directory to serve the SPA from instead of the copy compiled into the
    /// binary (`http_server::web_assets`). The dev escape hatch: point it at a
    /// live Vite output and a frontend rebuild takes effect with no
    /// recompile. Ignored when `serve_spa` is false.
    pub web_root: Option<PathBuf>,
    /// Serve HTTPS with the persisted self-signed certificate (see `tls.rs`).
    /// Required for non-loopback binds: browsers only grant secure-context
    /// APIs (async clipboard, etc.) over HTTPS. Loopback serving stays plain
    /// HTTP — the desktop webview won't accept a self-signed cert, and
    /// 127.0.0.1 is already a secure context.
    pub tls: bool,
    /// Extra SANs the TLS cert must carry beyond loopback and the auto-detected
    /// LAN IP. Detection reads the interface this process routes through, which
    /// inside a container is the bridge address rather than the host address
    /// clients dial — so containerized deployments must name the reachable
    /// address here or every client fails hostname verification.
    pub tls_sans: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum AuthMode {
    /// No authentication. Appropriate only for loopback binds.
    None,
    /// Per-device cookie (`lv_device=<id>.<secret>`) backed by the
    /// `remote_devices` table in the open gallery's cache.db. Pairing happens
    /// via `/pair/redeem` (QR token or 6-digit PIN). When an optional gallery
    /// password is set, the device must re-authenticate after the configured
    /// inactivity window.
    DeviceCookie,
}

impl HttpConfig {
    /// Loopback-only, no auth, OS-assigned port, no static serving. Used for
    /// local video playback from the desktop webview.
    pub fn local_only() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            auth: AuthMode::None,
            serve_spa: false,
            web_root: None,
            tls: false,
            tls_sans: Vec::new(),
        }
    }

    /// LAN-accessible, per-device cookie auth, serving the SPA compiled into
    /// the binary. `port` of 0 lets the OS assign one. Seeds `tls_sans` from
    /// the environment so a container can declare its reachable address
    /// without a CLI flag; callers append their own with
    /// [`Self::with_tls_sans`].
    pub fn remote(port: u16) -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port,
            auth: AuthMode::DeviceCookie,
            serve_spa: true,
            web_root: None,
            tls: true,
            tls_sans: super::tls::sans_from_env(),
        }
    }

    /// Serve the SPA from this directory rather than from the embedded copy.
    pub fn with_web_root(mut self, web_root: Option<PathBuf>) -> Self {
        self.web_root = web_root;
        self
    }

    /// Append additional TLS SANs (from a CLI flag) to whatever the
    /// environment already supplied.
    pub fn with_tls_sans(mut self, sans: impl IntoIterator<Item = String>) -> Self {
        self.tls_sans.extend(sans);
        self
    }
}
