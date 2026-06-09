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
use provider::ProviderRegistry;
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

/// Shared application state accessible from all Tauri commands.
///
/// Every field is `Arc`-wrapped, so `Clone` produces a cheap handle that
/// shares the same underlying state. This lets the HTTP server hold its own
/// `AppState` clone and observe the same live gallery as the Tauri commands.
#[derive(Clone)]
pub struct AppState {
    /// Active file provider registry (local, SMB, SFTP, S3)
    pub providers: Arc<RwLock<ProviderRegistry>>,

    /// SQLite cache database (thumbnails, tag index, media meta)
    /// Uses Mutex (not RwLock) because rusqlite::Connection is Send but not Sync.
    pub cache_db: Arc<Mutex<Option<CacheDb>>>,

    /// In-memory tag autocomplete engine
    pub autocomplete: Arc<AutocompleteEngine>,

    /// Detected hardware capabilities
    pub hardware: Arc<HardwareProfile>,

    /// Current gallery path (None if no gallery is open)
    pub current_gallery: Arc<RwLock<Option<String>>>,

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

    /// Read-only SQLite connection for the thumbnail protocol handler.
    /// Uses std::sync::Mutex (not tokio) because protocol handlers are synchronous.
    pub thumb_protocol_db: Arc<std::sync::Mutex<Option<rusqlite::Connection>>>,

    /// Deduplicates concurrent generate-on-miss thumbnail requests coming
    /// from the `lightview://thumb/...` protocol handler.
    pub thumb_gen_coalescer: Arc<ThumbGenCoalescer>,

    /// Filesystem watcher for detecting external file additions/removals.
    /// Uses std::sync::Mutex because FsWatcher uses std mpsc channels.
    pub fs_watcher: Arc<std::sync::Mutex<Option<FsWatcher>>>,

    /// Cancellation flag for the filesystem watcher background task.
    pub fs_watch_cancel: Arc<std::sync::atomic::AtomicBool>,

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
}

impl AppState {
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
            providers: Arc::new(RwLock::new(ProviderRegistry::new())),
            cache_db: Arc::new(Mutex::new(None)),
            autocomplete: Arc::new(AutocompleteEngine::new()),
            hardware: Arc::new(hardware),
            current_gallery: Arc::new(RwLock::new(None)),
            thumbnail_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            thumb_pool: Arc::new(thumb_pool),
            recent_galleries: Arc::new(Mutex::new(recent_galleries)),
            plugin_cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            thumb_protocol_db: Arc::new(std::sync::Mutex::new(None)),
            thumb_gen_coalescer: Arc::new(ThumbGenCoalescer::new()),
            fs_watcher: Arc::new(std::sync::Mutex::new(None)),
            fs_watch_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_written_settings: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(feature = "gpu")]
            gpu_pipeline,
            media_server_url: Arc::new(std::sync::OnceLock::new()),
            remote_server: Arc::new(Mutex::new(None)),
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
