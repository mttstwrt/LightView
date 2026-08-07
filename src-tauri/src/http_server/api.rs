//! Read-only command bridge for the web client.
//!
//! A remote browser has no Tauri IPC, so `POST /api/invoke` maps a
//! `{ command, args }` payload to the same domain logic the `#[tauri::command]`
//! handlers use (the shared `*_impl` functions). Only the commands in the
//! `dispatch` match are reachable — anything not listed is rejected with 403
//! even if a client forges the name.
//!
//! The allowlist is mostly read-only, plus a small set of metadata writes
//! (tag add/remove and rating, per-item and batch, the gallery-wide user-tag
//! rename/merge/delete behind the tag manager, plus the `last_viewed`
//! stamp behind the "Recently Viewed" sort) so a paired device can edit
//! from the viewer and multi-select, and the app-managed trash commands —
//! reversible deletes, additionally gated by the per-gallery
//! `remote.allow_delete` flag. Host-level operations — file ops (copy/move)
//! and plugins — remain off the list. Remote write access is gated by the
//! auth layer (device pairing + optional password); this allowlist is the
//! second boundary that bounds *what* a paired device can do.
//!
//! Arg structs use `rename_all = "camelCase"` so the JSON the frontend already
//! builds for Tauri's `invoke()` (which camelCases snake_case params) works
//! unchanged through this bridge.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

use super::server::ServerState;
use crate::commands::geo::GeoBbox;
use crate::sort::grouper::GroupBy;
use crate::sort::sorter::{SortField, SortOrder};
use crate::AppState;

#[derive(Deserialize)]
pub struct InvokeRequest {
    pub command: String,
    #[serde(default)]
    pub args: Value,
}

enum DispatchError {
    /// Command is not in the read-only allowlist.
    NotAllowed,
    /// Args JSON did not deserialize into the expected shape.
    BadArgs(serde_json::Error),
    /// The command ran but returned an error.
    Command(String),
}

/// POST /api/invoke
pub async fn invoke(
    State(state): State<ServerState>,
    Json(req): Json<InvokeRequest>,
) -> Response {
    match dispatch(&state.app, &req.command, req.args).await {
        Ok(value) => Json(value).into_response(),
        Err(DispatchError::NotAllowed) => (
            StatusCode::FORBIDDEN,
            format!("command not permitted: {}", req.command),
        )
            .into_response(),
        Err(DispatchError::BadArgs(e)) => {
            (StatusCode::BAD_REQUEST, format!("bad args: {e}")).into_response()
        }
        Err(DispatchError::Command(e)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, DispatchError> {
    serde_json::from_value(args).map_err(DispatchError::BadArgs)
}

fn ok<T: Serialize>(result: Result<T, String>) -> Result<Value, DispatchError> {
    let value = result.map_err(DispatchError::Command)?;
    serde_json::to_value(value).map_err(|e| DispatchError::Command(e.to_string()))
}

async fn dispatch(app: &AppState, command: &str, args: Value) -> Result<Value, DispatchError> {
    use crate::commands::{
        autocomplete, duplicates, filter, gallery, geo, media, plugins, settings, sort, tags,
        trash, viewer,
    };

    match command {
        "get_gallery_info" => ok(gallery::get_gallery_info_impl(app).await),

        // Single-round-trip boot: gallery info + default filter + sorted
        // items. The phone client's whole first paint hangs on this.
        "get_boot_state" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct A {
                sort_field: SortField,
                sort_order: SortOrder,
                group_by: GroupBy,
                sub_sort_field: Option<SortField>,
                sub_sort_order: Option<SortOrder>,
            }
            let a: A = parse(args)?;
            ok(gallery::get_boot_state_impl(
                app,
                a.sort_field,
                a.sort_order,
                a.group_by,
                a.sub_sort_field,
                a.sub_sort_order,
            )
            .await)
        }

        "get_gallery_default_filter" => {
            ok(settings::get_gallery_default_filter_impl(app).await)
        }

        // Read-only: lets the web client decide whether to show the upload
        // button and album field. Writing the config remains desktop-only.
        "get_upload_config" => ok(settings::get_upload_config_impl(app).await),

        // Read-only: what this client may do, so the UI hides unavailable
        // actions instead of surfacing 403s. Enforcement stays in this match.
        "get_server_capabilities" => ok(settings::get_server_capabilities_impl(app).await),

        "get_sorted_items" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct A {
                sort_field: SortField,
                sort_order: SortOrder,
                group_by: GroupBy,
                filter_paths: Option<Vec<String>>,
                sub_sort_field: Option<SortField>,
                sub_sort_order: Option<SortOrder>,
            }
            let a: A = parse(args)?;
            ok(sort::get_sorted_items_impl(
                app,
                a.sort_field,
                a.sort_order,
                a.group_by,
                a.filter_paths,
                a.sub_sort_field,
                a.sub_sort_order,
            )
            .await)
        }

        "get_timeline_index" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct A {
                items_per_row: usize,
            }
            let a: A = parse(args)?;
            ok(sort::get_timeline_index_impl(app, a.items_per_row).await)
        }

        "apply_filter" => {
            #[derive(Deserialize)]
            struct A {
                query: String,
            }
            let a: A = parse(args)?;
            ok(filter::apply_filter_impl(app, a.query).await)
        }

        "clear_filter" => ok(filter::clear_filter_impl(app).await),

        "get_media_meta" => {
            #[derive(Deserialize)]
            struct A {
                path: String,
            }
            let a: A = parse(args)?;
            ok(media::get_media_meta_impl(app, a.path).await)
        }

        // Server-side compute; returns the fresh thumbnail info so the web
        // client can cache-bust its <img> URL (no Tauri event channel here).
        "regenerate_thumbnail" => {
            #[derive(Deserialize)]
            struct A {
                path: String,
            }
            let a: A = parse(args)?;
            ok(media::regenerate_thumbnail_impl(app, a.path).await)
        }

        // Generation-only writes to the thumbnail cache — the same work the
        // /thumb route already does per-request on a miss, just batched.
        // Needed by the web grids' look-ahead warming and the settings
        // "Generate Missing Thumbnails" maintenance pass.
        "precache_thumbnails" => {
            #[derive(Deserialize)]
            struct A {
                paths: Vec<String>,
            }
            let a: A = parse(args)?;
            ok(media::precache_thumbnails_impl(app, a.paths).await)
        }

        "ensure_tier_thumbnails" => {
            #[derive(Deserialize)]
            struct A {
                paths: Vec<String>,
                tier: String,
            }
            let a: A = parse(args)?;
            ok(media::ensure_tier_thumbnails_impl(app, a.paths, a.tier).await)
        }

        "get_thumbhashes" => {
            #[derive(Deserialize)]
            struct A {
                paths: Vec<String>,
            }
            let a: A = parse(args)?;
            ok(media::get_thumbhashes_impl(app, a.paths).await)
        }

        "get_tags" => {
            #[derive(Deserialize)]
            struct A {
                path: String,
            }
            let a: A = parse(args)?;
            ok(tags::get_tags_impl(app, a.path).await)
        }

        "get_geo_points" => {
            #[derive(Deserialize)]
            struct A {
                bbox: GeoBbox,
                zoom: u8,
                filter: Option<String>,
            }
            let a: A = parse(args)?;
            ok(geo::get_geo_points_impl(app, a.bbox, a.zoom, a.filter).await)
        }

        "get_geo_paths" => {
            #[derive(Deserialize)]
            struct A {
                bbox: GeoBbox,
                filter: Option<String>,
            }
            let a: A = parse(args)?;
            ok(geo::get_geo_paths_impl(app, a.bbox, a.filter).await)
        }

        "autocomplete_tags" => {
            #[derive(Deserialize)]
            struct A {
                query: String,
                namespace: Option<String>,
                limit: Option<usize>,
            }
            let a: A = parse(args)?;
            ok(autocomplete::autocomplete_tags_impl(app, a.query, a.namespace, a.limit).await)
        }

        "get_recent_tags" => ok(autocomplete::get_recent_tags_impl(app).await),

        // --- Per-item metadata writes (allowed for paired devices) ---------
        "add_user_tag" => {
            #[derive(Deserialize)]
            struct A {
                path: String,
                tag: String,
            }
            let a: A = parse(args)?;
            ok(tags::add_user_tag_impl(app, a.path, a.tag).await)
        }

        "remove_user_tag" => {
            #[derive(Deserialize)]
            struct A {
                path: String,
                tag: String,
            }
            let a: A = parse(args)?;
            ok(tags::remove_user_tag_impl(app, a.path, a.tag).await)
        }

        "set_rating" => {
            #[derive(Deserialize)]
            struct A {
                path: String,
                rating: u8,
            }
            let a: A = parse(args)?;
            ok(tags::set_rating_impl(app, a.path, a.rating).await)
        }

        // Stamps `last_viewed` so the "Recently Viewed" sort reflects what was
        // opened on a phone, not just on the desktop. Writes one timestamp
        // column for a path the client can already read — the narrowest write
        // on this list.
        "record_view" => {
            #[derive(Deserialize)]
            struct A {
                path: String,
            }
            let a: A = parse(args)?;
            ok(viewer::record_view_impl(app, a.path).await)
        }

        "add_user_tag_batch" => {
            #[derive(Deserialize)]
            struct A {
                paths: Vec<String>,
                tag: String,
            }
            let a: A = parse(args)?;
            ok(tags::add_user_tag_batch_impl(app, a.paths, a.tag).await)
        }

        "remove_user_tag_batch" => {
            #[derive(Deserialize)]
            struct A {
                paths: Vec<String>,
                tag: String,
            }
            let a: A = parse(args)?;
            ok(tags::remove_user_tag_batch_impl(app, a.paths, a.tag).await)
        }

        "set_rating_batch" => {
            #[derive(Deserialize)]
            struct A {
                paths: Vec<String>,
                rating: u8,
            }
            let a: A = parse(args)?;
            ok(tags::set_rating_batch_impl(app, a.paths, a.rating).await)
        }

        // --- Gallery-wide tag management -----------------------------------
        // Still the metadata-write tier — these only rewrite the `user` tag
        // list inside companion sidecars, exactly like add/remove_user_tag —
        // but they fan out over every file carrying the tag, so a mistake is
        // wide. The UI confirms before calling; no media file is touched.
        "list_user_tags" => ok(tags::list_user_tags_impl(app).await),

        // Read-only: which files carry these tags (the manager's preview).
        "paths_for_user_tags" => {
            #[derive(Deserialize)]
            struct A {
                tags: Vec<String>,
                #[serde(default)]
                limit: Option<usize>,
            }
            let a: A = parse(args)?;
            ok(tags::paths_for_user_tags_impl(app, a.tags, a.limit).await)
        }

        "rename_user_tag" => {
            #[derive(Deserialize)]
            struct A {
                from: String,
                to: String,
            }
            let a: A = parse(args)?;
            ok(tags::rename_user_tag_impl(app, a.from, a.to).await)
        }

        "merge_user_tags" => {
            #[derive(Deserialize)]
            struct A {
                sources: Vec<String>,
                target: String,
            }
            let a: A = parse(args)?;
            ok(tags::merge_user_tags_impl(app, a.sources, a.target).await)
        }

        "delete_user_tags" => {
            #[derive(Deserialize)]
            struct A {
                tags: Vec<String>,
            }
            let a: A = parse(args)?;
            ok(tags::delete_user_tags_impl(app, a.tags).await)
        }

        // --- Remote tag application (worker tagging) ------------------------
        // A paired machine that ran an ML tagger locally pushes results here.
        // Metadata-write tier — same trust as add_user_tag_batch. Paths are
        // confined to the gallery inside the impl.
        "apply_plugin_tags" => {
            #[derive(Deserialize)]
            struct A {
                entries: Vec<plugins::PluginTagWrite>,
            }
            let a: A = parse(args)?;
            ok(plugins::apply_plugin_tags_impl(app, a.entries).await)
        }

        // --- Remote tagging job queue (worker + web client) -----------------
        // Coordination only — the server never runs a plugin for a remote
        // device. Web clients enqueue/cancel/observe; a paired lightview-worker
        // announces, claims, and reports progress, pushing the actual tag
        // writes through apply_plugin_tags above. Same metadata-write tier.
        // See docs/remote/worker-tagging.md.
        "worker_announce" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct A {
                worker_id: String,
                worker_name: String,
                plugins: Vec<plugins::PluginInfo>,
            }
            let a: A = parse(args)?;
            // The in-process executor's id is reserved — a remote device must
            // not be able to impersonate "the server" in the registry.
            if a.worker_id == crate::tagging::local::LOCAL_WORKER_ID {
                return Err(DispatchError::Command(format!(
                    "worker id '{}' is reserved",
                    a.worker_id
                )));
            }
            ok(crate::tagging::announce_worker(app, a.worker_id, a.worker_name, a.plugins, false)
                .await)
        }

        "claim_tagging_job" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct A {
                worker_id: String,
                plugin_names: Vec<String>,
            }
            let a: A = parse(args)?;
            ok(crate::tagging::claim_job(app, a.worker_id, a.plugin_names).await)
        }

        "update_tagging_job" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct A {
                job_id: String,
                worker_id: String,
                completed: usize,
                failed: usize,
            }
            let a: A = parse(args)?;
            ok(crate::tagging::update_job(app, a.job_id, a.worker_id, a.completed, a.failed).await)
        }

        "complete_tagging_job" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct A {
                job_id: String,
                worker_id: String,
                succeeded: usize,
                failed: usize,
            }
            let a: A = parse(args)?;
            ok(crate::tagging::complete_job(app, a.job_id, a.worker_id, a.succeeded, a.failed)
                .await)
        }

        "fail_tagging_job" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct A {
                job_id: String,
                worker_id: String,
                error: String,
            }
            let a: A = parse(args)?;
            ok(crate::tagging::fail_job(app, a.job_id, a.worker_id, a.error).await)
        }

        "enqueue_tagging_job" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct A {
                plugin_name: String,
                paths: Option<Vec<String>>,
                filter: Option<String>,
                // Pin the job to one worker (e.g. the server's in-process
                // executor) when several offer the same plugin.
                worker_id: Option<String>,
            }
            let a: A = parse(args)?;
            ok(
                crate::tagging::enqueue_job(app, a.plugin_name, a.paths, a.filter, a.worker_id)
                    .await,
            )
        }

        "cancel_tagging_job" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct A {
                job_id: String,
            }
            let a: A = parse(args)?;
            ok(crate::tagging::cancel_job(app, a.job_id).await)
        }

        "get_tagging_status" => ok(crate::tagging::get_status(app).await),

        // --- Duplicate detection --------------------------------------------
        // find = read + server-side hash compute; mark = metadata write, same
        // trust tier as tag edits. Resolving a group deletes via trash_files,
        // which carries its own gate below.
        "find_duplicates" => {
            #[derive(Deserialize)]
            struct A {
                threshold: Option<u32>,
            }
            let a: A = parse(args)?;
            ok(duplicates::find_duplicates_impl(app, a.threshold).await)
        }

        "mark_not_duplicates" => {
            #[derive(Deserialize)]
            struct A {
                paths: Vec<String>,
            }
            let a: A = parse(args)?;
            ok(duplicates::mark_not_duplicates_impl(app, a.paths).await)
        }

        "get_merge_candidates" => {
            #[derive(Deserialize)]
            struct A {
                paths: Vec<String>,
            }
            let a: A = parse(args)?;
            ok(duplicates::get_merge_candidates_impl(app, a.paths).await)
        }

        // Merge trashes the discarded copies, so it sits behind the same delete
        // gate as trash_files below.
        "merge_duplicates" if !settings::remote_delete_allowed(app).await => {
            Err(DispatchError::NotAllowed)
        }

        "merge_duplicates" => {
            #[derive(Deserialize)]
            struct A {
                plan: duplicates::MergePlan,
            }
            let a: A = parse(args)?;
            ok(duplicates::merge_duplicates_impl(app, a.plan).await)
        }

        // --- App-managed trash (gated by the per-gallery delete flag) ------
        // Trash is reversible (restore + retention-based purge), but hosts can
        // still revoke remote delete entirely; the check here is the security
        // boundary, the client-side capability gating is just UX.
        "trash_files" | "list_trash" | "restore_trash" | "purge_trash"
            if !settings::remote_delete_allowed(app).await =>
        {
            Err(DispatchError::NotAllowed)
        }

        "trash_files" => {
            #[derive(Deserialize)]
            struct A {
                paths: Vec<String>,
            }
            let a: A = parse(args)?;
            ok(trash::trash_files_impl(app, a.paths).await)
        }

        "list_trash" => ok(trash::list_trash_impl(app).await),

        "restore_trash" => {
            #[derive(Deserialize)]
            struct A {
                ids: Vec<String>,
            }
            let a: A = parse(args)?;
            ok(trash::restore_trash_impl(app, a.ids).await)
        }

        "purge_trash" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct A {
                ids: Option<Vec<String>>,
                older_than_days: Option<u32>,
            }
            let a: A = parse(args)?;
            ok(trash::purge_trash_impl(app, a.ids, a.older_than_days).await)
        }

        _ => Err(DispatchError::NotAllowed),
    }
}
