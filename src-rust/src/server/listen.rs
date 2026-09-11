//! Binding the one listener, and the trust that comes with it.
//!
//! **A process has exactly one listener**, because `lightview <dir>` and
//! `lightview --serve <dir>` cannot both hold a gallery. That is what makes
//! [`crate::server::auth::Trust`] a property of the process rather than a
//! per-connection decision, and it is the strongest available form of "no flag
//! widens `Owner`": there is no second listener whose trust could be confused
//! with the first's.
//!
//! **Loopback is plain HTTP and `--serve` is always TLS.** Not a preference:
//! browsers gate the async Clipboard API and its neighbours behind a secure
//! context, and the whole of `127.0.0.0/8` is a "potentially trustworthy
//! origin" while a LAN address is not. So the local bind gets its secure
//! context for free, and the LAN bind has to mint a certificate.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use crate::server::auth::{random_loopback, Trust};
use crate::server::tls;
use crate::state::AppState;

#[derive(Debug, thiserror::Error)]
pub enum ListenError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TLS setup failed: {0}")]
    Tls(String),
}

/// A bound socket that has not started serving yet.
///
/// Binding and serving are separate so the caller can learn the URL — the
/// loopback port is ephemeral — print it, arm the watcher, and only then open
/// the gate.
pub struct Bound {
    listener: std::net::TcpListener,
    pub address: SocketAddr,
    pub trust: Trust,
    tls: bool,
}

impl Bound {
    /// The origin this server answers on, for the `Origin` check and the
    /// launch URL.
    pub fn origin(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        match self.address.ip() {
            IpAddr::V4(ip) => format!("{scheme}://{ip}:{}", self.address.port()),
            IpAddr::V6(ip) => format!("{scheme}://[{ip}]:{}", self.address.port()),
        }
    }
}

/// Bind loopback on an ephemeral port, at `Owner`.
///
/// **A random address in `127.0.0.0/8`, not `127.0.0.1`.** Cookies are not
/// port-scoped and `SameSite` is site-scoped, so a session cookie set by
/// `127.0.0.1:<port>` reaches every other local port — a dev server, a
/// notebook, a downloaded repository's `npm run dev` — any of which can replay
/// it from a non-browser client where no `Origin` is expected. A random address
/// makes the cookie's host belong to this process alone.
pub fn bind_loopback() -> Result<Bound, ListenError> {
    let address = SocketAddr::new(IpAddr::V4(random_loopback()), 0);
    let listener = std::net::TcpListener::bind(address)?;
    let address = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    Ok(Bound {
        listener,
        address,
        trust: Trust::Owner,
        tls: false,
    })
}

/// Bind the LAN interface at `Device`, over TLS.
pub fn bind_serve(bind: IpAddr, port: u16) -> Result<Bound, ListenError> {
    let listener = std::net::TcpListener::bind(SocketAddr::new(bind, port))?;
    let address = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    Ok(Bound {
        listener,
        address,
        // A `0.0.0.0` bind includes `127.0.0.1`, so this listener grants
        // `Device` to every connection regardless of who dialled in. A rule
        // written as "the peer is loopback" would hand `Owner` to any local
        // process on a served host.
        trust: Trust::Device,
        tls: true,
    })
}

/// The address to show a human, which is never `0.0.0.0`.
pub fn advertised_address(bound: &Bound) -> String {
    if bound.address.ip() == IpAddr::V4(Ipv4Addr::UNSPECIFIED) {
        let host = tls::detect_lan_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "<this machine>".to_string());
        format!("https://{host}:{}", bound.address.port())
    } else {
        bound.origin()
    }
}

/// Serve until the process ends.
pub async fn serve(bound: Bound, state: Arc<AppState>) -> Result<(), ListenError> {
    let router = crate::server::routes::router(state.clone());

    if !bound.tls {
        let listener = tokio::net::TcpListener::from_std(bound.listener)?;
        axum::serve(listener, router).await?;
        return Ok(());
    }

    let material = tls::load_or_generate(&state.dirs, &state.config.tls_sans)
        .map_err(ListenError::Tls)?;
    let config = axum_server::tls_rustls::RustlsConfig::from_pem(
        material.cert_pem.clone(),
        material.key_pem.clone(),
    )
    .await
    .map_err(|e| ListenError::Tls(e.to_string()))?;

    axum_server::from_tcp_rustls(bound.listener, config)
        .serve(router.into_make_service())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loopback_bind_is_owner_on_a_process_private_address() {
        let bound = bind_loopback().expect("bind");
        assert_eq!(bound.trust, Trust::Owner);
        assert!(!bound.tls, "loopback is plaintext, by design");
        assert_ne!(bound.address.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(bound.address.ip().is_loopback());
        assert_ne!(bound.address.port(), 0, "the OS assigned no port");
        assert!(bound.origin().starts_with("http://127."));
    }

    #[test]
    fn two_loopback_binds_do_not_share_an_address() {
        // Which is the entire point: the session cookie's host must belong to
        // one process.
        let a = bind_loopback().unwrap();
        let b = bind_loopback().unwrap();
        assert_ne!(a.address, b.address);
    }

    #[test]
    fn a_served_bind_is_device_and_always_tls() {
        let bound = bind_serve(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).expect("bind");
        assert_eq!(
            bound.trust,
            Trust::Device,
            "a served listener must never grant Owner, even bound to loopback"
        );
        assert!(bound.tls);
        assert!(bound.origin().starts_with("https://"));
    }

    #[test]
    fn the_advertised_address_is_never_the_wildcard() {
        let bound = bind_serve(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).expect("bind");
        let shown = advertised_address(&bound);
        assert!(!shown.contains("0.0.0.0"), "showed the wildcard: {shown}");
        assert!(shown.starts_with("https://"));
    }
}
