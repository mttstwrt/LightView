//! LightView tagging worker.
//!
//! Runs on a capable machine and does the ML tagging a weak headless server
//! can't: it pairs with the server as a normal device, polls for tagging jobs
//! enqueued from the web UI, downloads image bytes over HTTPS, runs the
//! locally installed plugin (the same `data/plugins/` layout as the desktop
//! app), and pushes tags back via `apply_plugin_tags`. The server never
//! executes anything the worker sends — only tag data flows back.
//! Design: `docs/remote/worker-tagging.md`.
//!
//! Usage:
//!   lightview-worker pair --server https://<host>:<port> [--pin <code>] [--name <name>] [--yes] [--trust-new]
//!   lightview-worker run  [--plugins-dir <dir>] [--fit <px>] [--poll <secs>]
//!   lightview-worker status

mod config;
mod http;
mod job;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use lightview_lib::plugin::manifest::PluginManifest;

use config::WorkerConfig;
use job::LocalPlugin;

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("pair") => pair(&args[2..]).await,
        Some("run") => run(&args[2..]).await,
        Some("status") => status().await,
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

// ---------------------------------------------------------------------------
// pair
// ---------------------------------------------------------------------------

struct PairArgs {
    server: String,
    pin: Option<String>,
    name: Option<String>,
    yes: bool,
    trust_new: bool,
}

fn parse_pair(args: &[String]) -> Result<PairArgs, String> {
    let mut out = PairArgs {
        server: String::new(),
        pin: None,
        name: None,
        yes: false,
        trust_new: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--server" | "-s" => {
                out.server = args.get(i + 1).ok_or("--server requires a value")?.clone();
                i += 2;
            }
            "--pin" => {
                out.pin = Some(args.get(i + 1).ok_or("--pin requires a value")?.clone());
                i += 2;
            }
            "--name" | "-n" => {
                out.name = Some(args.get(i + 1).ok_or("--name requires a value")?.clone());
                i += 2;
            }
            "--yes" | "-y" => {
                out.yes = true;
                i += 1;
            }
            "--trust-new" => {
                out.trust_new = true;
                i += 1;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if out.server.is_empty() {
        return Err("`pair` requires --server <url>".to_string());
    }
    out.server = out.server.trim_end_matches('/').to_string();
    if !out.server.starts_with("https://") {
        return Err("server URL must be https:// (remote serving is HTTPS-only)".to_string());
    }
    Ok(out)
}

/// Pair with a server: capture + confirm + pin its TLS cert fingerprint, then
/// redeem a pairing PIN for the device cookie. `--trust-new` re-pins the cert
/// on an existing pairing (the server regenerates its cert when the LAN IP
/// changes) without needing a new PIN.
async fn pair(raw: &[String]) -> ExitCode {
    let args = match parse_pair(raw) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n");
            usage();
            return ExitCode::from(2);
        }
    };

    let fingerprint = match http::capture_fingerprint(&args.server).await {
        Ok(fp) => fp,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let fp_hex = http::fingerprint_hex(&fingerprint);
    println!("Server certificate SHA-256: {fp_hex}");

    if args.trust_new {
        let mut cfg = match config::load() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: --trust-new updates an existing pairing, but: {e}");
                return ExitCode::FAILURE;
            }
        };
        if !args.yes && !confirm("Pin this certificate and keep the existing pairing?") {
            return ExitCode::FAILURE;
        }
        cfg.server_url = args.server;
        cfg.cert_sha256 = fp_hex;
        if let Err(e) = config::save(&cfg) {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
        println!("Certificate re-pinned. Existing device cookie kept.");
        return ExitCode::SUCCESS;
    }

    if !args.yes
        && !confirm("Trust this server? (Compare with the fingerprint shown on the host.)")
    {
        return ExitCode::FAILURE;
    }

    let pin_code = match args.pin {
        Some(p) => p,
        None => match prompt("Enter pairing PIN (mint one with `lightview-headless pair <gallery>`): ") {
            Some(p) if !p.is_empty() => p,
            _ => {
                eprintln!("error: a pairing PIN is required");
                return ExitCode::FAILURE;
            }
        },
    };

    let worker_name = args.name.unwrap_or_else(|| {
        sysinfo::System::host_name().unwrap_or_else(|| "worker".to_string())
    });

    let cookie = match http::redeem_pin(
        &args.server,
        &fp_hex,
        pin_code.trim(),
        &format!("worker:{worker_name}"),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Keep a stable worker_id across re-pairs so the server-side identity
    // doesn't churn.
    let (worker_id, fit_edge, poll_secs) = match config::load() {
        Ok(old) => (old.worker_id, old.fit_edge, old.poll_secs),
        Err(_) => (uuid::Uuid::new_v4().to_string(), 1024, 3),
    };

    let cfg = WorkerConfig {
        server_url: args.server,
        cookie,
        cert_sha256: fp_hex,
        worker_id,
        worker_name: worker_name.clone(),
        fit_edge,
        poll_secs,
    };
    if let Err(e) = config::save(&cfg) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "Paired as '{worker_name}'. Config: {}\nInstall plugins under {} and start with: lightview-worker run",
        config::config_path().display(),
        default_plugins_dir().display(),
    );
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

fn parse_run(args: &[String]) -> Result<(Option<PathBuf>, Option<u32>, Option<u64>), String> {
    let mut plugins_dir = None;
    let mut fit = None;
    let mut poll = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--plugins-dir" => {
                let v = args.get(i + 1).ok_or("--plugins-dir requires a value")?;
                plugins_dir = Some(PathBuf::from(v));
                i += 2;
            }
            "--fit" => {
                let v = args.get(i + 1).ok_or("--fit requires a value")?;
                fit = Some(v.parse().map_err(|_| format!("invalid --fit: {v}"))?);
                i += 2;
            }
            "--poll" => {
                let v = args.get(i + 1).ok_or("--poll requires a value")?;
                poll = Some(v.parse().map_err(|_| format!("invalid --poll: {v}"))?);
                i += 2;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok((plugins_dir, fit, poll))
}

fn default_plugins_dir() -> PathBuf {
    lightview_lib::util::paths::data_dir().join("plugins")
}

fn load_plugins(dir: &PathBuf) -> Vec<LocalPlugin> {
    let mut plugins = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return plugins;
    };
    for entry in entries.flatten() {
        let manifest_path = entry.path().join("manifest.json");
        if manifest_path.exists() {
            match PluginManifest::load(&manifest_path) {
                Ok(manifest) => plugins.push(LocalPlugin {
                    manifest,
                    dir: entry.path(),
                }),
                Err(e) => log::warn!("skipping {}: {e}", manifest_path.display()),
            }
        }
    }
    plugins
}

async fn run(raw: &[String]) -> ExitCode {
    let (plugins_dir, fit, poll) = match parse_run(raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}\n");
            usage();
            return ExitCode::from(2);
        }
    };

    let mut cfg = match config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(fit) = fit {
        cfg.fit_edge = fit;
    }
    if let Some(poll) = poll {
        cfg.poll_secs = poll.max(1);
    }

    let dir = plugins_dir.unwrap_or_else(default_plugins_dir);
    let plugins = load_plugins(&dir);
    if plugins.is_empty() {
        eprintln!(
            "error: no plugins found under {} — install a plugin directory \
             (with manifest.json) there, e.g. the repo's plugins/wd-tagger",
            dir.display()
        );
        return ExitCode::FAILURE;
    }
    log::info!(
        "worker '{}' → {} with plugins: {}",
        cfg.worker_name,
        cfg.server_url,
        plugins
            .iter()
            .map(|p| p.manifest.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    job::run_loop(cfg, plugins).await;
    // run_loop only returns on unrecoverable setup errors (it retries
    // transport failures forever).
    ExitCode::FAILURE
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

async fn status() -> ExitCode {
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("server:      {}", cfg.server_url);
    println!("worker:      {} ({})", cfg.worker_name, cfg.worker_id);
    println!("cert pin:    {}", cfg.cert_sha256);
    println!("fit edge:    {}", cfg.fit_edge);

    let dir = default_plugins_dir();
    let plugins = load_plugins(&dir);
    println!(
        "plugins:     {} ({})",
        plugins.len(),
        if plugins.is_empty() {
            format!("none under {}", dir.display())
        } else {
            plugins
                .iter()
                .map(|p| p.manifest.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );

    match http::ServerClient::new(&cfg.server_url, &cfg.cookie, &cfg.cert_sha256) {
        Ok(client) => match client
            .invoke::<serde_json::Value>("get_tagging_status", serde_json::json!({}))
            .await
        {
            Ok(v) => {
                let workers = v["workers"].as_array().map(|a| a.len()).unwrap_or(0);
                let jobs = v["jobs"].as_array().map(|a| a.len()).unwrap_or(0);
                println!("server OK:   {workers} worker(s) registered, {jobs} job(s) tracked");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("server error: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn prompt(msg: &str) -> Option<String> {
    print!("{msg}");
    std::io::stdout().flush().ok()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    Some(line.trim().to_string())
}

fn confirm(msg: &str) -> bool {
    matches!(
        prompt(&format!("{msg} [y/N] ")).as_deref(),
        Some("y") | Some("Y") | Some("yes")
    )
}

fn usage() {
    eprintln!(
        "LightView tagging worker — runs ML tagger plugins for a remote LightView server\n\n\
         USAGE:\n  \
         lightview-worker pair --server https://<host>:<port> [--pin <code>] [--name <name>] [--yes] [--trust-new]\n  \
         lightview-worker run  [--plugins-dir <dir>] [--fit <px>] [--poll <secs>]\n  \
         lightview-worker status\n\n\
         COMMANDS:\n  \
         pair    Trust the server's certificate (TOFU) and redeem a pairing PIN.\n          \
                 --trust-new re-pins a rotated certificate, keeping the pairing.\n  \
         run     Announce to the server and process tagging jobs until killed.\n  \
         status  Print the local config/plugins and check server connectivity.\n"
    );
}
