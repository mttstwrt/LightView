use crate::companion::schema::MediaType;
use crate::filter::ast::{FilterExpr, RatingOp, TagNamespace};

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
}

/// Parse a filter query string into a FilterExpr.
///
/// Syntax examples:
///   vacation                                    → search all namespaces
///   user::vacation                              → specific namespace
///   plugin.face-recognition::person:alice       → plugin namespace
///   user::vacation AND user::family             → boolean AND
///   user::vacation OR user::travel              → boolean OR
///   NOT auto:indoor                             → boolean NOT
///   rating>=4                                   → rating filter
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
    let chars: Vec<char> = input.chars().collect();

    for (_i, &ch) in chars.iter().enumerate() {
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
    while !rest.is_empty() && rest[0].to_uppercase() == "OR" {
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
    while !rest.is_empty() && rest[0].to_uppercase() == "AND" {
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
    if tokens[0].to_uppercase() == "NOT" {
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

    // Bare tag: search all namespaces
    Ok((
        FilterExpr::Tag {
            namespace: TagNamespace::Any,
            value: token.clone(),
        },
        rest,
    ))
}

fn parse_rating<'a>(token: &str, rest: &'a [String]) -> ParseResult<'a> {
    let (op, val_str) = if let Some(v) = token.strip_prefix("rating>=") {
        (RatingOp::Gte, v)
    } else if let Some(v) = token.strip_prefix("rating<=") {
        (RatingOp::Lte, v)
    } else if let Some(v) = token.strip_prefix("rating=") {
        (RatingOp::Eq, v)
    } else {
        return Err(ParseError::InvalidRating(token.to_string()));
    };

    let value: u8 = val_str
        .parse()
        .map_err(|_| ParseError::InvalidRating(val_str.to_string()))?;

    Ok((FilterExpr::Rating { op, value }, rest))
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

    #[test]
    fn test_simple_tag() {
        let expr = parse_filter("vacation").unwrap();
        match expr {
            FilterExpr::Tag { namespace, value } => {
                assert_eq!(namespace, TagNamespace::Any);
                assert_eq!(value, "vacation");
            }
            _ => panic!("Expected Tag"),
        }
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
                assert!(matches!(op, RatingOp::Gte));
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
        match expr {
            FilterExpr::Tag { namespace, value } => {
                assert_eq!(namespace, TagNamespace::Any);
                assert_eq!(value, "example");
            }
            _ => panic!("Expected Tag, got {:?}", expr),
        }
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
        // to bare tag matching.
        match expr {
            FilterExpr::Tag { namespace, value } => {
                assert_eq!(namespace, TagNamespace::Any);
                assert_eq!(value, "rating:general");
            }
            _ => panic!("Expected bare Tag, got {:?}", expr),
        }
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
        match expr {
            FilterExpr::Tag { namespace, value } => {
                assert_eq!(namespace, TagNamespace::Any);
                assert_eq!(value, "hatsune_miku_(vocaloid)");
            }
            _ => panic!("Expected Tag, got {:?}", expr),
        }
    }

    #[test]
    fn test_grouping_parens_still_work() {
        // Grouping parentheses (space-separated) should still work.
        let expr = parse_filter("(vacation OR travel) AND family").unwrap();
        assert!(matches!(expr, FilterExpr::And { .. }));
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
