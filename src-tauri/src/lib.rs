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

use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::Mutex;

use cache::atlas::ThumbAtlas;
use cache::db::CacheDb;
use autocomplete::engine::AutocompleteEngine;
use hardware::HardwareProfile;
use pipeline::thumbnailer::{ThumbFormat, ThumbnailSettings};
use provider::ProviderRegistry;

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
pub struct AppState {
    /// Active file provider registry (local, SMB, SFTP, S3)
    pub providers: Arc<RwLock<ProviderRegistry>>,

    /// SQLite cache database (thumbnails, tag index, media meta)
    /// Uses Mutex (not RwLock) because rusqlite::Connection is Send but not Sync.
    pub cache_db: Arc<Mutex<Option<CacheDb>>>,

    /// BC7 thumbnail atlas (mmap-backed, GPU-direct path).
    /// None when atlas is not in use (remote galleries, HDD, no GPU).
    pub thumb_atlas: Arc<Mutex<Option<ThumbAtlas>>>,

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

    /// Whether the BC7 atlas path is active for the current gallery.
    /// Determined by hardware profile (NVMe + discrete GPU).
    pub use_bc7_atlas: Arc<std::sync::atomic::AtomicBool>,

    /// User-configurable thumbnail settings (format, dimensions, resize filter).
    pub thumbnail_settings: Arc<RwLock<ThumbnailSettings>>,

    /// Recently opened gallery paths, persisted to disk.
    pub recent_galleries: Arc<Mutex<Vec<RecentGallery>>>,

    /// Cancellation flag for plugin batch runs.
    pub plugin_cancelled: Arc<std::sync::atomic::AtomicBool>,

    /// Read-only SQLite connection for the thumbnail protocol handler.
    /// Uses std::sync::Mutex (not tokio) because protocol handlers are synchronous.
    pub thumb_protocol_db: Arc<std::sync::Mutex<Option<rusqlite::Connection>>>,

    /// Registry of active plugin daemons, keyed by plugin name.
    /// Daemons are started on first use and kept resident for the session.
    pub plugin_daemons: Arc<Mutex<std::collections::HashMap<String, Arc<plugin::daemon::PluginDaemon>>>>,

    /// Unified GPU pipeline for accelerated thumbnail generation and image transforms.
    /// None when no suitable GPU adapter was found at startup.
    #[cfg(feature = "gpu")]
    pub gpu_pipeline: Option<Arc<pipeline::gpu_pipeline::GpuPipeline>>,
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
                    log::info!("  GPU pipeline:   ACTIVE (resize, crop+resize, BC7 encode, transform)");
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
            thumb_atlas: Arc::new(Mutex::new(None)),
            autocomplete: Arc::new(AutocompleteEngine::new()),
            hardware: Arc::new(hardware),
            current_gallery: Arc::new(RwLock::new(None)),
            thumbnail_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            thumb_pool: Arc::new(thumb_pool),
            use_bc7_atlas: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            thumbnail_settings: Arc::new(RwLock::new(ThumbnailSettings::default())),
            recent_galleries: Arc::new(Mutex::new(recent_galleries)),
            plugin_cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            plugin_daemons: Arc::new(Mutex::new(std::collections::HashMap::new())),
            thumb_protocol_db: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(feature = "gpu")]
            gpu_pipeline,
        }
    }

    /// Determine the thumbnail format based on user settings.
    /// Always returns the user-configured format; the BC7 atlas path
    /// is an orthogonal storage optimization layered on top.
    pub fn thumb_format(&self) -> ThumbFormat {
        // Use blocking read — this is only called from sync contexts or
        // from within a tokio runtime where we can block briefly.
        // The RwLock is almost never contended.
        self.thumbnail_settings.blocking_read().format
    }

    /// Check if BC7 atlas should be used based on hardware profile.
    /// Called when opening a gallery to decide the thumbnail storage strategy.
    pub fn should_use_bc7(&self) -> bool {
        use hardware::StorageType;
        matches!(self.hardware.storage_type, StorageType::NVMe | StorageType::SSD)
            && self.hardware.gpu_compute
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
