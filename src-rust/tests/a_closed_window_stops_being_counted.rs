//! The one assumption behind "a local session ends with its last window":
//! that a browser going away actually drops the SSE stream, and promptly.
//!
//! Everything else about the policy — the grace period, the arming, a reload
//! not counting as a close, durable work outlasting the window — is decided by
//! pure logic and tested with a paused clock in `util::presence`. This is the
//! part no unit test can reach: whether hyper drops the response body when the
//! peer disappears, which is the only thing that makes the count fall. If it
//! does not, the feature is inert and nothing else would say so.
//!
//! So this drives a **real socket**: a real listener, a real request, and a
//! connection closed the way a closed tab closes one — abruptly, with no
//! shutdown handshake.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tower::ServiceExt;

use lightview::autocomplete::engine::AutocompleteEngine;
use lightview::cache::db::CacheDb;
use lightview::path::Root;
use lightview::pipeline::serve::ThumbService;
use lightview::server::auth::{LaunchSession, Trust, SESSION_COOKIE};
use lightview::server::config::ServerConfig;
use lightview::server::events::Events;
use lightview::services::settings::GallerySettings;
use lightview::state::{AppState, Gallery};
use lightview::util::paths::Dirs;

/// Generous: the keep-alive interval is fifteen seconds, and a connection with
/// no traffic is noticed on the next write into it. Failing this means the
/// stream is never dropped at all, not that it was slow.
const NOTICE_WITHIN: Duration = Duration::from_secs(60);

struct Fixture {
    state: Arc<AppState>,
    _gallery_dir: tempfile::TempDir,
    _state_dir: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let gallery_dir = tempfile::tempdir().unwrap();
    std::fs::write(gallery_dir.path().join("a.jpg"), b"jpeg").unwrap();
    let state_dir = tempfile::tempdir().unwrap();

    let dirs = Dirs::under(state_dir.path());
    dirs.ensure().unwrap();
    let root = Root::open(gallery_dir.path()).unwrap();
    let db = Arc::new(CacheDb::open_at(&dirs.gallery_cache(root.as_path())).unwrap());
    let pool = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap());
    let thumbs = Arc::new(ThumbService::new(db.clone(), root.clone(), pool, 1 << 20));

    let gallery = Arc::new(Gallery {
        root,
        db,
        thumbs,
        events: Arc::new(Events::new()),
        autocomplete: Arc::new(AutocompleteEngine::new()),
        settings: std::sync::RwLock::new(GallerySettings::default()),
        cache_dir: dirs.cache().to_path_buf(),
    });
    lightview::services::gallery::scan_and_index(&gallery).await.unwrap();

    let mut state = AppState::new(gallery, Trust::Owner, dirs, ServerConfig::default());
    state.launch = Some(Arc::new(LaunchSession::new()));
    state.origin = "http://127.0.0.1".to_string();
    state.mark_ready();

    Fixture {
        state: Arc::new(state),
        _gallery_dir: gallery_dir,
        _state_dir: state_dir,
    }
}

/// Redeem the launch token the way a browser does, for a session cookie.
async fn session_cookie(state: &Arc<AppState>) -> String {
    let token = state.launch.as_ref().unwrap().token();
    let request = Request::builder()
        .method("POST")
        .uri("/auth/launch")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ORIGIN, &state.origin)
        .body(Body::from(serde_json::json!({ "token": token }).to_string()))
        .unwrap();
    let response = lightview::server::routes::router(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let set = response
        .headers()
        .get(header::SET_COOKIE)
        .expect("a session cookie")
        .to_str()
        .unwrap()
        .to_string();
    let _ = response.into_body().collect().await;
    set.split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_connection_stops_counting_as_an_open_window() {
    let f = fixture().await;
    let cookie = session_cookie(&f.state).await;
    let presence = f.state.presence.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = lightview::server::routes::router(f.state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    assert_eq!(presence.windows(), 0, "nothing is open yet");
    assert!(!presence.seen_any(), "and nothing has been");

    // A browser's own request, byte for byte as far as the server can tell.
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(
            format!(
                "GET /api/events HTTP/1.1\r\nHost: {addr}\r\n\
                 Accept: text/event-stream\r\nCookie: {SESSION_COOKIE}={cookie}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    // Read past the status line so the handler has certainly run.
    let mut reader = BufReader::new(&mut socket);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("HTTP/1.1 200"), "got: {line:?}");

    assert_eq!(presence.windows(), 1, "an open stream is an open window");
    assert!(presence.seen_any(), "the watchdog is now armed");

    // What a closed tab does: the socket goes, with no goodbye.
    drop(socket);

    let deadline = Instant::now() + NOTICE_WITHIN;
    while presence.windows() > 0 {
        assert!(
            Instant::now() < deadline,
            "the stream was never dropped, so the count never falls and a local \
             session would never end"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    assert_eq!(presence.windows(), 0);
    assert!(
        presence.seen_any(),
        "having been open is remembered, or a reconnect would disarm the watchdog"
    );
    server.abort();
}
