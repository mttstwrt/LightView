//! One binary, three modes, and five verbs that administer the host.
//!
//! **One instance per role.** A machine that serves its own gallery and also
//! tags a remote one runs `--serve` and `lightview tag` as separate processes,
//! which is what the operating system is for. Within one *gallery* the modes
//! are mutually exclusive, enforced by the `flock` on its derived cache: two
//! writers on one `cache.db` behind an in-process mutex is the assumption that
//! lock exists to make true.
//!
//! **A second launch opens the first one's window rather than refusing.** The
//! package ships a `.desktop` file, so double-clicking a folder twice is an
//! ordinary user action, and "refused: already running" is a bad answer to it —
//! especially since the port is ephemeral, so the second process could not even
//! tell the user where the first one is. The lock holder keeps `instance.json`
//! current; a second launch reads it, opens a browser there, and exits 0.

pub mod args;

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use crate::autocomplete::engine::AutocompleteEngine;
use crate::cache::db::{CacheDb, CacheError};
use crate::cache::store;
use crate::path::Root;
use crate::pipeline::serve::ThumbService;
use crate::server::auth::{Instance, LaunchSession, Trust};
use crate::server::config::ServerConfig;
use crate::server::devices::{self, Devices, PairingKind};
use crate::server::events::Events;
use crate::server::{listen, tls};
use crate::services::gallery as gallery_service;
use crate::services::settings::GallerySettings;
use crate::state::{AppState, Gallery};
use crate::util::paths::Dirs;

use args::{Command, Invocation};

/// Parse, dispatch, and turn an error into an exit code and a message.
pub async fn run() -> std::process::ExitCode {
    let invocation = match args::parse(std::env::args().skip(1)) {
        Ok(i) => i,
        Err(message) => {
            eprintln!("lightview: {message}\n\n{}", args::USAGE);
            return std::process::ExitCode::from(2);
        }
    };

    match dispatch(invocation).await {
        Ok(code) => code,
        Err(message) => {
            eprintln!("lightview: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn dispatch(invocation: Invocation) -> Result<std::process::ExitCode, String> {
    let dirs = match &invocation.data_dir {
        Some(root) => Dirs::under(root),
        None => Dirs::from_env(),
    };
    dirs.ensure().map_err(|e| format!("could not create the state directories: {e}"))?;

    match invocation.command {
        Command::Help => {
            print!("{}", args::USAGE);
            Ok(std::process::ExitCode::SUCCESS)
        }
        Command::Open { dir } => open(&dirs, &dir).await,
        Command::Serve {
            dir,
            port,
            tls_sans,
        } => serve(&dirs, &dir, port, tls_sans).await,
        Command::Tag { dir, plugin, filter } => tag(&dirs, &dir, &plugin, filter.as_deref()).await,
        Command::Pair => pair(&dirs).await,
        Command::Devices => list_devices(&dirs).await,
        Command::RevokeDevice { id } => revoke_device(&dirs, &id).await,
        Command::SetPassword => set_password(&dirs, false),
        Command::ClearPassword => set_password(&dirs, true),
        Command::Cache => show_cache(&dirs),
        Command::PruneCache => prune_cache(&dirs),
    }
}

// ---------------------------------------------------------------------------
// The two serving modes
// ---------------------------------------------------------------------------

/// `lightview <dir>` — loopback, `Owner`, opens a browser.
async fn open(dirs: &Dirs, dir: &Path) -> Result<std::process::ExitCode, String> {
    let root = Root::open(dir).map_err(|_| format!("no such directory: {}", dir.display()))?;
    let cache_dir = dirs.gallery_cache(root.as_path());

    let db = match CacheDb::open_at(&cache_dir) {
        Ok(db) => Arc::new(db),
        // Not a failure to report as one.
        Err(CacheError::AlreadyOpen) => return open_the_running_one(&cache_dir),
        Err(e) => return Err(format!("could not open the gallery cache: {e}")),
    };

    let config = load_config(dirs)?;
    let bound = listen::bind_loopback().map_err(|e| format!("could not bind: {e}"))?;
    let launch = Arc::new(LaunchSession::new());

    let gallery = build_gallery(root, db, cache_dir.clone(), dirs, &config)?;
    let mut state = AppState::new(gallery.clone(), Trust::Owner, dirs.clone(), config);
    state.origin = bound.origin();
    state.launch = Some(launch.clone());
    let state = Arc::new(state);

    let url = format!("{}/?t={}", state.origin, launch.token());
    Instance::write(&cache_dir, &url)
        .map_err(|e| format!("could not write instance.json: {e}"))?;

    start(&state, &gallery).await?;

    // **Always printed.** On a headless host `xdg-open` fails and the URL
    // would otherwise be unknowable — which is also why there is no
    // `--no-browser` flag to define.
    println!("{url}");
    if let Err(e) = open_browser(&url) {
        eprintln!("lightview: could not launch a browser ({e}); open the URL above");
    }

    listen::serve(bound, state)
        .await
        .map_err(|e| format!("server stopped: {e}"))?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// The lock was held: find the live window and open it.
fn open_the_running_one(cache_dir: &Path) -> Result<std::process::ExitCode, String> {
    let Some(instance) = Instance::read(cache_dir) else {
        return Err(
            "this gallery is open in another process, but its instance.json could not be read"
                .to_string(),
        );
    };
    println!("{}", instance.url);
    if let Err(e) = open_browser(&instance.url) {
        eprintln!("lightview: could not launch a browser ({e}); open the URL above");
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// `lightview --serve <dir>` — LAN, `Device`, TLS, pairing.
async fn serve(
    dirs: &Dirs,
    dir: &Path,
    port: Option<u16>,
    tls_sans: Vec<String>,
) -> Result<std::process::ExitCode, String> {
    let root = Root::open(dir).map_err(|_| format!("no such directory: {}", dir.display()))?;

    // Before anything else: can this process replace companions the other
    // writer created? Refusing here turns a silent write failure discovered
    // weeks later into a message while the deployment is being set up.
    gallery_service::probe_write_access(&root)?;

    let cache_dir = dirs.gallery_cache(root.as_path());
    let db = match CacheDb::open_at(&cache_dir) {
        Ok(db) => Arc::new(db),
        Err(CacheError::AlreadyOpen) => {
            return Err(
                "this gallery is already open in another process.\n\
                 `lightview --serve` and `lightview <dir>` are mutually exclusive on one \
                 gallery: to browse a served gallery locally, point a browser at the served URL."
                    .to_string(),
            )
        }
        Err(e) => return Err(format!("could not open the gallery cache: {e}")),
    };

    let mut config = load_config(dirs)?;
    // `--port` overrides the file, which is what lets one account serve two
    // galleries at once — `server.toml` holds one port.
    if let Some(port) = port {
        config.port = port;
    }
    config.tls_sans.extend(tls::sans_from_env());
    config.tls_sans.extend(tls_sans);

    let bound = listen::bind_serve(config.bind, config.port)
        .map_err(|e| format!("could not bind {}:{}: {e}", config.bind, config.port))?;

    let gallery = build_gallery(root, db, cache_dir, dirs, &config)?;
    let mut state = AppState::new(gallery.clone(), Trust::Device, dirs.clone(), config);
    state.devices = Some(Arc::new(
        Devices::open(&dirs.devices_db()).map_err(|e| format!("could not open devices.db: {e}"))?,
    ));
    let state = Arc::new(state);

    start(&state, &gallery).await?;

    println!("{}", listen::advertised_address(&bound));
    if !state.config.has_password() {
        eprintln!(
            "lightview: no password is set. Paired devices need no second factor; \
             run `lightview password` to add one."
        );
    }
    eprintln!("lightview: run `lightview pair` to add a device.");

    listen::serve(bound, state)
        .await
        .map_err(|e| format!("server stopped: {e}"))?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// `lightview tag <dir> --plugin <name> [--filter <expr>]`.
///
/// **This is why there is no `--remote` mode and no worker binary.** The
/// problem it solves is real: the server is an N100 that cannot run the models
/// and the desktop has the GPU. An earlier design answered it with a
/// distributed job broker — a worker registry with liveness TTLs,
/// announce/claim/update/complete/fail, job pinning, two staleness clocks, a
/// credential file, a pairing verb and a certificate pin — all of it to move
/// bytes and results between two machines over HTTP.
///
/// The desktop can mount the gallery. So it does not need a protocol, it needs
/// a path: this opens the gallery the way every other mode does, runs the
/// plugin locally and writes companions. The server's own watcher picks them
/// up, because a write arriving over the share is `smbd` writing to the local
/// disk and `inotify` watches inodes.
///
/// It takes the cache lock like any other mode, so it cannot run against a
/// gallery this machine is already serving — the lock is what stops two
/// processes writing one derived cache, and a tagging run is exactly the case
/// that would.
async fn tag(
    dirs: &Dirs,
    dir: &Path,
    plugin_name: &str,
    filter: Option<&str>,
) -> Result<std::process::ExitCode, String> {
    let root = Root::open(dir).map_err(|_| format!("no such directory: {}", dir.display()))?;
    let plugins_dir = dirs.plugins();
    let Some(plugin) = crate::plugin::manifest::find(&plugins_dir, plugin_name) else {
        return Err(format!(
            "no plugin named {plugin_name:?} under {}",
            plugins_dir.display()
        ));
    };

    let cache_dir = dirs.gallery_cache(root.as_path());
    let db = match CacheDb::open_at(&cache_dir) {
        Ok(db) => Arc::new(db),
        Err(CacheError::AlreadyOpen) => {
            return Err(
                "this gallery is open in another LightView process; close it and try again"
                    .to_string(),
            )
        }
        Err(e) => return Err(format!("could not open the gallery cache: {e}")),
    };

    let config = load_config(dirs)?;
    let gallery = build_gallery(root, db, cache_dir, dirs, &config)?;

    // The index is what `--filter` runs against and what decides which files
    // exist, so the scan is not optional — and the desktop builds its own
    // derived cache for the gallery on this first run, which is what makes
    // subsequent runs read cached tiers locally.
    eprintln!("lightview: indexing…");
    gallery_service::scan_and_index(&gallery)
        .await
        .map_err(|e| format!("could not scan the gallery: {e}"))?;
    gallery.refresh_autocomplete().await;

    let paths = select_paths(&gallery, filter).await?;
    if paths.is_empty() {
        eprintln!("lightview: nothing matched");
        return Ok(std::process::ExitCode::SUCCESS);
    }

    eprintln!(
        "lightview: {} over {} file{}",
        plugin.manifest.display_name,
        paths.len(),
        if paths.len() == 1 { "" } else { "s" }
    );

    // The progress sink, and the only thing that differs from the UI's run.
    // One line, rewritten in place, and only when the count actually moves —
    // a run redirected to a log should not produce one line per file.
    let mut last = 0usize;
    let report = crate::plugin::run::run(&gallery, &plugin, &paths, |done, total| {
        if done == last {
            return;
        }
        last = done;
        eprint!("\rlightview: {done} / {total}");
        let _ = std::io::stderr().flush();
    })
    .await
    .map_err(|e| format!("\n{e}"))?;
    eprintln!();

    println!(
        "tagged {}, skipped {}, failed {}",
        report.tagged, report.skipped, report.failed
    );
    Ok(if report.failed > 0 {
        std::process::ExitCode::from(1)
    } else {
        std::process::ExitCode::SUCCESS
    })
}

/// What a tagging run covers: everything, or everything a filter matches.
///
/// Through `get_items` rather than a second query path, so `--filter` means
/// exactly what the same expression means in the filter bar — including its
/// ordering, which decides which files a cancelled run got to.
async fn select_paths(
    gallery: &Arc<Gallery>,
    filter: Option<&str>,
) -> Result<Vec<crate::path::RelPath>, String> {
    let request = crate::services::media::ItemsRequest {
        filter: filter.unwrap_or("").to_string(),
        ..Default::default()
    };
    let items = crate::services::media::get_items(gallery, &request)
        .await
        .map_err(|e| format!("{e}"))?;
    Ok(items.items.into_iter().map(|i| i.path).collect())
}

/// Everything the two serving modes do identically: scan, arm, open the gate,
/// then enrich in the background.
async fn start(state: &Arc<AppState>, gallery: &Arc<Gallery>) -> Result<(), String> {
    // The ceiling, at open. Leaving it to `lightview cache --prune` means a
    // folder processed once and never reopened leaves a cache nothing reclaims,
    // in a directory advertised as safe *because* it is budgeted.
    match store::prune_to_ceiling(
        &state.dirs.galleries(),
        state.config.cache_ceiling_bytes(),
        Some(&gallery.cache_dir),
    ) {
        Ok(report) if !report.removed.is_empty() => {
            log::info!(
                "evicted {} cold gallery caches ({} MB)",
                report.removed.len(),
                report.freed_bytes / (1024 * 1024)
            );
        }
        Ok(_) => {}
        Err(e) => log::warn!("could not enforce the cache ceiling: {e}"),
    }

    // Trash retention, from the gallery's own settings — never from
    // `server.toml`, or a desktop's default would delete a served gallery's
    // trash.
    let retention = gallery.settings().trash_retention_secs();
    let root = gallery.root.clone();
    let _ = tokio::task::spawn_blocking(move || {
        crate::services::trash::auto_purge(&root, retention)
    })
    .await;

    gallery_service::scan_and_index(gallery)
        .await
        .map_err(|e| format!("could not scan the gallery: {e}"))?;

    // **Armed before the gate opens.** A file arriving in between is in neither
    // the completed scan nor the watcher.
    let watcher = gallery_service::arm(gallery)
        .map_err(|e| format!("could not start the filesystem watcher: {e:?}"))?;
    let watching = gallery.clone();
    tokio::spawn(async move {
        let exit = gallery_service::watch(watching, watcher).await;
        // A serving process can do nothing useful without its gallery, and an
        // unmount under it is an operator event; a systemd unit restarts it
        // when the mount returns.
        eprintln!("lightview: the gallery went away: {exit:?}");
        std::process::exit(1);
    });
    state.mark_ready();

    // Enrichment is slow and the grid does not need it to paint.
    let enriching = gallery.clone();
    tokio::spawn(async move {
        if let Err(e) = gallery_service::enrich_and_index(&enriching).await {
            log::warn!("the open-time enrichment pass failed: {e}");
        }
    });
    crate::pipeline::idle::spawn(gallery.thumbs.clone());
    // The backstop for companions written where the watcher cannot see them.
    gallery_service::spawn_companion_sweep(gallery.clone());
    Ok(())
}

fn build_gallery(
    root: Root,
    db: Arc<CacheDb>,
    cache_dir: std::path::PathBuf,
    _dirs: &Dirs,
    _config: &ServerConfig,
) -> Result<Arc<Gallery>, String> {
    let profile = crate::hardware::HardwareProfile::detect();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(profile.thumbnail_threads())
        .thread_name(|i| format!("thumb-{i}"))
        .build()
        .map_err(|e| format!("could not build the thumbnail pool: {e}"))?;
    let budget = store::tier_budget_bytes(&cache_dir);
    let thumbs = Arc::new(ThumbService::new(
        db.clone(),
        root.clone(),
        Arc::new(pool),
        budget,
    ));
    let settings = GallerySettings::load(root.as_path());

    Ok(Arc::new(Gallery {
        root,
        db,
        thumbs,
        events: Arc::new(Events::new()),
        autocomplete: Arc::new(AutocompleteEngine::new()),
        settings: std::sync::RwLock::new(settings),
        cache_dir,
    }))
}

// ---------------------------------------------------------------------------
// Administering the host from a shell
// ---------------------------------------------------------------------------

/// `lightview pair` — mint a one-time code.
///
/// **Takes no gallery argument.** Pairings live in the state directory and are
/// a property of the account serving, so there is nothing per-gallery to name.
/// The consequence, stated because it is a real widening: a phone paired to
/// this machine is paired to every gallery this machine serves, now or later.
async fn pair(dirs: &Dirs) -> Result<std::process::ExitCode, String> {
    let store = Devices::open(&dirs.devices_db())
        .map_err(|e| format!("could not open devices.db: {e}"))?;
    let conn = store.writer().await;
    let code = devices::create_pairing(&conn, PairingKind::Pin)
        .map_err(|e| format!("could not create a pairing code: {e}"))?;
    println!("{code}");
    eprintln!("lightview: valid for ten minutes, single use. Ten wrong guesses cancel it.");
    Ok(std::process::ExitCode::SUCCESS)
}

/// `lightview devices` — the only way to see or undo a pairing.
///
/// It exists because nothing else could: the device-management UI is in the
/// part of the settings menu this rebuild cuts, and nothing is `Owner` under
/// `--serve`, so a web UI could not be the answer even if it survived. A lost
/// phone would otherwise stay paired forever — and to every gallery this
/// machine will ever serve.
async fn list_devices(dirs: &Dirs) -> Result<std::process::ExitCode, String> {
    let store = Devices::open(&dirs.devices_db())
        .map_err(|e| format!("could not open devices.db: {e}"))?;
    let conn = store.read().await;
    let rows = devices::list(&conn).map_err(|e| format!("could not list devices: {e}"))?;
    if rows.is_empty() {
        println!("no paired devices");
        return Ok(std::process::ExitCode::SUCCESS);
    }
    for row in rows {
        let last_seen = row
            .last_seen
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|d| d.to_rfc3339())
            .unwrap_or_else(|| "never".to_string());
        println!("{}\t{}\tlast seen {}", row.id, row.name, last_seen);
    }
    Ok(std::process::ExitCode::SUCCESS)
}

async fn revoke_device(dirs: &Dirs, id: &str) -> Result<std::process::ExitCode, String> {
    let store = Devices::open(&dirs.devices_db())
        .map_err(|e| format!("could not open devices.db: {e}"))?;
    let conn = store.writer().await;
    match devices::revoke(&conn, id) {
        Ok(true) => {
            println!("revoked {id}");
            Ok(std::process::ExitCode::SUCCESS)
        }
        Ok(false) => Err(format!("no such device: {id}")),
        Err(e) => Err(format!("could not revoke {id}: {e}")),
    }
}

/// `lightview password` — read the passphrase from **stdin**, never argv.
///
/// Argv lands in shell history and in `ps`. This stays a command rather than a
/// configuration key for a sharper reason than "it is an action": the stored
/// value is an argon2id PHC string, and "configuration is a file" cannot mean
/// asking a person to hand-compute a hash.
fn set_password(dirs: &Dirs, clear: bool) -> Result<std::process::ExitCode, String> {
    let path = dirs.server_toml();
    let mut config = ServerConfig::load(&path).map_err(|e| e.to_string())?;

    if clear {
        config.password_hash.clear();
        config.save(&path).map_err(|e| e.to_string())?;
        eprintln!("lightview: password cleared");
        return Ok(std::process::ExitCode::SUCCESS);
    }

    eprint!("Passphrase: ");
    let _ = std::io::stderr().flush();
    let mut passphrase = String::new();
    std::io::stdin()
        .read_line(&mut passphrase)
        .map_err(|e| format!("could not read the passphrase: {e}"))?;
    let passphrase = passphrase.trim_end_matches(['\n', '\r']);
    if passphrase.is_empty() {
        return Err("the passphrase was empty; use `--clear` to remove it".to_string());
    }

    config.password_hash = crate::server::auth::hash_password(passphrase)
        .map_err(|e| format!("could not hash the passphrase: {e}"))?;
    config.save(&path).map_err(|e| e.to_string())?;
    eprintln!("lightview: password set in {}", path.display());
    Ok(std::process::ExitCode::SUCCESS)
}

/// `lightview cache` — where the derived data is and how much of it there is.
fn show_cache(dirs: &Dirs) -> Result<std::process::ExitCode, String> {
    let galleries = dirs.galleries();
    let caches = store::survey(&galleries).map_err(|e| format!("could not read {}: {e}", galleries.display()))?;
    let total: u64 = caches.iter().map(|c| c.bytes).sum();

    println!("{}", galleries.display());
    println!("{} galleries, {} MB", caches.len(), total / (1024 * 1024));
    for cache in &caches {
        let opened = chrono::DateTime::from_timestamp(cache.last_opened, 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_else(|| "never".to_string());
        println!(
            "  {}\t{} MB\tlast opened {opened}",
            cache
                .dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            cache.bytes / (1024 * 1024)
        );
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn prune_cache(dirs: &Dirs) -> Result<std::process::ExitCode, String> {
    let config = load_config(dirs)?;
    let report = store::prune_to_ceiling(&dirs.galleries(), config.cache_ceiling_bytes(), None)
        .map_err(|e| format!("could not prune: {e}"))?;
    println!(
        "removed {} caches, freed {} MB",
        report.removed.len(),
        report.freed_bytes / (1024 * 1024)
    );
    for skipped in &report.skipped_in_use {
        // Never silently: unlinking a cache.db a live process holds open
        // discards its work at exit, so skipping is right — and saying so is
        // what stops "prune did nothing" being a mystery.
        println!("  in use, kept: {}", skipped.display());
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn load_config(dirs: &Dirs) -> Result<ServerConfig, String> {
    ServerConfig::load(&dirs.server_toml()).map_err(|e| e.to_string())
}

/// Hand the URL to the desktop. Failure is reported, never fatal.
fn open_browser(url: &str) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}
