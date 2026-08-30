//! Tokenizer and recursive-descent parser for the filter language.
//!
//! Precedence is the conventional one: `OR` binds loosest, then `AND`, then
//! `NOT`, with parentheses to override. Terms are matched by prefix against a
//! fixed set of forms, and **order matters** — `rating>=` is tried before a
//! bare tag, so `rating:general` (a real tag emitted by the WD taggers) falls
//! through to the tag branch instead of being mistaken for a rating filter.
//!
//! Dates accept a year, a year-month, or a full date and expand to the
//! inclusive range they name: `date=2024` is all of 2024. Four-digit years are
//! required rather than inferred, because guessing the century for `24-01-01`
//! silently produces the wrong range.
//!
//! A bare word is expanded *here*, into `tag OR filename OR description`, so
//! the tree the evaluator walks says exactly what the word matches. The
//! alternative — one leaf that the evaluator quietly compiles into three
//! predicates — would make `NOT fuji` a second place to get De Morgan right.

use chrono::NaiveDate;

use crate::companion::schema::MediaType;
use crate::filter::ast::{CompareOp, DateField, FilterExpr, NumericField, TagNamespace, TextField};

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("Unexpected end of input")]
    UnexpectedEnd,
    #[error("Unexpected token: {0}")]
    UnexpectedToken(String),
    #[error("Invalid rating value: {0}")]
    InvalidRating(String),
    #[error("Unknown media type: {0}")]
    UnknownMediaType(String),
    #[error("Invalid geo filter: {0}")]
    InvalidGeo(String),
    #[error("Invalid date filter: {0}")]
    InvalidDate(String),
    #[error("Invalid numeric filter: {0}")]
    InvalidNumeric(String),
    #[error("Empty {0} filter — nothing to match")]
    EmptyText(String),
}

/// Parse a filter query string into a FilterExpr.
///
/// Syntax examples:
///   vacation                                    → tag in any namespace, or the
///                                                 filename or description
///   name:fuji                                   → filename substring
///   desc:sunset                                 → description substring
///   user::vacation                              → specific namespace
///   plugin.face-recognition::person:alice       → plugin namespace
///   user::vacation AND user::family             → boolean AND
///   user::vacation OR user::travel              → boolean OR
///   NOT auto:indoor                             → boolean NOT
///   rating>=4                                   → rating filter
///   date>=2024-01-01                            → capture date on/after
///   date=2024                                   → captured during 2024
///   added<=2024-06 / viewed>=2023               → date added / last viewed
///   width>=1920 / height<=1080                  → pixel dimensions
///   size>=10mb / size<=500kb                    → file size (b/kb/mb/gb)
///   type:video                                  → media type filter
///   has::user                                   → namespace existence
///   (user::a OR user::b) AND NOT auto::indoor   → grouped expression
pub fn parse_filter(input: &str) -> Result<FilterExpr, ParseError> {
    let tokens = tokenize(input);
    if tokens.is_empty() {
        return Err(ParseError::UnexpectedEnd);
    }
    let (expr, remaining) = parse_or(&tokens)?;
    if !remaining.is_empty() {
        return Err(ParseError::UnexpectedToken(remaining[0].clone()));
    }
    Ok(expr)
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in input.chars() {
        match ch {
            '(' => {
                // Only treat as grouping when it starts a new token
                // (i.e., current is empty — preceded by whitespace or start of input).
                // Otherwise it's part of a tag like "hatsune_miku_(vocaloid)".
                if current.is_empty() {
                    tokens.push(ch.to_string());
                } else {
                    current.push(ch);
                }
            }
            ')' => {
                // Grouping close when current is empty (e.g. `) AND ...`) or
                // when the current token does NOT contain an opening paren
                // (meaning this `)` isn't balancing an in-tag `(`).
                // e.g. "travel)" → grouping close, but "vocaloid)" after
                // "miku_(" → part of the tag.
                if current.is_empty() || !current.contains('(') {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                    tokens.push(ch.to_string());
                } else {
                    current.push(ch);
                }
            }
            ' ' | '\t' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

// ---------------------------------------------------------------------------
// Recursive descent parser
// ---------------------------------------------------------------------------

type ParseResult<'a> = Result<(FilterExpr, &'a [String]), ParseError>;

fn parse_or<'a>(tokens: &'a [String]) -> ParseResult<'a> {
    let (mut left, mut rest) = parse_and(tokens)?;
    while !rest.is_empty() && rest[0].eq_ignore_ascii_case("OR") {
        let (right, r) = parse_and(&rest[1..])?;
        left = FilterExpr::Or {
            left: Box::new(left),
            right: Box::new(right),
        };
        rest = r;
    }
    Ok((left, rest))
}

fn parse_and<'a>(tokens: &'a [String]) -> ParseResult<'a> {
    let (mut left, mut rest) = parse_not(tokens)?;
    while !rest.is_empty() && rest[0].eq_ignore_ascii_case("AND") {
        let (right, r) = parse_not(&rest[1..])?;
        left = FilterExpr::And {
            left: Box::new(left),
            right: Box::new(right),
        };
        rest = r;
    }
    Ok((left, rest))
}

fn parse_not<'a>(tokens: &'a [String]) -> ParseResult<'a> {
    if tokens.is_empty() {
        return Err(ParseError::UnexpectedEnd);
    }
    if tokens[0].eq_ignore_ascii_case("NOT") {
        let (expr, rest) = parse_atom(&tokens[1..])?;
        Ok((FilterExpr::Not { expr: Box::new(expr) }, rest))
    } else {
        parse_atom(tokens)
    }
}

fn parse_atom<'a>(tokens: &'a [String]) -> ParseResult<'a> {
    if tokens.is_empty() {
        return Err(ParseError::UnexpectedEnd);
    }

    // Parenthesized group
    if tokens[0] == "(" {
        let (expr, rest) = parse_or(&tokens[1..])?;
        if rest.is_empty() || rest[0] != ")" {
            return Err(ParseError::UnexpectedToken("expected ')'".to_string()));
        }
        return Ok((expr, &rest[1..]));
    }

    let token = &tokens[0];
    let rest = &tokens[1..];

    // Rating filter: rating>=4, rating<=2, rating=5
    if token.starts_with("rating>=") || token.starts_with("rating<=") || token.starts_with("rating=") {
        return parse_rating(token, rest);
    }

    // Date filter: date>=2024-01-01, added<=2024-12, viewed=2024
    // (`date` = capture date, `added` = date added, `viewed` = last viewed).
    if let Some(expr) = parse_date(token)? {
        return Ok((expr, rest));
    }

    // Numeric metadata filter: width>=1920, height<=1080, size>=10mb
    if let Some(expr) = parse_numeric(token)? {
        return Ok((expr, rest));
    }

    // Media type filter: type:video, type:image, type:gif
    if let Some(type_val) = token.strip_prefix("type:") {
        let media_type = match type_val.to_lowercase().as_str() {
            "image" => MediaType::Image,
            "video" => MediaType::Video,
            "gif" => MediaType::Gif,
            _ => return Err(ParseError::UnknownMediaType(type_val.to_string())),
        };
        return Ok((FilterExpr::MediaType { value: media_type }, rest));
    }

    // Color label: color:green
    if let Some(color) = token.strip_prefix("color:") {
        return Ok((
            FilterExpr::ColorLabel {
                value: color.to_string(),
            },
            rest,
        ));
    }

    // Filename substring: name:fuji matches `mount_fuji.jpeg`. Checked before
    // the bare-tag fallthrough, so it shadows any tag literally spelled
    // `name:something` — which stays reachable through the namespace form,
    // `user::name:something`. Same for `desc:`.
    if let Some(needle) = token.strip_prefix("name:") {
        return Ok((text_term(TextField::Filename, needle)?, rest));
    }

    // Description substring: desc:sunset
    if let Some(needle) = token.strip_prefix("desc:") {
        return Ok((text_term(TextField::Description, needle)?, rest));
    }

    // Geo presence: has:geo, missing:geo
    if token == "has:geo" {
        return Ok((FilterExpr::HasGeo { present: true }, rest));
    }
    if token == "missing:geo" {
        return Ok((FilterExpr::HasGeo { present: false }, rest));
    }

    // Geo bounding box: geo:south,west,north,east (decimal degrees)
    if let Some(bbox_str) = token.strip_prefix("geo:") {
        return parse_geo_bbox(bbox_str, rest);
    }

    // Has namespace: has::user, has::plugin.geo
    if let Some(ns) = token.strip_prefix("has::") {
        let namespace = parse_namespace(ns);
        return Ok((FilterExpr::HasNamespace { namespace }, rest));
    }

    // Namespaced tag: user::vacation, plugin.face-recognition::rating:general
    if let Some(dcolon_pos) = token.find("::") {
        let ns_str = &token[..dcolon_pos];
        let tag_val = &token[dcolon_pos + 2..];
        let namespace = parse_namespace(ns_str);
        return Ok((
            FilterExpr::Tag {
                namespace,
                value: tag_val.to_string(),
            },
            rest,
        ));
    }

    // Bare namespace name → show all images tagged in that namespace.
    // "user" → all user-tagged images, "plugin.tagger" → all images
    // tagged by that plugin.
    if is_namespace_name(token) {
        let namespace = parse_namespace(token);
        return Ok((FilterExpr::HasNamespace { namespace }, rest));
    }

    // Bare word: the tag it names in any namespace, or a substring of the
    // filename, or a substring of the description.
    Ok((bare_word(token), rest))
}

/// Expand a bare word into `tag OR filename OR description`.
///
/// The tag half stays an **exact** match while the other two are substrings.
/// That asymmetry is deliberate: a tag is a token from a controlled vocabulary
/// that autocomplete completes for you, so substring-matching it would make
/// `cat` match `cathedral` and there would be no way to ask for just `cat`. A
/// filename and a description are prose nobody types in full.
///
/// Case is folded on the text halves only, and only over ASCII — see
/// [`text_term`] for why the two sides have to agree on that.
fn bare_word(token: &str) -> FilterExpr {
    let needle = token.to_ascii_lowercase();
    FilterExpr::Or {
        left: Box::new(FilterExpr::Tag {
            namespace: TagNamespace::Any,
            value: token.to_string(),
        }),
        right: Box::new(FilterExpr::Or {
            left: Box::new(FilterExpr::Text {
                field: TextField::Filename,
                value: needle.clone(),
            }),
            right: Box::new(FilterExpr::Text {
                field: TextField::Description,
                value: needle,
            }),
        }),
    }
}

/// Build an explicit `name:`/`desc:` term, folding the needle's case once here
/// instead of once per row in SQL.
///
/// **ASCII** folding specifically, because the other side of the comparison is
/// SQLite's `lower()`, which folds ASCII and nothing else. Folding the needle
/// more thoroughly than the column would turn an exact match into a miss:
/// `FÜJI` would become `füji` while `FÜJI.jpg` stayed `fÜji.jpg`, so typing a
/// filename exactly as it is spelled would find nothing.
///
/// An empty needle is an error rather than a match-everything: `instr(x, '')`
/// finds the empty string at position 1, so `name:` on its own would compile to
/// a term that matches every file that has a name — a filter written to narrow,
/// silently widening. That is the bug `color:red` had for a year.
fn text_term(field: TextField, needle: &str) -> Result<FilterExpr, ParseError> {
    if needle.is_empty() {
        return Err(ParseError::EmptyText(field.column().to_string()));
    }
    Ok(FilterExpr::Text {
        field,
        value: needle.to_ascii_lowercase(),
    })
}

fn parse_geo_bbox<'a>(bbox_str: &str, rest: &'a [String]) -> ParseResult<'a> {
    let parts: Vec<&str> = bbox_str.split(',').collect();
    if parts.len() != 4 {
        return Err(ParseError::InvalidGeo(format!(
            "expected south,west,north,east — got '{}'",
            bbox_str
        )));
    }
    let parse = |s: &str| -> Result<f64, ParseError> {
        s.trim()
            .parse::<f64>()
            .map_err(|_| ParseError::InvalidGeo(format!("'{}' is not a number", s)))
    };
    let south = parse(parts[0])?;
    let west = parse(parts[1])?;
    let north = parse(parts[2])?;
    let east = parse(parts[3])?;
    if !(-90.0..=90.0).contains(&south)
        || !(-90.0..=90.0).contains(&north)
        || !(-180.0..=180.0).contains(&west)
        || !(-180.0..=180.0).contains(&east)
    {
        return Err(ParseError::InvalidGeo(
            "coordinates out of WGS-84 range".to_string(),
        ));
    }
    if south > north {
        return Err(ParseError::InvalidGeo(
            "south must not exceed north".to_string(),
        ));
    }
    // west may exceed east when crossing the anti-meridian — that's allowed.
    Ok((
        FilterExpr::GeoBbox { south, west, north, east },
        rest,
    ))
}

fn parse_rating<'a>(token: &str, rest: &'a [String]) -> ParseResult<'a> {
    let (op, val_str) = if let Some(v) = token.strip_prefix("rating>=") {
        (CompareOp::Gte, v)
    } else if let Some(v) = token.strip_prefix("rating<=") {
        (CompareOp::Lte, v)
    } else if let Some(v) = token.strip_prefix("rating=") {
        (CompareOp::Eq, v)
    } else {
        return Err(ParseError::InvalidRating(token.to_string()));
    };

    let value: u8 = val_str
        .parse()
        .map_err(|_| ParseError::InvalidRating(val_str.to_string()))?;

    Ok((FilterExpr::Rating { op, value }, rest))
}

/// Try to parse a date-range filter token (`date`/`added`/`viewed` followed by
/// `>=`, `<=`, or `=`). Returns `Ok(None)` if the token isn't a date filter so
/// the caller can fall through to tag parsing; `Err` if it looks like one but
/// carries an invalid date literal.
fn parse_date(token: &str) -> Result<Option<FilterExpr>, ParseError> {
    // Check two-char operators before "=" so it doesn't shadow ">=" / "<=".
    for op in ["<=", ">=", "="] {
        let Some(pos) = token.find(op) else { continue };
        let field = match &token[..pos] {
            "date" => DateField::Taken,
            "added" => DateField::Added,
            "viewed" => DateField::Viewed,
            _ => return Ok(None), // not a date filter — let tag parsing handle it
        };
        let (lo, hi) = parse_date_literal(&token[pos + op.len()..])?;
        let (from, to) = match op {
            ">=" => (Some(lo), None),
            "<=" => (None, Some(hi)),
            _ => (Some(lo), Some(hi)), // "=" matches the whole period
        };
        return Ok(Some(FilterExpr::DateRange { field, from, to }));
    }
    Ok(None)
}

/// Parse a date literal into an inclusive `(start, end)` pair of Unix-second
/// timestamps (UTC), matching how the rest of the app interprets stored dates.
/// Accepts `YYYY` (whole year), `YYYY-MM` (whole month), or `YYYY-MM-DD` (one
/// day). A partial date denotes the full period it covers.
fn parse_date_literal(s: &str) -> Result<(i64, i64), ParseError> {
    let err = || ParseError::InvalidDate(s.to_string());
    let parts: Vec<&str> = s.split('-').collect();
    if parts[0].len() != 4 {
        return Err(err());
    }
    let year: i32 = parts[0].parse().map_err(|_| err())?;

    // `start` is the first instant of the period; `next` is the first instant
    // of the following period, so the inclusive end is `next - 1 second`.
    let (start, next) = match parts.len() {
        1 => (
            NaiveDate::from_ymd_opt(year, 1, 1).ok_or_else(err)?,
            NaiveDate::from_ymd_opt(year + 1, 1, 1).ok_or_else(err)?,
        ),
        2 => {
            let month: u32 = parts[1].parse().map_err(|_| err())?;
            let start = NaiveDate::from_ymd_opt(year, month, 1).ok_or_else(err)?;
            let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
            let next = NaiveDate::from_ymd_opt(ny, nm, 1).ok_or_else(err)?;
            (start, next)
        }
        3 => {
            let month: u32 = parts[1].parse().map_err(|_| err())?;
            let day: u32 = parts[2].parse().map_err(|_| err())?;
            let start = NaiveDate::from_ymd_opt(year, month, day).ok_or_else(err)?;
            (start, start.succ_opt().ok_or_else(err)?)
        }
        _ => return Err(err()),
    };

    let lo = start.and_hms_opt(0, 0, 0).ok_or_else(err)?.and_utc().timestamp();
    let hi = next.and_hms_opt(0, 0, 0).ok_or_else(err)?.and_utc().timestamp() - 1;
    Ok((lo, hi))
}

/// Try to parse a numeric-metadata filter token (`width`/`height`/`size`/
/// `filesize` followed by `>=`, `<=`, or `=`). Returns `Ok(None)` if the token
/// isn't a numeric filter so the caller can fall through to tag parsing; `Err`
/// if it looks like one but carries an invalid value.
fn parse_numeric(token: &str) -> Result<Option<FilterExpr>, ParseError> {
    // Check two-char operators before "=" so it doesn't shadow ">=" / "<=".
    for op_str in ["<=", ">=", "="] {
        let Some(pos) = token.find(op_str) else { continue };
        let field = match &token[..pos] {
            "width" => NumericField::Width,
            "height" => NumericField::Height,
            "size" | "filesize" => NumericField::FileSize,
            _ => return Ok(None), // not a numeric filter — let tag parsing handle it
        };
        let op = match op_str {
            ">=" => CompareOp::Gte,
            "<=" => CompareOp::Lte,
            _ => CompareOp::Eq,
        };
        let raw = &token[pos + op_str.len()..];
        let value = match field {
            NumericField::FileSize => parse_size(raw)?,
            _ => raw
                .parse::<i64>()
                .map_err(|_| ParseError::InvalidNumeric(token.to_string()))?,
        };
        return Ok(Some(FilterExpr::Numeric { field, op, value }));
    }
    Ok(None)
}

/// Parse a byte-size literal into a number of bytes. Accepts a plain number
/// (bytes) or a `b`/`kb`/`mb`/`gb` suffix (base-1024, matching how the viewer's
/// InfoPanel formats sizes). Fractional values like `1.5mb` are allowed.
fn parse_size(s: &str) -> Result<i64, ParseError> {
    let err = || ParseError::InvalidNumeric(s.to_string());
    let lower = s.trim().to_lowercase();
    let (num_str, mult): (&str, i64) = if let Some(n) = lower.strip_suffix("gb") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = lower.strip_suffix("mb") {
        (n, 1024 * 1024)
    } else if let Some(n) = lower.strip_suffix("kb") {
        (n, 1024)
    } else if let Some(n) = lower.strip_suffix('b') {
        (n, 1)
    } else {
        (lower.as_str(), 1)
    };
    let num: f64 = num_str.trim().parse().map_err(|_| err())?;
    if num < 0.0 || !num.is_finite() {
        return Err(err());
    }
    Ok((num * mult as f64) as i64)
}

/// Check if a bare token is a namespace name rather than a tag value.
fn is_namespace_name(s: &str) -> bool {
    matches!(s, "user" | "auto") || s.starts_with("plugin.")
}

fn parse_namespace(s: &str) -> TagNamespace {
    match s {
        "user" => TagNamespace::User,
        "auto" => TagNamespace::Auto,
        ns if ns.starts_with("plugin.") => {
            TagNamespace::Plugin(ns["plugin.".len()..].to_string())
        }
        _ => TagNamespace::Any,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert `expr` is a bare word's expansion — `tag OR filename OR
    /// description`, all three carrying `word` — and hand back the namespace
    /// the tag half searched.
    ///
    /// The tests below are about *tokenizing* (colons, parentheses, `=`), so
    /// they assert through this rather than restating the three-node shape each
    /// time.
    fn bare_word_parts(expr: &FilterExpr, word: &str) -> TagNamespace {
        let FilterExpr::Or { left, right } = expr else {
            panic!("Expected a bare-word expansion, got {:?}", expr)
        };
        let FilterExpr::Tag { namespace, value } = &**left else {
            panic!("Expected the tag term on the left, got {:?}", left)
        };
        assert_eq!(value, word, "tag half matches the word as typed");
        let FilterExpr::Or { left: name, right: desc } = &**right else {
            panic!("Expected the two text terms on the right, got {:?}", right)
        };
        let needle = word.to_ascii_lowercase();
        assert!(
            matches!(&**name, FilterExpr::Text { field: TextField::Filename, value } if *value == needle),
            "expected a lowercased filename term, got {:?}",
            name
        );
        assert!(
            matches!(&**desc, FilterExpr::Text { field: TextField::Description, value } if *value == needle),
            "expected a lowercased description term, got {:?}",
            desc
        );
        namespace.clone()
    }

    #[test]
    fn test_simple_tag() {
        let expr = parse_filter("vacation").unwrap();
        assert_eq!(bare_word_parts(&expr, "vacation"), TagNamespace::Any);
    }

    /// A bare word searches the filename too, so `fuji` finds `mount_fuji.jpeg`
    /// with no tag involved. Case is folded on the way into the text terms
    /// while the tag half keeps the spelling it was given — tag matching is
    /// exact, and `Japan` is not `japan`.
    #[test]
    fn test_bare_word_searches_text_case_folded() {
        let expr = parse_filter("Fuji").unwrap();
        assert_eq!(bare_word_parts(&expr, "Fuji"), TagNamespace::Any);
    }

    #[test]
    fn test_explicit_name_term() {
        let expr = parse_filter("name:FUJI").unwrap();
        assert!(matches!(
            expr,
            FilterExpr::Text { field: TextField::Filename, ref value } if value == "fuji"
        ));
    }

    #[test]
    fn test_explicit_desc_term() {
        let expr = parse_filter("desc:Sunset").unwrap();
        assert!(matches!(
            expr,
            FilterExpr::Text { field: TextField::Description, ref value } if value == "sunset"
        ));
    }

    /// The needle is folded exactly as far as SQLite folds the column, and no
    /// further: ASCII only. Folding the `Ü` here would make a filename typed
    /// exactly as it is spelled stop matching.
    #[test]
    fn test_needle_folds_ascii_only() {
        let expr = parse_filter("name:FÜJI").unwrap();
        assert!(matches!(
            expr,
            FilterExpr::Text { field: TextField::Filename, ref value } if value == "fÜji"
        ));
    }

    /// `name:` with nothing after it is rejected rather than matching every
    /// file — see [`text_term`].
    #[test]
    fn test_empty_text_term_is_rejected() {
        assert!(matches!(parse_filter("name:"), Err(ParseError::EmptyText(_))));
        assert!(matches!(parse_filter("desc:"), Err(ParseError::EmptyText(_))));
    }

    /// The explicit terms compose like any other, and negate correctly.
    #[test]
    fn test_text_terms_compose() {
        let expr = parse_filter("name:fuji AND NOT desc:winter").unwrap();
        let FilterExpr::And { left, right } = expr else {
            panic!("Expected And")
        };
        assert!(matches!(*left, FilterExpr::Text { field: TextField::Filename, .. }));
        assert!(matches!(*right, FilterExpr::Not { .. }));
    }

    #[test]
    fn test_namespaced_tag() {
        let expr = parse_filter("user::vacation").unwrap();
        match expr {
            FilterExpr::Tag { namespace, value } => {
                assert_eq!(namespace, TagNamespace::User);
                assert_eq!(value, "vacation");
            }
            _ => panic!("Expected Tag"),
        }
    }

    #[test]
    fn test_and() {
        let expr = parse_filter("user::vacation AND user::family").unwrap();
        assert!(matches!(expr, FilterExpr::And { .. }));
    }

    #[test]
    fn test_not() {
        let expr = parse_filter("NOT auto::indoor").unwrap();
        assert!(matches!(expr, FilterExpr::Not { .. }));
    }

    #[test]
    fn test_rating() {
        let expr = parse_filter("rating>=4").unwrap();
        match expr {
            FilterExpr::Rating { op, value } => {
                assert!(matches!(op, CompareOp::Gte));
                assert_eq!(value, 4);
            }
            _ => panic!("Expected Rating"),
        }
    }

    #[test]
    fn test_media_type() {
        let expr = parse_filter("type:video").unwrap();
        assert!(matches!(expr, FilterExpr::MediaType { value: MediaType::Video }));
    }

    #[test]
    fn test_complex() {
        let expr = parse_filter("(user::vacation OR user::travel) AND NOT auto::indoor").unwrap();
        assert!(matches!(expr, FilterExpr::And { .. }));
    }

    #[test]
    fn test_bare_namespace_user() {
        let expr = parse_filter("user").unwrap();
        match expr {
            FilterExpr::HasNamespace { namespace } => {
                assert_eq!(namespace, TagNamespace::User);
            }
            _ => panic!("Expected HasNamespace, got {:?}", expr),
        }
    }

    #[test]
    fn test_bare_namespace_plugin() {
        let expr = parse_filter("plugin.tagger").unwrap();
        match expr {
            FilterExpr::HasNamespace { namespace } => {
                assert_eq!(namespace, TagNamespace::Plugin("tagger".to_string()));
            }
            _ => panic!("Expected HasNamespace, got {:?}", expr),
        }
    }

    #[test]
    fn test_bare_tag_cross_namespace() {
        // A word that is NOT a namespace name should still search all namespaces
        let expr = parse_filter("example").unwrap();
        assert_eq!(bare_word_parts(&expr, "example"), TagNamespace::Any);
    }

    #[test]
    fn test_tag_and_namespace() {
        // "example AND user" → tag "example" across all ns AND all user-tagged images
        let expr = parse_filter("example AND user").unwrap();
        assert!(matches!(expr, FilterExpr::And { .. }));
    }

    #[test]
    fn test_tag_with_colon_in_value() {
        // Tags like "rating:general" should work — colon is part of the tag value
        let expr = parse_filter("rating:general").unwrap();
        // Without a namespace (no ::), this should be parsed contextually.
        // "rating:general" doesn't start with "rating>=" etc., so it falls through
        // to bare word matching.
        assert_eq!(bare_word_parts(&expr, "rating:general"), TagNamespace::Any);
    }

    #[test]
    fn test_namespaced_tag_with_colon_in_value() {
        // plugin.wd::rating:general — namespace is plugin.wd, tag value is "rating:general"
        let expr = parse_filter("plugin.wd::rating:general").unwrap();
        match expr {
            FilterExpr::Tag { namespace, value } => {
                assert_eq!(namespace, TagNamespace::Plugin("wd".to_string()));
                assert_eq!(value, "rating:general");
            }
            _ => panic!("Expected Tag, got {:?}", expr),
        }
    }

    #[test]
    fn test_tag_with_parentheses() {
        // Danbooru-style tags like "hatsune_miku_(vocaloid)" should not be
        // split by the tokenizer.
        let expr = parse_filter("hatsune_miku_(vocaloid)").unwrap();
        assert_eq!(
            bare_word_parts(&expr, "hatsune_miku_(vocaloid)"),
            TagNamespace::Any
        );
    }

    #[test]
    fn test_grouping_parens_still_work() {
        // Grouping parentheses (space-separated) should still work.
        let expr = parse_filter("(vacation OR travel) AND family").unwrap();
        assert!(matches!(expr, FilterExpr::And { .. }));
    }

    #[test]
    fn test_date_gte_full_day() {
        // date>=2024-03-15 → from = start of that day (UTC), to open.
        let expr = parse_filter("date>=2024-03-15").unwrap();
        match expr {
            FilterExpr::DateRange { field, from, to } => {
                assert_eq!(field, DateField::Taken);
                assert_eq!(from, Some(1710460800)); // 2024-03-15T00:00:00Z
                assert_eq!(to, None);
            }
            _ => panic!("Expected DateRange, got {:?}", expr),
        }
    }

    #[test]
    fn test_date_eq_whole_year() {
        // date=2024 → inclusive [2024-01-01T00:00:00Z, 2024-12-31T23:59:59Z].
        let expr = parse_filter("date=2024").unwrap();
        match expr {
            FilterExpr::DateRange { field, from, to } => {
                assert_eq!(field, DateField::Taken);
                assert_eq!(from, Some(1704067200)); // 2024-01-01T00:00:00Z
                assert_eq!(to, Some(1735689599)); // 2024-12-31T23:59:59Z
            }
            _ => panic!("Expected DateRange, got {:?}", expr),
        }
    }

    #[test]
    fn test_date_lte_whole_month_and_field() {
        // added<=2024-02 → to = end of Feb (leap year → 29th), from open.
        let expr = parse_filter("added<=2024-02").unwrap();
        match expr {
            FilterExpr::DateRange { field, from, to } => {
                assert_eq!(field, DateField::Added);
                assert_eq!(from, None);
                assert_eq!(to, Some(1709251199)); // 2024-02-29T23:59:59Z
            }
            _ => panic!("Expected DateRange, got {:?}", expr),
        }
    }

    #[test]
    fn test_date_viewed_field() {
        let expr = parse_filter("viewed>=2023").unwrap();
        assert!(matches!(
            expr,
            FilterExpr::DateRange { field: DateField::Viewed, .. }
        ));
    }

    #[test]
    fn test_date_composes_with_tag() {
        let expr = parse_filter("user::vacation AND date>=2024-01-01").unwrap();
        match expr {
            FilterExpr::And { left, right } => {
                assert!(matches!(left.as_ref(), FilterExpr::Tag { .. }));
                assert!(matches!(right.as_ref(), FilterExpr::DateRange { .. }));
            }
            _ => panic!("Expected And, got {:?}", expr),
        }
    }

    #[test]
    fn test_date_invalid_literal() {
        assert!(parse_filter("date>=notadate").is_err());
        assert!(parse_filter("date=2024-13").is_err()); // no month 13
        assert!(parse_filter("date>=24-01-01").is_err()); // year must be 4 digits
    }

    #[test]
    fn test_numeric_width_height() {
        let expr = parse_filter("width>=1920").unwrap();
        match expr {
            FilterExpr::Numeric { field, op, value } => {
                assert_eq!(field, NumericField::Width);
                assert!(matches!(op, CompareOp::Gte));
                assert_eq!(value, 1920);
            }
            _ => panic!("Expected Numeric, got {:?}", expr),
        }

        let expr = parse_filter("height<=1080").unwrap();
        assert!(matches!(
            expr,
            FilterExpr::Numeric { field: NumericField::Height, op: CompareOp::Lte, value: 1080 }
        ));
    }

    #[test]
    fn test_numeric_size_units() {
        // size>=10mb → 10 * 1024 * 1024 bytes
        let expr = parse_filter("size>=10mb").unwrap();
        match expr {
            FilterExpr::Numeric { field, op, value } => {
                assert_eq!(field, NumericField::FileSize);
                assert!(matches!(op, CompareOp::Gte));
                assert_eq!(value, 10 * 1024 * 1024);
            }
            _ => panic!("Expected Numeric, got {:?}", expr),
        }

        // Fractional + alias `filesize`, kb/gb, bare bytes
        assert!(matches!(
            parse_filter("filesize<=1.5gb").unwrap(),
            FilterExpr::Numeric { value, .. } if value == (1.5 * 1024.0 * 1024.0 * 1024.0) as i64
        ));
        assert!(matches!(
            parse_filter("size=500kb").unwrap(),
            FilterExpr::Numeric { value: 512000, .. }
        ));
        assert!(matches!(
            parse_filter("size>=2048").unwrap(),
            FilterExpr::Numeric { value: 2048, .. }
        ));
    }

    #[test]
    fn test_numeric_invalid() {
        assert!(parse_filter("width>=lots").is_err());
        assert!(parse_filter("size>=10tb").is_err()); // tb unsupported → not a number
    }

    #[test]
    fn test_numeric_composes() {
        let expr = parse_filter("width>=1920 AND height>=1080 AND size<=5mb").unwrap();
        // Left-associative: ((w AND h) AND size)
        assert!(matches!(expr, FilterExpr::And { .. }));
    }

    #[test]
    fn test_non_date_equals_falls_through_to_tag() {
        // A bare token with "=" that isn't a date field stays a bare word.
        let expr = parse_filter("foo=bar").unwrap();
        assert_eq!(bare_word_parts(&expr, "foo=bar"), TagNamespace::Any);
    }

    #[test]
    fn test_geo_bbox() {
        let expr = parse_filter("geo:37.7,-122.5,37.8,-122.4").unwrap();
        match expr {
            FilterExpr::GeoBbox { south, west, north, east } => {
                assert!((south - 37.7).abs() < 1e-9);
                assert!((west - -122.5).abs() < 1e-9);
                assert!((north - 37.8).abs() < 1e-9);
                assert!((east - -122.4).abs() < 1e-9);
            }
            _ => panic!("Expected GeoBbox, got {:?}", expr),
        }
    }

    #[test]
    fn test_geo_bbox_anti_meridian() {
        // west > east is allowed (Pacific-spanning bbox).
        let expr = parse_filter("geo:-30,170,30,-170").unwrap();
        assert!(matches!(expr, FilterExpr::GeoBbox { .. }));
    }

    #[test]
    fn test_geo_bbox_invalid_count() {
        assert!(parse_filter("geo:1,2,3").is_err());
    }

    #[test]
    fn test_geo_bbox_out_of_range() {
        assert!(parse_filter("geo:0,0,200,0").is_err());
    }

    #[test]
    fn test_has_geo() {
        let expr = parse_filter("has:geo").unwrap();
        assert!(matches!(expr, FilterExpr::HasGeo { present: true }));

        let expr = parse_filter("missing:geo").unwrap();
        assert!(matches!(expr, FilterExpr::HasGeo { present: false }));
    }

    #[test]
    fn test_geo_composes_with_filter() {
        // Bbox AND a tag — verifies parser slots the new atom into the
        // existing precedence chain.
        let expr = parse_filter("user::vacation AND geo:0,0,10,10").unwrap();
        match expr {
            FilterExpr::And { left, right } => {
                assert!(matches!(left.as_ref(), FilterExpr::Tag { .. }));
                assert!(matches!(right.as_ref(), FilterExpr::GeoBbox { .. }));
            }
            _ => panic!("Expected And, got {:?}", expr),
        }
    }

    #[test]
    fn test_tag_with_parens_in_expression() {
        // Tag with parens combined with boolean operators.
        let expr = parse_filter("hatsune_miku_(vocaloid) AND 1girl").unwrap();
        match &expr {
            FilterExpr::And { left, right } => {
                match left.as_ref() {
                    FilterExpr::Tag { value, .. } => assert_eq!(value, "hatsune_miku_(vocaloid)"),
                    _ => panic!("Expected Tag on left"),
                }
                match right.as_ref() {
                    FilterExpr::Tag { value, .. } => assert_eq!(value, "1girl"),
                    _ => panic!("Expected Tag on right"),
                }
            }
            _ => panic!("Expected And, got {:?}", expr),
        }
    }
}
