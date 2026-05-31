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
        op: CompareOp,
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
    /// Bounding box over GPS coordinates. Decimal degrees, WGS-84.
    /// When `west > east` the box crosses the anti-meridian.
    GeoBbox {
        south: f64,
        west: f64,
        north: f64,
        east: f64,
    },
    /// Match images that have (or lack) any GPS coordinates.
    HasGeo {
        present: bool,
    },
    /// Date/time range filter against a media_meta date column. `from` and `to`
    /// are inclusive Unix-second bounds; either side may be open (`None`).
    DateRange {
        field: DateField,
        from: Option<i64>,
        to: Option<i64>,
    },
    /// Numeric comparison against a media_meta column. `value` is in the
    /// column's native unit: pixels for width/height, bytes for file_size.
    Numeric {
        field: NumericField,
        op: CompareOp,
        value: i64,
    },
}

/// Which numeric column a [`FilterExpr::Numeric`] targets.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NumericField {
    /// Pixel width (`media_meta.width`).
    Width,
    /// Pixel height (`media_meta.height`).
    Height,
    /// File size in bytes (`media_meta.file_size`).
    FileSize,
}

impl NumericField {
    /// The `media_meta` column this field maps to.
    pub fn column(&self) -> &'static str {
        match self {
            NumericField::Width => "width",
            NumericField::Height => "height",
            NumericField::FileSize => "file_size",
        }
    }
}

/// Which date column a [`FilterExpr::DateRange`] targets.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DateField {
    /// Capture date (`media_meta.date_taken`).
    Taken,
    /// Date the file was added to the gallery (`media_meta.date_added`).
    Added,
    /// Date the file was last viewed (`media_meta.last_viewed`).
    Viewed,
}

impl DateField {
    /// The `media_meta` column this field maps to.
    pub fn column(&self) -> &'static str {
        match self {
            DateField::Taken => "date_taken",
            DateField::Added => "date_added",
            DateField::Viewed => "last_viewed",
        }
    }
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

/// A numeric comparison operator, shared by rating and numeric-metadata filters.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompareOp {
    Gte,
    Lte,
    Eq,
}

impl CompareOp {
    /// The SQL operator this maps to.
    pub fn as_sql(&self) -> &'static str {
        match self {
            CompareOp::Gte => ">=",
            CompareOp::Lte => "<=",
            CompareOp::Eq => "=",
        }
    }
}
