use axum::{middleware, routing::get, Router};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower_http::cors::{Any, CorsLayer};

use super::config::HttpConfig;
use super::middleware as mw;
use super::routes;

#[derive(Clone)]
pub struct ServerState {
    pub config: Arc<HttpConfig>,
}

pub struct RunningServer {
    pub addr: SocketAddr,
    #[allow(dead_code)]
    pub handle: JoinHandle<()>,
}

/// Bind a TCP listener and spawn the axum server. Returns the actual bound
/// address (port is OS-assigned when `config.port` is 0).
pub async fn start(config: HttpConfig) -> std::io::Result<RunningServer> {
    let requested = SocketAddr::new(config.bind, config.port);
    let listener = TcpListener::bind(requested).await?;
    let addr = listener.local_addr()?;

    log::info!("HTTP media server listening on http://{}", addr);

    let state = ServerState {
        config: Arc::new(config),
    };

    // Permissive CORS — server is loopback-only today. When remote access
    // lands this should be narrowed to the origin(s) we actually serve.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let router = Router::new()
        .route("/healthz", get(routes::healthz))
        .route("/media/{*path}", get(routes::media))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            mw::auth_layer,
        ))
        .layer(cors)
        .with_state(state);

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            log::error!("HTTP media server exited with error: {}", e);
        }
    });

    Ok(RunningServer { addr, handle })
}
