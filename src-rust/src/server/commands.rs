//! The one command table.
//!
//! `(name, argument struct, minimum trust)`, one arm per command, and **the
//! trust level is the first line of the arm rather than a lookup in a second
//! list**. That is the whole replacement for a 78-command registration plus a
//! 46-arm allowlist that had to agree with it by hand — a convention that
//! could, and did, drift.
//!
//! Three placements were decisions rather than omissions, and each is stated
//! where it is made below. **`restore_trash` is `Device`**: it writes a file
//! back to a path the user already chose, which is the inverse of a delete the
//! same client was allowed to make. **`purge_trash` is `Owner`**, because
//! permanent deletion is not "move to trash" and a remote client gets
//! move-to-trash. **`merge_duplicates` is `Owner`**, because it rewrites a
//! companion, stamps the keeper's mtime on disk and trashes the others; a
//! remote client may *find* duplicates and see the candidates, and may not
//! resolve them.
//!
//! There is no third tier. The bootstrap routes (`/healthz`, `/cert`,
//! `/pair/redeem`, `/auth/*`) are unauthenticated by necessity — there would
//! otherwise be no way past the auth layer the first time — but they are a
//! route group, not a trust level: **no command is ever reachable
//! unauthenticated.**

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::path::RelPath;
use crate::server::auth::Trust;
use crate::services::tags::WritableNamespace;
use crate::services::{duplicates, files, media, settings, tags, trash};
use crate::state::AppState;

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("no such command: {0}")]
    UnknownCommand(String),
    /// **403, and deliberately not the 404 the path rule uses.** A path 404
    /// hides whether a file exists; a command name is not secret — the whole
    /// table ships inside the SPA, and `get_capabilities` tells every client
    /// its own trust level. So there is nothing to conceal here, and a 403 is
    /// what makes a misconfigured deployment diagnosable instead of looking
    /// like a routing bug.
    #[error("forbidden")]
    Forbidden,
    #[error("bad arguments: {0}")]
    BadArguments(String),
    #[error("{0}")]
    Failed(String),
}

/// Run one command.
pub async fn dispatch(
    state: &Arc<AppState>,
    name: &str,
    args: Value,
) -> Result<Value, CommandError> {
    let gallery = state.gallery.clone();

    match name {
        // ---- Reading the gallery -------------------------------------------
        "get_capabilities" => {
            require(state, Trust::Device)?;
            Ok(json!(state.capabilities()))
        }
        "get_items" => {
            require(state, Trust::Device)?;
            let request: media::ItemsRequest = parse(args)?;
            let items = media::get_items(&gallery, &request).await.map_err(failed)?;
            Ok(json!(items))
        }
        "get_media_meta" => {
            require(state, Trust::Device)?;
            let a: PathArg = parse(args)?;
            let meta = media::get_media_meta(&gallery, &a.path)
                .await
                .map_err(failed)?;
            Ok(json!(meta))
        }
        "get_all_thumbnail_tiers" => {
            require(state, Trust::Device)?;
            let a: PathArg = parse(args)?;
            Ok(json!(media::get_tiers(&gallery, &a.path).await.map_err(failed)?))
        }
        "get_tier_totals" => {
            require(state, Trust::Device)?;
            let totals = media::tier_totals(&gallery).await.map_err(failed)?;
            Ok(json!(totals
                .into_iter()
                .map(|(tier, bytes)| json!({ "tier": tier, "bytes": bytes }))
                .collect::<Vec<_>>()))
        }
        "autocomplete" => {
            require(state, Trust::Device)?;
            let a: AutocompleteArgs = parse(args)?;
            Ok(json!(
                media::autocomplete(&gallery, &a.query, a.namespace.as_deref(), a.limit).await
            ))
        }
        "get_settings" => {
            require(state, Trust::Device)?;
            Ok(json!(gallery.settings()))
        }
        "set_default_filter" => {
            // `Device`, because under `--serve` the phone is the only UI there
            // is — and the default filter is user intent, which is why it lives
            // in the durable `settings.toml` rather than the derived database.
            require(state, Trust::Device)?;
            let a: FilterArg = parse(args)?;
            let next = settings::GallerySettings::set_default_filter(
                gallery.root.as_path(),
                &a.filter,
            )
            .map_err(failed)?;
            gallery.set_settings(next.clone());
            Ok(json!(next))
        }

        // ---- Writing metadata ----------------------------------------------
        "list_tags" => {
            require(state, Trust::Device)?;
            let a: NamespaceArg = parse(args)?;
            Ok(json!(tags::list(&gallery, a.namespace).await))
        }
        "paths_with_tags" => {
            require(state, Trust::Device)?;
            let a: PathsForTagsArgs = parse(args)?;
            Ok(json!(
                tags::paths_with_tags(&gallery, &a.tags, a.namespace, a.limit)
                    .await
                    .map_err(failed)?
            ))
        }
        "add_tags" => {
            require(state, Trust::Device)?;
            let a: TagWrite = parse(args)?;
            let n = tags::add(&gallery, &a.paths, &a.tags, a.namespace)
                .await
                .map_err(failed)?;
            Ok(json!({ "changed": n }))
        }
        "remove_tags" => {
            require(state, Trust::Device)?;
            let a: TagWrite = parse(args)?;
            let n = tags::remove(&gallery, &a.paths, &a.tags, a.namespace)
                .await
                .map_err(failed)?;
            Ok(json!({ "changed": n }))
        }
        "rename_tag" => {
            require(state, Trust::Device)?;
            let a: TagRename = parse(args)?;
            let n = tags::rename(&gallery, &a.from, &a.to, a.namespace)
                .await
                .map_err(failed)?;
            Ok(json!({ "changed": n }))
        }
        "merge_tags" => {
            require(state, Trust::Device)?;
            let a: TagMerge = parse(args)?;
            let n = tags::merge(&gallery, &a.sources, &a.target, a.namespace)
                .await
                .map_err(failed)?;
            Ok(json!({ "changed": n }))
        }
        "delete_tag" => {
            require(state, Trust::Device)?;
            let a: TagDelete = parse(args)?;
            let n = tags::delete(&gallery, &a.tag, a.namespace)
                .await
                .map_err(failed)?;
            Ok(json!({ "changed": n }))
        }
        "set_rating" => {
            require(state, Trust::Device)?;
            let a: RatingArgs = parse(args)?;
            tags::set_rating(&gallery, &a.paths, a.rating)
                .await
                .map_err(failed)?;
            Ok(json!({ "ok": true }))
        }
        "set_color_label" => {
            require(state, Trust::Device)?;
            let a: ColorArgs = parse(args)?;
            tags::set_color_label(&gallery, &a.paths, a.color_label)
                .await
                .map_err(failed)?;
            Ok(json!({ "ok": true }))
        }
        "set_notes" => {
            require(state, Trust::Device)?;
            let a: NotesArgs = parse(args)?;
            tags::set_notes(&gallery, &a.path, a.notes)
                .await
                .map_err(failed)?;
            Ok(json!({ "ok": true }))
        }
        "record_view" => {
            require(state, Trust::Device)?;
            let a: PathArg = parse(args)?;
            tags::record_view(&gallery, &a.path).await.map_err(failed)?;
            Ok(json!({ "ok": true }))
        }

        // ---- Thumbnails -----------------------------------------------------
        "regenerate_thumbnail" => {
            require(state, Trust::Device)?;
            let a: PathsArg = parse(args)?;
            media::regenerate(&gallery, &a.paths).await.map_err(failed)?;
            Ok(json!({ "ok": true }))
        }
        "precache_thumbnails" => {
            require(state, Trust::Device)?;
            let a: PrecacheArgs = parse(args)?;
            media::precache(&gallery, a.tier, &a.paths).await;
            Ok(json!({ "ok": true }))
        }

        // ---- Trash -----------------------------------------------------------
        "trash_files" => {
            require(state, Trust::Device)?;
            let a: PathsArg = parse(args)?;
            let root = gallery.root.clone();
            let paths = a.paths.clone();
            let id = tokio::task::spawn_blocking(move || trash::move_to_trash(&root, &paths))
                .await
                .map_err(failed)?
                .map_err(failed)?;
            {
                let conn = gallery.db.writer().await;
                for path in &a.paths {
                    crate::cache::db::forget_path(&conn, path).map_err(failed)?;
                }
            }
            gallery.events.send(crate::server::events::Event::FsChanged {
                added: Vec::new(),
                removed: a.paths,
            });
            Ok(json!({ "entry": id }))
        }
        "list_trash" => {
            require(state, Trust::Device)?;
            let root = gallery.root.clone();
            let entries = tokio::task::spawn_blocking(move || trash::list(&root))
                .await
                .map_err(failed)?
                .map_err(failed)?;
            Ok(json!(entries))
        }
        "restore_trash" => {
            // `Device`: writing a file back to a path the user already chose is
            // the inverse of a delete the same client was allowed to make. The
            // id stays opaque and the location travels in its own field, which
            // is what keeps this from being an arbitrary-file-move primitive.
            require(state, Trust::Device)?;
            let a: RestoreArgs = parse(args)?;
            let root = gallery.root.clone();
            let (id, rel) = (a.id.clone(), a.relative_path.clone());
            tokio::task::spawn_blocking(move || trash::restore(&root, &id, &rel))
                .await
                .map_err(failed)?
                .map_err(failed)?;
            crate::services::gallery::index_one(&gallery, &a.relative_path)
                .await
                .map_err(failed)?;
            gallery.events.send(crate::server::events::Event::FsChanged {
                added: vec![a.relative_path],
                removed: Vec::new(),
            });
            Ok(json!({ "ok": true }))
        }
        "purge_trash" => {
            // `Owner`: permanent deletion is not "move to trash", and a remote
            // client gets move-to-trash.
            require(state, Trust::Owner)?;
            let a: PurgeArgs = parse(args)?;
            let root = gallery.root.clone();
            match a.entry {
                Some(id) => {
                    tokio::task::spawn_blocking(move || trash::purge_entry(&root, &id))
                        .await
                        .map_err(failed)?
                        .map_err(failed)?;
                    Ok(json!({ "purged": 1 }))
                }
                // No entry named: empty the trash. Not "purge what the
                // retention window has expired" — that sweep runs once when
                // the gallery opens and needs no command, and a button called
                // Empty Trash that left last week's deletions in place would
                // be lying about what it did.
                None => {
                    let n = tokio::task::spawn_blocking(move || trash::purge_all(&root))
                        .await
                        .map_err(failed)?
                        .map_err(failed)?;
                    Ok(json!({ "purged": n }))
                }
            }
        }

        // ---- Duplicates -------------------------------------------------------
        "find_duplicates" => {
            require(state, Trust::Device)?;
            let a: DuplicateArgs = parse(args)?;
            let groups = duplicates::find(&gallery, a.threshold)
                .await
                .map_err(failed)?;
            Ok(json!(groups))
        }
        "get_merge_candidates" => {
            require(state, Trust::Device)?;
            let a: PathsArg = parse(args)?;
            Ok(json!(
                media::merge_candidates(&gallery, &a.paths)
                    .await
                    .map_err(failed)?
            ))
        }
        "merge_duplicates" => {
            // `Owner`: it rewrites a companion, stamps an mtime on disk and
            // trashes files. A remote client may find duplicates and see the
            // candidates, and may not resolve them.
            require(state, Trust::Owner)?;
            let plan: duplicates::MergePlan = parse(args)?;
            Ok(json!(duplicates::merge(&gallery, plan).await.map_err(failed)?))
        }

        // ---- Filesystem: the whole content of `Owner` -------------------------
        "copy_files" => {
            require(state, Trust::Owner)?;
            let a: TransferArgs = parse(args)?;
            let n = files::copy(&gallery, &a.paths, &a.destination).map_err(failed)?;
            Ok(json!({ "count": n }))
        }
        "move_files" => {
            require(state, Trust::Owner)?;
            let a: TransferArgs = parse(args)?;
            let n = files::move_files(&gallery, &a.paths, &a.destination).map_err(failed)?;
            {
                let conn = gallery.db.writer().await;
                for path in &a.paths {
                    crate::cache::db::forget_path(&conn, path).map_err(failed)?;
                }
            }
            gallery.events.send(crate::server::events::Event::FsChanged {
                added: Vec::new(),
                removed: a.paths,
            });
            Ok(json!({ "count": n }))
        }
        "clipboard_files" => {
            require(state, Trust::Owner)?;
            let a: ClipboardArgs = parse(args)?;
            files::clipboard(&gallery, &a.paths, a.cut).map_err(failed)?;
            Ok(json!({ "ok": true }))
        }
        "open_with" => {
            // The wire carries an **integer**, never a program name.
            require(state, Trust::Owner)?;
            let a: OpenWithArgs = parse(args)?;
            files::open_with(
                &gallery,
                &state.config.external_apps,
                a.app_index,
                &a.path,
            )
            .map_err(failed)?;
            Ok(json!({ "ok": true }))
        }
        "list_external_apps" => {
            require(state, Trust::Owner)?;
            Ok(json!(state
                .config
                .external_apps
                .iter()
                .map(|a| json!({ "label": a.label }))
                .collect::<Vec<_>>()))
        }
        // ---- Plugins ---------------------------------------------------------
        "list_plugins" => {
            // `Device`: a paired client may see what is installed, and under
            // `--serve` the honest answer is an empty list — plugins live
            // beside the viewer, and the server could not run the models
            // anyway. Labels only; no command, path or argument crosses the
            // wire in either direction.
            require(state, Trust::Device)?;
            let root = state.dirs.plugins();
            let listed = tokio::task::spawn_blocking(move || {
                crate::plugin::manifest::installed(&root)
                    .iter()
                    .map(crate::plugin::manifest::PluginInfo::from)
                    .collect::<Vec<_>>()
            })
            .await
            .map_err(failed)?;
            Ok(json!(listed))
        }

        "list_dirs" => {
            require(state, Trust::Owner)?;
            let a: DirsArgs = parse(args)?;
            // No path means the gallery root: it is the one directory the
            // server always has, and it is where a copy or move destination is
            // usually picked from. The client walks up from there through the
            // `parent` the listing carries, so it never has to know an
            // absolute path to start.
            let start = a
                .path
                .unwrap_or_else(|| gallery.root.as_path().to_path_buf());
            Ok(json!(files::list_dirs(&start).map_err(failed)?))
        }

        other => Err(CommandError::UnknownCommand(other.to_string())),
    }
}

/// The trust check. One line at the top of every arm.
fn require(state: &AppState, required: Trust) -> Result<(), CommandError> {
    if state.allows(required) {
        Ok(())
    } else {
        Err(CommandError::Forbidden)
    }
}

fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, CommandError> {
    serde_json::from_value(args).map_err(|e| CommandError::BadArguments(e.to_string()))
}

fn failed(e: impl std::fmt::Display) -> CommandError {
    CommandError::Failed(e.to_string())
}

// ---------------------------------------------------------------------------
// Argument shapes. Every path is a `RelPath`, which validates on deserialize.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PathArg {
    path: RelPath,
}

#[derive(Deserialize)]
struct PathsArg {
    paths: Vec<RelPath>,
}

#[derive(Deserialize)]
struct FilterArg {
    filter: String,
}

#[derive(Deserialize)]
struct AutocompleteArgs {
    query: String,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    20
}

#[derive(Deserialize)]
struct TagWrite {
    paths: Vec<RelPath>,
    tags: Vec<String>,
    namespace: WritableNamespace,
}

#[derive(Deserialize)]
struct TagRename {
    from: String,
    to: String,
    namespace: WritableNamespace,
}

#[derive(Deserialize)]
struct TagMerge {
    sources: Vec<String>,
    target: String,
    namespace: WritableNamespace,
}

#[derive(Deserialize)]
struct TagDelete {
    tag: String,
    namespace: WritableNamespace,
}

#[derive(Deserialize)]
struct NamespaceArg {
    namespace: WritableNamespace,
}

#[derive(Deserialize)]
struct PathsForTagsArgs {
    tags: Vec<String>,
    namespace: WritableNamespace,
    #[serde(default = "default_preview_limit")]
    limit: usize,
}

fn default_preview_limit() -> usize {
    120
}

#[derive(Deserialize)]
struct RatingArgs {
    /// A selection, because every write in the system takes one — rating one
    /// photo is a selection of one.
    paths: Vec<RelPath>,
    rating: Option<u8>,
}

#[derive(Deserialize)]
struct ColorArgs {
    paths: Vec<RelPath>,
    color_label: Option<String>,
}

#[derive(Deserialize)]
struct NotesArgs {
    path: RelPath,
    notes: Option<String>,
}

#[derive(Deserialize)]
struct PrecacheArgs {
    tier: crate::cache::tiers::ThumbTier,
    paths: Vec<RelPath>,
}

#[derive(Deserialize)]
struct RestoreArgs {
    /// `<epoch_ms>_<seq>`, digits and underscores. Never a path.
    id: String,
    relative_path: RelPath,
}

#[derive(Deserialize)]
struct PurgeArgs {
    /// One entry, or `None` for "everything past the retention window".
    #[serde(default)]
    entry: Option<String>,
}

#[derive(Deserialize)]
struct DuplicateArgs {
    #[serde(default = "default_threshold")]
    threshold: u32,
}

fn default_threshold() -> u32 {
    duplicates::DEFAULT_THRESHOLD
}

/// A destination is **absolute and unconfined**, which is the entire content of
/// the `Owner` level. Sources stay `RelPath`.
#[derive(Deserialize)]
struct TransferArgs {
    paths: Vec<RelPath>,
    destination: std::path::PathBuf,
}

#[derive(Deserialize)]
struct ClipboardArgs {
    paths: Vec<RelPath>,
    #[serde(default)]
    cut: bool,
}

#[derive(Deserialize)]
struct OpenWithArgs {
    /// An index into server-side configuration. There is no field here that
    /// could name a program.
    app_index: usize,
    path: RelPath,
}

#[derive(Deserialize)]
struct DirsArgs {
    /// Absent means "the gallery root" — see the command arm.
    #[serde(default)]
    path: Option<std::path::PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_traversal_in_a_command_argument_fails_to_deserialize() {
        // Not a check inside a handler — the argument type itself refuses it,
        // so there is no handler that could forget.
        let hostile = json!({ "paths": ["../../etc/passwd"] });
        assert!(serde_json::from_value::<PathsArg>(hostile).is_err());

        let ok = json!({ "paths": ["2026/january/a.jpg"] });
        assert!(serde_json::from_value::<PathsArg>(ok).is_ok());
    }

    #[test]
    fn a_restore_id_is_a_string_and_the_location_is_a_separate_field() {
        // The shape is what closes the arbitrary-file-move chain: an id that
        // could carry slashes would force the removal of the check that stops
        // it.
        let args: RestoreArgs = serde_json::from_value(json!({
            "id": "1700000000000_0",
            "relative_path": "2026/a.jpg"
        }))
        .unwrap();
        assert_eq!(args.id, "1700000000000_0");
        assert_eq!(args.relative_path.as_str(), "2026/a.jpg");

        // A path-shaped relative_path is refused by its own type.
        assert!(serde_json::from_value::<RestoreArgs>(json!({
            "id": "1700000000000_0",
            "relative_path": "../../../../tmp/evil/manifest.json"
        }))
        .is_err());
    }

    #[test]
    fn open_with_cannot_carry_a_program() {
        let args: OpenWithArgs =
            serde_json::from_value(json!({ "app_index": 0, "path": "a.jpg" })).unwrap();
        assert_eq!(args.app_index, 0);
        // There is no `command` field to populate, so this deserializes by
        // ignoring it rather than honouring it.
        let sneaky: OpenWithArgs = serde_json::from_value(
            json!({ "app_index": 0, "path": "a.jpg", "command": "/bin/sh" }),
        )
        .unwrap();
        assert_eq!(sneaky.app_index, 0);
    }

    #[test]
    fn a_tag_write_must_name_a_writable_namespace() {
        assert!(serde_json::from_value::<TagWrite>(json!({
            "paths": ["a.jpg"], "tags": ["x"], "namespace": "set"
        }))
        .is_ok());
        assert!(serde_json::from_value::<TagWrite>(json!({
            "paths": ["a.jpg"], "tags": ["x"], "namespace": "plugin.wd"
        }))
        .is_err());
        // Omitted entirely, rather than defaulting to `user`: a write with no
        // stated destination is a bug in the caller.
        assert!(serde_json::from_value::<TagWrite>(json!({
            "paths": ["a.jpg"], "tags": ["x"]
        }))
        .is_err());
    }
}
