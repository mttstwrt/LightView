# Design

Principle 1's five questions, in order. Requirements are cited as R1–R10 from
[requirements.md](requirements.md).

## The model in one paragraph

Every file has a **key**, a string, and Custom sorts by it ascending. A file's
key is the override stored in its sidecar, or else `DEFAULT_KEY`: its sort date
as a fixed-width number that grows smaller as the date grows later, followed by
its path. **Placing** a file behind A gives it a key that extends A's own —
`key(A)` plus a short suffix — so nothing that arrives later can land between
them, and A's key is written into A's sidecar if it was not already there, so
the anchor cannot move out from under it. A **block** is the files whose sidecar
names a set they are still in; it sorts at the **minimum key of its members**,
and inside it by `pos`, a string from the same generator. The ORDER BY is block
key or own key, then block name, then `pos`, then path: a total order.

## Placement

```
sort/order_key.rs       new, pure: after(), before(), spread(), default_key(alias)
sort/sorter.rs          SortField::Custom and its statement
sort/grouper.rs         no groups under Custom
companion/schema.rs     MetaCollection.order: Option<Order>
cache/db.rs             media_order table + index; in path_keyed_tables()
cache/index.rs          reindex_file also writes media_order; reports what changed
services/gallery.rs     the companion re-read stamp; the arrangement gate;
                        OrderChanged from the watcher and the sweeps
services/order.rs       new: place, lock_set, unlock_set, reset_order
services/tags.rs        edit() split into edit_companion(); remove, delete,
                        rename and merge keep order in step
services/duplicates.rs  the keeper adopts an order
server/commands.rs      the four commands — Trust::Device
server/events.rs        OrderChanged, "order-changed", domain Items
src-solidjs/            SortMenu, galleryStore, the viewer, the grid, the
                        command list
```

**`sort/order_key.rs` is a pure library** beside the sorter, its only SQL
consumer. It knows strings and SQL fragments and nothing about sidecars or
requests. `default_key(alias)` is defined once, the way
[`SORT_DATE`](../../../src-rust/src/sort/sorter.rs) is, and read by the
statement and the tests that pin it.

**`services/order.rs` is the only module that knows both keys and sidecars.**
It reads neighbours' effective keys from the cache, computes new keys, and
writes sidecars through `edit_companion`. It sits beside `services/tags.rs`
rather than inside it because a placement is not a tag write: it edits `meta`.
The one exception, `lock_set`, adds a set tag, and does it through the same core.

**`filter/` is untouched.** Blocks are ordered wherever they appear, filtered or
not, so nothing needs to know which set a filter names.

Dependencies point down only. `cache/` learns a table and nothing about who
fills it; the services learn a column set; the server learns four commands.

## Contract

### The sidecar — durable, read by other installations

```json
"meta": {
  "core":  { "rating": 4 },
  "order": { "key": "4611686016727387904photos/a.jpgV", "set": "comic", "pos": "V" }
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

**`DEFAULT_KEY` becomes a durable format.** Stored keys extend the keys they
were placed against, so changing the encoding would move every placed file
relative to every unplaced one, in every gallery, silently. It is pinned by a
test against known values, and AGENTS.md gains the line. The encoding:

```sql
printf('%019d', 4611686018427387904 - COALESCE(m.date_taken, m.mtime)) || m.path
```

Epoch seconds, offset by 2^62 so pre-1970 capture dates stay positive; nineteen
digits so the text order is the numeric order; SQLite's `printf` is 64-bit
(checked on 3.45). Newest first ascending, as Date orders (R1).

**The encoding is pinned; its inputs are not.** `date_taken` differs between
hosts for a video read in the local time zone, or on a host with no `ffprobe`.
That is why placing a file freezes its anchor: once A's key is stored, every host
agrees where A is and therefore where anything placed against it is. Unarranged
files differ between hosts under Custom exactly as they already do under Date.

**Keeping `order` in step with membership.** This build removes `order` from a
file when it removes the set that `order.set` names — `remove` and `delete` — so
a file taken out of a set goes back to its date position (R10), and a deleted
set dissolves its block. `rename` and `merge` carry `order.set` to the new name.

**An `order.set` naming a set the file is not in** can then only come from an
older build, which renames sets without knowing about `order`. The indexer
ignores that `order` **entirely** — the file sorts by date — and nothing deletes
it: it is the only surviving record of the arrangement, and renaming the set
back restores it. Ignoring only the `set` and keeping the `key` would clump every
former member at one shared key, which is where equal keys came from in the
first draft.

### The cache

```sql
CREATE TABLE IF NOT EXISTS media_order (
    path   TEXT PRIMARY KEY,
    key    TEXT,
    block  TEXT,
    pos    TEXT
);
CREATE INDEX IF NOT EXISTS idx_order_block ON media_order(block) WHERE block IS NOT NULL;
```

A row exists only for a file whose sidecar carries an `order` the indexer
honours. It joins `path_keyed_tables()`, and the existing test that reads
`sqlite_master` fails the build if it does not.

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
transaction, and the existing companion sweep re-reads every sidecar once. The
sweep writes a sidecar back only when the index knows a date the sidecar lacks,
which an earlier sweep has already completed, so the re-read touches no mtimes
on a share.

The rule `docs/cache/README.md` gains: **adding a table that a reader fills is a
stamp; changing an existing table's shape is still a bump.** An existing table
never changes shape without one, so "a silently mismatched schema" still cannot
happen.

Stated costs of alternating two builds on **one machine**, which share one
cache directory: a file the older build re-indexes gets its `index_state`
stamped without an order row, so the newer build skips it until its sidecar
next changes; and the older build's `forget_path` and prune do not know the
table, leaving orphan rows that nothing reads, because the statement starts from
`media_meta`. Two machines have two caches and are unaffected.

### The arrangement gate

The server serves before the open-time passes finish, and `reindex_companions`
runs last among them. Until it has run once, `media_order` may be empty, and a
placement computed against it would be written permanently into sidecars — a
`lock_set` could even pass its R4 check against rows that are not there yet. So
the four commands refuse, with a message the SPA shows, until the gallery's
first companion sweep completes. One flag on `Gallery`, set once.

### `reindex_file`

The one writer of companion-derived rows. Every caller — the tag commands, the
sweep at open, the hourly sweep, the watcher, the duplicate merge through
`index_one` — goes through it. It gains the `media_order` delete-and-insert
inside its existing transaction and returns whether that row changed. **Every
caller that sees a change sends `OrderChanged`**, whatever caused it: a tag
command that dissolved a block, another machine's placement arriving through the
watcher, a duplicate merge adopting an order. Reporting a change rather than a
write matters because every view already writes a sidecar; without it the
watcher would send `OrderChanged` for every photo looked at.

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
exactly that, and the alias test learns `o.`, `b.` and `m2.` (the filter's own
subqueries use only `ti`).

The block key is the minimum over the members' keys rather than a value stored
once, because there is nowhere to store it once — a set is a name several files
agree on, not an object. Locking writes the minimum onto every member and a move
rewrites every member, so the members agree; if a move is interrupted, or
another machine's watcher flushes halfway through a slow move over the share,
the minimum still shows one coherent block, and repeating the move repairs it.

Under Custom the grouper returns no groups, whatever `group_by` asked for. The
grid forces a row break at every group start, so month groups would split a
block across rows and give a pinned photo a row to itself; and
[frontend/](../../frontend/README.md) forbids labelling headers from a value the
list is not ordered by.

### The key generator

Keys compare by bytes — SQLite's default `BINARY` collation, and Rust's `str`
`Ord`, so the tests and the statement agree. Generated characters come from
`0-9A-Za-z`, so a sidecar stays readable with `grep`, and a generated key never
ends in `0`, so every key has room below it.

- `after(a, b)` — the shortest `a + suffix` below `b`. **Hugs `a`**: nothing can
  sort between a file and its anchor unless its own key extends the anchor's,
  which a default key can only do for a file with the same second and a path
  that extends the anchor's path.
- `before(b, a)` — the mirror, hugging `b` from below, for a drop with nothing
  visible above it.
- `spread(a, b, n)` — `n` evenly spaced keys between two bounds, so locking a
  two-hundred-member set gives two-hundred short positions rather than
  successively longer ones.

Placing between two neighbours whose keys are equal cannot produce a key between
them. After the pruning rules above this arises only from two machines placing
at the same spot at the same moment; `place` then re-keys the upper neighbour
first — one more write — and places against the result.

The top drop is the requirements' open question and one branch here: a **pin**
is a key below every possible default key (`"3"`, then `"2V"`, …); an **anchor**
is `before(key(first), None)`.

### Wire

- `SortField::Custom`, `"custom"`. **Takes no direction and no sub-sort**, and
  is not offered as a sub-sort: the order is total and was arranged by hand, so
  reversing it or breaking its ties means nothing.
- `SortedItem` gains `block: Option<String>` — non-null only under Custom.
- `place { path, after?, before? }` names the visible gap the drop landed in.
  - If either neighbour is in `path`'s own block: set `pos` immediately after
    `after` within the block, or immediately before `before` at its start.
  - Otherwise: move `path` — or its whole block, if it is in one — to
    immediately after `after` in the **full** order, or immediately before
    `before` when there is no `after`, or to the top when there is neither
    (R6). A neighbour inside a foreign block stands for that whole block, so a
    drop never lands inside one.
  - Writes: the moved file, or every member of the moved block; plus the anchor,
    if its key was not already stored.
- `lock_set { name, paths }`, paths in display order. **Reads every path's
  sidecar before writing any**, and refuses if one is in a different block,
  naming both (R4) — so a refusal never leaves a partial lock. Then adds
  `set::name` where missing, spreads `pos` over the paths, and writes the block
  key — the minimum of their current keys — onto each. If `name` is already a
  block, the paths append to its end with its key.
- `unlock_set { name }` removes `order` from every member; they return to their
  date positions (R10).
- `reset_order { paths }` removes `order` from each path: a loose file returns
  to its date position, a block member leaves its block and stays in its set
  (R10).
- `Event::OrderChanged`, `"order-changed"`, in `Domain::Items` so a client that
  lagged past one refetches. Sent by any caller of `reindex_file` that sees the
  order row change. The order commands send nothing else, except `lock_set`,
  which also added a tag and so sends what a tag write sends.

All four commands take `Trust::Device`, as every tag write does.

### Upkeep of what already exists

`edit()` in `services/tags.rs` hands its closure only the namespace's
`Vec<String>`, and applies one closure to every path. It becomes a thin wrapper
over `edit_companion(paths, Fn(&RelPath, &mut CompanionFile) -> bool)`, which
reindexes and returns what changed; each caller sends the events for what it
changed. Its consumers: `edit` itself, `remove`/`delete`/`rename`/`merge` for
their `order` upkeep, and `services/order.rs`.

- **Remove / delete** (Set) drop `order` where `order.set` is the set removed.
- **Rename** (Set) rewrites `order.set`.
- **Merge** (Set) **concatenates**: the target's block in its order, then each
  source's in the order given, `pos` re-spread across all of them, under the
  lowest key. Merge already rewrites every member's sidecar, so concatenating
  costs no writes that interleaving would not.
- **Duplicate merge** (`services/duplicates.rs`): the keeper keeps its own
  `order`; failing that, it adopts the smallest-key `order` among the losers
  whose set it absorbs.

### The SPA

- `"custom"` in the `SortField` union and a "Custom" entry in `SortMenu`, with
  the direction and sub-sort controls hidden. The scrollbar already returns no
  labels for a field it does not know, which is the right answer here.
- `galleryStore`: `order-changed` refreshes when the current sort is Custom.
  `refresh()` gains a sequence token so a slow response cannot overwrite a newer
  one, and placement calls go out one at a time.
- The viewer re-anchors by path after a refresh, so an arrangement arriving from
  another machine does not swap the file on screen.
- The arrangement commands show the gate's refusal as a message, not an error.

## Cost in concepts

One sidecar object, one table, one sort field, one key generator and one
durable key format, four commands, one event, one readiness flag, and the word
**block**.

The *excepts*, each permanent until removed:

- Custom takes no direction and no sub-sort, and has no groups.
- `order` sits beside `core` rather than in it, because `CoreMeta` has no
  `extra`.
- A table is added without a format bump, under an amended rule.
- The no-join test is narrowed to tier tables.
- Arranging is refused until the first companion sweep of an open completes.

Checked in the other direction first: nothing existing can be deleted to meet
this. The one statement it retires is prose — "Order comes from the gallery's
own sort" in [query/](../../query/README.md#sets).

## Alternatives

- **An ordinal in the tag string.** See requirements; it breaks every exact
  comparison on this build and every build before it.
- **Anchoring by identity** — `order.after = <path>`, which is R2 stated
  literally, and immune to every host difference. It lost on resolution: the
  grid's order becomes a linked list the statement must walk with a recursive
  CTE on every query; a deleted or renamed anchor dangles, and so does every
  file chained behind it; two machines placing different files after the same
  anchor produce a fork with no order; and a cycle is one bad edit away. A key
  that extends a frozen anchor's key gives the same "immediately after" with a
  plain `ORDER BY`.
- **The shortest key between the neighbours**, which the first draft used. It
  lands wherever the midpoint between two dates falls — possibly years from the
  anchor — so importing the second camera's photos from the same event puts
  them between a file and the file it was dropped behind.
- **A custom order scoped to a set or a folder**, as Lightroom and Photos scope
  theirs. It makes "where do unplaced files go" disappear, and the user chose a
  gallery-wide arrangement instead.
- **Placing every file.** Switching to Custom would write a sidecar for every
  file in the library, including thousands that had none, and new files would
  still need a rule.
- **Dense integer positions.** A long move rewrites up to N sidecars where a
  generated key rewrites one; at gallery scope every placement would renumber
  the gallery.
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

Taken on faith:

- Keys grow by about one character per repeated insertion behind the same
  anchor, which is harmless at the rate a person drags.
- The in-app move does not yet carry a file's sidecar (tracked separately).
  Until it does, a moved file loses its `order` along with its tags.

## The two checks principle 1 asks for

**Second implementation.** The key generator has two consumers from the start —
file keys and block positions. `edit_companion` has six. `OrderChanged` is one
event kind, not an abstraction. No plugin point.

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
  tab; "Reset to date order" on a selection.
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

- `cargo test`, from `src-rust/`:
  - the generator: `after`, `before` and `spread` property tests — strictly
    between, hugging, valid UTF-8, never ending in `0`, multi-byte neighbours;
    and a later-imported default key never sorting between a file and its
    anchor;
  - `default_key` pinned to known values;
  - an older build's round trip of `meta.order`, as the `auto` test does;
  - `reindex_file`: an honoured `order`, an ignored stale one, and a reported
    change;
  - Custom ordering over loose files, blocks, a filtered subset and ties, with
    no groups;
  - `place`: after, before, inside its own block, next to a foreign block, at
    the top, against an equal-key pair, and freezing an unstored anchor;
  - `lock_set` refusing before its first write; `unlock_set`; `reset_order`;
  - remove and delete dissolving, rename carrying, merge concatenating, a
    duplicate merge adopting;
  - the gate refusing before the first sweep;
  - the stamp filling `media_order` in an existing cache; the path-keyed table
    test covering the new table;
  - `OrderChanged` from the watcher for an order edit and not for a view, and
    its domain.
- `cargo clippy --all-targets --all-features`, and `npx tsc --noEmit` from
  `src-solidjs/`.
- `bash .claude/skills/verify/drive.sh`: lock a set and place a file over the
  command table, and read the order back from `get_items`; delete the cache and
  reopen, and the order survives; edit a sidecar by hand and see `order-changed`
  arrive; time a block move and the Custom query.
- `node .claude/skills/verify/grid.mjs`: the grid and the viewer walk Custom
  order; in phase 2, a scripted mouse drag moves a cell and the move survives a
  refresh.

### Docs, in the same change

[query/](../../query/README.md) (Custom, blocks, and the retired sentence),
[companion/](../../companion/README.md) (`meta.order` and its upkeep),
[cache/](../../cache/README.md) (the table and the amended rule),
[server/](../../server/README.md) (the commands, the gate, `order-changed`),
[frontend/](../../frontend/README.md) (Custom, blocks, drag, sequencing),
[duplicates/](../../duplicates/README.md) (a merge adopts an order), and
AGENTS.md (`DEFAULT_KEY` is a durable format).
