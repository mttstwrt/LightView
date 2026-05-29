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
}

#[derive(Debug, Clone)]
pub enum AuthMode {
    /// No authentication. Appropriate only for loopback binds.
    None,
    /// Require `Authorization: Bearer <token>` or `?token=<token>`.
    BearerToken(String),
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
        }
    }

    /// LAN-accessible, bearer-token auth, serving the SPA from `web_root`.
    /// `port` of 0 lets the OS assign one.
    pub fn remote(token: String, port: u16, web_root: Option<PathBuf>) -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port,
            auth: AuthMode::BearerToken(token),
            web_root,
        }
    }
}
