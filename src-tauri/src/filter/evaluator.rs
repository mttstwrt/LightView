//! Compiling a [`FilterExpr`] into SQL.
//!
//! There is one evaluation path, and it is this one: the AST becomes a `WHERE`
//! fragment against `media_meta m` plus a vector of bound parameters, and
//! SQLite does the work. There used to be a second, in-memory evaluator that
//! walked companion files; nothing ever called it, and keeping a parallel
//! implementation of the language's semantics around meant any divergence
//! between them was silent. It was deleted rather than wired up — evaluating a
//! filter by opening every sidecar is precisely the cost the tag index exists
//! to remove.
//!
//! The consequence to keep in mind when extending the language: **a term is
//! expressible only if the value it tests is indexed.** Tags come from
//! `tag_index` via `EXISTS`; everything else is a column on `media_meta`.
//! `ColorLabel` is the one term that has no column, and it is inert.
//!
//! Literals are pushed onto `params` and referenced positionally, never
//! interpolated — filter strings come from the user, and on the web client
//! from the network.

use crate::filter::ast::FilterExpr;

/// Build the `WHERE` fragment for `expr`, appending its bound values to
/// `params`. The caller supplies `media_meta` under the alias `m`; tag terms
/// bring in `tag_index` themselves via a correlated `EXISTS`.
pub fn to_sql(expr: &FilterExpr, params: &mut Vec<String>) -> String {
    match expr {
        FilterExpr::Tag { namespace, value } => {
            params.push(value.clone());
            let val_idx = params.len();

            match namespace.to_db_namespace() {
                Some(ns) => {
                    params.push(ns);
                    let ns_idx = params.len();
                    format!(
                        "EXISTS (SELECT 1 FROM tag_index ti WHERE ti.path = m.path AND ti.namespace = ?{} AND ti.tag = ?{})",
                        ns_idx, val_idx
                    )
                }
                None => {
                    // Any namespace
                    format!(
                        "EXISTS (SELECT 1 FROM tag_index ti WHERE ti.path = m.path AND ti.tag = ?{})",
                        val_idx
                    )
                }
            }
        }

        FilterExpr::And { left, right } => {
            let l = to_sql(left, params);
            let r = to_sql(right, params);
            format!("({} AND {})", l, r)
        }

        FilterExpr::Or { left, right } => {
            let l = to_sql(left, params);
            let r = to_sql(right, params);
            format!("({} OR {})", l, r)
        }

        FilterExpr::Not { expr } => {
            let inner = to_sql(expr, params);
            format!("NOT ({})", inner)
        }

        FilterExpr::Rating { op, value } => {
            params.push(value.to_string());
            let idx = params.len();
            format!("m.rating {} ?{}", op.as_sql(), idx)
        }

        FilterExpr::MediaType { value } => {
            params.push(value.as_str().to_string());
            let idx = params.len();
            format!("m.media_type = ?{}", idx)
        }

        FilterExpr::HasNamespace { namespace } => {
            match namespace.to_db_namespace() {
                Some(ns) => {
                    params.push(ns);
                    let idx = params.len();
                    format!(
                        "EXISTS (SELECT 1 FROM tag_index ti WHERE ti.path = m.path AND ti.namespace = ?{})",
                        idx
                    )
                }
                None => "1 = 1".to_string(), // Any = always true
            }
        }

        FilterExpr::ColorLabel { value } => {
            // Inert, and deliberately so rather than an error: `color_label`
            // lives only in the companion file, and this compiler's whole
            // premise is that a filter runs in SQLite over `media_meta` without
            // opening a single sidecar. Honouring the term means indexing the
            // column at scan time — a migration plus an indexer change, tracked
            // in docs/todo.md — not reading companions from here.
            //
            // The parser still accepts `color:red`, so the term widens the
            // result set instead of narrowing it. That is the wrong behaviour;
            // it is written down here and in the docs so it is a known gap
            // rather than a mystery.
            let _ = value;
            "1 = 1".to_string()
        }

        FilterExpr::GeoBbox { south, west, north, east } => {
            params.push(south.to_string());
            let s_idx = params.len();
            params.push(north.to_string());
            let n_idx = params.len();
            params.push(west.to_string());
            let w_idx = params.len();
            params.push(east.to_string());
            let e_idx = params.len();
            if west <= east {
                format!(
                    "(m.gps_lat IS NOT NULL AND m.gps_lat BETWEEN ?{} AND ?{} \
                       AND m.gps_lon BETWEEN ?{} AND ?{})",
                    s_idx, n_idx, w_idx, e_idx
                )
            } else {
                // Wrap around the anti-meridian.
                format!(
                    "(m.gps_lat IS NOT NULL AND m.gps_lat BETWEEN ?{} AND ?{} \
                       AND (m.gps_lon >= ?{} OR m.gps_lon <= ?{}))",
                    s_idx, n_idx, w_idx, e_idx
                )
            }
        }

        FilterExpr::HasGeo { present } => {
            if *present {
                "m.gps_lat IS NOT NULL".to_string()
            } else {
                "m.gps_lat IS NULL".to_string()
            }
        }

        FilterExpr::DateRange { field, from, to } => {
            // Column name comes from a fixed enum, not user input — safe to
            // interpolate. Bounds are bound as parameters.
            let col = field.column();
            let mut clauses = vec![format!("m.{} IS NOT NULL", col)];
            if let Some(from) = from {
                params.push(from.to_string());
                clauses.push(format!("m.{} >= ?{}", col, params.len()));
            }
            if let Some(to) = to {
                params.push(to.to_string());
                clauses.push(format!("m.{} <= ?{}", col, params.len()));
            }
            format!("({})", clauses.join(" AND "))
        }

        FilterExpr::Numeric { field, op, value } => {
            // Column name comes from a fixed enum, not user input — safe to
            // interpolate. The bound is a parameter. width/height are nullable,
            // so guard against NULL matching.
            let col = field.column();
            params.push(value.to_string());
            format!(
                "(m.{} IS NOT NULL AND m.{} {} ?{})",
                col,
                col,
                op.as_sql(),
                params.len()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::parser::parse_filter;

    #[test]
    fn test_to_sql_date_range_both_bounds() {
        // date=2024 → closed range against date_taken, two bound params.
        let expr = parse_filter("date=2024").unwrap();
        let mut params = Vec::new();
        let sql = to_sql(&expr, &mut params);
        assert_eq!(
            sql,
            "(m.date_taken IS NOT NULL AND m.date_taken >= ?1 AND m.date_taken <= ?2)"
        );
        assert_eq!(params, vec!["1704067200".to_string(), "1735689599".to_string()]);
    }

    #[test]
    fn test_to_sql_numeric() {
        let expr = parse_filter("width>=1920").unwrap();
        let mut params = Vec::new();
        let sql = to_sql(&expr, &mut params);
        assert_eq!(sql, "(m.width IS NOT NULL AND m.width >= ?1)");
        assert_eq!(params, vec!["1920".to_string()]);

        let expr = parse_filter("size<=5mb").unwrap();
        let mut params = Vec::new();
        let sql = to_sql(&expr, &mut params);
        assert_eq!(sql, "(m.file_size IS NOT NULL AND m.file_size <= ?1)");
        assert_eq!(params, vec![(5 * 1024 * 1024).to_string()]);
    }

    #[test]
    fn test_to_sql_date_range_field_and_open_end() {
        // viewed>=2024-01-01 → single lower bound against last_viewed.
        let expr = parse_filter("viewed>=2024-01-01").unwrap();
        let mut params = Vec::new();
        let sql = to_sql(&expr, &mut params);
        assert_eq!(sql, "(m.last_viewed IS NOT NULL AND m.last_viewed >= ?1)");
        assert_eq!(params.len(), 1);
    }
}
