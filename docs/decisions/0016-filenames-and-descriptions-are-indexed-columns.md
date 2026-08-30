# 0016 — Filenames and descriptions are indexed columns, searched by substring

## Context

The filter bar's bare word compiled to one thing: an exact match against
`tag_index` across every namespace. So a photo called `mount_fuji.jpeg` did not
answer to `fuji` unless somebody had tagged it, which is the first thing anyone
types when they open a folder they have not curated. Filenames are the metadata
every file already has, and none of it was reachable.

Separately, a paragraph describing what an image shows — written by a person or
generated for them — had nowhere to live. `meta.core.notes` existed, but nothing
in the UI wrote it, nothing could search it, and it is the wrong field anyway:
notes are private remarks, and a generator that may overwrite a description must
not be free to overwrite those.

Both requirements land on the same constraint. `filter/evaluator.rs` compiles
the AST to a `WHERE` fragment over `media_meta` and never opens a sidecar, which
is what makes filtering usable on a large gallery — and means **a field is
filterable only if it is indexed**. `color_label` is the worked example: it lived
in the companion, and `color:red` silently compiled to `1 = 1` for a year.

## Options considered

**Tokenise filenames and descriptions into `tag_index`.** No schema change, no
evaluator change, and bare-word search would find them with nothing new written
— `mount_fuji.jpeg` becomes the tags `mount` and `fuji`. But tags are the
autocomplete vocabulary, and a description is prose: every word in the gallery
would become a suggestion, and the tag manager would fill with words nobody
chose. It also inverts the ownership rule, since `tag_index` is rebuilt from
companions and these tags belong to no companion.

**An FTS5 virtual table over both fields.** The real full-text answer: ranked
matching, and a trigram tokeniser makes `%needle%` an index lookup instead of a
scan. But it is a second table to keep in step with `media_meta`, a second
failure mode when they drift, and a dependency on FTS5 being compiled into the
bundled SQLite. Nothing measured says the scan is too slow.

**Two plain columns on `media_meta`, matched with `instr`.** One migration, one
new AST node, one evaluator arm. The scan is unavoidable either way at this
size, and the table is already read in full by every unfiltered sort.

For the description's home: **reuse `notes`**, or **a new `meta.core.description`**.

For what a bare word searches: **tags only, with `name:`/`desc:` for the rest**,
or **all three, with the explicit terms as narrowing**.

## Decision

Two unindexed columns on `media_meta` — `filename` and `description` (schema
v18) — matched case-insensitively by substring with `instr(lower(col), ?)`. A
new `FilterExpr::Text { field, value }` node compiles them; `name:` and `desc:`
produce one directly, and a bare word is expanded **by the parser** into
`tag OR name OR desc`.

Expanding in the parser rather than teaching the evaluator that `Tag { Any }`
secretly means three predicates keeps the tree honest about what it matches, and
means `NOT fuji` is handled by the existing `Not` arm rather than by a second
place that has to get De Morgan right.

The description is a new companion field, not `notes`. The two have different
owners: a description is written *for* an audience and is the field a generator
targets, notes are the owner's private remarks. Merging them would make
"describe everything that has no description" unsafe to offer.

Tags stay **exact** while the two text fields match by substring. A tag is a
token from a controlled vocabulary that autocomplete completes for you, so
substring-matching it would make `cat` match `cathedral` and leave no way to ask
for just `cat`.

`filename` holds the basename, not the path, and duplicates a substring of the
primary key to do it. Matching the whole path would fold the gallery root into
every row — a library at `~/Photos-fuji` would return all of itself for `fuji` —
and SQLite has no portable basename to extract one at query time. The directory
scan that discovers each file already holds the name, so it fills the column.

## Consequences

A bare word is now one scan of `media_meta` where it was an index seek. On a
hundred-thousand-item gallery that is tens of milliseconds, paid when the user
presses Enter rather than per keystroke, and it is why neither column is
indexed: an unanchored `%needle%` cannot use a B-tree, so an index would cost
writes and disk to be ignored by every query. If it does become the bottleneck,
FTS5 with the trigram tokeniser is the upgrade, and it can be added underneath
the same `Text` node.

Bare-word results widen. Typing `IMG` now returns every `IMG_1234.jpg` in the
library, which is correct for a search box and is a change to what existing
queries return. `user::fuji` still means only the tag.

Case folding is ASCII-only, on both sides. The column goes through SQLite's
`lower()`, which folds nothing else, so the needle is folded with
`to_ascii_lowercase` rather than Rust's full-Unicode `to_lowercase` — otherwise
the two sides disagree and a filename typed exactly as it is spelled stops
matching (`FÜJI` → `füji` against a column reading `fÜji.jpg`). A gallery whose
names differ only by the case of a non-ASCII letter therefore needs that letter
typed as it appears.

Both terms compile with an `IS NOT NULL` guard around the `instr`. Without it
`NOT fuji` would discard every file that has no description, because
`false OR NULL` is NULL and SQLite does not select NULL.

`name:` and `desc:` shadow any tag literally spelled `name:x` or `desc:x`. The
namespace form (`user::name:x`) still reaches them, which is the same escape
hatch that keeps `rating:general` a tag rather than a rating comparison.

Folder names remain unsearchable. The term that would fix it is `path:` over the
*gallery-relative* path, which has the same property the basename does; it was
not added because nothing asked for it.

`description` is mirrored into the column by every path that writes it — the
`set_description` commands, the duplicate merge, and the companion indexing pass
at gallery open, which is also what picks up a sidecar edited by hand. Adding
the field to the companion needed no schema-version bump: an `Option` that older
builds do not write is additive in both directions.

Generating descriptions is not part of this. The plugin host implements one verb
(`tag`), and an `enrich` verb is already the recorded plan for writing structured
metadata — see [`plugins/`](../plugins/README.md), Track B. The field and its
write paths exist and are reachable from the web client's `/api/invoke`
allowlist, so that work adds a producer rather than a place to put the output.
