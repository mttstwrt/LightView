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
//! `tag_index` via `EXISTS`; everything else is a column on `media_meta`. A
//! field that lives only in the companion cannot be filtered on without first
//! being mirrored into a column — which is what `color_label` had to do.
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
            // `color:none` asks for the *absence* of a label, which is a
            // different predicate rather than a label that happens to be
            // spelled "none" — and is the form worth having, since "which of
            // these did I never triage?" is the question a colour workflow
            // actually asks.
            if value.eq_ignore_ascii_case("none") {
                return "m.color_label IS NULL".to_string();
            }
            // Stored lowercase by every write path, so the comparison is exact
            // rather than `COLLATE NOCASE` — which would not use the index.
            params.push(value.trim().to_lowercase());
            format!("m.color_label = ?{}", params.len())
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

        FilterExpr::Text { field, value } => {
            // Column name comes from a fixed enum, not user input — safe to
            // interpolate. The needle is a parameter, and arrives lowercased.
            //
            // `instr` rather than `LIKE '%' || ? || '%'`: the needle is whatever
            // the user typed, and `%` and `_` are wildcards inside `LIKE`, so
            // that form would need escaping to mean what it says. `instr` has no
            // metacharacters.
            //
            // The `IS NOT NULL` guard is load-bearing, not defensive padding.
            // `instr(lower(NULL), 'x')` is NULL, `false OR NULL` is NULL, and
            // `NOT NULL` is NULL — so without it, every file lacking a
            // description would silently drop out of `NOT fuji`, which expands
            // to a negated disjunction containing this term.
            let col = field.column();
            params.push(value.clone());
            format!(
                "(m.{} IS NOT NULL AND instr(lower(m.{}), ?{}) > 0)",
                col,
                col,
                params.len()
            )
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

    /// The colour term compiles to a real predicate against the indexed
    /// column. It used to be `1 = 1`, which silently *widened* a filter that
    /// the user wrote to narrow it.
    #[test]
    fn test_to_sql_color_label() {
        let expr = parse_filter("color:Red").unwrap();
        let mut params = Vec::new();
        assert_eq!(to_sql(&expr, &mut params), "m.color_label = ?1");
        // Normalized on the way in, matching what every write path stores.
        assert_eq!(params, vec!["red".to_string()]);
    }

    /// `color:none` is absence, not a label spelled "none" — and it binds no
    /// parameter, so the surrounding expression's numbering must still line up.
    #[test]
    fn test_to_sql_color_label_none() {
        let expr = parse_filter("color:none AND rating>=4").unwrap();
        let mut params = Vec::new();
        let sql = to_sql(&expr, &mut params);
        assert_eq!(sql, "(m.color_label IS NULL AND m.rating >= ?1)");
        assert_eq!(params, vec!["4".to_string()]);
    }

    /// A bare word compiles to the tag lookup it always did, plus a substring
    /// match on the filename and the description — which is what makes
    /// `mount_fuji.jpeg` answer to `fuji` with no tag on it.
    #[test]
    fn test_to_sql_bare_word_searches_tags_filename_and_description() {
        let expr = parse_filter("Fuji").unwrap();
        let mut params = Vec::new();
        let sql = to_sql(&expr, &mut params);
        // Composed rather than written out, so the assertion cannot drift from
        // the arms above on whitespace alone.
        let tag = "EXISTS (SELECT 1 FROM tag_index ti WHERE ti.path = m.path AND ti.tag = ?1)";
        let name = "(m.filename IS NOT NULL AND instr(lower(m.filename), ?2) > 0)";
        let desc = "(m.description IS NOT NULL AND instr(lower(m.description), ?3) > 0)";
        assert_eq!(sql, format!("({tag} OR ({name} OR {desc}))"));
        // The tag keeps the spelling it was given (tag matching is exact); the
        // text needles are folded once, here, rather than per row.
        assert_eq!(
            params,
            vec!["Fuji".to_string(), "fuji".to_string(), "fuji".to_string()]
        );
    }

    #[test]
    fn test_to_sql_explicit_text_terms() {
        let expr = parse_filter("name:fuji").unwrap();
        let mut params = Vec::new();
        assert_eq!(
            to_sql(&expr, &mut params),
            "(m.filename IS NOT NULL AND instr(lower(m.filename), ?1) > 0)"
        );
        assert_eq!(params, vec!["fuji".to_string()]);

        let expr = parse_filter("desc:sunset").unwrap();
        let mut params = Vec::new();
        assert_eq!(
            to_sql(&expr, &mut params),
            "(m.description IS NOT NULL AND instr(lower(m.description), ?1) > 0)"
        );
        assert_eq!(params, vec!["sunset".to_string()]);
    }

    /// The `IS NOT NULL` guard is what keeps a negated bare word from throwing
    /// away every file that has no description. Without it the term is NULL,
    /// the disjunction is NULL, and `NOT NULL` is NULL — which SQLite does not
    /// select. Assert it is present on both text terms under a `NOT`.
    #[test]
    fn test_to_sql_negated_bare_word_guards_null() {
        let expr = parse_filter("NOT fuji").unwrap();
        let mut params = Vec::new();
        let sql = to_sql(&expr, &mut params);
        assert!(sql.starts_with("NOT ("));
        assert_eq!(sql.matches("IS NOT NULL AND instr").count(), 2);
    }

    /// The compiled SQL, run against a real database rather than asserted as a
    /// string: the v18 columns exist, `instr(lower(…))` folds case, and — the
    /// one that would be silent — a negated bare word still returns the files
    /// that have no description at all.
    #[test]
    fn text_terms_run_against_the_real_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::cache::db::CacheDb::open(tmp.path()).unwrap();
        for (path, filename, description) in [
            ("/g/mount_fuji.jpeg", "mount_fuji.jpeg", None),
            ("/g/IMG_2001.jpg", "IMG_2001.jpg", Some("A volcano at dawn.")),
            ("/g/cat.png", "cat.png", None),
        ] {
            db.conn()
                .execute(
                    "INSERT INTO media_meta (path, file_size, media_type, filename, description)
                     VALUES (?1, 0, 'image', ?2, ?3)",
                    rusqlite::params![path, filename, description],
                )
                .unwrap();
        }

        let matching = |query: &str| -> Vec<String> {
            let expr = parse_filter(query).unwrap();
            let mut params = Vec::new();
            let where_clause = to_sql(&expr, &mut params);
            let sql = format!(
                "SELECT m.path FROM media_meta m WHERE {} ORDER BY m.path",
                where_clause
            );
            let refs: Vec<&dyn rusqlite::types::ToSql> = params
                .iter()
                .map(|s| s as &dyn rusqlite::types::ToSql)
                .collect();
            let mut stmt = db.conn().prepare(&sql).unwrap();
            let rows = stmt
                .query_map(refs.as_slice(), |r| r.get::<_, String>(0))
                .unwrap();
            rows.map(|r| r.unwrap()).collect()
        };

        // No tag anywhere in this database: these all match on text alone.
        assert_eq!(matching("FUJI"), vec!["/g/mount_fuji.jpeg"]);
        assert_eq!(matching("volcano"), vec!["/g/IMG_2001.jpg"]);
        assert_eq!(matching("desc:VOLCANO"), vec!["/g/IMG_2001.jpg"]);
        assert_eq!(matching("name:img"), vec!["/g/IMG_2001.jpg"]);
        // Both files without a description survive the negation.
        assert_eq!(
            matching("NOT volcano"),
            vec!["/g/cat.png", "/g/mount_fuji.jpeg"]
        );
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
