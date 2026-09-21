//! Companion schema upgrades.
//!
//! Called unconditionally on every read, so introducing version 2 is a change
//! to this one function rather than an audit of every call site. Only version 1
//! exists, so today it is the identity.

use crate::companion::schema::CompanionFile;

/// Migrate a companion file from an older schema version to the current version.
/// Returns the migrated companion file, or the original if no migration is needed.
pub fn migrate(companion: CompanionFile) -> CompanionFile {
    // Currently only schema version 1 exists; future migrations go here.
    //
    // Example for a future v1 → v2 migration:
    // if companion.schema_version == 1 {
    //     // Apply v1 → v2 changes
    //     companion.schema_version = 2;
    // }

    companion
}
