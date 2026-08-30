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
headers, and in-memory tag autocomplete.

**Not responsible for:** executing SQL — `filter/evaluator.rs` produces a
`WHERE` fragment plus bound parameters and hands them back;
[`cache/`](../cache/README.md) owns the connection. Not responsible for tag
storage either: autocomplete is a read-only view over `tag_counts`, refreshed
by whoever wrote a tag.

**Public interface:** `filter::parser::parse_filter`,
`filter::evaluator::to_sql`, `filter::ast::FilterExpr`,
`CacheDb::get_sorted_items`, `sort::grouper::compute_groups`, and
`AutocompleteEngine::{refresh, query}`.

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
vacation                                    a tag in any namespace — or the
                                            filename, or the description
name:fuji                                   filename only
desc:volcano                                description only
user::vacation                              a specific namespace
plugin.face-recognition::person:alice       a plugin namespace
Japan  Kyoto  United_States                 place names, from geocoded GPS
plugin.location::Georgia                    …narrowed, when a name is ambiguous
NOT auto::indoor                            negation
(user::a OR user::b) AND NOT auto::indoor   grouping
rating>=4                                   rating comparison
type:video                                  media type
has::user                                   namespace is non-empty
has:geo                                     has GPS coordinates
color:red  color:none                       colour label (or the absence of one)
date>=2024-01-01  date=2024  added<=2024-06 dates: taken / added / viewed
width>=1920  height<=1080                   pixel dimensions
size>=10mb  size<=500kb                     file size (b/kb/mb/gb)
```

## What a bare word searches

A bare word matches three things: the tag, the filename, and the description.
`fuji` finds `mount_fuji.jpeg` whether or not anyone tagged it, and finds a
photo whose description mentions the volcano. The parser expands the word into
`tag OR name OR desc` itself, so the tree the evaluator walks says exactly what
it matches — and `NOT fuji` gets De Morgan from the existing `Not` arm instead
of being a second place to get it right.

The three terms do not match the same way, and the asymmetry is deliberate:
**the tag is exact, the filename and description are substrings.** A tag is a
token from a controlled vocabulary that autocomplete completes for you, so
substring-matching it would make `cat` match `cathedral` with no way left to ask
for just `cat`. A filename and a description are prose nobody types in full.
Case is folded on the two text terms and not on the tag — `Japan` is still not
`japan`, which is what autocomplete is for. The folding is **ASCII-only on both
sides**, deliberately: the column goes through SQLite's `lower()`, which folds
nothing else, so folding the needle more thoroughly in Rust would turn an exact
match into a miss — `FÜJI` would become `füji` while `FÜJI.jpg` stayed
`fÜji.jpg`, and typing a filename exactly as spelled would find nothing.

`name:` and `desc:` narrow to one of the two text fields. They are checked
before the bare-word fallthrough, so they shadow any tag literally spelled
`name:something` — which stays reachable through the namespace form,
`user::name:something`, the same escape hatch that keeps `rating:general` a tag.
Neither accepts an empty needle: `instr(x, '')` finds the empty string at
position 1, so `name:` alone would compile to a term matching every file that
has a name, which is a filter written to narrow that silently widens.

The filename is the basename, not the path. Matching the whole path would fold
the gallery root into every row — a library living in `~/Photos-fuji` would
answer `fuji` with all of it — so `media_meta.filename` holds the basename with
its extension, populated by the same scan that discovers the file. Folder names
are therefore not searchable; if they should be, the term to add is `path:` over
the *gallery-relative* path, which has the same property the basename does.

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

**The tokenizer splits on whitespace and there is no quoting**, so no tag
containing a space can be named by any query — the parser reaches the second
word with nothing to do and rejects the expression rather than matching
anything. Every tag writer therefore joins words with underscores, which is
where `hatsune_miku_(vocaloid)` and `United_States` both come from. Adding
quoted strings to the language would lift the restriction; until then, a tag
with a space in it is unreachable, not merely awkward.

Place names are ordinary tags, not a term of their own. Geotagged media is
reverse-geocoded at gallery open and the country, region, and city names are
written into companion files under `plugin.location` — so `Japan` works as a
bare word for the same reason `vacation` does, and needs no new syntax. The
namespace prefix is there for the cases where a name is ambiguous on its own:
`plugin.location::Georgia` is the country and the US state but not a user tag
spelled the same way. Because tag matching is exact, `japan` will not match
`Japan`; autocomplete is case-insensitive and fuzzy, which is what covers it.
See [`geocode/`](../geocode/README.md) for how the names are derived and why
they are stored on disk rather than in the cache.

## From AST to SQL

`parse_filter` produces a `FilterExpr` tree; `evaluator::to_sql` walks it into a
`WHERE` fragment against `media_meta m`, pushing every literal onto a parameter
vector rather than interpolating it. Tag terms compile to an `EXISTS` subquery
over `tag_index`; everything else is a column comparison on `media_meta`.

Filtering therefore runs entirely in SQLite over the index, and never opens a
companion file. That is what makes it usable on a large gallery, and it is also
the constraint that shapes the language: **a field is filterable only if it is
indexed in `media_meta`.**

Colour label is the worked example. It lives in the companion at
`meta.core.color_label`, and for a long time `color:red` compiled to `1 = 1` —
so a term the user wrote to *narrow* a search silently widened it. Fixing it was
not a change to the evaluator but a change to what is indexed: a `color_label`
column (schema v17, with a partial index, since most rows have none) plus a
mirror in every path that writes the field. Values are lowercased on the way
into the column so `color:Red` and `color:red` are one query and the comparison
can use the index; the companion keeps whatever spelling it was given, because
it is the record of intent rather than the query surface.

`color:none` is the absence of a label rather than a label spelled "none" — it
compiles to `IS NULL` and binds no parameter. "Which of these have I not triaged
yet?" is the question a colour workflow actually asks.

The text terms are the same story twice over. `description` is a mirror of
`meta.core.description` written by every path that sets it, for the same reason
`color_label` is one. `filename` duplicates a substring of the primary key,
which looks redundant until you try to extract a basename in SQL — there is no
portable way, and the `rtrim`/`replace` trick mishandles Windows separators, so
the column is filled by the directory scan that already holds the name. Neither
column is indexed: an unanchored `%needle%` cannot use a B-tree, so an index
would cost writes to be ignored. They are scan columns, and a bare word is
consequently one pass over `media_meta` where it used to be an index seek —
tens of milliseconds on a large gallery, paid when the user presses Enter rather
than per keystroke. If that ever becomes the bottleneck, the answer is an FTS5
trigram index, not a B-tree.

Both terms compile with an `IS NOT NULL` guard, and that guard is load-bearing.
`instr(lower(NULL), 'x')` is NULL, `false OR NULL` is NULL, and `NOT NULL` is
NULL — which SQLite does not select. Without the guard, `NOT fuji` would throw
away every file that has no description at all.

`apply_filter` returns bare paths and takes no `DISTINCT`: `media_meta.path` is
the primary key and tag terms are correlated `EXISTS` subqueries rather than
joins, so a row cannot be produced twice. There is also no "clear the filter"
command, and deliberately so — clearing is the *absence* of a path list, which
`get_sorted_items` already expresses as `filter_paths: None`. A command that
returned every path so the caller could hand them all back read the whole table
and serialized the entire gallery as JSON to arrive at the same rows.

There was once a second, in-memory evaluator that walked companion files
directly, and it was the only thing that could have honoured a colour label
before the column existed. Nothing ever called it. Keeping a parallel
implementation of the language's semantics meant any divergence was silent — and
they had already diverged on exactly this term — so it was deleted rather than
wired up. Evaluating a filter by opening every sidecar is the cost the tag index
exists to remove.

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

The key a time grouping compares is a `(year, month, day)` triple, not the
rendered label, and the distinction is the cost of the pass. Formatting is the
expensive half — a `strftime` and a `String` per call — and a gallery of a
hundred thousand items holds a few hundred distinct periods at most, so deciding
the split on labels meant formatting every item to discover that nearly all of
them belonged to the group already open. The label is now produced once per
group. The two must stay in step: a key and its label are one-to-one per
granularity, and both spell a missing or unrepresentable timestamp "Unknown
date", which is what keeps those items grouped together.

The scrollbar's date markers are *not* computed here. There was a
`compute_timeline` that emitted an entry per month boundary and converted item
positions to row indices, but the frontend had independently grown its own
version in `App.tsx` over `sortedItems` — which it already holds, which also
covers name and size indicators, and which needs no `items_per_row` round-trip.
The backend one was deleted rather than kept as a second answer to the same
question.

## Autocomplete

Descriptions and filenames are absent from all of this, by construction rather
than by a filter: the vocabulary is built from `tag_counts`, and neither of them
is a tag. A description is a paragraph, so tokenising it into the tag vocabulary
would put every word in the gallery into the suggestion list.

`AutocompleteEngine` holds every unique tag in memory — about 300 KB at 5,000
tags, so there is no cache-eviction story to tell. It is refreshed from
`tag_counts` at gallery open and after any tag write.

It holds each tag's lowercase form alongside it, computed at refresh. That is
the traffic ratio made explicit: the vocabulary changes when someone edits a
tag, and is scanned in full on every keystroke in the filter bar, so folding
case per query meant thousands of allocations to answer one character with a
value that had not changed since the last write. For the same reason the
per-query map borrows its keys from the vocabulary instead of copying them —
only the tags that survive to become suggestions are ever cloned.

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
