//! Per-gallery settings, hardware reporting, and remote-access administration.
//!
//! Settings live in two places on purpose. Most are per-gallery, stored in
//! `gallery_meta` and mirrored to a hand-editable `settings.toml` inside the
//! gallery — the fs-watcher hot-reloads external edits to that file, comparing
//! against `AppState::last_written_settings` so the app's own saves don't
//! echo back as user edits. Process-level rendering preferences
//! (`RenderConfig`) are separate because they are read before GTK/WebKit init
//! and cannot take effect without a restart.
//!
//! The remote-access commands here — pairing codes, device lists, the gallery
//! password — are deliberately **desktop-only**: they administer the boundary
//! that `/api/invoke` enforces, so exposing them to a paired device would let
//! it widen its own access.

use serde::Serialize;
use std::path::PathBuf;

use crate::hardware::MemoryStatus;
use crate::http_server::devices::{self, DeviceRow, PairingKind, DEFAULT_INACTIVITY_SECS};
use crate::http_server::tls::detect_lan_ip;
use crate::http_server::uploads::{self, UploadConfig, UploadScheme};
use crate::http_server::{self, HttpConfig, RemoteAccess};
use crate::pipeline::thumbnailer::STANDARD_THUMB_SIZE;
use crate::views;
use crate::{AppState, RecentGallery};

/// Debug information about which hardware optimizations are active.
#[derive(Debug, Serialize)]
pub struct DebugInfo {
    pub storage_type: String,
    pub filesystem: String,
    pub cpu_cores: usize,
    pub total_ram_mb: u64,
    pub supports_reflink: bool,
    pub thumbnail_threads: usize,
    pub prefetch_count: usize,
    pub lru_cache_size: usize,
    pub standard_thumb_size: u32,
    pub sqlite_thumbnail_count: u64,
    pub gpu_resize_active: bool,
    pub gdk_backend: String,
    pub webkit_disable_dmabuf: bool,
}

/// Get current memory status (available + total RAM) for pressure-aware caching.
#[tauri::command]
pub async fn get_memory_status() -> Result<MemoryStatus, String> {
    Ok(MemoryStatus::sample())
}

/// Base URL of the local HTTP media server, used by `<video>` elements.
/// Returns an empty string if the server has not finished starting.
#[tauri::command]
pub async fn get_media_server_url(
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    Ok(state.media_server_url.get().cloned().unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Remote (LAN) web access
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct RemoteAccessInfo {
    /// The bound port (OS-assigned).
    pub port: u16,
    /// Detected LAN IPv4, if any (best-effort).
    pub lan_ip: Option<String>,
    /// Base URL of the running server, or None if no LAN IP found.
    pub base_url: Option<String>,
    /// Count of requests seen from non-loopback peers. Non-zero proves an
    /// external device actually reached the server.
    pub clients_seen: u64,
    /// Firewall guidance for this port, if a firewall appears to be active.
    pub firewall_hint: Option<String>,
}

/// Information about a paired device, suitable for the settings UI.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen: i64,
    pub last_auth_at: i64,
    pub revoked_at: Option<i64>,
}

impl From<DeviceRow> for DeviceInfo {
    fn from(row: DeviceRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            created_at: row.created_at,
            last_seen: row.last_seen,
            last_auth_at: row.last_auth_at,
            revoked_at: row.revoked_at,
        }
    }
}

/// A freshly minted pairing code that a phone can redeem at `/pair`.
/// `pairing_url` is what the QR code encodes — the device opens it,
/// the SPA's `/pair` page extracts `?code=` and POSTs to `/pair/redeem`.
#[derive(Debug, Clone, Serialize)]
pub struct PairingCode {
    pub code: String,
    pub kind: String,
    pub expires_at: i64,
    pub pairing_url: Option<String>,
}

/// Best-effort detection of the LAN IP used for the default route. Opens a UDP
/// socket "connected" to a public address (no packets are sent) and reads back
/// the local address the OS would route through.
/// If a host firewall is active, return a short hint with the command to allow
/// `port`. Linux-only; other platforms return None (the user manages their own).
#[cfg(target_os = "linux")]
fn detect_firewall_hint(port: u16) -> Option<String> {
    fn is_active(service: &str) -> bool {
        std::process::Command::new("systemctl")
            .args(["is-active", service])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
            .unwrap_or(false)
    }

    if is_active("firewalld") {
        Some(format!(
            "firewalld is active. Allow this port:\nsudo firewall-cmd --add-port={port}/tcp"
        ))
    } else if is_active("ufw") {
        Some(format!("ufw is active. Allow this port:\nsudo ufw allow {port}/tcp"))
    } else if is_active("nftables") || is_active("iptables") {
        Some(format!(
            "A firewall (nftables/iptables) is active — you may need to allow inbound TCP on port {port}."
        ))
    } else {
        None
    }
}

#[cfg(not(target_os = "linux"))]
fn detect_firewall_hint(_port: u16) -> Option<String> {
    None
}

fn build_info(remote: &RemoteAccess) -> RemoteAccessInfo {
    let addr_port = remote.addr.port();
    let lan_ip = detect_lan_ip().map(|ip| ip.to_string());
    // The remote server always terminates TLS (see HttpConfig::remote).
    let base_url = lan_ip
        .as_ref()
        .map(|ip| format!("https://{ip}:{addr_port}"));
    RemoteAccessInfo {
        port: addr_port,
        lan_ip,
        base_url,
        clients_seen: remote
            .remote_hits
            .load(std::sync::atomic::Ordering::Relaxed),
        firewall_hint: remote.firewall_hint.clone(),
    }
}

/// Start LAN-accessible remote web access (bind 0.0.0.0, per-device cookie
/// auth, SPA served from `dist/`). Idempotent: if already running, returns
/// the current info. Device pairings live in the open gallery's cache.db, so
/// they persist across restarts but are scoped to that gallery.
#[tauri::command]
pub async fn enable_remote_access(
    state: tauri::State<'_, AppState>,
    port: Option<u16>,
) -> Result<RemoteAccessInfo, String> {
    let mut guard = state.remote_server.lock().await;

    if let Some(existing) = guard.as_ref() {
        return Ok(build_info(existing));
    }

    let config = HttpConfig::remote(port.unwrap_or(0));
    let app_state: AppState = (*state).clone();
    let server = http_server::start(config, app_state)
        .await
        .map_err(|e| format!("Failed to start remote server: {e}"))?;

    log::info!("Remote access enabled on port {}", server.addr.port());

    // Web clients can now enqueue tagging jobs — offer this host's own
    // plugins as the "run on the server" option.
    crate::tagging::local::ensure_started(state.inner());

    let remote = RemoteAccess {
        addr: server.addr,
        handle: server.handle,
        remote_hits: server.remote_hits,
        firewall_hint: detect_firewall_hint(server.addr.port()),
    };
    let info = build_info(&remote);
    *guard = Some(remote);

    Ok(info)
}

/// Stop the remote-access server. No-op if it is not running.
#[tauri::command]
pub async fn disable_remote_access(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.remote_server.lock().await;
    if let Some(remote) = guard.take() {
        remote.handle.abort();
        log::info!("Remote access disabled");
    }
    Ok(())
}

/// Current remote-access info, or None if remote access is off.
#[tauri::command]
pub async fn get_remote_access_info(
    state: tauri::State<'_, AppState>,
) -> Result<Option<RemoteAccessInfo>, String> {
    let guard = state.remote_server.lock().await;
    Ok(guard.as_ref().map(build_info))
}

// ---------------------------------------------------------------------------
// Device + password admin (Tauri-only — never exposed over HTTP)
// ---------------------------------------------------------------------------

/// Full settings snapshot for the Remote Access panel: list of paired
/// devices plus whether a password is set and the configured inactivity
/// window.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteAuthState {
    pub devices: Vec<DeviceInfo>,
    pub password_set: bool,
    pub inactivity_secs: i64,
}

#[tauri::command]
pub async fn get_remote_auth_state(
    state: tauri::State<'_, AppState>,
) -> Result<RemoteAuthState, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    let conn = db.conn();
    let devices = devices::list_devices(conn)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(DeviceInfo::from)
        .collect();
    let password_set = devices::get_password_hash(conn)
        .map_err(|e| e.to_string())?
        .is_some();
    let inactivity_secs = devices::get_inactivity_secs(conn).unwrap_or(DEFAULT_INACTIVITY_SECS);
    Ok(RemoteAuthState {
        devices,
        password_set,
        inactivity_secs,
    })
}

/// Generate a fresh pairing code. `kind` is either `"qr"` (random hex token,
/// embedded in the QR URL) or `"pin"` (6 typeable digits).
#[tauri::command]
pub async fn generate_pairing_code(
    state: tauri::State<'_, AppState>,
    kind: String,
) -> Result<PairingCode, String> {
    let parsed_kind = match kind.as_str() {
        "qr" => PairingKind::Qr,
        "pin" => PairingKind::Pin,
        other => return Err(format!("unknown pairing kind: {other}")),
    };

    // Hold the cache_db lock for the SQL writes; build the URL outside it.
    let (code, expires_at) = {
        let db = state.cache_db.lock().await;
        let db = db.as_ref().ok_or("No gallery open")?;
        let conn = db.conn();
        let code = devices::create_pairing(conn, parsed_kind).map_err(|e| e.to_string())?;
        let expires_at = chrono::Utc::now().timestamp() + devices::PAIRING_TTL_SECS;
        (code, expires_at)
    };

    let base_url = {
        let guard = state.remote_server.lock().await;
        guard.as_ref().and_then(|r| {
            detect_lan_ip().map(|ip| format!("https://{ip}:{}", r.addr.port()))
        })
    };

    let pairing_url = base_url.map(|b| match parsed_kind {
        PairingKind::Qr => format!("{}/pair?code={}", b, code),
        // The PIN flow doesn't pre-fill — user types it on the phone.
        PairingKind::Pin => format!("{}/pair", b),
    });

    Ok(PairingCode {
        code,
        kind: kind.to_string(),
        expires_at,
        pairing_url,
    })
}

#[tauri::command]
pub async fn revoke_remote_device(
    state: tauri::State<'_, AppState>,
    device_id: String,
) -> Result<(), String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    devices::revoke_device(db.conn(), &device_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_remote_device(
    state: tauri::State<'_, AppState>,
    device_id: String,
) -> Result<(), String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    devices::delete_device(db.conn(), &device_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_remote_password(
    state: tauri::State<'_, AppState>,
    password: String,
) -> Result<(), String> {
    if password.is_empty() {
        return Err("password cannot be empty".to_string());
    }
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    devices::set_password(db.conn(), &password).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn clear_remote_password(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    devices::clear_password(db.conn()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_remote_inactivity(
    state: tauri::State<'_, AppState>,
    secs: i64,
) -> Result<(), String> {
    if secs < 0 {
        return Err("inactivity must be non-negative".to_string());
    }
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    devices::set_inactivity_secs(db.conn(), secs).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Device upload settings (per-gallery)
// ---------------------------------------------------------------------------

/// Read the per-gallery upload config. Shared by the desktop command and the
/// web client (over the `/api/invoke` bridge) so the phone knows whether to
/// show the upload button and the album field.
pub async fn get_upload_config_impl(state: &AppState) -> Result<UploadConfig, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    uploads::get_config(db.conn()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_upload_config(
    state: tauri::State<'_, AppState>,
) -> Result<UploadConfig, String> {
    get_upload_config_impl(&state).await
}

// ---------------------------------------------------------------------------
// Remote delete permission (per-gallery)
// ---------------------------------------------------------------------------

/// `gallery_meta` key gating destructive commands (trash/restore/purge) over
/// the `/api/invoke` bridge. Defaults to allowed — trash is reversible and
/// pairing already gates access — but a host can switch it off per gallery.
pub const REMOTE_ALLOW_DELETE_KEY: &str = "remote.allow_delete";

/// Whether paired web devices may use the trash commands. This is the
/// server-side enforcement point (`api.rs` checks it per dispatch); the
/// capability the web client sees merely mirrors it.
pub async fn remote_delete_allowed(state: &AppState) -> bool {
    let db = state.cache_db.lock().await;
    match db.as_ref() {
        Some(db) => db
            .get_gallery_meta(REMOTE_ALLOW_DELETE_KEY)
            .ok()
            .flatten()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(true),
        None => false,
    }
}

/// What a remote (web) client is allowed to do, so it can hide unavailable
/// actions up front instead of hitting 403s. `api.rs` enforces the same flags
/// server-side; the desktop client short-circuits to all-true without calling
/// this.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    /// Tag/rating writes — always available to paired devices.
    pub metadata_write: bool,
    /// App-managed trash commands (per-gallery `remote.allow_delete` flag).
    pub delete: bool,
    /// Host filesystem/process ops (copy/move to folders, clipboard, open
    /// with) — never available remotely.
    pub local_fs: bool,
    /// Running plugins on the host — never available remotely.
    pub plugins: bool,
}

pub async fn get_server_capabilities_impl(
    state: &AppState,
) -> Result<ServerCapabilities, String> {
    Ok(ServerCapabilities {
        metadata_write: true,
        delete: remote_delete_allowed(state).await,
        local_fs: false,
        plugins: false,
    })
}

#[tauri::command]
pub async fn get_server_capabilities(
    state: tauri::State<'_, AppState>,
) -> Result<ServerCapabilities, String> {
    get_server_capabilities_impl(&state).await
}

// ---------------------------------------------------------------------------
// Enabled views
// ---------------------------------------------------------------------------

/// The views this gallery offers, as the frontend's `ViewMode` strings. Read
/// by both clients (the web client needs it to hide layouts the gallery does
/// not offer); writing stays desktop-only, like every other setting that
/// configures the gallery rather than the device looking at it.
pub async fn get_enabled_views_impl(state: &AppState) -> Result<Vec<String>, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    Ok(views::enabled(db.conn())
        .into_iter()
        .map(|v| v.as_str().to_string())
        .collect())
}

#[tauri::command]
pub async fn get_enabled_views(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<String>, String> {
    get_enabled_views_impl(&state).await
}

/// Set the views this gallery offers (host-only, via the desktop UI). Cached
/// thumbnails for a view being turned off are left alone — see
/// [`crate::views`].
#[tauri::command]
pub async fn set_enabled_views(
    state: tauri::State<'_, AppState>,
    enabled: Vec<String>,
) -> Result<(), String> {
    let parsed = enabled
        .iter()
        .map(|name| views::View::parse(name).ok_or_else(|| format!("unknown view: {name}")))
        .collect::<Result<Vec<_>, _>>()?;
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    views::set_enabled(db.conn(), &parsed).map_err(|e| e.to_string())
}

/// Set the per-gallery remote-delete flag (host-only, via the desktop UI).
#[tauri::command]
pub async fn set_remote_delete_config(
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    db.set_gallery_meta(REMOTE_ALLOW_DELETE_KEY, if enabled { "1" } else { "0" })
        .map_err(|e| e.to_string())
}

/// Read the per-gallery remote-delete flag (desktop settings UI).
#[tauri::command]
pub async fn get_remote_delete_config(
    state: tauri::State<'_, AppState>,
) -> Result<bool, String> {
    Ok(remote_delete_allowed(&state).await)
}

/// Set the per-gallery upload config (host-only, via the desktop UI).
#[tauri::command]
pub async fn set_upload_config(
    state: tauri::State<'_, AppState>,
    enabled: bool,
    scheme: String,
) -> Result<(), String> {
    let scheme =
        UploadScheme::from_str(&scheme).ok_or_else(|| format!("unknown upload scheme: {scheme}"))?;
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    uploads::set_config(db.conn(), &UploadConfig { enabled, scheme }).map_err(|e| e.to_string())
}

/// Trigger a full re-index of all companion files.
#[tauri::command]
pub async fn reindex_gallery(
    state: tauri::State<'_, AppState>,
) -> Result<u64, String> {
    let gallery_path = state
        .current_gallery
        .read()
        .await
        .clone()
        .ok_or("No gallery open")?;

    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    // Clear existing index
    db.clear_tag_index().map_err(|e| e.to_string())?;

    // Walk the gallery and re-index all companion files
    let ext = crate::companion::schema::COMPANION_EXTENSION;
    let mut indexed = 0u64;
    for entry in walkdir::WalkDir::new(&gallery_path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let path_str = path.to_string_lossy();
        if !path_str.ends_with(ext) {
            continue;
        }

        // Reconstruct the media file path.
        // Companion can be alongside (dir/photo.jpg.lightview.json → dir/photo.jpg)
        // or in .lightview/companions/ (dir/.lightview/companions/photo.jpg.lightview.json → dir/photo.jpg)
        let companion_str = path_str.to_string();
        let base = companion_str.strip_suffix(ext).unwrap_or(&companion_str);

        let media_path_str = if let Some(parent) = path.parent() {
            let parent_name = parent.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if parent_name == "companions" {
                // .lightview/companions/photo.jpg.lightview.json → ../../photo.jpg
                if let Some(lightview_dir) = parent.parent() {
                    if let Some(gallery_dir) = lightview_dir.parent() {
                        let filename = std::path::Path::new(base)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(base);
                        gallery_dir.join(filename).to_string_lossy().to_string()
                    } else {
                        base.to_string()
                    }
                } else {
                    base.to_string()
                }
            } else {
                base.to_string()
            }
        } else {
            base.to_string()
        };

        if let Ok(contents) = std::fs::read_to_string(path) {
            if let Ok(companion) = crate::companion::reader::parse_companion(&contents) {
                let _ = db.reindex_tags_for_file(&media_path_str, &companion);
                indexed += 1;
            }
        }
    }

    // Rebuild counts
    db.rebuild_tag_counts().map_err(|e| e.to_string())?;

    // Refresh autocomplete
    if let Ok(counts) = db.query_all_tag_counts() {
        state.autocomplete.refresh(counts).await;
    }

    Ok(indexed)
}

/// Trigger a full thumbnail rebuild (clear and regenerate).
#[tauri::command]
pub async fn rebuild_thumbnails(
    state: tauri::State<'_, AppState>,
) -> Result<u64, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    let cleared = db.clear_thumbnails().map_err(|e| e.to_string())?;

    // TODO: Kick off background thumbnail regeneration pipeline

    Ok(cleared as u64)
}

#[tauri::command]
pub async fn get_debug_info(
    state: tauri::State<'_, AppState>,
) -> Result<DebugInfo, String> {
    let hw = &*state.hardware;

    let sqlite_thumbs = {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            db.conn()
                .query_row("SELECT COUNT(*) FROM thumbnails", [], |row| row.get::<_, u64>(0))
                .unwrap_or(0)
        } else {
            0
        }
    };

    log::info!("=== Hardware Debug Info ===");
    log::info!("  Storage:      {:?}", hw.storage_type);
    log::info!("  Filesystem:   {}", hw.filesystem);
    log::info!("  CPU cores:    {}", hw.cpu_cores);
    log::info!("  RAM:          {} MB", hw.total_ram_mb);
    log::info!("  Reflink:      {}", hw.supports_reflink);
    log::info!("  Thumb size:   {}x{}", STANDARD_THUMB_SIZE, STANDARD_THUMB_SIZE);
    let gpu_active = state.has_gpu();

    log::info!("  SQLite cache: {} thumbnails", sqlite_thumbs);
    log::info!("  GPU pipeline: {}", gpu_active);
    log::info!("  Thumb pool:   {} threads", hw.thumbnail_threads());
    log::info!("  Prefetch:     {} images", hw.prefetch_count());
    log::info!("  LRU cache:    {} images", hw.lru_cache_size());
    log::info!("==========================");

    Ok(DebugInfo {
        storage_type: format!("{:?}", hw.storage_type),
        filesystem: hw.filesystem.clone(),
        cpu_cores: hw.cpu_cores,
        total_ram_mb: hw.total_ram_mb,
        supports_reflink: hw.supports_reflink,
        thumbnail_threads: hw.thumbnail_threads(),
        prefetch_count: hw.prefetch_count(),
        lru_cache_size: hw.lru_cache_size(),
        standard_thumb_size: STANDARD_THUMB_SIZE,
        sqlite_thumbnail_count: sqlite_thumbs,
        gpu_resize_active: gpu_active,
        gdk_backend: std::env::var("GDK_BACKEND").unwrap_or_default(),
        webkit_disable_dmabuf: std::env::var("WEBKIT_DISABLE_DMABUF_RENDERER")
            .map(|v| v == "1")
            .unwrap_or(false),
    })
}

/// Path to the human-editable settings file inside a gallery's `.lightview`
/// folder. This file (not the SQLite cache) is the source of truth for app
/// settings, so it can be inspected and hand-edited.
fn settings_toml_path(gallery: &str) -> PathBuf {
    std::path::Path::new(gallery)
        .join(".lightview")
        .join("settings.toml")
}

/// Convert the frontend's JSON settings blob into a pretty TOML document.
fn json_to_toml(json: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    toml::to_string_pretty(&value).map_err(|e| e.to_string())
}

/// Convert a TOML settings document back into the JSON shape the frontend
/// expects from `load_gallery_settings`. Used by the fs watcher to translate
/// hand edits into the payload pushed to the frontend.
pub(crate) fn toml_to_json(toml_str: &str) -> Result<String, String> {
    let value: serde_json::Value = toml::from_str(toml_str).map_err(|e| e.to_string())?;
    serde_json::to_string(&value).map_err(|e| e.to_string())
}

/// Load the gallery's settings as a JSON string, reading `settings.toml` first.
/// If the file is missing but a legacy `gallery_meta.app_settings` blob exists
/// (galleries indexed before settings moved to a file), it's migrated to
/// `settings.toml` and returned. Returns `None` when no settings exist yet.
async fn load_settings_json(state: &AppState, gallery: &str) -> Result<Option<String>, String> {
    let path = settings_toml_path(gallery);
    if path.exists() {
        let toml_str = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        return Ok(Some(toml_to_json(&toml_str)?));
    }
    // Legacy fallback: settings used to live in the SQLite cache. Migrate the
    // blob out to settings.toml (best-effort) so future reads use the file.
    let legacy = {
        let db = state.cache_db.lock().await;
        let db = db.as_ref().ok_or("No gallery open")?;
        db.get_gallery_meta("app_settings").map_err(|e| e.to_string())?
    };
    if let Some(json) = &legacy {
        if let Ok(toml_str) = json_to_toml(json) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Record before writing so the fs watcher recognizes this as a
            // self-write and doesn't echo it back as an external edit.
            *state.last_written_settings.lock().unwrap() = Some(toml_str.clone());
            let _ = std::fs::write(&path, toml_str);
        }
    }
    Ok(legacy)
}

/// Save frontend app settings to the current gallery's `.lightview/settings.toml`.
#[tauri::command]
pub async fn save_gallery_settings(
    state: tauri::State<'_, AppState>,
    settings_json: String,
) -> Result<(), String> {
    let gallery = state
        .current_gallery
        .read()
        .await
        .clone()
        .ok_or("No gallery open")?;
    let toml_str = json_to_toml(&settings_json)?;
    let path = settings_toml_path(&gallery);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Record before writing so the fs watcher recognizes this as a self-write
    // and doesn't bounce it back to the frontend as an external edit.
    *state.last_written_settings.lock().unwrap() = Some(toml_str.clone());
    std::fs::write(&path, toml_str).map_err(|e| e.to_string())
}

/// Load frontend app settings from the current gallery's `.lightview/settings.toml`.
#[tauri::command]
pub async fn load_gallery_settings(
    state: tauri::State<'_, AppState>,
) -> Result<Option<String>, String> {
    let gallery = state
        .current_gallery
        .read()
        .await
        .clone()
        .ok_or("No gallery open")?;
    load_settings_json(&state, &gallery).await
}

/// The gallery-wide default filter, shared by the desktop app and any LAN
/// web client. Lives inside the full app-settings blob, but is exposed on its
/// own so the read-only web bridge can read it without leaking the rest of the
/// local settings (display prefs, external app commands, etc.).
#[derive(Debug, Serialize)]
pub struct DefaultFilter {
    pub enabled: bool,
    pub query: String,
}

/// Read just the `default_filter` field out of the gallery's stored settings.
/// Returns `None` when no gallery settings have been saved yet.
pub async fn get_gallery_default_filter_impl(
    state: &AppState,
) -> Result<Option<DefaultFilter>, String> {
    let gallery = state
        .current_gallery
        .read()
        .await
        .clone()
        .ok_or("No gallery open")?;
    let Some(json) = load_settings_json(state, &gallery).await? else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    let Some(df) = value.get("default_filter") else {
        return Ok(None);
    };
    Ok(Some(DefaultFilter {
        enabled: df.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
        query: df
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    }))
}

#[tauri::command]
pub async fn get_gallery_default_filter(
    state: tauri::State<'_, AppState>,
) -> Result<Option<DefaultFilter>, String> {
    get_gallery_default_filter_impl(&state).await
}

/// Lightweight performance snapshot for the debug overlay.
/// Reads /proc/self/io for disk bandwidth and queries cache stats.
#[derive(Debug, Serialize)]
pub struct PerfSnapshot {
    /// Bytes read from disk since process start (from /proc/self/io).
    pub disk_read_bytes: u64,
    /// Bytes written to disk since process start (from /proc/self/io).
    pub disk_write_bytes: u64,
    /// Number of cached thumbnails in SQLite.
    pub cached_thumbnails: u64,
    /// Total byte size of cached thumbnail data.
    pub cache_size_bytes: u64,
    /// Current rayon thread pool active thread count (approximate).
    pub thumb_pool_active_threads: usize,
}

/// Read /proc/self/io counters (Linux-only, returns 0 on other platforms).
fn read_proc_io() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(contents) = std::fs::read_to_string("/proc/self/io") {
            let mut read_bytes = 0u64;
            let mut write_bytes = 0u64;
            for line in contents.lines() {
                if let Some(val) = line.strip_prefix("read_bytes: ") {
                    read_bytes = val.trim().parse().unwrap_or(0);
                } else if let Some(val) = line.strip_prefix("write_bytes: ") {
                    write_bytes = val.trim().parse().unwrap_or(0);
                }
            }
            return (read_bytes, write_bytes);
        }
    }
    (0, 0)
}

#[tauri::command]
pub async fn get_perf_snapshot(
    state: tauri::State<'_, AppState>,
) -> Result<PerfSnapshot, String> {
    let (disk_read_bytes, disk_write_bytes) = read_proc_io();

    let (cached_thumbnails, cache_size_bytes) = {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            let count: u64 = db.conn()
                .query_row("SELECT COUNT(*) FROM thumbnails", [], |row| row.get(0))
                .unwrap_or(0);
            let size: u64 = db.conn()
                .query_row("SELECT COALESCE(SUM(LENGTH(thumbnail)), 0) FROM thumbnails", [], |row| row.get(0))
                .unwrap_or(0);
            (count, size)
        } else {
            (0, 0)
        }
    };

    Ok(PerfSnapshot {
        disk_read_bytes,
        disk_write_bytes,
        cached_thumbnails,
        cache_size_bytes,
        thumb_pool_active_threads: state.hardware.thumbnail_threads(),
    })
}

/// Get the list of recently opened galleries (most recent first).
#[tauri::command]
pub async fn get_recent_galleries(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<RecentGallery>, String> {
    let recents = state.recent_galleries.lock().await;
    Ok(recents.clone())
}

/// Remove a gallery from the recent list (e.g. if the folder no longer exists).
#[tauri::command]
pub async fn remove_recent_gallery(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let mut recents = state.recent_galleries.lock().await;
    recents.retain(|r| r.path != path);
    crate::save_recent_galleries(&recents);
    Ok(())
}

/// Read the process-level render config (GPU acceleration / GTK backend).
/// These env-driven settings take effect only at startup, so the UI must
/// prompt for a restart after changing them.
#[tauri::command]
pub async fn get_render_config() -> Result<crate::RenderConfig, String> {
    Ok(crate::load_render_config())
}

/// Persist the render config. Takes effect on the next app start.
#[tauri::command]
pub async fn set_render_config(
    gpu_acceleration: Option<bool>,
    gtk_backend: Option<String>,
) -> Result<(), String> {
    crate::save_render_config(&crate::RenderConfig { gpu_acceleration, gtk_backend })
}

/// Open a file with an external application.
/// Spawns the command as a detached process — does not wait for it to exit.
#[tauri::command]
pub async fn open_with(
    _state: tauri::State<'_, AppState>,
    command: String,
    args: Vec<String>,
) -> Result<(), String> {
    tokio::process::Command::new(&command)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to launch '{}': {}", command, e))?;
    Ok(())
}

#[cfg(test)]
mod settings_file_tests {
    use super::{json_to_toml, toml_to_json};

    // Mirrors the frontend DEFAULT_SETTINGS shape (the real serialized payload).
    const SAMPLE: &str = r##"{
        "display": {
            "thumbnail_size": 200,
            "grid_gap": 2,
            "background_color": "#0a0a0a",
            "video_hover_preview": false,
            "video_autoplay_loop": false,
            "gif_autoplay_grid": false,
            "video_autoplay_grid": false,
            "video_autoplay_max_seconds": 30,
            "scroll_blur": false,
            "map_dark_mode": true,
            "justified_high_detail": true
        },
        "performance": { "preload_count": 3, "lru_cache_size": 5, "thumbnail_threads": 6 },
        "storage": { "companion_location": "lightview_folder" },
        "default_filter": { "enabled": false, "query": "" },
        "external_apps": [
            { "label": "Gwenview", "command": "gwenview", "args": ["{file}"] },
            { "label": "GIMP", "command": "gimp", "args": ["{file}"] }
        ]
    }"##;

    #[test]
    fn round_trips_through_toml() {
        let toml = json_to_toml(SAMPLE).expect("json -> toml");
        // Spot-check it's actually TOML and hand-readable.
        assert!(toml.contains("[display]"));
        assert!(toml.contains("scroll_blur = false"));
        assert!(toml.contains("[[external_apps]]"));

        let json = toml_to_json(&toml).expect("toml -> json");
        let a: serde_json::Value = serde_json::from_str(SAMPLE).unwrap();
        let b: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(a, b, "settings must survive a json->toml->json round trip");
    }
}
