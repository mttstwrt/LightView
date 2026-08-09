//! Headless LightView server.
//!
//! Boots the same backend the desktop app uses (`AppState` + the axum HTTP
//! server) without the Tauri webview, so a gallery can be browsed from any
//! device over the LAN. Device pairings and the optional gallery password live
//! in the gallery's own `.lightview/cache.db`, so a device paired through the
//! desktop app stays paired here — subject to the browser scoping the
//! `lv_device` cookie to the origin (host:port) it paired against.
//!
//! Usage:
//!   lightview-headless serve <gallery-path> [--port <port>] [--tls-san <host>]
//!                            [--web-root <dir>]
//!   lightview-headless pair  <gallery-path>
//!
//! `serve` opens the gallery and listens on 0.0.0.0 with per-device cookie auth,
//! serving the SPA compiled into this binary (`--web-root` overrides it with a
//! directory, for development against a live Vite build). `pair` mints a single
//! 6-digit PIN, prints it, and exits — redeem it at
//! `https://<host>:<port>/pair` on the device.
//!
//! `--tls-san` names an address the cert must cover that this process can't
//! detect on its own. In Docker that's mandatory: detection sees the container's
//! bridge IP, so without it every client fails hostname verification and lands
//! on a fragile click-through exception (which a standalone PWA cannot even
//! re-accept). See `http_server::tls`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lightview_lib::AppState;
use lightview_lib::cache::db::CacheDb;
use lightview_lib::commands::gallery::{close_gallery_impl, open_gallery_impl};
use lightview_lib::http_server::devices::{self, PairingKind};
use lightview_lib::http_server::{self, HttpConfig};

/// Default LAN port. Fixed (not OS-assigned) so the origin stays stable across
/// restarts — browsers scope the `lv_device` pairing cookie to host:port, so a
/// changing port would force every device to re-pair.
const DEFAULT_PORT: u16 = 8787;

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("serve") => match parse_serve(&args[2..]) {
            Ok(a) => serve(a).await,
            Err(e) => {
                eprintln!("error: {e}\n");
                usage();
                ExitCode::from(2)
            }
        },
        Some("pair") => match args.get(2) {
            Some(path) => pair(Path::new(path)),
            None => {
                eprintln!("error: `pair` requires a gallery path\n");
                usage();
                ExitCode::from(2)
            }
        },
        Some("-h") | Some("--help") | Some("help") => {
            usage();
            ExitCode::SUCCESS
        }
        _ => {
            usage();
            ExitCode::from(2)
        }
    }
}

/// Open the gallery and serve it over the LAN until the server task ends.
async fn serve(args: ServeArgs) -> ExitCode {
    let ServeArgs { path, port, tls_sans, web_root } = args;
    let path = match std::fs::canonicalize(&path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot resolve gallery path {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    if !path.is_dir() {
        eprintln!("error: not a directory: {}", path.display());
        return ExitCode::FAILURE;
    }
    let path_str = path.to_string_lossy().to_string();

    let state = AppState::new();

    if let Some(root) = &web_root
        && !root.is_dir()
    {
        eprintln!("error: --web-root {} is not a directory", root.display());
        return ExitCode::FAILURE;
    }

    // Bind the listener *before* the gallery scan, which can take minutes on
    // a large library. Until the gallery finishes opening, the gallery gate
    // in the HTTP layer answers every request with a 503 "starting" page —
    // a sign of life instead of a refused connection.
    let config = HttpConfig::remote(port)
        .with_web_root(web_root)
        .with_tls_sans(tls_sans);
    let server = match http_server::start(config, state.clone()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to start server on port {port}: {e}");
            return ExitCode::FAILURE;
        }
    };
    log::info!(
        "LightView headless listening on https://0.0.0.0:{} (self-signed cert) — \
         serving a 503 \"starting\" page until the gallery finishes opening",
        server.addr.port()
    );

    log::info!("Opening gallery: {path_str}");
    match open_gallery_impl(&state, path_str.clone(), || {
        log::info!("Background tag indexing complete");
    })
    .await
    {
        Ok(info) => log::info!("Gallery opened: {} media files — serving", info.total_media),
        Err(e) => {
            eprintln!("error: failed to open gallery: {e}");
            return ExitCode::FAILURE;
        }
    }

    // Watch the gallery for files added/removed on disk (including device
    // uploads) so connected web clients live-update. No Tauri handle here —
    // the watcher relays solely through the broadcast the SSE route subscribes
    // to. The desktop app starts this from its `open_gallery` command instead.
    lightview_lib::commands::gallery::start_fs_watcher(None, &state, &path_str);

    // Serve tagging jobs with the server's own plugins (if any are installed
    // under data/plugins) alongside any remote lightview-worker.
    lightview_lib::tagging::local::ensure_started(&state);

    log::info!("Pair a device with:  lightview-headless pair {path_str}");

    // Park until either the server task ends (it only does so on error) or the
    // container/user asks us to stop. `docker stop` sends SIGTERM and then
    // SIGKILLs after its grace period, so the handler has to be quick — which
    // `close_gallery_impl` is: it flushes the buffered tier-access marks and
    // checkpoints, but deliberately does not evict.
    let exit = tokio::select! {
        joined = server.handle => match joined {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                log::error!("server task ended unexpectedly: {e}");
                ExitCode::FAILURE
            }
        },
        signal = shutdown_signal() => {
            log::info!("{signal} received — closing gallery");
            ExitCode::SUCCESS
        }
    };

    if let Err(e) = close_gallery_impl(&state).await {
        log::warn!("close on shutdown failed: {e}");
    }
    exit
}

/// Resolve when the process is asked to stop, naming whichever signal arrived.
///
/// SIGTERM is the one that matters — it is what `docker stop` and systemd send.
/// SIGINT is here so a foreground run in a terminal takes the same path rather
/// than dying with the marks unflushed.
async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("cannot listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
                unreachable!()
            }
        };
        tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = tokio::signal::ctrl_c() => "SIGINT",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
}

/// Mint a one-time pairing PIN for the gallery and print it. The PIN is stored
/// in the gallery's `.lightview/cache.db`, so the running `serve` process (which
/// shares that db) will accept it at `/pair/redeem`.
fn pair(path: &Path) -> ExitCode {
    // Refuse to invent a gallery: opening a path with no cache would silently
    // create a fresh cache.db and mint a PIN the running server never sees
    // (its migration log lines are the tell). The path must be exactly what
    // the server is serving — its startup log says `Opening gallery: <path>`
    // — and the server must have opened it at least once.
    let db_file = path.join(".lightview").join("cache.db");
    if !db_file.is_file() {
        eprintln!("error: no gallery cache at {}", db_file.display());
        eprintln!(
            "`pair` must be given the same path the running server is serving \
             (check its `Opening gallery:` log line — inside Docker that's the \
             container-side path, e.g. /gallery), and the server must have been \
             started against it at least once."
        );
        return ExitCode::FAILURE;
    }
    let db = match CacheDb::open(path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: cannot open gallery cache at {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    match devices::create_pairing(db.conn(), PairingKind::Pin) {
        Ok(pin) => {
            println!("Pairing PIN: {pin}");
            println!(
                "Single-use, expires in {} minutes. Redeem it at \
                 https://<host>:<port>/pair on the device.",
                devices::PAIRING_TTL_SECS / 60
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: failed to create pairing code: {e}");
            ExitCode::FAILURE
        }
    }
}

struct ServeArgs {
    path: PathBuf,
    port: u16,
    tls_sans: Vec<String>,
    /// Serve the SPA from this directory instead of the copy compiled into
    /// the binary.
    web_root: Option<PathBuf>,
}

/// Parse `serve` args: a single positional gallery path plus optional
/// `--port`/`-p`, `--web-root`, and repeatable `--tls-san`.
fn parse_serve(args: &[String]) -> Result<ServeArgs, String> {
    let mut path: Option<PathBuf> = None;
    let mut port: u16 = DEFAULT_PORT;
    let mut tls_sans: Vec<String> = Vec::new();
    let mut web_root: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" | "-p" => {
                let v = args.get(i + 1).ok_or("--port requires a value")?;
                port = v.parse().map_err(|_| format!("invalid port: {v}"))?;
                i += 2;
            }
            "--web-root" => {
                let v = args.get(i + 1).ok_or("--web-root requires a value")?;
                web_root = Some(PathBuf::from(v));
                i += 2;
            }
            "--tls-san" => {
                let v = args.get(i + 1).ok_or("--tls-san requires a value")?;
                let parsed = lightview_lib::http_server::tls::split_sans(v);
                if parsed.is_empty() {
                    return Err("--tls-san requires a non-empty value".to_string());
                }
                tls_sans.extend(parsed);
                i += 2;
            }
            other if other.starts_with('-') => return Err(format!("unknown flag: {other}")),
            other => {
                if path.is_some() {
                    return Err(format!("unexpected extra argument: {other}"));
                }
                path = Some(PathBuf::from(other));
                i += 1;
            }
        }
    }
    Ok(ServeArgs {
        path: path.ok_or("`serve` requires a gallery path")?,
        port,
        tls_sans,
        web_root,
    })
}

fn usage() {
    eprintln!(
        "LightView headless server\n\n\
         USAGE:\n  \
         lightview-headless serve <gallery-path> [--port <port>] [--tls-san <host>]\n                              \
         [--web-root <dir>]\n  \
         lightview-headless pair  <gallery-path>\n\n\
         COMMANDS:\n  \
         serve   Open a gallery and serve it over the LAN (default port {DEFAULT_PORT}).\n  \
         pair    Mint a one-time 6-digit pairing PIN for the gallery and exit.\n\n\
         OPTIONS:\n  \
         --web-root <dir>  Serve the web app from this directory instead of the copy\n                    \
         compiled into the binary. For development against a live Vite build.\n  \
         --tls-san <host>  Additional IP or DNS name the TLS certificate must cover.\n                    \
         Repeatable; also accepts a comma-separated list. Required when the\n                    \
         address clients dial isn't one this process can detect — notably in\n                    \
         Docker, where detection sees only the container's bridge IP and every\n                    \
         client would otherwise fail hostname verification. May also be set via\n                    \
         the {san_env} environment variable. Changing it re-mints the cert, so\n                    \
         each browser shows the trust prompt once more.\n",
        san_env = lightview_lib::http_server::tls::SAN_ENV_VAR
    );
}
