# Field report — design

Five findings, five independent changes, each its own commit.

Revised after an independent review, which found two things wrong and several
missing. Both corrections are kept visible below rather than quietly folded in,
because one of them (section D) reversed a claim that the change was free.

---

## A. Dates (finding 4)

### What is actually wrong

Four defects stack into "almost nothing has a date".

1. **`date_taken` is EXIF-only and nothing falls back.** `sort/sorter.rs:100`
   orders by `m.date_taken … NULLS LAST`; `services/media.rs:148` hands the panel
   that column and no other. A PNG screenshot, an image out of a messaging app,
   an editor export and **every video** carry no `DateTimeOriginal`, so all of
   them are NULL. The `mtime` the scan already stores on every row —
   `db.rs:93`, NOT NULL, unix seconds, written by `insert_scanned` — is read by
   nothing in the tree.

2. **A file that arrives while the server is running never has its header
   read.** The watcher's ingest (`gallery.rs:736-781`) stats the file, calls
   `insert_scanned`, indexes the companion, and stops. `backfill_exif` runs once,
   at open. An rsync or Samba drop — the headline deployment — lands files with
   no capture time *and no GPS*, which also costs them their place tags.

3. **The backfill's skip gate makes that permanent, and all three of its
   conjuncts are wrong.** The gate is
   `WHERE date_taken IS NULL AND gps_lat IS NULL AND width IS NULL`
   (`gallery.rs:149-151`). `width IS NULL` stands in for "nothing learned yet",
   on the reasoning that any decode sets `width` — but the grid decodes on
   sight (`serve.rs:250-260`), so the file from defect 2 is thumbnailed the
   moment it appears and is excluded forever. The other two are no better: a
   photo that has GPS and no date, or a date and no GPS, is excluded regardless
   of `width`. There is no relaxation of this gate that fixes it; the predicate
   it wants to express is not in the schema.

4. **The header is read twice, and one of the two reads is truncated.**
   `exif.rs:75` slurps a 1 MB head and parses it for the timestamp; `exif.rs:81`
   opens the file again with a `BufReader` for GPS. A container whose metadata
   box sits past the first megabyte yields coordinates but no date. The 1 MB cap
   never bounded anything either, because the unbounded read is on the next line.

### The fix

**1. Sorting and grouping coalesce; the column keeps its meaning.**
`date_taken` stays the camera's `DateTimeOriginal`, NULL when there isn't one.
What changes is what the *order* is computed from:

```
SortField::Date => COALESCE(m.date_taken, m.mtime) <order>
```

`mtime` is NOT NULL, so the result never is and `NULLS LAST` on that arm becomes
dead code and goes.

**`SortedItem::date_taken` is renamed to `date`** and selected from the same
expression. It is on the wire (`SortedItem` derives `Serialize`, ships inside
`Items`, mirrors to `types.ts`) and has three consumers that must all agree with
the order: `grouper.rs` for the headers, and on the client
`scrollIndicators.ts:19`, whose scrollbar labels read it for the date sort — a
scrubber labelled from a different value than the scroll position is sorted by
is a scrubber that lies. Keeping the name `date_taken` on a field that sometimes
holds an mtime would be the exact conflation the Alternatives below reject,
arrived at on a different field by accident. The rename is the point: the type
says which of the two dates it carries.

**`pipeline/idle.rs:118` uses the same expression.** It orders the thumbnail
warm-up backlog by `m.date_taken DESC NULLS LAST`, and its module doc justifies
that as "the order the default date-descending sort presents, so the first
screen is warm first". Leave it and that stated invariant is false — the warmer
would warm a different first screen than the grid shows.

**2. The panel says which date it is showing.** `MediaMeta` gains `mtime: i64`
beside `date_taken: Option<i64>`. The panel shows `Taken` when `date_taken` is
present and `Modified` otherwise. Nothing is invented and nothing is conflated.

**Two other types keep the strict meaning.** `MediaMeta` and
`DuplicateCandidate` carry true EXIF `date_taken`. The merge path is why it
matters: `MergeDialog.tsx:127` agrees a capture time across a duplicate group
from `date_taken` only, and `services/duplicates.rs:151` stamps it onto the
survivor's on-disk mtime. Fed a coalesced date that would stamp a file's mtime
from its own mtime — a no-op wearing the costume of a decision, in the one place
the code calls out as silent data loss if it gets it wrong.

**3. `exif_read` replaces a predicate the schema cannot express.** A new column,
`exif_read INTEGER NOT NULL DEFAULT 0`, set to 1 for every file whose header has
been looked at, found or not. The gate becomes `WHERE exif_read = 0`.

**4. Ingest probes, and the backfill becomes the resume path.** One helper,
`probe_and_store(gallery, paths)`, called from the watcher's ingest on the files
it just inserted and from `backfill_exif` on whatever still reads 0. Both run
off the writer lock. A newly arrived photo gets its date and its coordinates in
the same breath as its companion.

**5. One parse.** `exif::read` opens once, `BufReader`, both facts from one
`exif::Reader`. `read_head` is deleted. The doc comment at `exif.rs:28-30`
claims the upload path's mtime stamp "is what the gallery indexes as
`date_taken`" — false today, true after this change, and worth fixing either
way. A pleasant consequence: `upload.rs` already stamps an uploaded file's mtime
to its EXIF capture time before the rename, so under COALESCE an uploaded photo
sorts correctly the instant the watcher sees it, before any probe runs.

**6. The index.** `db.rs:109` is `ON media_meta(date_taken DESC)`, which
`ORDER BY COALESCE(…)` cannot use. A `FORMAT_VERSION` bump is the only moment an
expression index on `COALESCE(date_taken, mtime)` can be added without a second
bump that re-thumbnails every library, and this change is already bumping.
`idle.rs:118` has a `LIMIT` and definitely wants it.

**7. `mtime = 0` stops meaning 1970.** `gallery.rs:757` falls back to 0 when
`modified()` fails. Inert today; after COALESCE that file sorts to 1970-01-01
and earns its own "January 1970" group header. It becomes the ingest time.

**8. A merged survivor's row follows its file.** `duplicates.rs:151` stamps the
keeper's mtime, then `index_one` re-reads only the *companion* — it never
re-stats the file, so `media_meta.mtime` keeps the old value. Today invisible;
after COALESCE the survivor sorts by a stale date until the next open, then
silently jumps position. The stamp updates the row in the same breath.

Adding a column bumps `FORMAT_VERSION` to 2, deleting and rebuilding the cache.
That is the only migration mechanism there is, and here it is what repairs the
reporter's library rather than a cost.

### Placement

`cache/db.rs` (column, index, version), `cache/meta.rs` (`set_probed`),
`sort/sorter.rs` (order expression, field rename), `pipeline/idle.rs` (same
expression), `services/media.rs` (`mtime` on the payload), `services/gallery.rs`
(`probe_and_store`, called from two places), `services/duplicates.rs` (the row
follows the stamp), `pipeline/exif.rs` (one parse). Dependencies point the way
they already do: `services` → `pipeline` → `cache`.

### Contract

- **Wire:** `MediaMeta` gains `mtime` (additive). `SortedItem.date_taken` →
  `date`, and its meaning changes from capture time to sort date — a breaking
  rename, deliberately breaking so every consumer is recompiled past it.
- **Cache:** `FORMAT_VERSION` 1 → 2. Derived, regenerated on next open.
- **Durable:** nothing. No companion field changes.
- **Filters keep the strict date.** `date=2024` and `date>=…`
  (`filter/evaluator.rs:141`) compile against `date_taken` and stay there: a
  filter that claimed to find photos *taken* in 2024 and returned files *copied*
  in 2024 would be worse than the bug being fixed. (An earlier draft argued this
  from `has:date`. There is no such term — `parser.rs:242` has `has:geo`,
  `missing:geo` and `has::<namespace>` and nothing else — so the asymmetry is
  cheaper than claimed, resting on the `date=` family alone.)

### Cost in concepts

One column, `exif_read`, and one *except*: **sort and group use a fallback date,
filters do not.** Stated in `docs/query/README.md` and `docs/gallery/README.md`.
Against it, the `width IS NULL` proxy and one duplicated read path are deleted.

### Alternatives

- **Write `mtime` into `date_taken` when EXIF has none.** One line, no column.
  Rejected: `date_taken` stops meaning capture time, and nothing can later
  distinguish a real timestamp from a filled-in one — so the merge stamp above
  becomes a no-op and a plugin that learns a true date has no way to know it may
  overwrite.
- **`date_added` as the fallback.** NOT NULL, no new thought. Rejected: it is
  when *this gallery* first saw the file, so a library imported in one afternoon
  sorts into one afternoon.
- **Delete the backfill pass entirely.** The review's strongest point, and it
  nearly wins: once ingest probes, the only rows needing a backfill are rows
  created before ingest probed — and the version bump guarantees there are none.
  `insert_scanned` is the single funnel for both the scan and the watcher and
  could return the paths it actually inserted. That deletes `backfill_exif`, its
  gate, the `exif_read` column and the bump together — four concepts for one.

  **Rejected because of section B.** A first open of a large library enriches
  for minutes; adding "exit when the last window closes" makes an interrupted
  enrichment a routine event rather than a crash. Probe-on-insert alone is not
  resumable: a file inserted and not yet probed when the process exits is never
  probed again, because it is never inserted again. The column is what makes the
  pass a genuine no-op on a warm cache *and* a correct resume on a cold one.
  That is the named requirement principle 2 asks for, and it only exists because
  the two changes ship together — worth stating, since either alone would make
  the other's design wrong.
- **Have the thumbnailer read EXIF while it has the file open.** It is already
  paying the open. Rejected: it puts a metadata policy inside the decode path,
  which then owes it to every caller including the on-demand serve inside a
  request.

### Assumptions

- **`mtime` is meaningful on this library.** Unmeasured. `rsync -a` and most
  camera imports preserve it; plain `cp` does not. Where it is wrong it is wrong
  in a way the panel now discloses, which is the most this can honestly offer.
- **`read_from_container` seeks to the metadata block rather than reading the
  file whole.** True for JPEG and TIFF, approximately true for HEIF and for
  PNG/WebP chunk-walking, and asserted rather than measured. It is why deleting
  the 1 MB cap is safe; if it is wrong, a large raw file costs more I/O per probe
  than it does today.

---

## B. Ending a local session (finding 2)

### The signal

The SSE stream at `GET /api/events` already is the answer. A browser tears the
connection down when the tab closes, and the 15-second keep-alive means a
connection whose peer is gone is noticed within one interval even when no events
flow. A count of live streams is a count of open windows. On the client
`ipc.ts:419` opens exactly one `EventSource` from an unconditional `onMount` in
`App.tsx:235`, with no readiness or auth precondition — a mounted `App` always
holds one.

### The mechanism

`server/presence.rs`, new, ~80 lines:

```rust
pub struct Presence { live: AtomicUsize, seen_any: AtomicBool, busy: AtomicUsize }
```

an RAII `guard()` the SSE handler moves into its stream body, a `busy()` guard
for durable background work, and `watch_for_exit`, a task that ticks every five
seconds.

Policy:

- **Local mode only.** A `--serve` deployment must outlive every client.
- **Armed only after a first client has connected** (`seen_any`), or a slow
  browser start kills the process before it is used.
- **Five minutes of continuous zero**, re-checked each tick rather than armed
  once. Not thirty seconds: Chrome and Safari **discard backgrounded tabs** under
  memory pressure, closing their sockets while the tab stays in the strip and
  reloads on focus. At thirty seconds "the last window closed" and "the last
  window was backgrounded" are the same observation. Five minutes costs nothing
  — the process is idle, and a `lightview <dir>` inside the window is a fast
  attach to the running one, which is the correct outcome anyway.
- **Never mid-durable-write.** HTTP graceful shutdown covers requests, and
  **does not cover the writes that matter**: `cli/mod.rs:384` spawns
  `enrich_and_index` detached, `backfill_locations` runs `modify_companion` per
  geotagged file on a blocking thread, and `spawn_companion_sweep` keeps
  running. On the first open of a large library those run for minutes while the
  user browses — so the naive version exits mid-`modify_companion` and leaves a
  `.tmp` in the gallery, the exact outcome this paragraph exists to prevent.
  Those tasks hold a `busy()` guard; the watchdog will not exit while one is
  outstanding.
- **Graceful, on both branches.** `listen.rs:106-128` has two serve paths:
  `axum::serve` (loopback) takes a shutdown *future*, `axum_server` (TLS) takes a
  `Handle`. `serve` grows one shutdown-future parameter and the TLS branch
  honours it by spawning a task that awaits it and calls
  `handle.graceful_shutdown`. `--serve` passes `pending()`. A parameter one
  branch silently ignored would be a trap inside the one function whose module
  doc is about there being exactly one listener.
- One line to stderr, and exit 0.

Loopback is what makes a long grace safe in the other direction: a suspended
laptop does not drop a TCP connection where both endpoints are the same machine,
so a closed lid is not a closed window.

### Placement

`server/presence.rs`, new. `routes.rs` takes a guard in the SSE handler,
`state.rs` holds the `Presence`, `cli/mod.rs` spawns the watchdog and wraps the
enrichment tasks, `listen.rs` grows the shutdown parameter. Presence depends on
nothing. A new module with its own responsibility is a subsystem-map change:
`docs/README.md` and `docs/architecture.md` get it.

### Contract

No wire change. A behaviour change to `lightview <dir>`, in
`docs/server/README.md`.

### Cost in concepts

One: *the local process exits when its last window closes*. The concept the
reporter already assumed existed.

### Alternatives

- **`ThumbService::activity`, the idle signal already in the tree.** Zero new
  concepts, and wrong: a tab parked on a grid makes no requests, so the session
  would die under someone looking at it. Presence is not activity — the mirror
  of the warning already written at `serve.rs:79` that the subscriber count must
  not be read as activity. Both directions of that confusion end up named.
- **`beforeunload` → `POST /api/goodbye`.** Never fires on a crash, a force quit
  or on mobile, so the timeout has to exist anyway and then does the work alone.
- **A client heartbeat.** A second liveness channel beside the one already open.
- **Leave it, document `Ctrl-C`.** Nothing in "Open with LightView" suggests a
  process was started.

### Assumptions and known gaps

- **hyper drops the SSE stream promptly on disconnect**, within roughly the
  keep-alive interval. Unmeasured; `drive.sh` will assert it.
- **Two states hold a window but no stream.** `/pair` (`index.tsx:54`) returns
  before `App` mounts, and `phase: "ended"` has had its stream 401'd — per the
  HTML spec a non-`text/event-stream` response fails `EventSource` permanently
  rather than retrying. Neither can kill a process that a live window belongs
  to: `/pair` is unreachable from the loopback SPA's own navigation, and "ended"
  only arises across a restart. But "count of streams" is not exactly "count of
  windows", and this is where it differs.
- **A second `lightview <dir>` launched in the last seconds of the window** opens
  a browser at a URL whose process is exiting. The disarm happens when the new
  client *connects*, not when the second process launches. Five minutes rather
  than thirty seconds is most of the mitigation; `instance.json` is removed
  before shutdown begins so a launcher that reads it after that point starts
  fresh instead.

---

## C. Pinned locations in the picker (finding 1)

`DirListing` gains `places: Vec<Place>` — `{ label, path }` — the gallery root,
`$HOME`, and whichever XDG user directories exist, read from
`~/.config/user-dirs.dirs` when present and falling back to the conventional
names under `$HOME`. Only paths that exist are listed, so the sidebar never
offers a dead end.

`list_dirs(path: &Path)` (`files.rs:177`) has no access to the gallery root and
takes it as a parameter. The set is constant for the process, so it is computed
once into a `OnceLock` rather than re-parsing `user-dirs.dirs` on every
navigation.

It rides on the listing already being fetched rather than a second command. The
client keeps its guarantee: it still never assembles a path, only sends back one
the server named — which is why `DirListing` already carries a server-chosen
`parent`, so `places` adds a field to a structure whose contract covers it.

**Alternatives.** A `dirs` crate for ~30 lines of parsing (an existing
dependency beats a new one, and none here does this). A separate `list_places`
command, which buys nothing for a constant. A typed path field, not asked for,
and it would hand the server a string it did not choose.

---

## D. Short rows in the justified grid (finding 5)

`computeJustifiedLayout` calls `flush(end, justify)` with `justify = false`
before every forced group break (`justifiedLayout.ts:170`) and for the trailing
row. An unjustified row sits at its target height and leaves the width empty.
Default grouping is monthly and `App.tsx:370` always passes `groupStarts`, so
**every group ends ragged** — and under finding 4 the missing dates scattered the
few dated files into one- and two-item months, each with its own short row.
Fixing dates removes most of the symptom; the rule is still wrong on its own.

**The first draft of this section was wrong** and the review caught it. It
proposed deleting the `justify` flag and letting `clamp(h, minRowHeight,
maxRowHeight)` bound the stretch, claiming a lone image would be "left-aligned
exactly as today" at a cost of "negative — one parameter and one branch
removed". Measured at the real defaults (`JustifiedGrid.tsx:314` passes none, so
`0.5×` and `2×` target apply), at container 1600, gap 4, target 240:

| final row | today | justify + clamp |
|---|---|---|
| 4 landscape | 240px, 91% of width | 265px, **100%** |
| 3 landscape | 240px, 68% | 354px, **100%** |
| 2 square | 294px, 37% | 480px, 60% |
| 1 landscape | 240px, 23% | 480px, 45% |

The clamp fixes rows of three or more and makes rows of one or two **double the
height of every row above them and still ragged** — worse than the complaint,
and more conspicuous for being rarer once dates are fixed. `justify` was not
approximating the clamp; it encodes a different rule, and the clamp cannot
recover it.

So the branch stays and gets the rule it should have had. A final row justified
is always taller than its neighbours — it is the row that never reached its
commit height — and the question is only by how much:

```ts
const FINAL_ROW_STRETCH = 1.5;
```

Justify when `justifiedH <= targetFor(avg) * FINAL_ROW_STRETCH`, otherwise sit
at the natural height. Rows of three or more fill the width at a height that
reads as one of the rows above; rows of one or two stay where they are today,
which is the honest rendering of "there were only two left". Half a line more
than the current rule, and one named constant carrying the measurement.

**Cost.** One constant. The earlier claim of negative cost is withdrawn.

**Alternatives.** Stretching unconditionally (the table's bottom two rows).
Deleting the flag (same). Leaving it (the reporter can see it).

**Noted, not fixed:** `JustifiedGrid.tsx:464` feeds `createScrollDynamics` a
constant `rowHeight: () => targetRowHeight() + gap()`. Rows that can reach 1.5×
target make that estimate worse. It drives buffer sizing, not layout, and is
already approximate.

---

## E. `makepkg` (finding 3)

`options=('!lto')` in `PKGBUILD`: `aws-lc-sys` compiles its own C and assembly
through makepkg's `CFLAGS`, and `-flto=auto` produces bytecode `rust-lld` cannot
link. Verified by the reporter on Arch. Opting the package out beats
hand-stripping `CFLAGS`, which would have to guess which flags the build script
forwards.

### And the half that has not failed yet

`Cargo.lock` is in `.gitignore` and untracked, while `PKGBUILD` builds
`--locked`. That works for the reporter only because `makepkg` runs in
`$startdir` — the repository itself — where an earlier manual `cargo build` had
already written one. **On a fresh clone `makepkg -si` fails**, on its first
command, with the lock file needing an update that `--locked` forbids.

Wider than the package: this crate has a `[[bin]]`, and the container image and
the release workflow both build from clean checkouts, so all three re-resolve
dependencies from scratch today. A semver-compatible upstream release is enough
to break a build with no change on this side and nothing in git to bisect.
Committing `Cargo.lock` is the convention for a crate shipping a binary and is
what `--locked` already assumed.

---

## Documentation

Principle 5 obliges both sides of every changed contract:

- `docs/query/README.md`, `docs/gallery/README.md` — the sort/filter asymmetry.
- `docs/frontend/README.md` — the `SortedItem.date` rename and what the
  scrollbar labels now mean.
- `docs/duplicates/README.md` — a merged survivor now moves in the grid.
- `docs/cache/README.md` — `exif_read`, the expression index, version 2.
- `docs/server/README.md` — a local session ends with its last window.
- `docs/README.md`, `docs/architecture.md` — `server/presence.rs`.
- `docs/build-and-verify.md` — the lock file is committed.

## Verification

- `drive.sh`: a file with no EXIF sorts among the dated ones rather than last;
  `/api/dirs` returns places and each one exists; an SSE client that disconnects
  drops the live count; a local process with no client exits and releases the
  gallery lock; one with a `busy()` guard outstanding does not.
- `grid.mjs`: no row before a group header is both shorter than the container
  and below the stretch ceiling.
- Rust unit tests: the coalesced order expression; `exif_read` set by a probe
  that found nothing; the places list skipping what does not exist; a merged
  survivor's row mtime following its file.
