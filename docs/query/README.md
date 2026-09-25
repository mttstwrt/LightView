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

## The date a file sorts by is not the date it was taken

`media_meta` keeps both and they mean different things.

- **`date_taken`** is the camera's EXIF `DateTimeOriginal`, and NULL when the
  file has none — which is most of a library. Screenshots, exports, anything
  out of a messaging app and every video carry no capture time.
- **`mtime`** is the file's modification time. Always present.

**Sorting and grouping coalesce; filtering does not.** The `date` sort orders by
`COALESCE(date_taken, mtime)` — see `SORT_DATE` in
[`sort/sorter.rs`](../../src-rust/src/sort/sorter.rs), which both the sort and
the idle warmer read so the warm-up order cannot drift from the order on screen.
The `date=2024` family in [`filter/`](../../src-rust/src/filter/) compiles
against `date_taken` alone.

That asymmetry is the one *except* in this area, and it is deliberate in both
directions. Ordering has to place every file somewhere, and `date_taken` alone
swept most of the library into one undated heap at the end. Filtering has to
mean what it says: `date=2024` asks for photographs *taken* in 2024, and
answering it with files merely *copied* in 2024 would be a worse wrong than the
heap — silently, since a filter gives no sign of what it included.

The viewer's info panel resolves the ambiguity for the reader rather than
hiding it: it prints `Taken …` when there is a capture time and `Modified …`
when there is not, so a file's position in the grid is always explainable.

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

- **A set has no order of its own until someone gives it one.** Under every
  column sort its members sort like any other files. A set given an order
  becomes a *block* in the [Custom order](#the-custom-order) — which a set
  needed the moment its files' names stopped carrying one: downloads named by
  hash, two cameras' numbering, pages scanned out of sequence.
- **A merge unions `set::` tags onto the keeper**, like user tags — without it,
  merging a set member silently drops that member's set.
- **Sets are cheap and fluid.** Renaming one rewrites every member's sidecar;
  trashing a member shrinks it silently. A set is not a durable object with an
  identity, it is a name several files agree on.

**"Not a duplicate" is not stored** — it is derived from set co-membership. See
[duplicates/](../duplicates/README.md#sets-are-what-not-a-duplicate-means-now).

## The Custom order

A sort a person arranges by hand. **Whatever nobody arranged stays where the
date order puts it**, so an unarranged gallery reads exactly as Date does, and a
new file lands where its date puts it. It takes no direction and no sub-sort —
the order is total and was made by hand, so there is nothing to reverse and no
tie to break — and it has no group headers, which would be labelled by a date
the list is not ordered by.

**Every file has a key, a string, and Custom sorts by it.** A file's key is the
one stored in its sidecar's [`meta.order`](../companion/README.md#the-custom-order-lives-beside-core),
or else its default: its sort date as a fixed-width number, newest first, then
its path — see [`order_key`](../../src-rust/src/sort/order_key.rs). Placing a
file writes one key into one sidecar and renumbers nothing.

**A placement hugs a neighbour.** A file dropped behind P gets P's key plus a
short suffix, so nothing that arrives later can land between them. The midpoint
between P and the next file would land wherever the midpoint of their *dates*
falls — possibly years from P — and the second camera's photos from the same
event would slot in between. And P's key is written into P's sidecar if it was
only a default: dates can be read again later, and read differently on another
host — a video's container time in the host's zone, a file time on a host
without `ffprobe` — and a frozen anchor cannot move out from under what was
placed against it. Unarranged files still sort by each host's own reading, as
they do under Date.

A drop at the very top anchors to the file that was first, rather than pinning:
files that arrive later still land above it, where they would have landed had
nothing been moved.

**A set given an order is a block.** Its members are contiguous, in the order
given, and the block sits at the **lowest key among its members** — a minimum
rather than one stored value, because a set is a name, not an object, and has
nowhere to store one. Locking writes that minimum onto every member and moving
the block rewrites every member, so they agree; if a move is interrupted, the
minimum still keeps the block in one place. Sets nobody ordered — a burst, a
face cluster spanning ten years — stay loose.

A file is in **at most one block**: it appears once in the grid, so it cannot be
contiguous inside two. The sidecar holds one `order.set`, so a second is
unrepresentable, and locking a file that is in another block is refused before
anything is written.

What the operations do, all through [`services/order`](../../src-rust/src/services/order.rs):

| Operation | Effect |
|---|---|
| `place` | into the gap between the two neighbours the person saw. A gap touching the file's own block reorders the set; any other moves the file, or its whole block. A neighbour inside a foreign block stands for the block, so a drop never lands inside one. Under a filter, the file lands right behind the visible neighbour, ahead of anything hidden |
| `lock_set` | the files, in display order, into a block — appended if the set already is one |
| `unlock_set` | the members back to their date places; the set stays |
| `reset_order` | these files back to their date places |

And what the set operations do to a block: **removing** a file from a set, or
deleting the set, takes it out of the block — back to its date place, rather
than clumped at the block's key; **renaming** carries the block; **merging**
concatenates the blocks under the lower key. An `order` naming a set the file
is no longer in can then only come from an older build, which renames sets
without knowing about `order`; the index ignores it whole, and nothing deletes
it — renaming the set back restores it.

Measured on a debug build over 20,000 files on local disk: the Custom query
~0.35 s against Date's ~0.30 s; placing a file ~50 ms; moving a 200-member block
~0.3 s, one sidecar write per member. The block move on a network share is
unmeasured.

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
- **Never change `DEFAULT_KEY`'s encoding.** Stored keys were generated relative
  to default keys, so a change moves every arranged file relative to every
  unarranged one, in every gallery, with no error.
- **Take a file out of a set only through the tag service**, which takes it out
  of the set's block in the same write.
