//! Step 4's acceptance, driven against the real router.
//!
//! The `Router` is a tower `Service`, so `oneshot` exercises genuine routing,
//! middleware, extractors and responses without a socket. What that cannot
//! cover — TLS, the actual bind, a browser — is step 5's and step 6's job.
//!
//! The four things the design says must be true here:
//!
//! - `curl` reaches every route.
//! - An unauthenticated call is 401.
//! - An `Owner` command on a non-loopback bind is refused.
//! - **On a loopback bind, redeeming a launch token and then calling the
//!   directory-listing endpoint succeeds** — the `Owner` half of the trust
//!   table is the least-exercised surface in the system, because a browser
//!   recipe that pairs a device and drives `--serve` verifies only that `Owner`
//!   is *refused*.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use lightview::autocomplete::engine::AutocompleteEngine;
use lightview::cache::db::CacheDb;
use lightview::path::Root;
use lightview::pipeline::serve::ThumbService;
use lightview::server::auth::{LaunchSession, Trust, SESSION_COOKIE};
use lightview::server::config::ServerConfig;
use lightview::server::devices::{self, Devices, PairingKind, DEVICE_COOKIE};
use lightview::server::events::Events;
use lightview::services::settings::GallerySettings;
use lightview::state::{AppState, Gallery};
use lightview::util::paths::Dirs;

struct Harness {
    router: axum::Router,
    _gallery_dir: tempfile::TempDir,
    _state_dir: tempfile::TempDir,
    state: Arc<AppState>,
}

async fn harness(trust: Trust, password: Option<&str>) -> Harness {
    let gallery_dir = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(gallery_dir.path().join("2026")).unwrap();

    let mut img = image::RgbImage::new(64, 48);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgb([(x * 4) as u8, (y * 4) as u8, 128]);
    }
    img.save(gallery_dir.path().join("2026/a.png")).unwrap();

    let dirs = Dirs::under(state_dir.path());
    dirs.ensure().unwrap();

    let root = Root::open(gallery_dir.path()).unwrap();
    let db = Arc::new(CacheDb::open_at(&dirs.gallery_cache(root.as_path())).unwrap());
    let pool = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(2).build().unwrap());
    let thumbs = Arc::new(ThumbService::new(db.clone(), root.clone(), pool, 1 << 24));

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

    let config = ServerConfig {
        password_hash: password
            .map(|p| lightview::server::auth::hash_password(p).unwrap())
            .unwrap_or_default(),
        ..Default::default()
    };

    let mut state = AppState::new(gallery, trust, dirs.clone(), config);
    match trust {
        Trust::Owner => {
            state.launch = Some(Arc::new(LaunchSession::new()));
            state.origin = "http://127.42.7.9:54321".to_string();
        }
        Trust::Device => {
            state.devices = Some(Arc::new(Devices::open(&dirs.devices_db()).unwrap()));
        }
    }
    state.mark_ready();

    let state = Arc::new(state);
    Harness {
        router: lightview::server::routes::router(state.clone()),
        _gallery_dir: gallery_dir,
        _state_dir: state_dir,
        state,
    }
}

impl Harness {
    async fn send(&self, request: Request<Body>) -> (StatusCode, String) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).to_string())
    }

    async fn send_full(&self, request: Request<Body>) -> axum::response::Response {
        self.router.clone().oneshot(request).await.unwrap()
    }

    fn get(&self, uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    fn invoke(&self, command: &str, args: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/invoke")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "command": command, "args": args }).to_string(),
            ))
            .unwrap()
    }

    fn with_cookie(mut request: Request<Body>, name: &str, value: &str) -> Request<Body> {
        request.headers_mut().insert(
            header::COOKIE,
            header::HeaderValue::from_str(&format!("{name}={value}")).unwrap(),
        );
        request
    }

    /// Redeem the launch token the way a browser does.
    async fn redeem_launch(&self) -> String {
        let token = self.state.launch.as_ref().unwrap().token();
        let request = Request::builder()
            .method("POST")
            .uri("/auth/launch")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ORIGIN, &self.state.origin)
            .body(Body::from(
                serde_json::json!({ "token": token }).to_string(),
            ))
            .unwrap();
        let response = self.send_full(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let set = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        set.split(';')
            .next()
            .unwrap()
            .split_once('=')
            .unwrap()
            .1
            .to_string()
    }

    /// Pair a device the way a phone does.
    async fn pair(&self) -> String {
        let devices = self.state.devices.as_ref().unwrap();
        let code = {
            let conn = devices.writer().await;
            devices::create_pairing(&conn, PairingKind::Token).unwrap()
        };
        let request = Request::builder()
            .method("POST")
            .uri("/pair/redeem")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "code": code, "name": "phone" }).to_string(),
            ))
            .unwrap();
        let response = self.send_full(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let set = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        set.split(';')
            .next()
            .unwrap()
            .split_once('=')
            .unwrap()
            .1
            .to_string()
    }
}

// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bootstrap_routes_answer_without_any_credential() {
    // Unauthenticated by necessity: there would otherwise be no way past the
    // auth layer the first time. A route group, not a trust level.
    let h = harness(Trust::Device, None).await;

    let (status, body) = h.send(h.get("/healthz")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");

    let (status, body) = h.send(h.get("/auth/status")).await;
    assert_eq!(status, StatusCode::OK);
    let status_json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status_json["trust"], "device");
    assert_eq!(status_json["pairing"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unauthenticated_call_is_401() {
    let h = harness(Trust::Device, None).await;
    for request in [
        h.invoke("get_items", serde_json::json!({})),
        h.get("/thumb/j/2026/a.png"),
        h.get("/media/2026/a.png"),
        h.get("/api/events"),
    ] {
        let (status, _) = h.send(request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_loopback_401_says_the_session_ended_and_offers_no_pairing() {
    // The dead end, by design: there is no pairing flow on loopback, and a
    // browser cannot read `instance.json` to find the new URL — that is exactly
    // the filesystem access the trust model exists to withhold.
    let h = harness(Trust::Owner, None).await;
    let (status, body) = h.send(h.invoke("get_items", serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("session has ended"), "body was: {body}");
    assert!(body.contains("new tab"), "body was: {body}");

    let (_, status_body) = h.send(h.get("/auth/status")).await;
    let status_json: serde_json::Value = serde_json::from_str(&status_body).unwrap();
    assert_eq!(status_json["pairing"], false, "loopback offered pairing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owner_command_is_refused_on_a_served_bind() {
    // The whole security rule, from the outside.
    let h = harness(Trust::Device, None).await;
    let cookie = h.pair().await;

    for command in [
        "list_dirs",
        "copy_files",
        "move_files",
        "clipboard_files",
        "open_with",
        "purge_trash",
        "merge_duplicates",
        "get_recent_galleries",
    ] {
        let request = Harness::with_cookie(
            h.invoke(command, serde_json::json!({ "path": "/tmp" })),
            DEVICE_COOKIE,
            &cookie,
        );
        let (status, _) = h.send(request).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{command} was reachable from a served bind"
        );
    }

    // And a `Device` command on the same connection works, so the refusals
    // above are about trust rather than about a broken session.
    let request = Harness::with_cookie(
        h.invoke("get_items", serde_json::json!({})),
        DEVICE_COOKIE,
        &cookie,
    );
    let (status, _) = h.send(request).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redeeming_a_launch_token_unlocks_the_owner_half() {
    // The least-reviewed surface in the design: a browser recipe that pairs a
    // device and drives `--serve` verifies only that `Owner` is *refused*.
    let h = harness(Trust::Owner, None).await;
    let session = h.redeem_launch().await;

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("Photos")).unwrap();

    let request = Harness::with_cookie(
        h.invoke(
            "list_dirs",
            serde_json::json!({ "path": dir.path().to_string_lossy() }),
        ),
        SESSION_COOKIE,
        &session,
    );
    let (status, body) = h.send(request).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("Photos"), "body: {body}");

    // And the dedicated picker route, which dispatches through the same table.
    let mut request = h.get(&format!(
        "/api/dirs?path={}",
        urlencoding(&dir.path().to_string_lossy())
    ));
    request.headers_mut().insert(
        header::COOKIE,
        header::HeaderValue::from_str(&format!("{SESSION_COOKIE}={session}")).unwrap(),
    );
    let (status, body) = h.send(request).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("Photos"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_launch_token_is_single_use() {
    let h = harness(Trust::Owner, None).await;
    let token = h.state.launch.as_ref().unwrap().token();

    let redeem = |token: String| {
        Request::builder()
            .method("POST")
            .uri("/auth/launch")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ORIGIN, "http://127.42.7.9:54321")
            .body(Body::from(
                serde_json::json!({ "token": token }).to_string(),
            ))
            .unwrap()
    };

    let (status, _) = h.send(redeem(token.clone())).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h.send(redeem(token)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a spent token was accepted a second time"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cross_origin_post_is_refused_on_loopback() {
    // The `Origin` rule, from the outside. A *site*-level comparison would pass
    // the second case, and that is an attacker on another local port.
    let h = harness(Trust::Owner, None).await;
    let session = h.redeem_launch().await;

    let with_origin = |origin: &str| {
        let mut request = Harness::with_cookie(
            h.invoke("get_items", serde_json::json!({})),
            SESSION_COOKIE,
            &session,
        );
        request.headers_mut().insert(
            header::ORIGIN,
            header::HeaderValue::from_str(origin).unwrap(),
        );
        request
    };

    let (status, _) = h.send(with_origin("http://127.42.7.9:54321")).await;
    assert_eq!(status, StatusCode::OK);

    for hostile in [
        "http://127.0.0.1:3000",
        "http://127.42.7.9:3000",
        "https://127.42.7.9:54321",
        "http://evil.example",
    ] {
        let (status, _) = h.send(with_origin(hostile)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{hostile} was accepted");
    }

    // No Origin at all is allowed: `curl` does not send one, and requiring it
    // would break every non-browser client for no gain.
    let request = Harness::with_cookie(
        h.invoke("get_items", serde_json::json!({})),
        SESSION_COOKIE,
        &session,
    );
    let (status, _) = h.send(request).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paired_device_reaches_the_grid_and_the_bytes() {
    let h = harness(Trust::Device, None).await;
    let cookie = h.pair().await;

    let request = Harness::with_cookie(
        h.invoke("get_items", serde_json::json!({})),
        DEVICE_COOKIE,
        &cookie,
    );
    let (status, body) = h.send(request).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("2026/a.png"), "body: {body}");

    // A thumbnail at every rung, through the real route.
    for tier in ["js", "j", "jm", "jh"] {
        let request = Harness::with_cookie(
            h.get(&format!("/thumb/{tier}/2026/a.png")),
            DEVICE_COOKIE,
            &cookie,
        );
        let response = h.send_full(request).await;
        assert_eq!(response.status(), StatusCode::OK, "tier {tier}");
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/webp"
        );
        assert!(response.headers().contains_key(header::ETAG), "tier {tier}");
    }

    // And the original, with range support the viewer depends on.
    let request = Harness::with_cookie(
        h.get("/media/2026/a.png"),
        DEVICE_COOKIE,
        &cookie,
    );
    let response = h.send_full(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::ACCEPT_RANGES).unwrap(),
        "bytes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_request_gets_a_real_206() {
    // Without this, `<video>` scrubbing does not work in any browser, and the
    // ported viewer assumes it does.
    let h = harness(Trust::Device, None).await;
    let cookie = h.pair().await;

    let mut request = Harness::with_cookie(
        h.get("/media/2026/a.png"),
        DEVICE_COOKIE,
        &cookie,
    );
    request
        .headers_mut()
        .insert(header::RANGE, header::HeaderValue::from_static("bytes=0-9"));

    let response = h.send_full(request).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(header::CONTENT_LENGTH).unwrap(),
        "10"
    );
    assert!(response
        .headers()
        .get(header::CONTENT_RANGE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("bytes 0-9/"));

    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_traversal_in_a_path_capture_is_404_not_a_file() {
    let h = harness(Trust::Device, None).await;
    let cookie = h.pair().await;

    for hostile in [
        "/media/..%2F..%2Fetc%2Fpasswd",
        "/thumb/j/..%2F..%2Fetc%2Fpasswd",
        "/media/2026/../../../../etc/passwd",
    ] {
        let request = Harness::with_cookie(h.get(hostile), DEVICE_COOKIE, &cookie);
        let (status, body) = h.send(request).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{hostile} → {body}");
        assert!(!body.contains("root:"), "{hostile} served /etc/passwd");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_password_challenge_fires_only_past_the_inactivity_window() {
    let h = harness(Trust::Device, Some("hunter2")).await;
    let cookie = h.pair().await;

    // A freshly paired device has just authenticated, so it is not challenged.
    let request = Harness::with_cookie(
        h.invoke("get_items", serde_json::json!({})),
        DEVICE_COOKIE,
        &cookie,
    );
    let (status, _) = h.send(request).await;
    assert_eq!(status, StatusCode::OK);

    // Age it past the window.
    {
        let devices = h.state.devices.as_ref().unwrap();
        let conn = devices.writer().await;
        conn.execute("UPDATE devices SET last_auth_at = 0", []).unwrap();
    }
    let request = Harness::with_cookie(
        h.invoke("get_items", serde_json::json!({})),
        DEVICE_COOKIE,
        &cookie,
    );
    let response = h.send_full(request).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // The header is what `ipc.ts` distinguishes a challenge from a revoked
    // pairing by; without it the client would redirect to pairing instead.
    assert_eq!(
        response.headers().get(header::WWW_AUTHENTICATE).unwrap(),
        "LV-Password"
    );

    // Clearing it restores access; a wrong password does not.
    let wrong = Harness::with_cookie(
        Request::builder()
            .method("POST")
            .uri("/auth/password")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "password": "nope" }).to_string(),
            ))
            .unwrap(),
        DEVICE_COOKIE,
        &cookie,
    );
    let (status, _) = h.send(wrong).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let right = Harness::with_cookie(
        Request::builder()
            .method("POST")
            .uri("/auth/password")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "password": "hunter2" }).to_string(),
            ))
            .unwrap(),
        DEVICE_COOKIE,
        &cookie,
    );
    let (status, _) = h.send(right).await;
    assert_eq!(status, StatusCode::OK);

    let request = Harness::with_cookie(
        h.invoke("get_items", serde_json::json!({})),
        DEVICE_COOKIE,
        &cookie,
    );
    let (status, _) = h.send(request).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_route_answers_503_until_the_watcher_is_armed() {
    // A file arriving between "the gallery is set" and "the watcher is
    // listening" is in neither the completed scan nor the watcher.
    let gallery_dir = tempfile::tempdir().unwrap();
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

    let mut state = AppState::new(gallery, Trust::Device, dirs.clone(), ServerConfig::default());
    state.devices = Some(Arc::new(Devices::open(&dirs.devices_db()).unwrap()));
    // Deliberately *not* ready.
    let state = Arc::new(state);
    let router = lightview::server::routes::router(state.clone());

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/thumb/j/a.png")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    // The bootstrap group is outside the gate, so a client can still discover
    // what it is talking to while the gallery opens.
    let response = router
        .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capabilities_tell_a_client_what_it_may_not_do() {
    // The frontend keeps components that offer `Owner` actions; without this it
    // would offer them to a phone and collect refusals.
    let h = harness(Trust::Device, None).await;
    let cookie = h.pair().await;
    let request = Harness::with_cookie(
        h.invoke("get_capabilities", serde_json::json!({})),
        DEVICE_COOKIE,
        &cookie,
    );
    let (status, body) = h.send(request).await;
    assert_eq!(status, StatusCode::OK);
    let caps: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(caps["trust"], "device");
    assert_eq!(caps["clipboard"], false);
}

/// Minimal percent-encoding for a query value.
fn urlencoding(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
