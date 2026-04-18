use std::net::{IpAddr, Ipv4Addr};

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub bind: IpAddr,
    pub port: u16,
    pub auth: AuthMode,
}

#[derive(Debug, Clone)]
pub enum AuthMode {
    /// No authentication. Appropriate only for loopback binds.
    None,
    /// Require `Authorization: Bearer <token>` or `?token=<token>`.
    BearerToken(String),
}

impl HttpConfig {
    /// Loopback-only, no auth, OS-assigned port. Used for local video
    /// playback from the desktop webview.
    pub fn local_only() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            auth: AuthMode::None,
        }
    }
}
