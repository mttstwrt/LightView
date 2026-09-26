//! Plugins: what is installed, and the contract they are held to.
//!
//! A plugin is a directory under the state directory's `plugins/`, holding a
//! `manifest.json` and whatever the manifest's command needs. **The server
//! never receives or executes code**: a request carries a plugin *name*, and
//! the only thing a name can select is an actual child of the install root.
//!
//! That invariant is structural rather than checked. There is no path join:
//! [`manifest::installed`] scans the directory, and a lookup is a comparison
//! against what the scan found, so an absolute name or a `..` selects nothing.
//! A join could not be made safe by a check after it — `Path::join` with an
//! absolute argument discards the base entirely, and a `manifest.name == name`
//! comparison guards nothing, because whoever writes the manifest writes its
//! name field. The directory name is the identity; the manifest's `name` is
//! checked against it.
//!
//! **A plugin answers with tags and meta for one file, and nothing else.**
//! There is no result kind for proposed groups or findings, because nothing
//! would receive one — and a result kind nothing receives is the shape of a
//! silent bug. The one decision grouping needed early is made: a confirmed
//! group name is a `set::` tag, and grouping by selection works today through
//! the tag commands.

pub mod input;
pub mod manifest;
pub mod run;
pub mod runner;
