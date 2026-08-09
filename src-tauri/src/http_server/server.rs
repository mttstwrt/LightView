//! Router assembly, binding, and the TLS handshake.
//!
//! The layering here *is* the security model, so the grouping is worth reading
//! before adding a route. Three tiers:
//!
//! * **Protected** — media, thumbnails, `/api/*`. Behind
//!   [`super::middleware::auth_layer`].
//! * **Bootstrap** — `/pair/redeem`, `/auth/*`, `/cert`. These cannot sit
//!   behind the auth layer or there would be no way past it the first time.
//!   `/cert` is one step further out still: the point of downloading the
//!   certificate is to fix a browser that cannot establish trust yet, which is
//!   upstream of holding a cookie. It is public regardless — every TLS
//!   handshake hands the same bytes to anyone who connects.
//! * **Static** — the SPA. Outside auth because the shell must load before the
//!   client can pair; it contains no gallery data. Served from the bytes
//!   compiled into the binary ([`super::web_assets`]) unless `web_root` names
//!   a directory to use instead.
//!
//! The upload body limit is applied to `/api/upload` alone rather than
//! globally, so the other routes keep axum's small default and a hostile client
//! cannot use any of them to buffer half a gigabyte.

use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::net::TcpSocket;
use tokio::task::JoinHandle;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};

use super::api;
use super::auth_routes;
use super::config::HttpConfig;
use super::middleware as mw;
use super::routes;
use super::tls;
use super::web_assets;
use crate::AppState;

#[derive(Clone)]
pub struct ServerState {
    pub config: Arc<HttpConfig>,
    pub app: AppState,
    /// Count of requests from non-loopback (i.e. remote) peers. A non-zero
    /// value proves an external device reached the server — the only reliable
    /// signal that the port is actually allowed through the firewall.
    pub remote_hits: Arc<AtomicU64>,
}

pub struct RunningServer {
    pub addr: SocketAddr,
    #[allow(dead_code)]
    pub handle: JoinHandle<()>,
    pub remote_hits: Arc<AtomicU64>,
}

/// A running remote-access server, retained in `AppState` so it can be
/// queried for status and aborted when remote access is disabled. The
/// per-device cookies and any optional gallery password live in the open
/// gallery's cache.db — this struct only tracks the running server.
pub struct RemoteAccess {
    pub addr: SocketAddr,
    pub handle: JoinHandle<()>,
    pub remote_hits: Arc<AtomicU64>,
    /// Pre-computed firewall guidance for this port, if a firewall is active.
    pub firewall_hint: Option<String>,
}

/// Bind a TCP listener and spawn the axum server. Returns the actual bound
/// address (port is OS-assigned when `config.port` is 0).
pub async fn start(config: HttpConfig, app: AppState) -> std::io::Result<RunningServer> {
    let requested = SocketAddr::new(config.bind, config.port);
    let socket = if requested.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    // Permit immediate rebind to a fixed port after the previous server is
    // stopped, instead of failing with "address in use" while the old socket
    // lingers in TIME_WAIT.
    socket.set_reuseaddr(true)?;
    socket.bind(requested)?;
    let listener = socket.listen(1024)?;
    let addr = listener.local_addr()?;

    log::info!(
        "HTTP media server listening on {}://{}",
        if config.tls { "https" } else { "http" },
        addr
    );

    let use_tls = config.tls;
    // Captured before `config` moves into the state, which the router consumes.
    let tls_sans = config.tls_sans.clone();
    let remote_hits = Arc::new(AtomicU64::new(0));
    let state = ServerState {
        config: Arc::new(config),
        app,
        remote_hits: remote_hits.clone(),
    };

    // Permissive CORS. This comment used to say "loopback-only today, narrow it
    // when remote access lands" — remote access has landed, so here is the
    // actual reasoning.
    //
    // Narrowing buys nothing on either instance. The loopback media server is
    // fetched cross-origin by the webview itself, which is the whole point of
    // its existence. The LAN server's protection is the `lv_device` cookie,
    // and a cookie the browser will not send cross-site is not made safer by an
    // origin allowlist; a same-origin SPA is unaffected either way. Meanwhile
    // there is no fixed origin to name, since the server is reached by whatever
    // LAN address or hostname the client happened to dial.
    //
    // What is load-bearing is `expose_headers`: the webview reads the
    // `X-Gif-*` atlas metadata off the fetch Response, and custom response
    // headers are invisible to JS cross-origin unless they are exposed.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    // Cap an upload request body. Generous enough for a batch of phone photos
    // or a short video, bounded so a hostile client can't exhaust memory/disk
    // in a single request (each file is buffered while being written).
    const MAX_UPLOAD_BYTES: usize = 512 * 1024 * 1024;

    let protected = Router::new()
        .route("/media/{*path}", get(routes::media))
        .route("/thumb/{tier}/{*path}", get(routes::thumb))
        .route("/gif-atlas/{tier}/{*path}", get(routes::gif_atlas))
        .route("/thumbhash/{*path}", get(routes::thumbhash))
        .route("/api/invoke", post(api::invoke))
        .route("/api/events", get(routes::events))
        // The body limit applies only to the upload route; the other data
        // routes keep axum's small default.
        .route(
            "/api/upload",
            post(routes::upload).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .layer(middleware::from_fn_with_state(state.clone(), mw::auth_layer));

    // Bootstrap endpoints: pair a device, prove the gallery password, or
    // query auth state. These can't sit behind the auth layer or there's no
    // way to get past it the first time.
    // `/cert` belongs here too: the whole point of downloading the certificate
    // is to fix a browser that can't establish trust, which is upstream of
    // having a device cookie at all.
    let bootstrap = Router::new()
        .route("/cert", get(routes::cert))
        .route("/pair/redeem", post(auth_routes::redeem))
        .route("/auth/password", post(auth_routes::submit_password))
        .route("/auth/status", get(auth_routes::status));

    let mut router = Router::new()
        .route("/healthz", get(routes::healthz))
        .merge(protected)
        .merge(bootstrap);

    if state.config.serve_spa {
        // Wrap the static file service in its own router so the cache-policy
        // middleware applies only to SPA assets — not to media/thumb/api,
        // which have their own caching needs. See `mw::static_cache_control`.
        // Both branches sit behind it so the embedded and on-disk paths cannot
        // drift in their caching.
        let static_files = Router::new()
            .fallback_service(match state.config.web_root.clone() {
                Some(web_root) => {
                    let index = web_root.join("index.html");
                    log::info!("Serving web app from {}", web_root.display());
                    Router::new()
                        .fallback_service(ServeDir::new(&web_root).fallback(ServeFile::new(index)))
                }
                None => Router::new().fallback(web_assets::serve),
            })
            .layer(middleware::from_fn(mw::static_cache_control));
        router = router.fallback_service(static_files);
    }

    let router = router
        // Hold every route (except /healthz) behind a 503 "starting" page
        // until a gallery is open — the headless server binds before the
        // gallery scan finishes.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            mw::gallery_gate,
        ))
        // Outermost layer: count remote peers across every route (including
        // unauthenticated static assets — the first thing a browser fetches).
        .layer(middleware::from_fn_with_state(
            state.clone(),
            mw::track_remote_hits,
        ))
        .layer(cors)
        .with_state(state);

    // Serve with peer-address info so the tracking middleware can tell remote
    // clients from loopback ones.
    let make_service = router.into_make_service_with_connect_info::<SocketAddr>();
    let handle = if use_tls {
        // Browsers only grant secure-context APIs over HTTPS, so the remote
        // server terminates TLS with a persisted self-signed cert (tls.rs).
        let material = tls::load_or_generate(&tls_sans).map_err(std::io::Error::other)?;
        let rustls_config =
            axum_server::tls_rustls::RustlsConfig::from_pem(material.cert_pem, material.key_pem)
                .await?;
        let std_listener = listener.into_std()?;
        tokio::spawn(async move {
            if let Err(e) =
                axum_server::from_tcp_rustls(std_listener, rustls_config).serve(make_service).await
            {
                log::error!("HTTPS media server exited with error: {}", e);
            }
        })
    } else {
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, make_service).await {
                log::error!("HTTP media server exited with error: {}", e);
            }
        })
    };

    Ok(RunningServer {
        addr,
        handle,
        remote_hits,
    })
}
