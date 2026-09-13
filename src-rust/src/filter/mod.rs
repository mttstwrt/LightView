//! The filter query language: string → [`ast::FilterExpr`] → SQL.
//!
//! [`parser`] tokenizes and parses; [`ast`] is the tree plus the small
//! vocabularies (which column a numeric or date term targets, which namespace
//! a tag term means); [`evaluator`] compiles the tree to a `WHERE` fragment.
//!
//! Two syntax decisions shape everything else. A single colon belongs to the
//! *tag value* (`character:alice` is one tag), so namespaces use a double
//! colon — the ML taggers emit colon-bearing tags constantly and would
//! otherwise be unquotable. And a bare word searches every namespace, because
//! that is what someone typing into a search box means.
//!
//! See `docs/query/README.md` for the language reference.

pub mod ast;
pub mod parser;
pub mod evaluator;
