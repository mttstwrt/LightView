use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::filter::evaluator;
use crate::filter::parser;
use crate::AppState;

/// A viewport bounding box in WGS-84 decimal degrees.
/// `west > east` is allowed and means the box crosses the anti-meridian.
#[derive(Debug, Deserialize)]
pub struct GeoBbox {
    pub south: f64,
    pub west: f64,
    pub north: f64,
    pub east: f64,
}

#[derive(Debug, Serialize)]
pub struct GeoCluster {
    /// Centroid of the points in this cluster.
    pub lat: f64,
    pub lon: f64,
    pub count: u32,
    /// One representative path — the frontend uses this for the marker thumbnail
    /// and to open the viewer when the user clicks a singleton cluster.
    pub sample_path: String,
}

#[derive(Debug, Serialize)]
pub struct GeoQueryResult {
    pub clusters: Vec<GeoCluster>,
    /// Total count of points in the bbox (before clustering). Lets the
    /// frontend show "1,247 photos in view" without summing cluster counts.
    pub total: u32,
}

/// Return clustered geotagged photos within a bounding box.
///
/// `zoom` is a Leaflet-style zoom level (0–20). It's used to size the
/// clustering grid: at low zoom many points collapse into a single cluster;
/// at high zoom each point tends to be its own cluster.
///
/// `filter` is an optional filter query string (the same syntax used by
/// `apply_filter`). The bbox predicate is AND-ed onto whatever the user
/// already has selected, so the map view composes with topbar filters.
#[tauri::command]
pub async fn get_geo_points(
    state: tauri::State<'_, AppState>,
    bbox: GeoBbox,
    zoom: u8,
    filter: Option<String>,
) -> Result<GeoQueryResult, String> {
    get_geo_points_impl(&state, bbox, zoom, filter).await
}

pub async fn get_geo_points_impl(
    state: &AppState,
    bbox: GeoBbox,
    zoom: u8,
    filter: Option<String>,
) -> Result<GeoQueryResult, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    // Build the WHERE clause: optional user filter AND the bbox predicate.
    // We thread it through `to_sql` so the bbox uses the same parameter
    // numbering as everything else.
    let mut params: Vec<String> = Vec::new();
    let where_clause = match &filter {
        Some(q) if !q.trim().is_empty() => {
            let user_expr = parser::parse_filter(q).map_err(|e| e.to_string())?;
            let bbox_expr = crate::filter::ast::FilterExpr::GeoBbox {
                south: bbox.south,
                west: bbox.west,
                north: bbox.north,
                east: bbox.east,
            };
            let combined = crate::filter::ast::FilterExpr::And {
                left: Box::new(user_expr),
                right: Box::new(bbox_expr),
            };
            evaluator::to_sql(&combined, &mut params)
        }
        _ => {
            let bbox_expr = crate::filter::ast::FilterExpr::GeoBbox {
                south: bbox.south,
                west: bbox.west,
                north: bbox.north,
                east: bbox.east,
            };
            evaluator::to_sql(&bbox_expr, &mut params)
        }
    };

    let sql = format!(
        "SELECT m.path, m.gps_lat, m.gps_lon FROM media_meta m WHERE {}",
        where_clause
    );

    let mut stmt = db.conn().prepare(&sql).map_err(|e| e.to_string())?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    let cell_deg = cluster_cell_size(zoom);
    let mut buckets: HashMap<(i64, i64), Bucket> = HashMap::new();
    let mut total: u32 = 0;

    for row in rows {
        let (path, lat, lon) = match row {
            Ok(t) => t,
            Err(_) => continue,
        };
        total += 1;
        let key = (
            (lat / cell_deg).floor() as i64,
            (lon / cell_deg).floor() as i64,
        );
        let entry = buckets.entry(key).or_insert(Bucket {
            sum_lat: 0.0,
            sum_lon: 0.0,
            count: 0,
            sample_path: String::new(),
        });
        entry.sum_lat += lat;
        entry.sum_lon += lon;
        entry.count += 1;
        if entry.sample_path.is_empty() {
            entry.sample_path = path;
        }
    }

    let mut clusters: Vec<GeoCluster> = buckets
        .into_values()
        .map(|b| GeoCluster {
            lat: b.sum_lat / b.count as f64,
            lon: b.sum_lon / b.count as f64,
            count: b.count,
            sample_path: b.sample_path,
        })
        .collect();

    // Largest clusters first so the frontend can render heaviest-on-top.
    clusters.sort_by(|a, b| b.count.cmp(&a.count));

    Ok(GeoQueryResult { clusters, total })
}

struct Bucket {
    sum_lat: f64,
    sum_lon: f64,
    count: u32,
    sample_path: String,
}

/// Return all photo paths within a bounding box, optionally AND-ed with a
/// user filter. Used when the map view collapses a clicked stack into the
/// grid view: the frontend computes the cluster's cell bbox at the current
/// zoom and asks for the exact set of paths inside it.
///
/// Unlike `get_geo_points`, no clustering is applied — the response is the
/// full path list. Callers should bound the bbox themselves (a single
/// cluster cell is well-bounded; a world-view bbox is not).
#[tauri::command]
pub async fn get_geo_paths(
    state: tauri::State<'_, AppState>,
    bbox: GeoBbox,
    filter: Option<String>,
) -> Result<Vec<String>, String> {
    get_geo_paths_impl(&state, bbox, filter).await
}

pub async fn get_geo_paths_impl(
    state: &AppState,
    bbox: GeoBbox,
    filter: Option<String>,
) -> Result<Vec<String>, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    let mut params: Vec<String> = Vec::new();
    let bbox_expr = crate::filter::ast::FilterExpr::GeoBbox {
        south: bbox.south,
        west: bbox.west,
        north: bbox.north,
        east: bbox.east,
    };
    let where_clause = match &filter {
        Some(q) if !q.trim().is_empty() => {
            let user_expr = parser::parse_filter(q).map_err(|e| e.to_string())?;
            let combined = crate::filter::ast::FilterExpr::And {
                left: Box::new(user_expr),
                right: Box::new(bbox_expr),
            };
            evaluator::to_sql(&combined, &mut params)
        }
        _ => evaluator::to_sql(&bbox_expr, &mut params),
    };

    let sql = format!(
        "SELECT m.path FROM media_meta m WHERE {}",
        where_clause
    );

    let mut stmt = db.conn().prepare(&sql).map_err(|e| e.to_string())?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;

    let mut paths: Vec<String> = Vec::new();
    for row in rows {
        if let Ok(p) = row {
            paths.push(p);
        }
    }
    Ok(paths)
}

/// Pick a clustering cell size in decimal degrees for a given zoom level.
///
/// Each zoom step halves a tile's degree span (`360 / 2^zoom`), so we tie
/// the cell size to ~1/4 of a tile. That puts roughly 4×4 = 16 cells per
/// tile, which empirically gives readable clustering: at zoom 0 the whole
/// world is ~16 cells, at zoom 18 a city block is its own cell.
fn cluster_cell_size(zoom: u8) -> f64 {
    let z = zoom.min(20) as f64;
    (360.0 / 2f64.powf(z)) / 4.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_size_shrinks_with_zoom() {
        assert!(cluster_cell_size(0) > cluster_cell_size(5));
        assert!(cluster_cell_size(5) > cluster_cell_size(15));
        assert!(cluster_cell_size(20) > 0.0);
    }
}
