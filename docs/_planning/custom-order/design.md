# Design

Principle 1's five questions, in order. Requirements are cited as R1–R9 from
[requirements.md](requirements.md).

## The model in one paragraph

Every file has a **key**, a string, and Custom sorts by it ascending. A file's
key is the override stored in its sidecar, or else `DEFAULT_KEY`: its sort date
as a fixed-width number that grows smaller as the date grows later, followed by
its path. Paths are unique, so keys are. **Placing** a file gives it a key
strictly between its new neighbours' keys — one sidecar write, nothing
renumbered. A **block** is the files whose sidecar names a set they are still
in; it sorts at the **minimum key of its members**, and inside it by `pos`,
another string from the same generator. The ORDER BY is block key or own key,
then block name, then `pos`, then path: a total order, so no two queries can
disagree about ties.

## Placement

```
sort/order_key.rs       new, pure: between(), spread(), default_key(alias)
sort/sorter.rs          SortField::Custom and its statement
companion/schema.rs     MetaCollection.order: Option<Order>
cache/db.rs             media_order table + index; in path_keyed_tables()
cache/index.rs          reindex_file also writes media_order; reports a change
services/gallery.rs     the companion re-read stamp; OrderChanged from the
                        watcher and the sweeps
services/order.rs       new: place, lock_set, unlock_set
services/tags.rs        edit() split into edit_companion(); rename and merge
                        carry order.set
services/duplicates.rs  the keeper adopts an order
server/commands.rs      place, lock_set, unlock_set — Trust::Device
server/events.rs        OrderChanged, "order-changed"
src-solidjs/            SortMenu, galleryStore, scrollIndicators, the viewer,
                        the grid, the command list
```

**`sort/order_key.rs` is a pure library**, beside the sorter that is its only
SQL consumer. It knows strings and SQL fragments, nothing about sidecars or
requests. `default_key(alias)` returns the SQL expression the same way
[`SORT_DATE`](../../../src-rust/src/sort/sorter.rs) is a constant: one
definition, read by the statement and by the tests that pin it.

**`services/order.rs` is the only module that knows both keys and sidecars.**
It reads neighbours' effective keys from the cache, computes a new key, and
writes sidecars through `edit_companion`. It sits beside `services/tags.rs`
rather than inside it because a placement is not a tag write: it edits `meta`,
and the set it touches is read, never changed.

**`filter/` is untouched.** An earlier draft derived "the set this filter pins"
from the AST; the block model does not need it, because blocks are ordered
wherever they appear, filtered or not.

Dependencies point down only. `cache/` learns a table and nothing about who
fills it; the services learn a column set; the server learns three commands.

## Contract

### The sidecar — durable, read by other installations

```json
"meta": {
  "core":  { "rating": 4 },
  "order": { "key": "4611686016727387904photos/a.jpgU", "set": "comic", "pos": "V" }
}
```

`Order { key, set, pos }`, every field `Option<String>` with `#[serde(default)]`,
the object omitted when all three are absent.

**On `MetaCollection`, not in `meta.core`.** `CoreMeta` has no `extra`, so a
field added there is dropped by any older build the next time it saves a rating
— the data loss [companion/](../../companion/README.md) draws its line at.
`MetaCollection` has `#[serde(flatten)] extra`, so an older build round-trips
`order` untouched: it ignores the arrangement and never destroys it. No
`CURRENT_SCHEMA_VERSION` bump — the change is additive and `serde(default)`
covers a sidecar without it.

**Strings from day one**, so the representation never has to change type; a
type change in a sidecar makes the whole file fail to deserialize, and
`modify_companion` then refuses every write to it.

**`DEFAULT_KEY` becomes a durable format.** Stored keys are written relative to
it, so changing its encoding would move every placed file relative to every
unplaced one, in every gallery, silently. It is pinned by a test against known
values, and AGENTS.md gains the line. The encoding:

```sql
printf('%019d', 4611686018427387904 - COALESCE(m.date_taken, m.mtime)) || m.path
```

Epoch seconds, offset by 2^62 so pre-1970 capture dates stay positive; nineteen
digits so the text order is the numeric order; SQLite's `printf` is 64-bit
(checked on 3.45). Newest first ascending, so a new file sorts above older ones
exactly as Date does (R1, R2).

**No pruning.** The indexer ignores `order.set` when it names a set the file is
no longer in, and treats the file as loose with its key. Nothing deletes the
stale name: after an older build renames a set, that name is the only surviving
record of the arrangement, and a file re-added to its set snaps back into place.

### The cache

```sql
CREATE TABLE IF NOT EXISTS media_order (
    path   TEXT PRIMARY KEY,
    key    TEXT,
    block  TEXT,   -- order.set, only while the file is in that set
    pos    TEXT
);
CREATE INDEX IF NOT EXISTS idx_order_block ON media_order(block) WHERE block IS NOT NULL;
```

A row exists only for a file whose sidecar carries `order`. It joins
`path_keyed_tables()`, and the existing test that reads `sqlite_master` fails
the build if it does not.

**No `FORMAT_VERSION` bump — an amendment to a written rule, argued here.**
[cache/](../../cache/README.md) says to bump for any schema change "including an
added column". A bump re-thumbnails every library, and on a gallery with no
sidecars it permanently erases `date_added`, `last_viewed` and `date_rated` —
R8 forbids exactly that, and
[`services/gallery.rs`](../../../src-rust/src/services/gallery.rs) already makes
the same argument for why a *reader* change is a stamp and not a bump.

`schema_sql()` runs `CREATE TABLE IF NOT EXISTS` on every open, so the table
appears in an existing cache by itself. What it lacks is content, and that is a
reader's catch-up: a `companion_index_version` stamp in `gallery_meta`, following
`reprobe_videos_if_the_reader_changed`, clears `index_state` and stamps in one
transaction, and the existing companion sweep re-reads every sidecar once.

The rule `docs/cache/README.md` gains: **adding a table that a reader fills is a
stamp; changing an existing table's shape is still a bump.** An existing table
never changes shape without one, so "a silently mismatched schema" still cannot
happen.

Stated cost: two builds alternating on **one machine** share one cache
directory, and a file the older build re-indexes gets its `index_state` stamped
without an order row, so the newer build skips it until its sidecar next
changes. Two machines have two caches and are unaffected. A bump would instead
rebuild the cache on every alternation.

### `reindex_file`

The one writer of companion-derived rows — every caller (the tag commands, the
sweep at open, the hourly sweep, the watcher, the duplicate merge) already goes
through it. It gains the `media_order` delete-and-insert inside its existing
transaction, and returns whether that row changed. It sets `block` only when
`order.set` is one of the file's `tags.set`.

### The statement

```sql
WITH blocks AS (
    SELECT o.block, MIN(COALESCE(o.key, <default_key m2>)) AS key
    FROM media_order o JOIN media_meta m2 ON m2.path = o.path
    WHERE o.block IS NOT NULL
    GROUP BY o.block
)
SELECT <cols>, o.block
FROM media_meta m
LEFT JOIN media_order o ON o.path = m.path
LEFT JOIN blocks b ON b.block = o.block
WHERE <filter>
ORDER BY COALESCE(b.key, o.key, <default_key m>), o.block, o.pos, m.path
```

Only Custom joins; every other sort's statement is unchanged and selects `NULL`
in the block column, so the positional row mapper has one shape. The no-join
test guards the overflow-page cost of joining a **tier** table; it is narrowed to
exactly that, and the alias test learns `o.`, `b.` and `m2.`.

The block key is the minimum over the members' keys rather than a value stored
once, because there is nowhere to store it once — a set is a name several files
agree on, not an object. Locking writes the minimum onto every member and a move
rewrites every member, so the members agree; if a move is interrupted, the
minimum still puts the whole block in one place, and repeating the move repairs
it.

### Wire

- `SortField::Custom`, `"custom"`. **Takes no direction and no sub-sort**, and
  is not offered as a sub-sort: the order is total and was arranged by hand, so
  reversing it or breaking its ties means nothing. The one sort-level *except*.
- `SortedItem` gains `block: Option<String>` — non-null only under Custom.
- `place { path, after?, before? }` names the visible gap the drop landed in.
  - If either neighbour is in `path`'s own block: set `pos` immediately after
    `after` within the block (or immediately before `before` at the block's
    start). One sidecar write.
  - Otherwise: move `path` — or its whole block, if it is in one — to
    immediately after `after` in the **full** order, or immediately before
    `before` when there is no `after`, or to the top when there is neither
    (R6). A neighbour inside a foreign block stands for that whole block, so a
    drop never lands inside one. One sidecar write for a file; one per member
    for a block.
  - The key is `between(key(anchor), key(successor))`, both read from the full
    order with `path` or its block skipped.
- `lock_set { name, paths }`, paths in display order:
  adds `set::name` where missing, spreads `pos` over the paths, and writes the
  block key — the minimum of their current keys — onto each. If `name` is
  already a block, the paths append to its end with its key. **Refuses** a path
  already in a different block, naming both (R4).
- `unlock_set { name }` removes `order` from every member; they return to their
  date positions.
- `Event::OrderChanged` (`"order-changed"`), from the three commands and from
  every non-command caller of `reindex_file` that reports a change — the
  watcher's flush, the sweep at open, the hourly sweep. That is R7: today the
  watcher only announces a change to the tag *vocabulary*
  (`services/gallery.rs`, `flush`), which a reorder never is. The commands do
  not send `ItemsChanged`, because no `SortedItem` field changes.

All three commands take `Trust::Device`, as every tag write does.

### Upkeep of what already exists

`edit()` in `services/tags.rs` hands its closure only the namespace's
`Vec<String>`, and applies one closure to every path. It becomes a thin wrapper
over `edit_companion(paths, Fn(&RelPath, &mut CompanionFile) -> bool)`, which
keeps its reindex-then-events tail. Its consumers: `edit` itself, `rename` and
`merge` (to carry `order.set`), and `services/order.rs`.

- **Rename** (Set) rewrites `order.set` from source to target.
- **Merge** (Set) does the same for every source, so two blocks become one, at
  the lower key, with their `pos` values interleaved.
- **Remove / delete** change nothing in `order` (no pruning, above).
- **Duplicate merge** (`services/duplicates.rs`): the keeper keeps its own
  `order`; failing that, it adopts the smallest-key `order` among the losers
  whose set it absorbs.

### The key generator

`between(a: Option<&str>, b: Option<&str>) -> String` returns a valid UTF-8
string strictly between `a` and `b` in byte order, `None` meaning unbounded.
`spread(a, b, n)` returns `n` evenly spaced keys between two bounds, so locking
a two-hundred-member set gives two-hundred short positions rather than
successively longer ones. Byte order is what SQLite's default `BINARY`
collation compares, and what Rust's `str` `Ord` compares, so the tests and the
statement agree.

`between` prefers the **shortest** key in range, and that choice is what
answers the requirements' open question: `between(None, key(first))` is then a
key like `"3"`, below every date anyone will photograph, so a drop at the top is
a pin. Keeping `b`'s date prefix instead would make it an anchor. Either is one
branch in this function; the durable format does not care.

When the two bounds are equal — possible only after concurrent edits on two
machines — the upper bound is treated as absent, and the file lands one item
late in a state only a race produces.

### The SPA

- `"custom"` in the `SortField` union and a "Custom" entry in `SortMenu`, with
  the direction and sub-sort controls hidden.
- No scrollbar labels under Custom: [frontend/](../../frontend/README.md) forbids
  labelling a scrubber from a value the list is not ordered by.
- `galleryStore`: `order-changed` refreshes when the current sort is Custom.
  `refresh()` gains a sequence token so a slow response cannot overwrite a newer
  one, and placement calls go out one at a time.
- The viewer re-anchors by path after a refresh, so an arrangement arriving from
  another machine does not swap the file on screen.

## Cost in concepts

One sidecar object, one table, one sort field, one key generator and one
durable key format, three commands, one event, and the word **block**.

The *excepts*, each permanent until removed:

- Custom takes no direction and no sub-sort.
- `order` sits beside `core` rather than in it, because `CoreMeta` has no
  `extra`.
- A table is added without a format bump, under an amended rule.
- The no-join test is narrowed to tier tables.

Checked in the other direction first: nothing existing can be deleted to meet
this. The one statement it retires is prose — "Order comes from the gallery's
own sort" in [query/](../../query/README.md#sets).

## Alternatives

- **An ordinal in the tag string.** See requirements; it breaks every exact
  comparison on this build and every build before it.
- **A custom order scoped to a set or a folder**, as Lightroom and Photos scope
  theirs. It makes "where do unplaced files go" disappear, and the user chose a
  gallery-wide arrangement instead.
- **Placing every file.** Switching to Custom would write a sidecar for every
  file in the library, including thousands that had none, and new files would
  still need a rule.
- **Dense integer positions.** A long move rewrites up to N sidecars where a
  fractional key rewrites one; and every placement at gallery scope would
  renumber the gallery.
- **A per-set order file** (`.lightview/sets/<name>.json`, ordered paths). One
  write per reorder, but a new durable file type, a second source of
  membership, and a new lock.
- **Columns on `media_meta` or `tag_index`.** Changing an existing table's shape
  forces the bump R8 forbids.
- **Correlated subqueries instead of the join.** Three probes per row where one
  join does the work, and the reason for the no-join rule — a tier table's
  overflow pages — does not apply to a table of three short strings.

## Assumptions

Unmeasured, and each measured in phase 1:

- **A block move writes one sidecar per member**, each fsynced and renamed. On a
  NAS a two-hundred-member set may take seconds. The grid moves it optimistically
  either way.
- **The Custom statement at 20k rows** — a `LEFT JOIN` on a primary key and a
  CTE over block members — is expected to cost milliseconds.
- **The re-read after upgrade writes nothing back.** The sweep mirrors dates into
  sidecars that lack them; after any earlier sweep none should, so a re-read of
  every sidecar should touch no mtimes on a share. If it does, every other
  machine re-reads them too.

Taken on faith:

- Keys grow by about one character per repeated insertion at the same spot,
  which is harmless at the rate a person drags.
- The in-app move does not yet carry a file's sidecar (tracked separately). Until
  it does, a moved file loses its `order` along with its tags.

## The two checks principle 1 asks for

**Second implementation.** `between` has two consumers from the start — file
keys and block positions. `edit_companion` has four. `OrderChanged` is one event
kind, not an abstraction. No plugin point.

**Seam.** Two places resisted placement, and both are findings rather than
workarounds. `CoreMeta` lacks the `extra` every other durable collection has, so
the application's own new fields cannot live with its old ones; adding it now
would protect the *next* field, not this one, since deployed builds already lack
it. And "any schema change is a bump" conflated adding a table with changing
one — only the second can leave a schema silently mismatched.

## The work

**Phase 1 — the arrangement, without drag.** Everything above, plus:
- "Lock as set…" on a selection, reusing `DuplicatesPanel`'s `NameSetField`.
- "Unlock set" on a block member's context menu and in the tag manager's Sets
  tab.
- "Move to…" on the context menu: pick a target cell (or "to top") and `place`
  after it. The phone's path (R9), and phase 1's test surface.
- Block members marked in the grid, the first carrying the set's name.

**Phase 2 — desktop drag** in `JustifiedGrid`, mouse only: a movement threshold
before a drag starts so a click is still a click; an insertion caret between
cells; no gap offered inside a foreign block, and a member dragged outside its
own block carries the block; edge auto-scroll; an optimistic splice, then
`place`, then a sequenced refresh. After
[a grid that holds still](../a-grid-that-holds-still/requirements.md) R1, so a
placed item slides rather than teleports.

Each phase is one PR. Phase 1 fixes the durable format, so the format stays open
to revision until it merges.

### Verification

- `cargo test`, from `src-rust/`: `between` and `spread` property tests (strictly
  between, valid UTF-8, unbounded ends, multi-byte neighbours); `default_key`
  pinned to known values; an older build's round trip of `meta.order` (the
  pattern the `auto` test uses); `reindex_file` writes `block` only for a set
  the file is in, and reports a change; Custom ordering over loose files, blocks,
  a filtered subset and ties; `place` after, before, inside its own block, next
  to a foreign block, and at the top; `lock_set` refusing a file in another
  block; rename and merge carrying `order.set`; a duplicate merge adopting an
  order; the stamp filling `media_order` in an existing cache; the path-keyed
  table test covering the new table; the watcher sending `OrderChanged` for an
  order edit and not for a rating.
- `cargo clippy --all-targets --all-features`, and `npx tsc --noEmit` from
  `src-solidjs/`.
- `bash .claude/skills/verify/drive.sh`: lock a set and place a file over the
  command table, and read the order back from `get_items`; delete the cache and
  reopen, and the order survives — it lives in the sidecars; edit a sidecar by
  hand and see `order-changed` arrive; time a block move and the Custom query.
- `node .claude/skills/verify/grid.mjs`: the grid and the viewer walk Custom
  order; in phase 2, a scripted mouse drag moves a cell and the move survives a
  refresh.

### Docs, in the same change

[query/](../../query/README.md) (Custom, blocks, and the retired sentence),
[companion/](../../companion/README.md) (`meta.order`),
[cache/](../../cache/README.md) (the table and the amended rule),
[server/](../../server/README.md) (the commands and `order-changed`),
[frontend/](../../frontend/README.md) (Custom, blocks, drag, sequencing),
[duplicates/](../../duplicates/README.md) (a merge adopts an order), and
AGENTS.md (`DEFAULT_KEY` is a durable format).
