//! What one process holds while it is running.
//!
//! Two structs, and the split is the trust boundary. [`Gallery`] is everything
//! about the open folder — the services operate on it and know nothing above
//! it. [`AppState`] adds what the *listener* knows: which trust level it
//! grants, where machine-local state lives, how it authenticates.
//!
//! **`AppState::trust` is set once, at bind, and nothing writes it again.**
//! `lightview <dir>` and `lightview --serve <dir>` cannot both hold one
//! gallery, so a process has exactly one listener; the listener's trust is the
//! process's trust. There is no request-derived path that could raise it, which
//! is the strongest available form of "no flag widens `Owner`".

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::autocomplete::engine::AutocompleteEngine;
use crate::cache::db::CacheDb;
use crate::path::Root;
use crate::pipeline::serve::ThumbService;
use crate::server::auth::{LaunchSession, Trust};
use crate::server::config::ServerConfig;
use crate::server::devices::Devices;
use crate::server::events::Events;
use crate::services::settings::GallerySettings;
use crate::util::paths::Dirs;

/// The open gallery. Everything a service needs, and nothing about HTTP.
pub struct Gallery {
    /// Canonicalized once at open. Every relative key, the watcher's
    /// `strip_prefix` and the cache key all derive from this one value, so they
    /// cannot disagree about what "the same gallery" is.
    pub root: Root,
    pub db: Arc<CacheDb>,
    pub thumbs: Arc<ThumbService>,
    pub events: Arc<Events>,
    pub autocomplete: Arc<AutocompleteEngine>,
    /// `.lightview/settings.toml`, hot-reloaded by the watcher.
    pub settings: std::sync::RwLock<GallerySettings>,
    /// This gallery's derived-cache directory, which is also where the lock and
    /// `instance.json` live.
    pub cache_dir: PathBuf,
}

impl Gallery {
    pub fn settings(&self) -> GallerySettings {
        self.settings
            .read()
            .expect("gallery settings poisoned")
            .clone()
    }

    pub fn set_settings(&self, next: GallerySettings) {
        *self.settings.write().expect("gallery settings poisoned") = next;
    }

    /// Refresh the autocomplete vocabulary from the index, and tell clients.
    ///
    /// One aggregate over an indexed table at the moments the engine already
    /// refreshes — which is the whole replacement for the `tag_counts` table.
    pub async fn refresh_autocomplete(&self) {
        let counts = {
            let conn = self.db.read().await;
            crate::cache::index::tag_counts(&conn)
        };
        match counts {
            Ok(counts) => {
                self.autocomplete.refresh(counts).await;
                self.events.send(crate::server::events::Event::TagsIndexed);
            }
            Err(e) => log::warn!("could not refresh the tag vocabulary: {e}"),
        }
    }
}

/// The process.
pub struct AppState {
    pub gallery: Arc<Gallery>,
    /// The listener's ceiling. Written once, at bind.
    pub trust: Trust,
    pub dirs: Dirs,
    pub config: ServerConfig,
    /// Pairings. `None` on a loopback bind, which has no pairing flow at all.
    pub devices: Option<Arc<Devices>>,
    /// The launch token and session. `None` under `--serve`.
    pub launch: Option<Arc<LaunchSession>>,
    /// The origin this server answers on, for the `Origin` check. Empty under
    /// `--serve`, where a `0.0.0.0` bind has no single origin to name and
    /// `Sec-Fetch-Site` does the job instead.
    pub origin: String,
    /// **503 until the initial scan has completed and the watcher is armed.**
    ///
    /// A file arriving between "the gallery is set" and "the watcher is
    /// listening" is in neither the completed scan nor the watcher, and nothing
    /// ever notices it. The old window was milliseconds because the watcher
    /// started right after; a fresh implementation without the gate would make
    /// it the entire initial scan.
    ready: AtomicBool,
}

impl AppState {
    pub fn new(
        gallery: Arc<Gallery>,
        trust: Trust,
        dirs: Dirs,
        config: ServerConfig,
    ) -> Self {
        Self {
            gallery,
            trust,
            dirs,
            config,
            devices: None,
            launch: None,
            origin: String::new(),
            ready: AtomicBool::new(false),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Open the gate. Called **after** the watcher is armed, never before.
    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }

    /// Whether this listener may run a command needing `required`.
    pub fn allows(&self, required: Trust) -> bool {
        self.trust.allows(required)
    }
}

/// What the client is told about itself, so the UI does not offer what the
/// server will refuse.
///
/// The frontend keeps components that offer copy, move, clipboard and
/// open-with, and hides what the client cannot do — but the store that used to
/// answer that question went with the desktop/web split. Without this, ported
/// panels would offer `Owner` actions to a phone and collect 403s. The server
/// enforces regardless; this exists only so the UI does not lie.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Capabilities {
    pub trust: Trust,
    pub upload: bool,
    /// A runtime question rather than a compile-time one: the X11 clipboard
    /// backend fails on a Wayland session without XWayland, and on a process
    /// with no display at all.
    pub clipboard: bool,
}

impl AppState {
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            trust: self.trust,
            upload: self.config.uploads_enabled,
            clipboard: self.trust == Trust::Owner && crate::file_clipboard::available(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_readiness_gate_starts_closed() {
        // Open-by-default would put a file arriving during the initial scan in
        // neither the scan nor the watcher.
        let dirs = Dirs::under("/tmp/lv-state-test");
        let gallery = test_gallery();
        let state = AppState::new(gallery, Trust::Device, dirs, ServerConfig::default());
        assert!(!state.is_ready());
        state.mark_ready();
        assert!(state.is_ready());
    }

    #[test]
    fn a_served_listener_never_reports_owner_capabilities() {
        let dirs = Dirs::under("/tmp/lv-state-test");
        let state = AppState::new(
            test_gallery(),
            Trust::Device,
            dirs,
            ServerConfig::default(),
        );
        let caps = state.capabilities();
        assert_eq!(caps.trust, Trust::Device);
        assert!(!caps.clipboard, "a served bind offered the clipboard");
    }

    fn test_gallery() -> Arc<Gallery> {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let db = Arc::new(CacheDb::open_at(cache_dir.path()).unwrap());
        let pool = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap());
        let thumbs = Arc::new(ThumbService::new(
            db.clone(),
            root.clone(),
            pool,
            1024 * 1024,
        ));
        // The temp dirs are leaked deliberately: this state outlives them and
        // the test only inspects fields that do not touch the filesystem.
        let cache_path = cache_dir.keep();
        std::mem::forget(dir);
        Arc::new(Gallery {
            root,
            db,
            thumbs,
            events: Arc::new(Events::new()),
            autocomplete: Arc::new(AutocompleteEngine::new()),
            settings: std::sync::RwLock::new(GallerySettings::default()),
            cache_dir: cache_path,
        })
    }
}
