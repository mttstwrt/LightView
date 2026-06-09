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

    log::info!("HTTP media server listening on http://{}", addr);

    let remote_hits = Arc::new(AtomicU64::new(0));
    let state = ServerState {
        config: Arc::new(config),
        app,
        remote_hits: remote_hits.clone(),
    };

    // Permissive CORS — server is loopback-only today. When remote access
    // lands this should be narrowed to the origin(s) we actually serve.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        // The webview fetches this loopback server cross-origin, so custom
        // response headers (the `X-Gif-*` atlas metadata) must be exposed or
        // JS can't read them off the fetch Response.
        .expose_headers(Any);

    // Data routes carry sensitive content and sit behind the auth layer.
    // Static SPA assets do not — the app shell needs to load before the
    // client can pair its device and attach the cookie to later requests.
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
    let bootstrap = Router::new()
        .route("/pair/redeem", post(auth_routes::redeem))
        .route("/auth/password", post(auth_routes::submit_password))
        .route("/auth/status", get(auth_routes::status));

    let mut router = Router::new()
        .route("/healthz", get(routes::healthz))
        .merge(protected)
        .merge(bootstrap);

    if let Some(web_root) = state.config.web_root.clone() {
        if web_root.is_dir() {
            let index = web_root.join("index.html");
            // Wrap the static file service in its own router so the cache-policy
            // middleware applies only to SPA assets — not to media/thumb/api,
            // which have their own caching needs. See `mw::static_cache_control`.
            let static_files = Router::new()
                .fallback_service(ServeDir::new(&web_root).fallback(ServeFile::new(index)))
                .layer(middleware::from_fn(mw::static_cache_control));
            router = router.fallback_service(static_files);
            log::info!("Serving web app from {}", web_root.display());
        } else {
            log::warn!(
                "web_root {} does not exist; SPA will not be served",
                web_root.display()
            );
        }
    }

    let router = router
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
    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, make_service).await {
            log::error!("HTTP media server exited with error: {}", e);
        }
    });

    Ok(RunningServer {
        addr,
        handle,
        remote_hits,
    })
}
