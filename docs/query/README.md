# query/

[← docs](../README.md)

**Responsible for** every question a client can ask about *which* items:
the filter language and its SQL compilation, sorting, grouping, and the
autocomplete vocabulary. Also the home of what a **set** is, since a set is a
tag.

**Not responsible for** what the answer *contains* — that is the items query in
`services/media` — or for where the tags came from
([companion/](../companion/README.md)).

**Depends on** [`cache/`](../cache/README.md) for the index it compiles against.
**Depended on by** the items command, `lightview tag --filter`, and the tag
manager.

## One query, filter and sort together

The filter compiles into the same statement the sort orders. The round trip it
replaces — filter server-side, hand a list of matched paths back to the client,
hand them straight to the server again to be re-expanded — is gone, along with
the payload it cost on every sort change.

A field is filterable **only if it is indexed**. That is not a performance note,
it is the shape of the language: the index list in the schema is the list of
things you can ask about, which is why it lives in the schema rather than being
discovered later by a slow gallery.

## The language

```
vacation                          any namespace
user::vacation                    one namespace
set::kellys-comic                 set membership
plugin.wd::beach                  a plugin's bucket
"two words"                       a tag containing a space
NOT plugin.wd::indoor             negation
rating>=4                         rating
date=2024   date>=2024-01-01      capture date — a year, a year-month, or a day
added<=2024-06   viewed>=2023     date added, last viewed
width>=1920   height<=1080        pixel dimensions
size>=10mb   size<=500kb          file size (b/kb/mb/gb)
type:video                        media type
color:red                         colour label
has::user   has::set              namespace existence
has:geo     missing:geo           whether coordinates exist at all
(a OR b) AND NOT set::burst       grouping
```

Precedence is conventional: `OR` loosest, then `AND`, then `NOT`, parentheses to
override.

Two details that are decisions rather than accidents:

- **Term matching order.** `rating>=` is tried before a bare tag, so
  `rating:general` — a real tag the WD taggers emit — falls through to the tag
  branch rather than being mistaken for a rating filter.
- **Four-digit years are required.** Guessing the century for `24-01-01`
  silently produces the wrong range, and a date filter that is quietly wrong is
  worse than one that refuses.

Quoted strings exist because a tag can contain a space and the tokenizer
otherwise splits it into two terms that match nothing.

The expression tree is serializable in both directions, so the filter bar's
structured controls can hand back a tree rather than round-tripping through
query text.

## Sets

**Set membership is a tag** — a `set` namespace alongside `user` and
`plugin.<name>`, one tag per member.

```
set::vacation-burst-3     a burst that is not forty duplicates
set::kellys-comic         a work that exists as several images
set::alice                a face cluster, once a person has named it
```

That is the entire data model: no new file, no new table, no new wire format,
no new filter syntax. The tag index, autocomplete, grouping and every tag-write
command apply unchanged, and it is reconstructable from companions because it
*is* companion content.

Every tag-write command takes a `namespace` of `user` or `set` and nothing
else. A plugin namespace is never writable this way — a plugin bucket is
replaced wholesale by its own run — and the Rust type has no variant for one, so
a request naming it fails to deserialize rather than reaching a check. One
parameter on an existing family rather than a parallel family, because the
operations are identical and only the destination differs. Sets get their whole
surface from that: create is a batch add over a selection, rename is `rename`,
merging two clusters is `merge`, and the tag manager lists both namespaces.

Three consequences, each chosen:

- **Order comes from the gallery's own sort.** A comic's pages are `page01.jpg`,
  `page02.jpg`. An ordinal per member would be a second thing to keep in step
  with the filename, for a case the filename already answers.
- **A merge unions `set::` tags onto the keeper**, like user tags — without it,
  merging a set member silently drops that member's set.
- **Sets are cheap and fluid.** Renaming one rewrites every member's sidecar;
  trashing a member shrinks it silently. A set is not a durable object with an
  identity, it is a name several files agree on.

**"Not a duplicate" is not stored** — it is derived from set co-membership. See
[duplicates/](../duplicates/README.md#sets-are-what-not-a-duplicate-means-now).

## Autocomplete

The whole vocabulary is held in memory — roughly 300 KB at 5,000 unique tags —
so a query is a linear scan with no I/O and there is no cache-eviction policy to
reason about. It is fed by one `SELECT namespace, tag, COUNT(*) … GROUP BY`
at each refresh.

**There is no `tag_counts` table**, and its absence is deliberate: it was keyed
`(namespace, tag)` rather than by path, so it sat outside the
[path-keyed sweep](../cache/README.md#every-path-keyed-table-is-swept-together)
that has no exceptions; it needed two maintenance paths of its own; and it was
rebuilt per apply batch at a cost that scaled with the library rather than the
batch. This engine already holds every tag with a count, at the moments the
caller already refreshes it.

Ranking is four tiers — exact, prefix, substring, subsequence — and suggestions
are **deduplicated across namespaces**, summing counts: someone typing `beach`
wants one suggestion, not the same word once per namespace. Narrowing is what
the `user::beach` syntax is for.

The subsequence tier compares **character** counts on both sides. Comparing a
matched-character tally against a byte length meant any query containing a
multi-byte character could never satisfy the branch, so fuzzy matching was
silently dead for every non-ASCII query while the three higher tiers kept
working.

## Invariants a caller must uphold

- **Only ask about indexed fields.** Adding a filter term means adding an index
  in the same change, and both live in [`cache/`](../cache/README.md).
- **A writable namespace is `user` or `set`.** Never a plugin bucket.
- **Refresh autocomplete after a tag write.** The vocabulary is a cache of the
  index; a write that skips the refresh leaves the filter bar suggesting tags
  that no longer exist.
