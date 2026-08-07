# Query: filtering, sorting, grouping, autocomplete

[← docs index](../README.md) · [architecture](../architecture.md)

Three modules that together answer "which files, in what order?": `filter/`
turns a query string into a set of paths, `sort/` orders and groups them, and
`autocomplete/` suggests tags while the user types the query. They are grouped
on one page because they share a vocabulary — namespaces, media types, the
`media_meta` columns — and because a change to one is usually a change to
another.

**Responsible for:** the filter query language (tokenizer, parser, AST, SQL
compilation), the sort field vocabulary and its `ORDER BY` construction, group
headers, the scrollbar timeline index, and in-memory tag autocomplete.

**Not responsible for:** executing SQL — `filter/evaluator.rs` produces a
`WHERE` fragment plus bound parameters and hands them back;
[`cache/`](../cache/README.md) owns the connection. Not responsible for tag
storage either: autocomplete is a read-only view over `tag_counts`, refreshed
by whoever wrote a tag.

**Public interface:** `filter::parser::parse_filter`,
`filter::evaluator::to_sql`, `filter::ast::FilterExpr`,
`CacheDb::get_sorted_items`, `CacheDb::compute_timeline`,
`sort::grouper::compute_groups`, and `AutocompleteEngine::{refresh, query}`.

**Depends on:** [`cache/`](../cache/README.md) (the `media_meta`, `tag_index`,
and `tag_counts` tables) and [`companion/`](../companion/README.md) (the
`MediaType` vocabulary and namespace names).

**Depended on by:** `commands/filter.rs`, `commands/sort.rs`,
`commands/autocomplete.rs`, and the matching arms of the
[`/api/invoke` allowlist](../remote/README.md). On the frontend, `filterStore`
and `settingsStore`.

## The filter language

A query is a boolean expression over terms. `AND`, `OR`, `NOT`, and parentheses
work as expected; a bare word with no operator searches every namespace.

```
vacation                                    any namespace contains this tag
user::vacation                              a specific namespace
plugin.face-recognition::person:alice       a plugin namespace
NOT auto::indoor                            negation
(user::a OR user::b) AND NOT auto::indoor   grouping
rating>=4                                   rating comparison
type:video                                  media type
has::user                                   namespace is non-empty
has:geo                                     has GPS coordinates
date>=2024-01-01  date=2024  added<=2024-06 dates: taken / added / viewed
width>=1920  height<=1080                   pixel dimensions
size>=10mb  size<=500kb                     file size (b/kb/mb/gb)
```

Two parsing details are worth knowing because they look like bugs otherwise.
First, a colon is part of a tag value, not a separator: `rating:general` is the
*tag* `rating:general`, not a rating comparison, because the rating forms are
`rating>=`, `rating<=`, and `rating=`. Second, namespaces use a double colon
(`user::x`) precisely so that single colons stay available inside tag values —
which the ML taggers rely on heavily (`character:...`, `person:...`).

Dates accept a year, a year-month, or a full date, and expand to the inclusive
range that spells: `date=2024` is all of 2024, not midnight on New Year's Day.
The four-digit year requirement is deliberate — `24-01-01` is rejected rather
than guessed at.

## From AST to SQL

`parse_filter` produces a `FilterExpr` tree; `evaluator::to_sql` walks it into a
`WHERE` fragment against `media_meta m`, pushing every literal onto a parameter
vector rather than interpolating it. Tag terms compile to an `EXISTS` subquery
over `tag_index`; everything else is a column comparison on `media_meta`.

Filtering therefore runs entirely in SQLite over the index, and never opens a
companion file. That is what makes it usable on a large gallery, and it is also
the constraint that shapes the language: **a field is filterable only if it is
indexed in `media_meta`.** Colour label is the standing exception — it lives
only in the companion, so `ColorLabel` compiles to a tautology and the term is
silently ignored. Making it work means adding a `color_label` column and
indexing it at scan time; see [`todo.md`](../todo.md).

## Sorting and grouping

`get_sorted_items` selects from `media_meta m` with a `LEFT JOIN thumbnails t`
— the join exists purely to inline the ~25-byte ThumbHash placeholder into the
items payload, so the grid can paint every cell blurry before any thumbnail
request goes out. On the remote web client that is the difference between a grey
grid and a recognisable one on first paint.

That join is also a trap, and the reason `order_expr` qualifies **every** column
with the `m.` alias: `thumbnails` has its own `path` and `media_type` columns,
so a bare column name makes SQLite reject the whole statement as ambiguous.
Sorting by name or media type — and any sort using either as a tiebreaker —
broke outright when they were unqualified. Keep them qualified regardless of
whether a future join brings the name into scope.

A filtered list is passed as a JSON array bound to one parameter and expanded
with `json_each`, rather than a generated `IN (?, ?, …)` list. One prepared
statement serves every filter size, so the statement cache stays warm.

Grouping is a separate, purely in-memory pass: `compute_groups` walks the
already-sorted items and emits `{label, start_index, count}` headers wherever
the group key changes. It never re-sorts, so a grouping that disagrees with the
sort field produces fragmented headers rather than a reordered list — grouping
describes the order, it does not impose one.

`compute_timeline` is the same idea for the scrollbar: walk the dates
descending, emit an entry at each month boundary, and convert the item position
to a row index using the caller's `items_per_row`. The row index is therefore
only valid for the layout that asked for it, which is why it takes that
parameter rather than caching a single index.

## Autocomplete

`AutocompleteEngine` holds every unique tag in memory — about 300 KB at 5,000
tags, so there is no cache-eviction story to tell. It is refreshed from
`tag_counts` at gallery open and after any tag write.

Matching is a four-tier score: exact, prefix, substring, then subsequence
(fuzzy). Results are deduplicated *across* namespaces, summing counts and
keeping the best score, because a user typing `beach` wants one suggestion, not
the same word once per namespace; narrowing to a namespace is what the
`user::beach` syntax is for. When the input looks like a namespace name, the
engine also emits namespace suggestions marked with the sentinel namespace
`_namespace`, which the frontend renders differently.

The subsequence tier compares character counts, not byte counts. That
distinction was a real bug: comparing a matched-character tally against
`needle.len()` meant any needle containing a multi-byte character could never
satisfy the branch, so fuzzy matching was silently dead for every non-ASCII
query while the three higher tiers kept working.
