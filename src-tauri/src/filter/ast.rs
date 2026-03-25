use serde::{Deserialize, Serialize};

use crate::companion::schema::MediaType;

/// A filter expression that can be evaluated against companion data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FilterExpr {
    /// Match a specific tag in a namespace
    Tag {
        namespace: TagNamespace,
        value: String,
    },
    /// Logical AND of two expressions
    And {
        left: Box<FilterExpr>,
        right: Box<FilterExpr>,
    },
    /// Logical OR of two expressions
    Or {
        left: Box<FilterExpr>,
        right: Box<FilterExpr>,
    },
    /// Logical NOT of an expression
    Not {
        expr: Box<FilterExpr>,
    },
    /// Rating comparison
    Rating {
        op: RatingOp,
        value: u8,
    },
    /// Media type filter
    MediaType {
        value: MediaType,
    },
    /// Has any tags in a given namespace
    HasNamespace {
        namespace: TagNamespace,
    },
    /// Color label match
    ColorLabel {
        value: String,
    },
}

/// Tag namespace specifier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TagNamespace {
    User,
    Auto,
    Plugin(String),
    /// Search across all namespaces
    Any,
}

impl TagNamespace {
    /// Convert to the string representation used in the tag_index table.
    pub fn to_db_namespace(&self) -> Option<String> {
        match self {
            TagNamespace::User => Some("user".to_string()),
            TagNamespace::Auto => Some("auto".to_string()),
            TagNamespace::Plugin(name) => Some(format!("plugin.{}", name)),
            TagNamespace::Any => None, // means search all
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RatingOp {
    Gte,
    Lte,
    Eq,
}
