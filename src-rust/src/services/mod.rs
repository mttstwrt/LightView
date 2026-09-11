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

pub mod settings;
pub mod trash;
