//! The domain layer: what the application does, with no HTTP in sight.
//!
//! These take state, or pieces of it, and never learn what a route is. That
//! placement is the single largest structural rule in the rebuild: the adapter
//! above them collapses from two (Tauri commands and HTTP routes, kept in step
//! by a `*_impl` naming convention) to one, and the temptation when that
//! happens is to let the domain logic fall into the adapter with it. Batch
//! thumbnail orchestration, gallery open and fs-watch, tag writes across a
//! selection, plugin lifecycle — those are ~4,500 lines of domain code, not
//! adapter code, and they live here.

pub mod duplicates;
pub mod files;
pub mod gallery;
pub mod media;
pub mod order;
pub mod settings;
pub mod tags;
pub mod trash;

/// A gallery over `root` with a fresh cache, for the services' tests.
#[cfg(test)]
pub(crate) fn test_gallery(root: &std::path::Path) -> crate::state::Gallery {
    use std::sync::Arc;

    use crate::autocomplete::engine::AutocompleteEngine;
    use crate::cache::db::CacheDb;
    use crate::path::Root;
    use crate::pipeline::serve::ThumbService;
    use crate::server::events::Events;
    use crate::services::settings::GallerySettings;

    let cache_dir = tempfile::tempdir().unwrap().keep();
    let root = Root::open(root).unwrap();
    let db = Arc::new(CacheDb::open_at(&cache_dir).unwrap());
    let pool = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap());
    let thumbs = Arc::new(ThumbService::new(db.clone(), root.clone(), pool, 1 << 20));
    crate::state::Gallery {
        root,
        db,
        thumbs,
        events: Arc::new(Events::new()),
        autocomplete: Arc::new(AutocompleteEngine::new()),
        settings: std::sync::RwLock::new(GallerySettings::default()),
        cache_dir,
        arrangeable: std::sync::atomic::AtomicBool::new(false),
    }
}
