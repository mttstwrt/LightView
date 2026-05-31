//! Read-only command bridge for the web client.
//!
//! A remote browser has no Tauri IPC, so `POST /api/invoke` maps a
//! `{ command, args }` payload to the same domain logic the `#[tauri::command]`
//! handlers use (the shared `*_impl` functions). Only the commands in the
//! `dispatch` match are reachable — any write command (tagging, ratings, file
//! ops, plugins) is rejected with 403 even if a client forges the name. This
//! is the security boundary that keeps remote access read-only.
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
    use crate::commands::{autocomplete, filter, gallery, geo, media, settings, sort, tags};

    match command {
        "get_gallery_info" => ok(gallery::get_gallery_info_impl(app).await),

        "get_gallery_default_filter" => {
            ok(settings::get_gallery_default_filter_impl(app).await)
        }

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

        _ => Err(DispatchError::NotAllowed),
    }
}
