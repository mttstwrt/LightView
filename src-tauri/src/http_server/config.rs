use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub bind: IpAddr,
    pub port: u16,
    pub auth: AuthMode,
    /// Directory of the built SPA to serve at `/`. `None` disables static
    /// serving (the desktop webview loads the app from Tauri, not over HTTP).
    pub web_root: Option<PathBuf>,
    /// Serve HTTPS with the persisted self-signed certificate (see `tls.rs`).
    /// Required for non-loopback binds: browsers only grant secure-context
    /// APIs (async clipboard, etc.) over HTTPS. Loopback serving stays plain
    /// HTTP — the desktop webview won't accept a self-signed cert, and
    /// 127.0.0.1 is already a secure context.
    pub tls: bool,
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
            web_root: None,
            tls: false,
        }
    }

    /// LAN-accessible, per-device cookie auth, serving the SPA from `web_root`.
    /// `port` of 0 lets the OS assign one.
    pub fn remote(port: u16, web_root: Option<PathBuf>) -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port,
            auth: AuthMode::DeviceCookie,
            web_root,
            tls: true,
        }
    }
}
