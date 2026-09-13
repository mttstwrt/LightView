//! Plugins: what is installed, and the contract they are held to.
//!
//! A plugin is a directory under the state directory's `plugins/`, holding a
//! `manifest.json` and whatever the manifest's command needs. **The server
//! never receives or executes code**: a request carries a plugin *name*, and
//! the only thing a name can select is an actual child of the install root.
//!
//! That invariant is structural rather than checked. The implementation this
//! replaces resolved a name with `plugin_dir.join(name)` — and `Path::join`
//! with an absolute argument discards the base entirely, while `..` walks out
//! of it. The `manifest.name == name` check that followed was not a guard,
//! because whoever writes the manifest writes its name field. There is no join
//! here at all: [`installed`] scans the directory and a lookup is a comparison
//! against what the scan found, so a name that is not one of those selects
//! nothing.

pub mod input;
pub mod manifest;
pub mod run;
pub mod runner;
