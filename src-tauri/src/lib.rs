//! LightView's backend library: `AppState` and the modules it holds together.
//!
//! Everything the process knows lives in [`AppState`], which is `Clone` over
//! shared `Arc`s so the HTTP server can hold its own handle and observe the
//! same live gallery as the Tauri commands. That is what makes
//! `lightview-headless` possible: the same state, minus the webview.
//!
//! The lock type on each field is a decision, not a default, and the two that
//! matter most:
//!
//! * `cache_db` is a `tokio::Mutex`, not an `RwLock`, because
//!   `rusqlite::Connection` is `Send` but not `Sync`. It serializes every write
//!   in the process, so nothing expensive may run while it is held.
//! * `thumb_protocol_db` is a `std::sync::RwLock` around an
//!   `Arc<ThumbProtocolPool>` because its readers are synchronous and hold the
//!   outer lock only long enough to clone the `Arc` — they must not serialize
//!   on it.
//!
//! Per-field rationale is on the fields themselves. `docs/architecture.md` has
//! the map of how the modules below fit together.

pub mod companion;
pub mod provider;
pub mod cache;
pub mod pipeline;
pub mod plugin;
pub mod filter;
pub mod autocomplete;
pub mod sort;
pub mod hardware;
pub mod commands;
pub mod util;
pub mod http_server;
pub mod tagging;
pub mod thumb_serve;
pub mod gif_serve;
pub mod file_clipboard;

use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::Mutex;

use cache::coalescer::ThumbGenCoalescer;
use cache::db::CacheDb;
use autocomplete::engine::AutocompleteEngine;
use hardware::HardwareProfile;
use provider::local::LocalProvider;
use util::fs_watch::FsWatcher;

/// Maximum number of recent galleries to remember.
const MAX_RECENT_GALLERIES: usize = 10;

/// A recently opened gallery entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecentGallery {
    pub path: String,
    /// Unix timestamp (seconds) of when this gallery was last opened.
    pub last_opened: i64,
}

/// Process-level rendering preferences, read at startup *before* GTK/WebKit
/// init (so they can't be normal per-gallery settings). Changing these
/// requires an app restart. `None` means "use the built-in default".
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RenderConfig {
    /// Enable the WebKitGTK DMA-BUF/GPU compositing path. `Some(false)` forces
    /// the software path (the old crash-avoidance default); `None` → built-in.
    pub gpu_acceleration: Option<bool>,
    /// GDK backend ("x11" or "wayland"). `None` → built-in default ("x11").
    pub gtk_backend: Option<String>,
}

fn render_config_path() -> std::path::PathBuf {
    util::paths::data_dir().join("render.json")
}

/// Load the render config, or defaults if absent/unreadable.
pub fn load_render_config() -> RenderConfig {
    match std::fs::read_to_string(render_config_path()) {
        Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
        Err(_) => RenderConfig::default(),
    }
}

/// Persist the render config. Best-effort.
pub fn save_render_config(cfg: &RenderConfig) -> Result<(), String> {
    let path = render_config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())
}

/// A small pool of read-only SQLite connections for serving cached thumbnails.
///
/// SQLite WAL mode allows many simultaneous readers, but a single `Connection`
/// behind a `Mutex` serializes them. This hands each request one of `N`
/// connections (round-robin, preferring an idle one) so reads on the thumbnail
/// hot path run in parallel instead of one-at-a-time.
pub struct ThumbProtocolPool {
    conns: Vec<std::sync::Mutex<rusqlite::Connection>>,
    next: std::sync::atomic::AtomicUsize,
}

impl ThumbProtocolPool {
    /// Open `n` read-only WAL connections to `db_path`. Returns `None` if not a
    /// single connection could be opened.
    pub fn open(db_path: &std::path::Path, n: usize) -> Option<Self> {
        let n = n.max(1);
        let mut conns = Vec::with_capacity(n);
        for _ in 0..n {
            match rusqlite::Connection::open_with_flags(
                db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            ) {
                Ok(conn) => {
                    let _ = conn.execute_batch("PRAGMA journal_mode=WAL;");
                    // 8MB page cache per connection — modest, since N of them
                    // exist and the OS page cache backs the WAL underneath.
                    let _ = conn.execute_batch("PRAGMA cache_size=-8000;");
                    // Sorts / IN-list temp tables stay in RAM, not a temp file.
                    let _ = conn.execute_batch("PRAGMA temp_store=MEMORY;");
                    // Memory-map the DB (256MB) so this read-only blob-serving
                    // connection reads thumbnails from a mapped page instead of
                    // read() syscalls + buffer copies. mmap is per-connection.
                    let _ = conn.execute_batch("PRAGMA mmap_size=268435456;");
                    conns.push(std::sync::Mutex::new(conn));
                }
                Err(e) => {
                    log::warn!(
                        "Failed to open read-only DB connection {} for protocol handler: {}",
                        conns.len(),
                        e
                    );
                }
            }
        }
        if conns.is_empty() {
            None
        } else {
            Some(Self { conns, next: std::sync::atomic::AtomicUsize::new(0) })
        }
    }

    /// Run `f` with one of the pool's connections. Picks round-robin, preferring
    /// an idle connection (`try_lock`); blocks on the chosen one if all are busy.
    pub fn with_conn<T>(&self, f: impl FnOnce(&rusqlite::Connection) -> T) -> T {
        let n = self.conns.len();
        let start = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % n;
        for off in 0..n {
            let idx = (start + off) % n;
            if let Ok(guard) = self.conns[idx].try_lock() {
                return f(&guard);
            }
        }
        let guard = self.conns[start].lock().unwrap();
        f(&guard)
    }
}

/// Shared application state accessible from all Tauri commands.
///
/// Every field is `Arc`-wrapped, so `Clone` produces a cheap handle that
/// shares the same underlying state. This lets the HTTP server hold its own
/// `AppState` clone and observe the same live gallery as the Tauri commands.
#[derive(Clone)]
pub struct AppState {
    /// File access for the open gallery. `None` when no gallery is open;
    /// set by `open_gallery` and cleared by `close_gallery`, so it tracks
    /// `current_gallery` exactly.
    pub provider: Arc<RwLock<Option<Arc<LocalProvider>>>>,

    /// SQLite cache database (thumbnails, tag index, media meta)
    /// Uses Mutex (not RwLock) because rusqlite::Connection is Send but not Sync.
    pub cache_db: Arc<Mutex<Option<CacheDb>>>,

    /// In-memory tag autocomplete engine
    pub autocomplete: Arc<AutocompleteEngine>,

    /// Detected hardware capabilities
    pub hardware: Arc<HardwareProfile>,

    /// Current gallery path (None if no gallery is open)
    pub current_gallery: Arc<RwLock<Option<String>>>,

    /// Canonicalized gallery root, resolved once at open. Used by the HTTP
    /// routes for path-confinement checks so they don't have to
    /// canonicalize the root on every request.
    pub canonical_gallery_root: Arc<RwLock<Option<std::path::PathBuf>>>,

    /// Atomic generation counter for cancelling stale thumbnail work
    pub thumbnail_generation: Arc<std::sync::atomic::AtomicU64>,

    /// Dedicated thread pool for CPU-bound thumbnail generation.
    /// Bounded to available CPU cores — all thumbnail work is dispatched here
    /// instead of spawning unbounded blocking tasks.
    pub thumb_pool: Arc<rayon::ThreadPool>,

    /// Recently opened gallery paths, persisted to disk.
    pub recent_galleries: Arc<Mutex<Vec<RecentGallery>>>,

    /// Cancellation flag for plugin batch runs.
    pub plugin_cancelled: Arc<std::sync::atomic::AtomicBool>,

    /// Pool of read-only SQLite connections for the thumbnail protocol handler
    /// and the HTTP `/thumb` route. `std::sync::RwLock` (not tokio) because the
    /// reads are synchronous; readers take the read lock only briefly to clone
    /// the inner `Arc<ThumbProtocolPool>`, then release it, so they don't
    /// serialize on the outer lock.
    pub thumb_protocol_db: Arc<std::sync::RwLock<Option<Arc<ThumbProtocolPool>>>>,

    /// Deduplicates concurrent generate-on-miss thumbnail requests coming
    /// from the `lightview://thumb/...` protocol handler.
    pub thumb_gen_coalescer: Arc<ThumbGenCoalescer>,

    /// Bounded cache of decoded ThumbHash placeholder PNGs, keyed by path.
    /// Avoids re-decoding + re-encoding the same tiny PNG on every scroll.
    /// Cleared whenever the active gallery's protocol pool is swapped.
    pub thumbhash_png_cache: Arc<thumb_serve::ThumbhashPngCache>,

    /// Filesystem watcher for detecting external file additions/removals.
    /// Uses std::sync::Mutex because FsWatcher uses std mpsc channels.
    pub fs_watcher: Arc<std::sync::Mutex<Option<FsWatcher>>>,

    /// Cancellation flag for the filesystem watcher background task.
    pub fs_watch_cancel: Arc<std::sync::atomic::AtomicBool>,

    /// Generation counter for the idle backfill worker (thumbnails + phashes).
    /// Each gallery open bumps it; a running worker exits when the count moves
    /// past the one it was started with.
    pub idle_generation: Arc<std::sync::atomic::AtomicU64>,

    /// Unix millis of the last user-driven thumbnail request (desktop protocol
    /// handler or frontend batch commands). The idle worker stays quiet until
    /// this has been stale for a while, so it never competes with an active
    /// desktop user; connected web clients are detected via `fs_change_tx`
    /// subscriber count instead.
    pub last_thumb_activity: Arc<std::sync::atomic::AtomicU64>,

    /// Broadcasts filesystem change batches to remote web clients. The desktop
    /// webview gets these via Tauri's `gallery:fs-changed` event, but a browser
    /// has no Tauri IPC, so the HTTP server's `/api/events` SSE route subscribes
    /// here and relays each batch. The fs-watcher publishes; late subscribers
    /// only see changes from their subscription onward, which is what we want.
    pub fs_change_tx: tokio::sync::broadcast::Sender<commands::gallery::FsChangeEvent>,

    /// The TOML content the app itself last wrote to `settings.toml`. The fs
    /// watcher compares against this so it only emits `settings:changed` for
    /// genuine external (hand) edits, not the app's own saves.
    pub last_written_settings: Arc<std::sync::Mutex<Option<String>>>,

    /// Unified GPU pipeline for accelerated thumbnail generation and image transforms.
    /// None when no suitable GPU adapter was found at startup.
    #[cfg(feature = "gpu")]
    pub gpu_pipeline: Option<Arc<pipeline::gpu_pipeline::GpuPipeline>>,

    /// Base URL of the local HTTP media server (e.g. `http://127.0.0.1:52431`).
    /// Set once at startup; `<video>` elements load from this server because
    /// WebKitGTK rejects non-http(s) schemes for media elements.
    pub media_server_url: Arc<std::sync::OnceLock<String>>,

    /// The running remote-access (LAN) server, if enabled. Separate from the
    /// loopback media server above so enabling remote access never adds auth
    /// to the desktop webview's own requests.
    pub remote_server: Arc<Mutex<Option<http_server::RemoteAccess>>>,

    /// Remote-tagging job queue + worker registry: web clients enqueue jobs,
    /// a paired `lightview-worker` claims them and pushes tags back. In-memory
    /// on purpose — see `tagging` module docs.
    pub tagging: Arc<Mutex<tagging::TaggingState>>,

    /// Broadcasts tagging job/worker changes to web clients; the HTTP server's
    /// `/api/events` SSE route merges this with `fs_change_tx`. Kept separate
    /// because `fs_change_tx`'s subscriber count doubles as the "web clients
    /// connected" signal for the idle backfill worker.
    pub tagging_event_tx: tokio::sync::broadcast::Sender<tagging::TaggingSseEvent>,

    /// One-shot guard for the in-process tagging executor (`tagging::local`),
    /// which is started lazily by headless `serve` / `enable_remote_access`.
    pub local_tagger_started: Arc<std::sync::atomic::AtomicBool>,

    /// Paths served from LRU-capped tiers since the last eviction pass, awaiting
    /// a batched `accessed_at` write. See [`AppState::record_tier_access`].
    /// A std mutex, not tokio's: this is taken from the synchronous serve path
    /// and only ever held for a set insert.
    pub pending_tier_accesses: Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                cache::thumbnails::ThumbTier,
                std::collections::HashSet<String>,
            >,
        >,
    >,
}

/// Per-tier ceiling on buffered access marks before the oldest batch is dropped.
/// Sized so a long browse is covered but a runaway can't grow without bound;
/// each entry is one path string.
const MAX_PENDING_TIER_ACCESSES: usize = 20_000;

impl AppState {
    /// Note that `path` was served from an LRU-capped tier, so the next
    /// eviction pass treats it as warm.
    ///
    /// Buffered in memory rather than written through: the serve path reads via
    /// the read-only connection pool and must not take the writer lock — an
    /// `UPDATE` per thumbnail request would serialize the whole grid behind the
    /// single cache-db mutex. `take_tier_accesses` drains this from a writer.
    pub fn record_tier_access(&self, tier: cache::thumbnails::ThumbTier, path: &str) {
        if !cache::thumbnails::is_lru_capped(tier) {
            return;
        }
        let mut pending = self.pending_tier_accesses.lock().unwrap();
        let set = pending.entry(tier).or_default();
        // Bound the buffer: a very long browse between eviction passes would
        // otherwise accumulate one entry per distinct path viewed. Dropping the
        // oldest marks only costs those rows their warm status.
        if set.len() >= MAX_PENDING_TIER_ACCESSES {
            set.clear();
        }
        set.insert(path.to_string());
    }

    /// Drain the buffered access marks for `tier`.
    pub fn take_tier_accesses(&self, tier: cache::thumbnails::ThumbTier) -> Vec<String> {
        let mut pending = self.pending_tier_accesses.lock().unwrap();
        pending
            .get_mut(&tier)
            .map(|set| std::mem::take(set).into_iter().collect())
            .unwrap_or_default()
    }

    /// Record user-driven thumbnail activity so the idle backfill worker backs
    /// off while someone is actively browsing (see `last_thumb_activity`).
    pub fn touch_thumb_activity(&self) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_thumb_activity
            .store(now_ms, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn new() -> Self {
        let hardware = HardwareProfile::detect();
        let num_threads = hardware.thumbnail_threads();

        log::info!("=== LightView Hardware Detection ===");
        log::info!("  Storage type:   {:?}", hardware.storage_type);
        log::info!("  Filesystem:     {}", hardware.filesystem);
        log::info!("  CPU cores:      {}", hardware.cpu_cores);
        log::info!("  RAM:            {} MB", hardware.total_ram_mb);
        log::info!("  Reflink:        {}", hardware.supports_reflink);
        log::info!("  Thumb threads:  {}", num_threads);
        log::info!("  Prefetch count: {}", hardware.prefetch_count());
        log::info!("  LRU cache size: {}", hardware.lru_cache_size());

        #[cfg(feature = "gpu")]
        let gpu_pipeline = {
            log::info!("  GPU pipeline:   probing...");
            match pipeline::gpu_pipeline::GpuPipeline::new() {
                Some(p) => {
                    log::info!("  GPU pipeline:   ACTIVE (resize, crop+resize, transform)");
                    Some(Arc::new(p))
                }
                None => {
                    log::info!("  GPU pipeline:   unavailable (CPU fallback)");
                    None
                }
            }
        };

        log::info!("====================================");

        let thumb_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .thread_name(|i| format!("thumb-{i}"))
            .build()
            .expect("Failed to create thumbnail thread pool");
        let recent_galleries = load_recent_galleries();

        Self {
            provider: Arc::new(RwLock::new(None)),
            cache_db: Arc::new(Mutex::new(None)),
            autocomplete: Arc::new(AutocompleteEngine::new()),
            hardware: Arc::new(hardware),
            current_gallery: Arc::new(RwLock::new(None)),
            canonical_gallery_root: Arc::new(RwLock::new(None)),
            thumbnail_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            thumb_pool: Arc::new(thumb_pool),
            recent_galleries: Arc::new(Mutex::new(recent_galleries)),
            plugin_cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            thumb_protocol_db: Arc::new(std::sync::RwLock::new(None)),
            thumbhash_png_cache: Arc::new(thumb_serve::ThumbhashPngCache::default()),
            thumb_gen_coalescer: Arc::new(ThumbGenCoalescer::new()),
            fs_watcher: Arc::new(std::sync::Mutex::new(None)),
            fs_watch_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            idle_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_thumb_activity: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            // Capacity bounds memory if no clients are connected; a slow client
            // that lags just gets a Lagged signal and triggers a full refresh.
            fs_change_tx: tokio::sync::broadcast::channel(64).0,
            last_written_settings: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(feature = "gpu")]
            gpu_pipeline,
            media_server_url: Arc::new(std::sync::OnceLock::new()),
            remote_server: Arc::new(Mutex::new(None)),
            tagging: Arc::new(Mutex::new(tagging::TaggingState::default())),
            tagging_event_tx: tokio::sync::broadcast::channel(64).0,
            local_tagger_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_tier_accesses: Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
        }
    }

    /// Check if the GPU pipeline is available.
    #[cfg(feature = "gpu")]
    pub fn has_gpu(&self) -> bool {
        self.gpu_pipeline.is_some()
    }

    #[cfg(not(feature = "gpu"))]
    pub fn has_gpu(&self) -> bool {
        false
    }

    /// Record a gallery as recently opened. Persists to disk.
    pub async fn add_recent_gallery(&self, path: &str) {
        let mut recents = self.recent_galleries.lock().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // Remove existing entry for this path (if any) to move it to the front
        recents.retain(|r| r.path != path);
        recents.insert(
            0,
            RecentGallery {
                path: path.to_string(),
                last_opened: now,
            },
        );
        recents.truncate(MAX_RECENT_GALLERIES);

        save_recent_galleries(&recents);
    }
}

// ---------------------------------------------------------------------------
// Recent galleries persistence
// ---------------------------------------------------------------------------

fn recent_galleries_path() -> std::path::PathBuf {
    util::paths::data_dir().join("recent.json")
}

fn load_recent_galleries() -> Vec<RecentGallery> {
    let path = recent_galleries_path();
    match std::fs::read_to_string(&path) {
        Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn save_recent_galleries(recents: &[RecentGallery]) {
    let path = recent_galleries_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(recents) {
        let _ = std::fs::write(&path, json);
    }
}
