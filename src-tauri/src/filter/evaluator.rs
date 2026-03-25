use crate::companion::schema::CompanionFile;
use crate::filter::ast::{FilterExpr, RatingOp, TagNamespace};

/// Evaluate a filter expression against a single companion file (in-memory path).
/// Returns true if the companion file matches the filter.
pub fn evaluate(expr: &FilterExpr, companion: &CompanionFile) -> bool {
    match expr {
        FilterExpr::Tag { namespace, value } => match namespace {
            TagNamespace::Any => companion
                .all_tags()
                .iter()
                .any(|(_, t)| t == value),
            _ => {
                let ns_str = namespace
                    .to_db_namespace()
                    .unwrap_or_default();
                companion
                    .tags_in_namespace(&ns_str)
                    .iter()
                    .any(|t| t == value)
            }
        },

        FilterExpr::And { left, right } => {
            evaluate(left, companion) && evaluate(right, companion)
        }

        FilterExpr::Or { left, right } => {
            evaluate(left, companion) || evaluate(right, companion)
        }

        FilterExpr::Not { expr } => !evaluate(expr, companion),

        FilterExpr::Rating { op, value } => {
            let rating = companion
                .meta
                .core
                .as_ref()
                .and_then(|c| c.rating)
                .unwrap_or(0);
            match op {
                RatingOp::Gte => rating >= *value,
                RatingOp::Lte => rating <= *value,
                RatingOp::Eq => rating == *value,
            }
        }

        FilterExpr::MediaType { value } => companion.media_type == *value,

        FilterExpr::HasNamespace { namespace } => {
            let ns_str = namespace
                .to_db_namespace()
                .unwrap_or_default();
            !companion.tags_in_namespace(&ns_str).is_empty()
        }

        FilterExpr::ColorLabel { value } => companion
            .meta
            .core
            .as_ref()
            .and_then(|c| c.color_label.as_ref())
            .map_or(false, |l| l == value),
    }
}

/// Build a SQL WHERE clause from a FilterExpr for use with the tag_index
/// and media_meta tables. Returns (sql_fragment, params).
///
/// This is used for large galleries where in-memory evaluation is too slow.
/// The caller must join tag_index and media_meta as needed.
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
            let op_str = match op {
                RatingOp::Gte => ">=",
                RatingOp::Lte => "<=",
                RatingOp::Eq => "=",
            };
            format!("m.rating {} ?{}", op_str, idx)
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
            // Color label is stored in companion file, not in media_meta.
            // For SQL path, we'd need an additional index column.
            // For now, fall back to a placeholder that always matches.
            // TODO: Add color_label column to media_meta for SQL filtering.
            let _ = value;
            "1 = 1".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::schema::{CompanionFile, MediaType as MT};
    use crate::filter::parser::parse_filter;

    fn make_companion(user_tags: &[&str]) -> CompanionFile {
        let mut c = CompanionFile::new("test.jpg", MT::Image);
        c.tags.user = user_tags.iter().map(|s| s.to_string()).collect();
        c
    }

    #[test]
    fn test_eval_simple_tag() {
        let c = make_companion(&["vacation", "family"]);
        let expr = parse_filter("user:vacation").unwrap();
        assert!(evaluate(&expr, &c));

        let expr2 = parse_filter("user:work").unwrap();
        assert!(!evaluate(&expr2, &c));
    }

    #[test]
    fn test_eval_and() {
        let c = make_companion(&["vacation", "family"]);
        let expr = parse_filter("user:vacation AND user:family").unwrap();
        assert!(evaluate(&expr, &c));

        let expr2 = parse_filter("user:vacation AND user:work").unwrap();
        assert!(!evaluate(&expr2, &c));
    }

    #[test]
    fn test_eval_not() {
        let c = make_companion(&["vacation"]);
        let expr = parse_filter("NOT user:work").unwrap();
        assert!(evaluate(&expr, &c));
    }

    #[test]
    fn test_eval_media_type() {
        let c = make_companion(&[]);
        let expr = parse_filter("type:image").unwrap();
        assert!(evaluate(&expr, &c));

        let expr2 = parse_filter("type:video").unwrap();
        assert!(!evaluate(&expr2, &c));
    }
}
