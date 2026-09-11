# Field report — design

Five findings, five independent changes, in the order of how much they matter.
Each lands as its own commit so each can be reverted on its own.

---

## A. Dates (finding 4)

### What is actually wrong

Four separate defects stack into "almost nothing has a date".

1. **`date_taken` is EXIF-only and nothing falls back.** `sort/sorter.rs`
   orders by `m.date_taken … NULLS LAST` and `services/media.rs` hands the panel
   that column and no other. A PNG screenshot, an image out of a messaging app,
   an export from an editor and **every video** carry no `DateTimeOriginal`, so
   they are all NULL. The `mtime` the scan already stores on every row — NOT
   NULL, unix seconds — is never read by anything.

2. **A file that arrives while the server is running never gets EXIF read.**
   `services/gallery.rs` ingest (the watcher's `insert_scanned` path) records
   size and mtime, indexes the companion, and stops. `backfill_exif` runs once,
   at open. So an rsync or Samba drop — the headline deployment — lands files
   with no capture time *and no GPS*, which also costs them their reverse-geocoded
   place tags.

3. **The backfill's skip gate makes that permanent.** The gate is
   `WHERE date_taken IS NULL AND gps_lat IS NULL AND width IS NULL`. The third
   conjunct stands in for "nothing has been learned about this file yet", on the
   reasoning that any decode sets `width`. But the grid decodes on sight: the
   file from defect 2 is thumbnailed the moment it appears, so by the next open
   it has a width and is excluded from the backfill **forever**. Nothing ever
   revisits it.

4. **The header is read twice, and one of the two reads is truncated.**
   `pipeline::exif::read` slurps a 1 MB head into a `Vec` and parses it for the
   timestamp, then opens the file a second time with a `BufReader` for GPS. A
   container whose metadata box sits past the first megabyte therefore yields
   coordinates but no date — the two facts disagree about the same file.

### The fix

**1. Sorting and grouping coalesce; the column keeps its meaning.**
`date_taken` stays exactly what it is — the camera's `DateTimeOriginal`, and
NULL when there isn't one. What changes is what the *order* is computed from:

```
SortField::Date => COALESCE(m.date_taken, m.mtime) <order>
```

`mtime` is NOT NULL, so the coalesced value never is, and `NULLS LAST` on that
arm becomes dead code and goes.

`sort::SortedItem::date_taken` is renamed to `date` and selected from the same
`COALESCE`. It has three consumers and all three must agree with the order:
`grouper.rs` (the headers), and on the client `scrollIndicators.ts`, whose
scrollbar labels read `it.date_taken` for the date sort — a scrubber labelled
from a different value than the scroll position is sorted by is a scrubber that
lies. The rename is the point: the type now says which of the two dates it
holds. The "Unknown date" group stops occurring, which is the visible win.

**Two other types keep the name and the strict meaning.** `MediaMeta` (the info
panel) and `DuplicateCandidate` (the duplicates panel and `MergeDialog`) carry
true EXIF `date_taken`. The merge path is the reason it matters:
`MergeDialog.tsx` agrees a capture time across a duplicate group and
`services/duplicates.rs:148` stamps it onto the survivor's mtime. Fed a
coalesced date, that would stamp a file's mtime from its own mtime — a no-op
wearing the costume of a decision, in the one place the code calls out as
"silent data loss" if it gets it wrong.

**2. The panel says which date it is showing.** `MediaMeta` gains `mtime: i64`
beside the existing `date_taken: Option<i64>`. The panel shows `Taken` when
`date_taken` is present and `Modified` otherwise — one label, chosen from data
the client already has. Nothing is invented and nothing is conflated.

**3. `exif_read` replaces the `width IS NULL` proxy.** A new column,
`exif_read INTEGER NOT NULL DEFAULT 0`, set to 1 for every file whose header
has been looked at, whether or not anything was found. The gate becomes
`WHERE exif_read = 0`. This is the concept `width IS NULL` was impersonating,
and naming it is what lets the pass be a true no-op on a warm cache *and* still
reach a file that was decoded before it was probed.

Adding a column bumps `FORMAT_VERSION` to 2, which deletes and rebuilds the
cache — the only migration mechanism there is, and here it is what repairs an
existing library rather than a cost.

**4. Ingest probes.** The watcher's ingest path runs the same header read the
backfill does, on the files it just inserted, off the writer lock. One shared
helper, `probe_and_store(gallery, paths)`, called from both `backfill_exif` and
`ingest`. A newly arrived photo gets its date and its coordinates in the same
breath as its companion, which is what defect 2's fix has to mean.

**5. One parse.** `exif::read` opens once, with a `BufReader`, and pulls both
facts from one `exif::Reader` result. `read_head` is deleted. This is strictly
less I/O than the pair it replaces — `read_from_container` seeks to the metadata
block rather than reading a file whole, so the 1 MB bound it removes was not
buying the protection its comment claimed.

**6. Videos get a capture time.** `pipeline::video::probe` already parses
ffprobe's JSON for width, height, duration and location; it reads
`format.tags.creation_time` (RFC 3339, UTC) as well and `VideoInfo` gains
`date_taken`. Plausibility floor: anything before 1990 is discarded, the same
reasoning as `is_plausible` for coordinates — QuickTime's zero date is
1904-01-01 and a container with an unset field reports it confidently.

### Placement

`cache/meta.rs` (column, `set_probed`), `cache/db.rs` (schema, version),
`sort/sorter.rs` (order expression), `services/media.rs` (`mtime` on the
payload), `services/gallery.rs` (the shared probe helper, called from two
places), `pipeline/exif.rs` (one parse), `pipeline/video.rs` (creation time).
Dependencies point the way they already do: `services` → `pipeline` → `cache`.
Nothing lower learns about anything higher.

### Contract

- **Wire:** `MediaMeta` gains `mtime`. Additive; an older client ignores it.
- **Cache:** `FORMAT_VERSION` 1 → 2. Derived data, regenerated on next open.
- **Durable:** nothing. No companion field changes.
- **Behaviour:** `date=2024` and `has:date` keep filtering on real capture time
  — a filter that claimed to find photos taken in 2024 and returned files
  *copied* in 2024 would be worse than the bug being fixed. Only sort and group
  coalesce, because ordering must total-order everything and filtering must not.

### Cost in concepts

One column, `exif_read`, and one *except*: **sort and group use a fallback date,
filters do not.** That asymmetry is deliberate and is stated in
`docs/query/README.md` and `docs/gallery/README.md`. Set against it, one concept
is deleted — the `width IS NULL` proxy — and one duplicated read path goes.

### Alternatives

- **Write `mtime` into `date_taken` when EXIF has none.** One line, no column,
  everything downstream works unchanged. Rejected: `date_taken` stops meaning
  capture time, `has:date` can no longer answer its question, and nothing can
  later distinguish a real timestamp from a filled-in one — so a plugin that
  learns the true date has no way to know it may overwrite.
- **`date_added` as the fallback** instead of `mtime`. It is NOT NULL and needs
  no new thought. Rejected: it is when *this gallery* first saw the file, so a
  library imported in one afternoon sorts into one afternoon.
- **Keep the `width IS NULL` gate and re-run the backfill on every open.**
  Rejected: it re-reads every EXIF-less file's header forever, which is the
  precise cost the gate exists to avoid.
- **Have the thumbnailer read EXIF while it has the file open.** Tempting — it
  is already paying the open. Rejected: it puts a metadata policy inside the
  decode path, which then owes it to every caller, including the on-demand serve
  in a request. The probe is cheap and belongs where the other probes are.

### Assumptions

- **`mtime` is meaningful on this library.** Unmeasured. `rsync -a` and most
  camera imports preserve it; a plain `cp` does not. If it is wrong for a given
  file it is wrong in a way the panel now discloses, which is the most this can
  honestly offer.
- **ffprobe reports `creation_time` for the user's clips.** Unmeasured for phone
  video specifically; absent, a video falls back to `mtime` like anything else,
  so the failure mode is the status quo.

---

## B. Ending a local session (finding 2)

### The signal

The SSE stream at `GET /api/events` already *is* the answer, and there is no
second thing to build: a browser tears the connection down when the tab closes,
and the 15-second keep-alive means a connection whose peer has gone is noticed
within one interval even when no events flow. A count of live streams is a count
of open windows.

### The mechanism

`server/presence.rs` — a new ~60-line module holding

```rust
pub struct Presence { live: AtomicUsize, seen_any: AtomicBool }
```

with an RAII `guard()` the SSE handler holds for the life of its stream, and
`watch_for_exit(presence, handle)`, a task that ticks every five seconds and
tracks how long `live` has been zero.

Policy, all of it:

- **Local mode only.** Wired up on the `lightview <dir>` path and nowhere else.
  A `--serve` deployment must outlive every client; a phone that locks its
  screen is not a shutdown request.
- **Armed only after the first client has ever connected** (`seen_any`), or a
  slow browser start kills the process before it is used.
- **Thirty seconds of continuous zero**, re-checked at each tick rather than
  armed once — so a second `lightview <dir>` landing inside the window (which
  opens a tab against this process, see `open_the_running_one`) simply disarms
  it, with no cancellation to get wrong.
- **Graceful**, via `axum::serve(...).with_graceful_shutdown(...)`: in-flight
  requests finish. A `modify_companion` interrupted between its lock and its
  rename would leave a temp file in a user's gallery, and that tree is the one
  place the design promises is safe to `rsync`.
- One line to stderr, and exit 0.

Loopback is what makes the 30 seconds safe: a suspended laptop does not drop a
TCP connection where both endpoints are the same machine, so a closed lid is not
a closed window.

### Placement

`server/presence.rs`, new. `routes.rs` takes a guard in the SSE handler,
`state.rs` holds the `Presence`, `cli/mod.rs` spawns the watchdog on the local
path, `listen.rs` grows a shutdown future parameter. Presence depends on
nothing; everything else depends on it.

### Contract

No wire change. A behaviour change to `lightview <dir>`, documented in
`docs/server/README.md`.

### Cost in concepts

One: *the local process exits when its last window closes*. It is the concept
the reporter already assumed existed.

### Alternatives

- **`ThumbService::activity`, the idle signal that already exists.** Zero new
  concepts, and wrong: a tab parked on a grid makes no requests, so the session
  would die under a user looking at it. Presence is not activity — which is the
  mirror image of the warning already written on `Activity`, that the subscriber
  count must not be read as activity. Both directions of that confusion are now
  named in code.
- **`beforeunload` → `POST /api/goodbye`.** Never fires on a crash, a force
  quit, or on mobile, so it needs the timeout as a fallback regardless — and
  then the timeout is doing the work alone.
- **A client heartbeat.** A second liveness channel next to the one already
  open, plus a client timer to maintain.
- **Leave it, and document `Ctrl-C`.** Rejected: nothing in "Open with
  LightView" suggests a process was started.

### Assumptions

- **hyper drops the SSE stream promptly when a client disconnects**, within
  roughly the keep-alive interval. Currently unmeasured — `drive.sh` will assert
  it: connect a client, drop it, watch the count fall.

---

## C. Pinned locations in the picker (finding 1)

`DirListing` gains `places: Vec<Place>` — `{ label, path }` — computed by
`services/files.rs`: the gallery root, `$HOME`, and whichever of the XDG user
directories exist, read from `~/.config/user-dirs.dirs` when present and falling
back to the conventional names under `$HOME`. Only paths that exist are listed,
so the sidebar never offers a dead end.

It rides on the listing that is already being fetched rather than a second
command: the set is constant for the process, six short strings, and one round
trip is simpler than two. The client keeps its existing guarantee — it still
never assembles a path, it only sends back one the server just named.

**Alternatives.** A `dirs` crate dependency, for ~30 lines of file parsing
(principle 2: an existing dependency beats a new one, and none here does it). A
separate `list_places` command, which buys nothing for a constant. A typed path
field, which is not what was asked for and would hand the server a string it did
not choose — the one property this component was built to have.

**Cost.** One field on an existing response, one function. No new concept: a
listing already carries navigable paths the client did not construct.

---

## D. Short rows in the justified grid (finding 5)

`computeJustifiedLayout` calls `flush(end, justify)` with `justify = false` in
two places: before every forced group break, and for the trailing row. An
unjustified row sits at its target height and leaves the rest of the width
empty. Default grouping is monthly, so **every month ends in a short row** — and
under finding 4 the dates were mostly missing, scattering the few dated files
into many one- and two-item months, each with its own ragged row. Fixing dates
removes most of the symptom; the layout rule is still wrong on its own terms.

The fix is to justify those rows too and let the existing
`clamp(h, minRowHeight, maxRowHeight)` do what the `justify` flag was
approximating. A four-image trailing row lands just above target and fills the
width; a lone image would need an absurd height and is stopped at
`maxRowHeight`, left-aligned exactly as today. The graceful case gets the good
answer and the grotesque case keeps the current one, from a clamp that is
already there.

`justify` then has one remaining caller shape and the parameter goes with it.

**Cost.** Negative — one parameter and one branch removed.

**Alternatives.** A separate, tighter cap for final rows (a new knob for a case
the existing clamp already bounds); stretching unconditionally (the grotesque
case is real); leaving it (the reporter can see it).

---

## E. `makepkg` (finding 3)

`options=('!lto')` in `PKGBUILD`, with the comment explaining that `aws-lc-sys`
compiles its own C and assembly through makepkg's `CFLAGS` and that `-flto=auto`
produces bytecode `rust-lld` cannot link. Verified by the reporter on Arch.
Opting the package out beats hand-stripping `CFLAGS`, which would have to guess
at which flags the build script forwards.

### And the half of it that has not failed yet

`Cargo.lock` is in `.gitignore` and untracked, while `PKGBUILD` builds with
`--locked`. That combination works for the reporter only because `makepkg` runs
in `$startdir` — the repository itself — where an earlier manual `cargo build`
had already written a lock file. **On a fresh clone `makepkg -si` fails**, on
the first command, with the lock file needing an update that `--locked` forbids.

The same gap is wider than the package: this crate has a `[[bin]]`, and the
container image and the release workflow both build from a clean checkout, so
today every one of them re-resolves dependencies from scratch. A semver-
compatible upstream release is enough to break a build with no change on this
side and nothing in git to bisect.

Committing `Cargo.lock` is the convention for a crate that ships a binary and it
is what `--locked` was already assuming. One line out of `.gitignore`, one file
added.

---

## Verification

- `drive.sh`: a file with no EXIF sorts among the dated ones rather than last;
  `/api/dirs` returns places and every one of them exists; an SSE client that
  disconnects drops the live count; a local process with no client exits and
  releases the gallery lock.
- `grid.mjs`: no row before a group header is shorter than the container width
  unless its height is at the clamp.
- Rust unit tests: the coalesced order expression; `exif_read` set on a probe
  that found nothing; `creation_time` parsing including the 1904 rejection; the
  places list skipping what does not exist.
