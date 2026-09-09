# LightView rebuild — plan

**Status:** awaiting approval. No code until it is approved (principle 1).

**This document is self-sufficient by design.** It is written to be executed by a
session with no memory of the conversation that produced it, so every operative
decision is restated here rather than referenced. [`../../refactor.md`](../../refactor.md)
holds the *argument* for these decisions and the inventory of the system being
replaced; it is worth reading for context but nothing in it is required to
execute this plan.

**One document, not the `requirements.md` + `design.md` pair** principle 1
describes. Requirements are section 1 below. The split exists to keep a plan
reviewable; for a change this size the two halves reference each other on nearly
every line, and separating them would mean reading both to understand either.

**Reviewed.** An independent cold read (principle 1) plus an adversarial
self-review produced twenty-eight findings, all folded into the text below
rather than appended — including one **false claim** the first draft made about
the existing code, corrected in section 4. Where a finding forced a decision
that had not been made, the decision is stated inline and repeated in section 7.

**On completion:** fold what is durable into `docs/`, delete
`docs/_planning/rebuild/`, and delete `docs/refactor.md` — its inventory
describes a system that will no longer exist.

---

## 1. Requirements

What the rebuilt system must do. Everything else in this document serves these.

### Functional

1. **`lightview <dir>`** — open a gallery for local use. Full selection-scoped
   filesystem operations (copy, move, clipboard, open-with, trash).
2. **`lightview --serve <dir>`** — serve a gallery over the LAN. Remote clients
   get metadata writes, upload, and move-to-trash. **No filesystem access
   beyond that**, and no way to enable it.
3. **`lightview --remote <url>`** — attach this machine's plugins to a remote
   instance and run its tagging jobs. Replaces the `lightview-worker` binary.
4. **One grid view: justified.** Aspect-preserving rows. No square grid, no map.
5. **Storage that suits both usage patterns** — a folder of two hundred images
   processed once and never reopened, and a stable library of many thousands.
6. **Duplicate detection with durable "these are not duplicates"** that is
   visible, nameable, and not per-pair clutter.
7. **Plugin execution** for auto-tagging: a plugin runs over a gallery, locally
   or on a paired machine, and writes tags back.

   **Grouping outputs a person confirms and names — face clustering and the
   like — is deliberately *not* in this rebuild.** Its durable half is: naming a
   group writes `set::<name>` on every member, which section 3.9 delivers in
   full and which is reachable today from a selection. What is deferred is the
   channel by which a *plugin* proposes a grouping. See "Grouping is deferred"
   in section 3.10 for why, and for what it costs to add later.

### Non-functional

8. **Fewer concepts.** The measure is how many times the word *except* is needed
   to describe the system truthfully. The system being replaced needs it about
   thirty-five times; the target is about thirteen, and the survivors must be
   properties of the problem rather than of the history.
9. **Smaller.** ~26,900 lines of Rust and ~19,800 of TypeScript become roughly
   18,500 and 13,000. **If the finished tree is not smaller, something was added
   that nobody asked for.**
10. **The photos and their companion files are the only durable data.**
    Everything else must be reconstructable from them.

### Explicit non-requirements

- Backwards compatibility with the current cache database. There is none, and
  no migration code exists to provide it.
- Preserving `not_duplicates` verdicts. They are abandoned; re-marking a handful
  of duplicates is cheaper than owning translation code that runs once.
- A native desktop window. Measured, the WebKitGTK window was slower than a
  browser on the same machine.
- Any intermediate state being runnable. The first working thing is the whole
  system.

---

## 2. The five questions (principle 1)

### Placement

The rebuild keeps the existing dependency direction, which is the part of the
current architecture that was right. Three layers, and nothing points upward:

```
  pure libraries   filter · sort · autocomplete · geocode · companion · util
        ↑          (take a connection or a struct; know nothing above them)
  services         cache · pipeline · plugin · tagging · sets
        ↑          (take state or pieces of it; no HTTP, no IPC types)
  adapter          server (routes + one command table) + cli
```

The single largest structural change is that the top layer collapses from **two
adapters to one**. Today `commands/` (Tauri) and `http_server/api.rs` (HTTP) are
parallel entry points kept in step by a `*_impl` naming convention; with no
Tauri there is one dispatch and the convention disappears.

The failure this plan must not commit: a lower layer learning about a higher
one. Specifically — `cache/` must not know what a route is, and the pure
libraries must not gain a dependency on `AppState`. They have none today
(verified: zero references across 3,594 lines) and that is why they port
unchanged.

### Contract

Five contracts change. Two are durable and need care; three are local.

| Contract | Other side | Change | Risk |
|---|---|---|---|
| Companion file `<media>.lightview.json` | other LightView installs, `grep`, the user | **`tags.set: []` added as a sibling of `tags.user`; `tags.auto` removed.** Every field of the tag and meta structs gains `#[serde(default)]`, which is what makes an old sidecar parse. Schema version and `migrate()` hook stay. | Low, *given the serde attributes* — without them an old file fails to parse |
| `.lightview/trash/` layout | the user's own filesystem | **Replaced.** `<epoch_ms>/<gallery-relative path>` instead of `<epoch_ms>_<seq>/` + `meta.json` | Low — old entries are not read; purge them before switching or leave them inert |
| `cache.db` | nothing but this process | **Replaced**, moved out of the gallery, and deletable on a version mismatch | None — fully derived |
| `/api/invoke` + routes | the SPA, and a `--remote` instance | **Replaced** by one command table with trust levels | None — both sides ship together |
| Plugin NDJSON protocol | plugins on disk | `api_version: 1` only; input quantized to tier edges. **No new result kinds.** | Low — bundled plugins are rewritten in the same change, and the shape is unchanged |

**The companion file is the only thing here that cannot be regenerated.** Treat
any change to it as the largest commitment in the plan.

### Cost in concepts

The plan is overwhelmingly subtractive. What it *adds*:

- **One namespace** (`set`) in the tag vocabulary — but it replaces a table, a
  sweep exception, and a pairwise data model, so the net is negative.
- **One trust level distinction** (`Device` / `Owner`) — but it replaces an
  80-command list, a 48-arm allowlist, and the implicit relationship between
  them.
- **One CLI mode** (`--remote`) — but it deletes a binary, a cargo feature, a
  config file, and a release artifact.
Nothing else is added. Every other change removes. An earlier draft also added
a `groups` plugin result kind; it is deferred, for the reasons in section 3.10.

**Cases still needing the word *except*** after this plan, all of them
properties of the problem rather than the history:

1. Four decoders inside one decode function (formats genuinely differ)
2. `fit_rgba` as a second entry shape, for video frames at a chosen timestamp
3. Only `jm`/`jh` are byte-budgeted (the unbounded tier is small)
4. The HEIC transcode cache sits in front of one decoder
5. Companion reads fall back to the alongside location; writes do not
6. `rating:x` is a tag, `rating>=x` is a comparison; `::` is a namespace, `:` is not
7. Grid cells keyed by path, pruned surgically
8. Two single-flight slots; the drain re-arms, the warm slot deliberately does not
9. `warping` must not be cleared by `markSettled()`
10. Speculation shares the one bounded pool, so it is gated on "nothing outstanding"
11. `dist/` must exist before any `cargo` command
12. Self-signed TLS behind NAT needs its SANs named by hand
13. An offline-capable web client has caches that can lie
14. `.safe-panel` sets all four paddings and overrides `p-*`
15. Two files sharing a `set::` tag are never offered as a duplicate pair —
    **including when they genuinely are duplicates.** Two identical scans inside
    a 200-page comic will not be found. Accepted: the alternative is storing
    pairwise verdicts again.
16. A gallery mounted at different paths on two machines gets two derived
    caches. Today the cache lives *inside* the gallery and is shared by every
    machine that mounts it; keying by hash-of-canonical-root gives that up. It
    is the price of getting the blob out of the photo folder, and it is a real
    regression for a NAS mount browsed locally as well as served.

Anything beyond this list that a plan step introduces is a regression against
requirement 8 and needs to be argued for explicitly.

### Alternatives

| Considered | Why it lost |
|---|---|
| **Iced** for the desktop UI | `--serve` needs a web client regardless, so this means maintaining two complete UIs forever — the opposite of requirement 8 |
| **`tao`/`wry` shell** around the loopback URL | Keeps one frontend and re-adds a native window; still available later if a browser tab proves unacceptable, but the window is not currently wanted |
| **Incremental refactor**, nine shippable steps | At ~60% rewrite each step negotiates with the shape it replaces; requirement for runnable intermediates was explicitly waived |
| **New repository** with a salvage list | Loses history for no benefit; the same result is achievable on a branch |
| **`sets.json`** as a durable set store | Violates requirement 10 — a set is expressible as tags in files that already exist |
| **Keeping `not_duplicates`** and adding sets alongside | Two answers to one question; the pairwise table is what the complaint was about |
| **Migrating the cache schema** | The database is fully derived, so deleting and rebuilding is strictly simpler and the migration code would be permanent |
| **Compatibility shim** so the old SPA drives the new backend | Doubles the API surface for the duration; the dark period was accepted instead |

### Assumptions

Named as assumptions because they are not measured. If one is wrong, the
consequence is stated.

1. **A browser renders the grid at least as well as WebKitGTK did.** Basis: the
   user reports the native window measurably underperformed a browser on the
   same machine. If wrong, the fallback is a `tao`/`wry` shell around the same
   loopback URL — the frontend does not change.
2. **The grid's scroll tuning still earns its keep without WebKitGTK.** The
   decode gate, staged tier upgrade and fling handling were tuned for
   main-thread decode. Deliberately **not** re-measured. If scrolling regresses,
   that is when to revisit — not before.
3. **A 512px `j` tier is sufficient input for the ML taggers.** They declare
   1024 today and will receive `jm` (1280); a 448-pixel model receives `j` (512).
   Models downsize internally. If a tagger degrades, raise its declared edge.
4. **dHash over aspect-preserving `j` bytes groups duplicates as well as over
   square-cropped `m` bytes.** The hash downsamples to 9×8 regardless. If
   grouping degrades, the threshold is already a parameter.
5. **A browser plays animated GIFs acceptably.** The sprite-sheet atlas existed
   solely for a WebKitGTK bug. If memory is a problem on iOS, the answer is
   `createImageBitmap` + explicit `close()`, not a resurrected atlas.
6. **All-pairs Hamming duplicate detection is fine at this library's scale.**
   Quadratic; fifty million comparisons at ten thousand images. It will not hold
   at a hundred thousand. Leave it until it hurts.

---

## 3. Target architecture

### 3.1 One binary, three modes

```
lightview <dir>              serve <dir> on 127.0.0.1:<ephemeral>, open a browser at it
lightview --serve <dir>      serve <dir> on 0.0.0.0:<port> over TLS, with pairing
lightview --remote <url>     attach to a remote instance, offer this machine's plugins
lightview pair               mint a one-time pairing PIN for this machine, and exit
lightview remote-pair --server <url> --pin <n> [--name X] [--trust-new]
                             redeem a PIN against a remote server and store the
                             credential this machine will use for --remote
lightview password           set or clear the gallery password, reading it from
                             stdin — never from argv, where it lands in shell
                             history and `ps`
lightview cache              show the derived-cache directory and its size
lightview cache --prune      evict least-recently-opened galleries to the budget
```

**`pair` takes no gallery argument.** Pairings live in the data dir and are a
property of this machine serving (section 3.3), so there is nothing per-gallery
to name. The consequence, stated because it is a real widening: a phone paired
to this machine is paired to every gallery this machine serves, now or later.
That is consistent with all paired devices being equally trusted, and it is the
cost of removing the per-gallery cookie-name mint.

**`--remote` must not open a browser.** Attaching a GPU machine to a NAS is a
long-running background job — a systemd unit on a headless desktop. Coupling it
to a foreground process buys nothing, because browsing a remote gallery is a
browser pointed at its URL and needs no binary at all.

**One instance per role.** A machine that both serves its own gallery and hosts
plugins for a remote one runs two processes, each with its own config.
`--serve` and `--remote` are mutually exclusive. The alternative — one process
holding a list of attachments — means a repeated config section, a lifecycle
where one failing attachment must not take down the server, and interleaved
logs. Two processes get all of that from the operating system.

### 3.2 Trust is a property of the bind

One command table. Each entry carries the minimum trust it requires.

| Level | Reachable from | Covers |
|---|---|---|
| `Device` | any paired client | browse · sorted items · filter · autocomplete · media and thumbnail routes · tags, ratings, colour labels, notes, sets · `record_view` · `get_media_meta` · `regenerate_thumbnail` · **trash: move-to-trash, list, restore** · upload · duplicate detection and `get_merge_candidates` · enqueue/cancel a tagging job · `apply_plugin_tags` and the worker claim/update/complete/fail set |
| `Owner` | **a loopback bind only** | copy · move · clipboard · open-with · open a gallery · list a directory (the picker) · install a plugin · **`purge_trash`** · **`merge_duplicates`** · write `.lightview/settings.toml` |

Three of those placements were unstated in the first draft and are decisions,
not omissions. **`restore_trash` is `Device`** — it writes a file back to a path
the user already chose, which is the inverse of a delete the same client was
allowed to make. **`purge_trash` is `Owner`**, because permanent deletion is not
"move to trash" and requirement 2 says remote clients get move-to-trash.
**`merge_duplicates` is `Owner`**, because it rewrites a companion, stamps the
keeper's mtime on disk, and trashes the others; a remote client may *find*
duplicates and see the candidates, and may not resolve them. The frontend hides
what the client cannot do, and the server enforces it regardless.

**`Owner` is granted by a loopback bind and by nothing else. There is no flag
that widens it.** This is the single security rule of the system and it must
survive every later change.

The bootstrap routes — `/healthz`, `/cert`, `/pair/redeem`, `/auth/password`,
`/auth/status` — are unauthenticated by necessity, since there would otherwise
be no way past the auth layer the first time. They are a route group, **not** a
trust level: no *command* is ever reachable unauthenticated, and naming them in
the table would imply a third tier of command that does not exist.

**Path confinement is universal, with no exception.** Every route that resolves
a filesystem path canonicalizes it and compares against the gallery root
canonicalized once at open, returning **404, not 403** — a 403 confirms the
existence of files the caller has no business knowing about. What `Owner`
widens is the *destination* of a copy or move, which was never confined and
never can be. Sources confined always; destinations confined never; one trust
level deciding who may name a destination.

Uploads enforce confinement three times over, because the filename comes from
an untrusted device: reduce to basename with traversal rejected, require the
extension to resolve to a known media type, and confirm the resolved
destination is inside the root before writing a byte.

### 3.3 Storage

**Durable — the photos and their companion files. Nothing else.**

```
<gallery>/
  2026/january/photo.jpeg                       the media
  .lightview/
    companions/2026/january/photo.jpeg.lightview.json
    settings.toml                               display prefs + the default filter
    trash/<epoch_ms>/2026/january/photo.jpeg    a trashed file, path = provenance
    trash/<epoch_ms>/2026/january/photo.jpeg.lightview.json
```

`settings.toml` is durable and belongs in this list — the first draft left it out
of the diagram while describing it two paragraphs later, and put the **default
filter** in the derived database, where a format bump would have deleted a user
setting. The default filter is user intent and lives here.

Kilobytes to low megabytes. Safe to copy, sync, or read with `grep`. Delete
everything else and reopen, and nothing is lost but time.

**The data dir holds two different things, and conflating them is dangerous.**

```
<data_dir>/
  galleries/<sha256-of-canonical-root>/cache.db   DERIVED — disposable, budgeted
  ─────────────────────────────────────────────
  tls/                                            NOT derived: private key
  server.toml                                     NOT derived: password hash, SANs
  devices.db                                      NOT derived: every pairing
  remote.toml                                     NOT derived: --remote credential
  install_id                                      NOT derived: cookie-name suffix
  plugins/<name>/                                 NOT derived: installed plugin code
```

**The budget and `lightview cache --prune` apply to `galleries/` only.** Nothing
else under the data dir is regenerable: losing `devices.db` re-pairs every phone
by hand, and losing `tls/` re-prompts every browser. The first draft filed all of
it under one "derived, disposable" heading, which is how a future maintainer
clearing a cache un-pairs the household.

Consequences, all of them currently missing:

1. The photo folder stops accumulating a multi-hundred-megabyte opaque blob.
2. Every derived cache is in one place, so it can have **one total-size ceiling
   with least-recently-opened eviction across galleries**, reachable from
   `lightview cache`.
3. Paths in the database become **gallery-relative**, since the root is no
   longer implied by the file's location. `rebase_root` and `infer_old_root`
   are not written.

**Accepted cost, two of them.** Keying by a hash of the canonical root means
moving a gallery re-thumbnails it — and it also means a gallery mounted at
different paths on two machines no longer shares one cache. Today the cache
lives inside the gallery, so a NAS mount is thumbnailed once and read by
everything; after this it is thumbnailed per machine that opens it locally. The
serve-plus-browse flow is unaffected, since a browser holds no cache of its own. An id file in `.lightview/` would avoid that for two lines,
and is deliberately not in the design — an id is not something the user needs,
and requirement 10 earns its power by having no exceptions. Add it later if
moving large galleries turns out to be a habit.

**A read-only gallery improves:** derived data goes to the local data dir, so
only the durable half degrades rather than nothing working.

#### Configuration is a file; commands are actions

`server.toml` in the data dir, read at startup and on change: bind address,
port, TLS SANs, password hash, inactivity window, upload enable and scheme,
trash retention, cache budget.

**There is no remote-delete flag.** Today one exists and gates the whole trash
group plus merge; requirement 2 makes move-to-trash something a remote client
*gets*, not something it might get, so a flag could only narrow `Device` into a
third trust state — an *except* that is not on section 2's list. Two consequences
worth naming rather than discovering: **`purge_trash` and `merge_duplicates`
become unreachable remotely**, where today they are reachable with the flag on,
and a gallery can no longer be served with deletion turned off. Not commands. A headless
deployment configures itself by editing a file, which is what a headless
deployment expects.

Pairing stays a command (`lightview pair` prints a PIN) because minting a code
is an action, not a setting. **So does the password**, for a sharper reason: the
stored value is an argon2id PHC string, and "configuration is a file" cannot mean
asking a person to hand-compute a hash. `lightview password` reads the passphrase
from stdin, hashes it, and writes the field; `lightview password --clear`
removes it. The hash format is argon2id with the crate's default parameters,
unchanged from today.

**The password gates the `--serve` bind only.** A loopback session is already
authenticated by the launch token, and someone with local shell access has the
photos regardless. This is also a contract change worth naming: the password
moves from per-gallery to per-machine along with the pairings.

Display preferences stay per-client: `.lightview/settings.toml` for a local
gallery, browser local storage for a remote one. A phone and a desktop looking
at the same gallery must not fight over thumbnail size.

#### The database

Three thumbnail tables plus the index. **No migration list, no
`SCHEMA_VERSION` derivation, no idempotency tests.** One `format_version`
integer in `gallery_meta`; if it does not match the build's, delete the file and
re-index.

| Table | Holds |
|---|---|
| `media_meta` | relative path (PK), media type, size, mtime, `date_taken`, `date_added`, `last_viewed`, `last_rated`, rating, width, height, duration, `gps_lat`, `gps_lon`, `color_label` |
| `tag_index` | `(path, namespace, tag)` — rebuilt from companions |
| `tag_counts` | `(namespace, tag) → count` — rebuilt from `tag_index`; feeds autocomplete |
| `index_state` | companion mtime per path, so re-indexing skips unchanged files |
| `thumbs_j` | 512px fit — also carries the ThumbHash blob and the `phash` column |
| `thumbs_jm` | 1280px fit — LRU byte-budgeted |
| `thumbs_jh` | 2560px fit — LRU byte-budgeted |
| `gallery_meta` | key/value: `format_version`, location tagger version |

The location tagger version stays here despite being a stamp rather than a
cache, and the coupling must be stated: **deleting the derived cache causes one
re-geocode pass, which rewrites every geotagged sidecar.** That is a derived
wipe triggering durable writes. It is idempotent and costs one pass, so it is
accepted rather than designed around — but "delete everything else and reopen,
and nothing is lost but time" means *time*, including sidecar mtimes moving.

**Every path-keyed table is swept together.** Keep a single
`path_keyed_tables()` source of truth and a test asserting that removing a
media row clears every one of them — the failure it prevents is a
multi-megabyte blob keyed to a path nothing can reach again. There is no
`not_duplicates`, so there is no table sitting outside that sweep.

PRAGMAs, carried over because they were measured: `journal_mode=WAL`,
`temp_store=MEMORY`, `mmap_size=268435456` (per connection), `cache_size` 64 MB
on the writer and 8 MB per read-only pool connection.

Connection strategy, carried over: one writer behind a `tokio::Mutex` (because
`rusqlite::Connection` is `Send` but not `Sync`), and a read-only pool of 2–6
connections from `available_parallelism` for the thumbnail serve path. **Never
hold the writer across a decode, an encode, or a subprocess.** More than one
statement means a transaction — SQLite autocommits per statement, so a
variable-length write loop pays a WAL commit each time.

### 3.4 Trash

`.lightview/trash/<epoch_ms>/<gallery-relative path>`. The timestamp segment is
the deletion time **and** the uniqueness key; everything after it is the
original path. There is no metadata file.

- **Purge** is `read_dir`, parse the numeric name, compare against the retention
  window, `remove_dir_all`. No file reads.
- **Restore** moves the media back to `<root>/<relative path>`, refusing if
  something already occupies it, `create_dir_all` for a vanished parent, then
  prunes empty directories back up to the timestamp directory. **The companion
  goes to the current write location** — `.lightview/companions/<relative
  path>.lightview.json` — not alongside the media where the trash entry keeps
  it. A naive path-mirroring restore drops it beside the photo, where the read
  fallback in section 3.7 still finds it, so it *appears* to work and the next
  metadata write forks a second sidecar. The trash round-trip test must assert
  the companion's destination, not just the media's.
- **One delete is one directory**, which makes undoing an operation a natural
  unit.
- The companion sits alongside the media inside the trash, uniformly, whichever
  location it came from.

Two things a bare path mirror could not do, and why the timestamp segment
exists: mtime cannot carry the deletion time (a rename preserves it, and
preserving it is the point — restoring a file with a rewritten mtime is silent
data loss), and relative paths are not unique over time (trash, restore, edit,
trash again).

`.lightview` is skipped by the media scan, the companion indexer and the
fs-watcher. Keep all three.

### 3.5 Thumbnails — one pipeline

Three tiers, one family, all aspect-preserving, all WebP.

| Tier | Segment | Longest edge | Bounded | Also carries |
|---|---|---|---|---|
| `j` | `j` | 512 | no | ThumbHash blob, `phash`, GIF handling, tag-panel thumbs |
| `jm` | `jm` | 1280 | **LRU** | the viewer's progressive underlay |
| `jh` | `jh` | 2560 | **LRU** | high zoom |

**One render path, and this is the invariant that keeps it one:**

```
decode_image(path, edge)  →  fit_dims + resize_rgba  →  encode WebP
  dispatch on format           one implementation        one encoder
```

`decode_image` dispatches on source type — JPEG with scale-on-decode, HEIC via
`libheif`, video via `ffmpeg`, everything else via the `image` crate — and
converges on RGBA immediately. That is dispatch, not duplication.

**Every cached thumbnail is `generate_for_path_fit(path, edge)` at one of three
edges.** Anything that makes this untrue — a tier derived from a larger tier
instead of decoded, a second encoder, a GPU fast path — is a new failure mode
and must be argued for, not slipped in as an optimization.

**Refused in advance: deriving `jm` from a cached `jh`.** The Micro-from-Standard
fast path it would imitate earned its branch by saving a 16× decode on the rung
the grid hammered hardest. `jm` from `jh` saves 4×, on an LRU-bounded tier that
is rarely cold, in exchange for a second way a thumbnail can be wrong.

**A tier is a cached edge.** The serve path, the batch warm, the plugin input
path and the `?fit=` route all call the same function; the difference is only
which edge they ask for and whether the result is stored. Put the
cache-and-coalesce wrapper around that one function so every caller inherits
coalescing.

**Serving.** `GET /thumb/{tier}/{*rel}` reads through the read-only pool; on a
miss it elects one generator while others wait on a `Notify`, bounded to three
attempts. **The waiter enrols in the wake queue before re-checking the cache** —
reversing those two steps loses a notify that races with the generator's
release. A cancelled generator wakes its waiters having produced nothing; the
woken waiter re-checks, finds the slot free, and becomes the generator. ETag
revalidation on the response, so a phone returning after `max-age` expiry
refreshes its grid for a few hundred bytes per thumbnail.

**Disk budget** on `jm` and `jh`: 10% of free disk, floored at 512 MiB, capped
at 8 GiB, per tier, overridable with `LIGHTVIEW_TIER_BUDGET_MB`. Byte-budgeted
with a window function accumulating warmest-first, keyed on an `accessed_at`
column, evicting only past **1.25×** the budget and then trimming all the way
back. Freshly written rows seed `accessed_at` to now — leaving it at the column
default marks every new row maximally cold, so the next pass deletes exactly
what was just generated. Access marks buffer in memory because the read path
holds a read-only connection, and are drained **immediately before** an eviction
pass; reversing that order evicts what the user is looking at. Both write paths
enforce the budget, not just the batch one.

**Idle backfill** warms `j` when no user-driven thumbnail request has landed in
60 s, newest-first (the order the default date-descending sort presents),
re-checking between work units. It also computes perceptual hashes.

**Video**, carried over intact because it is knowledge rather than code:
`ffmpeg` does the downscale in its filter graph at exact pixel dimensions, so a
4K clip crosses the pipe at ~1.8 MB instead of ~33 MB. Those dimensions come
from the probe **with display-matrix rotation applied** — phone clips are
landscape on disk and portrait on screen, and a mismatch here is what makes
`.MOV` thumbnails come back sideways. Every invocation is timeout-bounded. The
same probe lifts the container's ISO 6709 location tag (phones spell the key
three ways) into the GPS columns, and a probe with no location must not clear a
stored one.

**The idle backfill's "is anyone looking" signal must be rebuilt.** Today it is
`fs_change_tx.receiver_count() > 0` — *no web client is subscribed* — because the
desktop user was a separate kind of client detected by a thumbnail-activity
timestamp. After this rebuild **the local user is an SSE subscriber**, so that
counter is true whenever anyone has the gallery open in a browser and the
backfill would never run at all — taking perceptual hashing, and therefore
duplicate detection, with it. **The activity timestamp is the only signal.** The
subscriber count no longer means anything and must not be consulted; the
sentence above is the authoritative statement of `is_idle`.

**Placeholders must not write source dimensions.** A `0×0` write both fills the
`width IS NULL` gap that guards the column and hands the grid a degenerate
aspect ratio.

### 3.5b Serving media

`server/` is written fresh, so the media route is written from nothing and these
three behaviours must be carried deliberately. Each is currently load-bearing and
none is recoverable from the tier pipeline.

**Range requests.** `Accept-Ranges: bytes` and real `206 Partial Content`
responses. Without them `<video>` scrubbing does not work in any browser, and the
ported viewer assumes it. Stream the range rather than buffering it — the current
implementation notes that buffering allocated `len - N` bytes per seek.

**HEIC is transcoded on the serve path, not only in the decoder.** No browser
renders HEIC, so a full-resolution request for a `.heic` original returns JPEG,
through the same bounded transcode cache keyed on `(path, mtime)`. This is
distinct from `decode_image`'s HEIC branch, which produces thumbnails. Wiring
only the latter ships a build where HEIC thumbnails work and the viewer is blank
— a failure that survives to production because the grid looks correct.

**`?fit=<edge>`** returns an aspect-preserving WebP resize of a still, through
the same `generate_for_path_fit` the tiers use and the same coalescer. It applies
to `jpg`/`jpeg`/`png`/`webp` only; GIF and video fall back to the whole file. It
exists for plugin input above the top tier and for nothing else, now that the
grid uses tiers only.

**`?frame=<i>&frames=<n>`** extracts one evenly-spaced still from a clip and
returns it as WebP, honouring `?fit=` as the frame's edge. **This is a fourth
required behaviour, not an optional one** — it is the entire reason a `--remote`
plugin host needs no `ffmpeg` and does not pull whole videos across the LAN, and
section 3.10 promises that every executor prepares input identically. Omit it and
`--remote` silently cannot tag video, which is the failure this rebuild inherits
a fix for. Step 7's acceptance requires a clip in the test gallery for exactly
this reason.

**Paths on the wire are gallery-relative**, matching the database. Two
exceptions, both `Owner`: a copy or move *destination* is absolute by necessity,
and a plugin's temp file path is absolute by protocol. Carry the encoding rule
with it — **percent-encode each path segment independently and leave `/`
literal**, because axum's router decodes captures but rejects paths containing
raw encoded slashes. A single `encodeURIComponent` over the whole path 404s every
file in a subdirectory.

### 3.6 Query — filter, sort, group, autocomplete

Ported essentially unchanged. The grammar is the contract; keep it exactly.

```
vacation                    bare word — any namespace
user::vacation              namespaced ("::" so single colons stay inside tag values)
plugin.face::person:alice   plugin namespace
set::kellys-comic           set membership (new)
Japan  Kyoto                place names, written as tags by the geocoder
NOT auto::indoor            negation; AND / OR / parentheses
rating>=4                   rating comparison — but rating:general is a TAG
type:video                  media type
has::user  has::set  has:geo   namespace non-empty / has coordinates
color:red  color:none       colour label, or its absence (IS NULL, binds nothing)
date>=2024-01-01  date=2024 dates: taken / added / viewed; a year expands to its range
width>=1920  size>=10mb     pixel dimensions, file size (b/kb/mb/gb)
```

Removed: the `GeoBbox` term (its only caller was the map view) and the `auto`
namespace (nothing ever wrote it). Added: `set`.

**Add quoted strings to the tokenizer.** Today a tag containing a space cannot
be named by any query — it fails outright, and autocomplete offers such tags, so
clicking a suggestion returns a 500. The tokenizer already handles one
context-sensitive character (`(` groups only when it starts a token, which is
what keeps `hatsune_miku_(vocaloid)` intact), so a quote state belongs in the
same loop. Two frontend spots move with it: the current-token regex in the
filter bar would split inside a quoted phrase, and inserting a suggestion
containing whitespace must wrap it in quotes.

**Compilation.** `parse_filter` produces an AST; the evaluator walks it into a
`WHERE` fragment plus bound parameters against `media_meta`. Tag terms compile
to correlated `EXISTS` subqueries over `tag_index`; everything else is a column
comparison. **A field is filterable only if it is indexed** — that is the
constraint the language is shaped by, and the reason `rating` and `color_label`
are mirrored from the companion into columns.

**Sorting** selects from `media_meta` with a `LEFT JOIN` on the `j` tier, purely
to inline the ~25-byte ThumbHash into the payload so the grid paints every cell
blurry before any thumbnail request goes out. **Qualify every column with the
table alias** — the joined table has its own `path` and `media_type`, and a bare
column name makes SQLite reject the statement as ambiguous. A filtered path list
binds as one JSON parameter expanded with `json_each`, so one prepared statement
serves every filter size.

**Grouping** is a separate in-memory pass over the already-sorted list, emitting
`{label, start_index, count}` where the group key changes. Compare a
`(year, month, day)` triple, not the rendered label — formatting is the
expensive half, and a hundred thousand items hold a few hundred distinct
periods. Key and label must stay one-to-one per granularity, and both spell an
unrepresentable timestamp "Unknown date".

**Autocomplete** holds every unique tag in memory (~300 KB at 5,000 tags) with
its lowercase form precomputed at refresh — folding case per query means
thousands of allocations to answer one keystroke with a value that has not
changed since the last write. Four-tier score: exact, prefix, substring,
subsequence. Deduplicate across namespaces, summing counts. **The subsequence
tier compares character counts, not byte counts** — comparing against
`needle.len()` silently kills fuzzy matching for every non-ASCII query.

### 3.7 Companion files

The record of intent, and the only durable data. Format unchanged:

```
{ schema_version, file, file_hash, media_type, created, modified,
  tags: { user: [...],
          set:  [...],                        // NEW — sibling of user, user-owned
          plugins: { "<name>": { version, tags: [...], ...extras } } },
  meta: { core: { rating, date_rated, color_label, notes, media, location },
          plugins: { "<name>": {...} } } }
```

**`tags.set` is a sibling of `tags.user`, not a plugin bucket.** A plugin bucket
is versioned and replaced wholesale on a re-run, which is right for geocoded
place names and exactly wrong for a set: a set is user-owned and must survive
re-tagging.

**Unknown keys are preserved on write, not dropped.** `#[serde(default)]` makes
an old sidecar *parse*; it does not stop the next write from erasing what the
struct no longer models. Removing the `auto` field means the first rating change
or plugin run silently deletes a user's `auto` tags from the one file that
cannot be regenerated. So `TagCollection` and `MetaCollection` each carry a
`#[serde(flatten)] extra: Map<String, Value>` and round-trip it untouched. The
decision in section 4 — that `auto` tags are dropped — is a decision about the
*index*, and this is what keeps it from quietly becoming a decision about the
*file*.

**Every field of the tag and meta structs takes `#[serde(default)]`.** None of
them has it today. That attribute — not the schema version — is what makes an
old sidecar without `set`, and a new one without `auto`, parse rather than fail.
The claim that "an old file reads correctly in the new build" is true *because
of* this line and false without it.

`user` is never overwritten by anything but the user. A plugin writes only under
its own key, so a re-run replaces that plugin's output and touches nothing else.
`rating` and `color_label` are mirrored into `media_meta` columns by every path
that sets them, and rebuilt from the companion at index time.

**Writes are atomic**: serialize to a uniquely-named temp file **in the target
directory** (same filesystem, therefore an atomic rename) and rename into place.
A reader sees the old file or the new one, never a truncated one. The writer
stamps `modified`, not the caller.

**One write location** — `.lightview/companions/`. The `companion_location`
setting is deleted; it had a UI control and never reached a write, since every
path called `CompanionLocation::default()`. **Keep the read fallback** to the
alongside location, which costs nothing and means a gallery holding older
sidecars keeps resolving them with no migration pass.

`schema_version` is stamped on write, checked on read, and `migrate()` is called
unconditionally on every parse. It is the identity function today. **This is the
one migration hook that stays**, because a sidecar is a wire format holding data
that cannot be regenerated.

### 3.8 Geocoding

Ported unchanged. GeoNames *cities1000* embedded via `reverse_geocoder`
(~144,000 places, 7.9 MB), lazily built behind a `OnceLock` so a gallery with no
geotagged media never pays for it. Nearest-neighbour in a k-d tree, with two
ceilings that stop confident nonsense: past **25 km** the city tag is dropped
(country and region are still right at that range); past **100 km** nothing is
emitted at all.

Names go into the companion under `tags.plugins["location"]`, **spaces joined
with underscores**, so `Japan` works as a bare filter word for the same reason
`vacation` does and `grep -rl Kyoto` finds it without LightView. Only whitespace
is normalised — accents, apostrophes and hyphens tokenize fine.

It reuses the plugin-bucket container without being a plugin, and that stays
correct next to the new `set` namespace: a location bucket is versioned and
replaced wholesale on a re-geocode, while a set tag is user-owned and must
survive re-runs. Different lifetimes, different homes.

Runs in the background task after the GPS backfill and **before** the companion
index pass, so the sidecars it writes are picked up in the same sweep. A version
stamp in `gallery_meta` forces exactly one re-resolve pass when the emitted tags
would change, and is stamped only if every write succeeded.

Accuracy, measured against twelve landmarks: country 12/12, region 11/12, city
4/11. A dense metropolis is subdivided into wards with their own centroids, so
the nearest entry to a landmark is routinely a neighbour. **Country and region
are dependable; the city tag is "the nearest named place".**

### 3.9 Sets — one concept for three features

**Set membership is a tag.** A `set` namespace alongside `user` and
`plugin.<name>`, one tag per member:

```
set::vacation-burst-3      a burst that is not forty duplicates
set::kellys-comic          a work that exists as several images
set::alice                 a face cluster, once a person has named it
```

**The tag-write commands take a namespace parameter.** Every one of them is
user-hardcoded today — add, remove, the batch forms, rename, merge, delete, list
— so "the tag-write commands apply unchanged" was wrong. Each gains a
`namespace` argument accepting `user` or `set` and nothing else; a plugin
namespace is never writable this way, since a plugin bucket is replaced
wholesale by its own run. One parameter on an existing family, rather than a
parallel family, because the operations are identical and only the destination
differs.

That gives sets their whole surface for free: create is a batch add over a
selection, rename is `rename`, merge two clusters is `merge`, delete is
`delete`, and the tag manager lists both namespaces instead of one.

That is the entire data model. No new file, no new table, no new wire format, no
new filter syntax. The tag index, `tag_counts`, autocomplete, grouping and the
tag-write commands all apply unchanged, and it is reconstructable from
companions because it *is* companion content.

**"Not a duplicate" is not stored.** It is derived: two files sharing any
`set::` tag are never offered as a duplicate pair — one `EXISTS` clause in the
finder. Forty burst frames cost forty tag rows instead of 780 pairwise ones, and
the user sees a name rather than a list of negations.

**Order comes from the gallery's own sort.** A comic strip's pages are
`page01.jpg`, `page02.jpg`. Storing an ordinal per member would be a second
thing to keep in step with the filename, for a case the filename answers. Add
one only when a set genuinely needs an order its filenames do not carry.

**One kind, no `source` field.** A confirmed variants group does not persist —
the merge trashes the extras, so one file survives and there is no set left. An
unconfirmed one is a candidate, recomputed from hashes on demand. What remains
is one relation: these belong together. A plugin wanting attribution already has
`meta.plugins[<name>]`.

**A merge unions `set::` tags onto the keeper**, like user tags. Without it,
merging a set member silently drops that member's set. A keeper ending up in two
sets is fine — suppression is pairwise co-membership, so two sets do not become
one through it.

**Sets are cheap and fluid, deliberately.** Renaming one rewrites every member's
sidecar; trashing a member shrinks it silently. That is accepted — a set is not
a durable object with an identity, it is a name several files agree on.

**Duplicate detection**, otherwise unchanged: a 64-bit dHash computed from the
cached `j` thumbnail (already decoded, already in the database, so hashing a
gallery costs no source decodes), stored in a `phash` column so it dies with the
thumbnail it describes. All-pairs Hamming comparison; threshold is a parameter
for precision, not for cost.

**Merge**, unchanged: fold several copies' metadata onto one survivor and trash
the rest. User tags union (editable), plugin tags union (automatic), rating /
colour / notes / location as per-field picks, file mtime stamped with
`filetime`, embedded EXIF GPS promotable into the keeper's companion. **Image
bytes are never rewritten** — there is no EXIF write path, and GPS is the one
exception precisely because it can be captured without touching them. The dialog
resolves conflicts; the backend applies a fully-resolved plan.

### 3.10 Plugins and tagging

**The protocol.** The host spawns the plugin as a subprocess and speaks
streaming NDJSON over stdin/stdout:

- request `{"action":"tag","path":"/abs/path.webp"}`
- result `{"path":"...","tags":[...],"meta":{...}}` or `{"path":"...","error":"..."}`
- **exactly one result per request**, including an error result for anything the
  plugin cannot process
- **plugins must consume requests as they arrive and emit each result as soon as
  it is ready** — never buffer stdin to EOF. A plugin that waits for EOF
  deadlocks any job larger than the download window by construction.
- `LIGHTVIEW_JOB_TOTAL` in the environment carries the expected request count,
  replacing the read-to-EOF sizing pattern.

**`api_version: 1` only.** Version 0 predates the streaming contract and is
refused everywhere — not refused in one place and allowed in another. A version
newer than the host is refused too.

**The host decides input, and a plugin never sees a video.** `plugin/input.rs`
scales a still to the requested edge, splits a clip into `input.video_frames`
samples (default 5, clamped to 16) sent as ordinary still requests, and merges
the results: a **union** of the per-frame tag sets, plus a **redone argmax** for
`rating:`, because a rating is one choice rather than a set. A single-part item
passes through untouched. All executors share this machinery, so a plugin cannot
behave differently depending on where it ran.

**Input is quantized up to a cached tier edge.** A plugin declares the longest
edge it wants; the host serves the smallest tier at least that big — ≤512 → `j`,
≤1280 → `jm`, ≤2560 → `jh`, above that decode from source. **Round up, never
down**: a model handed a smaller image than it trained on has lost information it
cannot recover. The plugin host reads the same `/thumb/<tier>/<path>` URLs a
browser does. Video frames are the irreducible exception — a frame at a timestamp
is not a tier.

**The payoff is conditional, and the condition is not automatic.** "A job over a
warmed gallery does no decoding at all" holds only for the tier the idle worker
warms, which is `j`. The three bundled ML taggers currently declare
`max_edge: 1024`, which rounds up to `jm` — so every image in a job would trigger
a full `jm` generation *on the server*, which is the exact cost this change
exists to remove, on the machine least able to pay it. **Rewrite the bundled
taggers to declare 512**; models downsize internally, so the loss is nil.

State the general rule where a plugin author will read it: **a plugin declaring
an edge above the warmed tier pays one generation per image.** If a tagger
genuinely needs `jm`, the answer is to warm `jm` for that gallery, not to absorb
the decode silently.

**Delete the stubs.** `ExecutionConfig::Wasm`, advisory `capabilities`
(`ReadImage`, `NetworkAccess` — there is no sandbox), and `ui.context_menu_items`
promise things that do not exist. A stub that errors is worse than an honest
absence.

**Grouping is deferred, and the result kind is deleted rather than stubbed.**

An earlier draft added a `groups` result kind — a plugin proposes groupings, the
user names one, the name becomes `set::<name>` on every member — and claimed it
needed "no new storage at all". That claim does not survive the motivating case.
Wiring a face-clustering plugin to a confirmation screen needs four things this
plan does not have:

1. **A way to get proposals off a remote host.** Tags travel back through
   `apply_plugin_tags`; groups have no equivalent command. Under `--remote` the
   model runs on a desktop and the panel is served by the NAS, so proposals are
   produced in the wrong process with nothing to carry them.
2. **A terminal-line contract.** `PluginResult.path` is required, so a
   standalone `{"groups": …}` line does not deserialize. Worse, the download
   window releases a permit only when a result matches a request — a plugin that
   withholds output until it has seen every face **deadlocks past 64 images**,
   which is precisely the failure this codebase shipped for a year. A clustering
   plugin must emit a per-image acknowledgement to keep permits recycling and
   then a terminal line, and that is a contract, not an implementation detail.
3. **Durable proposals.** "Losing them on restart costs one re-run" is true of a
   tagger and false of clustering, where a re-run is hours of GPU across the
   library. They need a `plugin_groups` table in the derived cache, path-keyed,
   in the sweep list.
4. **Regions, and a channel home.** `{id, label, paths}` points at whole photos,
   so a group shot with five people lands in five clusters with nothing
   distinguishing which face is which — the feature does not work for the case
   it exists for. And cluster ids are not stable across runs, so without a
   channel carrying already-confirmed names back to the plugin, a second pass
   re-proposes everyone.

**Why deferring is cheap.** Each of those is additive against decisions this plan
already makes: a command is one row in the command table; a table is one line in
`path_keyed_tables()`, covered automatically by its test, behind a
`format_version` bump that deletes and rebuilds for free; `path` becoming
`Option` plus a `groups` field is backwards compatible and gated by
`api_version: 2`, which exists for exactly this; `region` and `known` are
additive JSON with `serde(default)`.

**The one decision that had to be made early is made:** a confirmed group name is
a `set::` tag in the companion (section 3.9). That is the load-bearing choice, it
is independent of how a grouping gets proposed, and sets need nothing from the
clustering case — naming, many members, and one photo in several sets all work
identically for a burst. Regions are proposal-time scaffolding, discarded on
confirmation; if face boxes are ever wanted durably, `meta.plugins["face"]`
already exists to hold them.

**Deleted, not stubbed.** A result kind a plugin can emit into with nothing
receiving it reports success and does nothing — the shape of the video-tagging
bug this rebuild inherits a fix for, and the reason this plan deletes the `Wasm`
variant rather than leaving it erroring. The same rule applies to an addition of
mine.

**And principle 2 says so directly:** *no abstraction, interface, or plugin point
for a single implementation.* A `groups` protocol with no plugin is a plugin
point with **zero**. Designing it now also means designing it with nothing to
test against, which is how a shape turns out wrong the first time a real
clustering plugin meets it.

Build it when a plugin exists that needs it. Grouping by selection — what a
burst or a comic actually needs — works from day one through the tag commands in
section 3.9.

**Findings are deferred for the same reason**, and more comfortably: the
`choice`/`confirm`/`label` shapes, a `pending::` filter term and two extra tables
are an elaborate design for plugins that do not exist. Nothing here forecloses
them.

**One executor, one queue.** Every plugin run goes through the job queue,
including a local one: one code path, one progress display, one cancel. The
`--remote` executor is the same job loop parameterized on a byte source (HTTP
fetch vs local read) and a result sink (`apply_plugin_tags` over HTTP vs
directly). Both already share `plan_parts`, `InputPolicy`, `PartTracker` and
`MergedItem`.

**Constants, carried over because they were arrived at by failure:**

| Constant | Value | Why |
|---|---|---|
| files-on-disk window | 64 | bounds a remote host's temp dir |
| `STALE_AFTER_RESULTS` | 128 | abandon a request once the plugin answered this many *others* — a count, not a clock, so a slow CPU tagger never sheds images |
| `IDLE_RECLAIM` | 5 min | clears a job's tail, where no further results arrive to drive the count; only once the plugin has answered something, so a first-run model download is never mistaken for a wedge |
| `NO_RESULT_STALL` | 20 min | outer backstop; refreshed **only by results that matched** |
| apply batch | 32 | results per `apply_plugin_tags` |
| `MAX_LOCAL_PENDING` | 32 | in-process executor's pending window, with the compile-time invariant `MAX_VIDEO_FRAMES * 2 <= MAX_LOCAL_PENDING` — a clip must never fill the window by itself |
| worker TTL / announce | 45 s / 15 s | registry liveness |
| job stall / no-progress | 90 s / 30 min | requeue vs. fail |
| finished jobs retained | 50 | |

**Requests are keyed on the temp file *name*, not the full path.** A plugin that
canonicalizes its input under a symlinked `TMPDIR` echoes back a different
string for the same file; keying on the name makes directory-level rewriting
harmless. Log an unmatched result rather than dropping it silently.

**Liveness and progress are separate clocks.** A heartbeat proves the process is
alive, not that the job is moving — a tagger's first run legitimately produces
nothing for minutes while it loads a model. Track `progressed_at` separately and
**fail** (not requeue) a job that stops progressing; requeueing hands the same
wedge to the same worker forever. Reap stalled jobs lazily inside announce,
claim, enqueue, status **and heartbeat** — the heartbeat is the only traffic a
wedged job produces.

**The server never receives or executes code.** A job carries a plugin *name*;
an instance only runs manifests installed under its own `data_dir()/plugins`.

**Move the ML taggers out of this repository.** Keep `plugins/example-auto-tagger`
— dependency-free `python3`, and what the verification recipe drives. The three
ML taggers are personal tools on their own release cadence, pinned to model
repositories and CUDA stacks the gallery knows nothing about. They share a
virtualenv that is not in the repo: each manifest runs
`{plugin_dir}/../.venv/bin/python`, a sibling in the *install* root, so they must
be installed into a common parent and something must build that venv from their
`requirements.txt` files.

### 3.11 Remote access

TLS is always on for a non-loopback bind, because browsers gate the async
Clipboard API and friends behind a secure context. Self-signed ECDSA, persisted,
regenerated when the LAN IP changes or expiry nears. The certificate carries
`basicConstraints CA:TRUE` and `keyCertSign` alongside its serverAuth EKU — not
because it signs anything, but because iOS and macOS only offer the full-trust
toggle for CA certificates, and a click-through exception is per-origin and
short-lived. `GET /cert` serves the PEM unauthenticated; it leaks nothing, since
every handshake hands out the same certificate, and it must be reachable
*before* the browser trusts the connection enough to pair.

**SANs behind NAT or Docker.** Interface detection sees only the interface this
process routes through — inside a container that is the bridge address, never
the host address clients dial. Name the reachable address explicitly with
`--tls-san` or `LIGHTVIEW_TLS_SAN`. Getting this wrong fails quietly: desktop
browsers survive on a click-through exception that iOS drops readily.

**Pairing.** A device holds a cookie `lv_device=<device_id>.<secret>`. The server
stores a **SHA-256** hash of the secret — deliberately not argon2: the secret is
32 random bytes, so a slow hash buys nothing against that search space, and
verification runs on *every thumbnail request*. Comparison is length-checked and
constant-time. Enrollment is a short-lived, single-use row: a 6-digit PIN typed
by hand or a 32-byte hex token in a QR code — the PIN is safe because of the
10-minute TTL and single-use redemption, not because six digits are hard to
guess.

Pairings live in the **data dir**, not the gallery, because they are a property
of this machine serving. That removes the per-gallery cookie-name mint that
existed because cookies are scoped by host and not by port.

**A `--remote` instance has no browser, so none of the above reaches it.** It
needs its own credential, stored at `<data_dir>/remote.toml`, mode 0600:

| Field | Purpose |
|---|---|
| `server_url` | where to attach |
| `cookie` | `<name>=<id>.<secret>`, obtained by redeeming a PIN |
| `cert_sha256` | **trust-on-first-use pin of the server's end-entity certificate** — the server is self-signed, so this, not a CA, is what authenticates it |
| `instance_id` | stable uuid minted at pair time; identifies this host in the worker registry |
| `instance_name` | what the web UI shows |
| `poll_secs` | claim interval, default 3 |

`lightview remote-pair --server <url> --pin <n>` redeems the PIN, captures and
prints the certificate fingerprint for confirmation, and writes the file.
**`--pin` is required only on a first pairing.** `remote-pair --server <url>
--trust-new` re-pins the certificate against an existing `remote.toml` and keeps
the cookie, which is the whole point of a re-pin: no new device row, no new
secret, nothing for a human to redeem.

Certificate rotation must produce an error naming `--trust-new` rather than a
bare TLS failure. **Never disable verification as a workaround; the pin is the
whole authentication story on that leg.**

**A DHCP lease change is the sharp edge here.** The certificate regenerates when
the LAN IP changes, which breaks every attached `--remote` instance until someone
runs `--trust-new` — on the deployment the mode exists for, a headless systemd
unit. Two mitigations, both cheap and neither novel: give the server a static
address or a reserved lease, and name every address the clients dial in
`--tls-san` so a change does not re-mint. Say this in the deployment docs rather
than letting each user discover it once.

**Auth is on the hot path.** It runs on every thumbnail request, so it must not
take the writer lock and must not write unconditionally. Read through the
read-only pool; rate-limit any `last_seen` touch.

**Change notification.** The fs-watcher and the tagging queue publish to **one**
broadcast channel, relayed as SSE on `/api/events` with a typed event kind per
message. Late subscribers see only events from subscription onward — a
reconnecting phone should re-fetch state, not replay history.

They are two channels today for one reason: the fs channel's subscriber count
doubled as the "is anyone watching?" signal for the idle worker, and tagging
traffic must not make the server think a user is present. Section 3.5 abolishes
that signal, so the reason is gone and the second channel with it. One channel,
one stream, one concept fewer.

**Uploads** are the one write channel from a device. Once a file lands, the
ordinary fs-watcher ingests it — uploads have no separate indexing path.

### 3.12 Frontend

One SolidJS bundle, one runtime. `lib/ipc.ts` remains the only module that talks
to the backend, but it no longer branches on transport: everything is
`POST /api/invoke` plus the media/thumb routes. Delete `isTauri()`,
`safeListen`, the dual-default capabilities store, and the Tauri dependencies.

Two things `ipc.ts` must keep absorbing so they never leak to callers: a `401`
carrying `WWW-Authenticate: LV-Password` raises a challenge, waits for the modal
and retries — **concurrent 401s share one pending promise**, or a grid firing
twenty requests produces twenty stacked modals; and a `401` *without* that header
means the cookie is missing or revoked, which emits a not-paired event and
redirects to pairing.

**Kept, ported near-verbatim** — this is measured, tuned code and section 6 says
it is not to be reconsidered: `JustifiedGrid`, `MediaViewer`, `VideoPlayer`,
`InfoPanel`, `ThumbnailCell`, `SelectionBar`, `ContextMenu`, `ScrollBar`,
`TopBar`, `FilterBar`, `SortMenu`, `CommandMenu`, `TagManagerPanel`,
`DuplicatesPanel`, `MergeDialog`, `TrashPanel`, `AutoTagPanel`, `UploadSheet`,
`PairApp`, `PasswordModal`, `ConnectionBanner`, and every `lib/` primitive:
`scrollDynamics`, `loadPriority`, `thumbSwap`, `thumbProgress`,
`galleryControls`, `wheelScroll`, `thumbRegeneration`, `justifiedLayout`,
`urlVersions`, `pathIndex`, `thumbQueue`, `fetchLoop`, `cellSources`,
`loadedUrls`, `scrollHost`, `bootSnapshot`, `viewerCache`, `thumbhashPlaceholder`.

**Three components in the keep list need real work, and calling them
"near-verbatim" was wrong.**

- **`DuplicatesPanel`** calls `markNotDuplicates`, which no longer exists. Its
  replacement gesture is *name a set*: a text field with autocomplete over
  existing `set::` tags, writing a batch add across the group. The rest of the
  panel — detection, grouping, thresholds, the merge entry point — is unchanged.
- **`AutoTagPanel`** loses its desktop/web branch (one runtime now) and keeps
  the worker roster, the per-plugin run entries and the job list. It gains
  nothing: plugin-proposed grouping is deferred, so there is no proposal section
  to render.
- **`TrashPanel`** renders an entry id that today is `<epoch_ms>_<seq>` and is
  validated as digits and underscores. Section 3.4's layout makes one timestamp
  directory hold many files, so **an entry id is now `<epoch_ms>/<relative
  path>`** — it contains slashes and takes the per-segment encoding rule from
  section 3.5b. `list_trash` returns `{id, original_path, file_name, deleted_at,
  size}` as before, with `deleted_at` parsed from the leading segment and
  `original_path` being everything after it.

**`lib/ipc.ts` is written fresh, not ported.** Every one of its ~84 call
wrappers targets a command name and argument shape that section 3.2 replaces.
The *components* calling it port near-verbatim; the module underneath them does
not. Note also that `MediaViewer` imports `invoke` from `@tauri-apps/api/core`
directly today, so "`ipc.ts` is the only module that talks to the backend" is a
goal of this rebuild rather than a description of what is being ported.

**Three components in the keep list call the Tauri file dialog** — the gallery
opener in `App.tsx`, the copy/move destination in `ContextMenu.tsx`, and the
plugin install path in `AutoTagPanel.tsx`. A browser cannot return a filesystem
path; the File System Access API yields a handle, not a path, and only in
Chromium. **The replacement is an `Owner`-only directory-listing endpoint behind
a small picker component**, which needs no new dependency and works headless.

That does not contradict the settled decision that local mode is
selection-scoped. That decision is about what a *remote* client may reach and
about not putting a bypass in `path_in_gallery` — a directory listing is
`Owner`, loopback-only, and returns directory names rather than media, which the
local user can already enumerate with any file manager. `rfd` was the
alternative and loses: a new dependency that links GTK or a desktop portal, on a
process that may have no display.

**Deleted:** `GalleryGrid`, `MapView`, `ViewSwitcher`, `gridLayout`, `GifCanvas`,
`DebugOverlay`, `Sparkline`, `DevtoolsApp`, `perfMonitor`, `metricRows`,
`devtools.html`, `WindowResizeGrips`, `TitleBar`, and most of `SettingsMenu`
(1,333 lines → roughly 300: Display, Thumbnails, Default Filter). Merge
`pluginStore`, `taggingStore` and `thumbnailProgressStore` into one activity
store.

**`App.tsx` is rewritten, not ported.** It has no entry in either list because it
is neither: it hosts every panel and imports both `@tauri-apps/api/window` and
the dialog plugin. The panel wiring, the scroll host, the keyboard handling and
the scrollbar indicator builders port; the window controls and the dialog calls
go.

**The rest of `lib/`, decided rather than left out.** Ported: `mediaExts`,
`mediaPlayback`, `openAtBottom`, `clientPrefs`, `swControl` (the recovery-page
flow depends on it), `touch`, `viewerTransition`, `wheel`, `version`, `types`.
Rewritten: `runtime` and `memoryPressure`.

`runtime` needs care rather than deletion. It is named above only as the home of
`isTauri`/`safeListen`, but it also defines **`isMobile()` as `isWeb() && width <
640`**. Delete `isWeb()` and that silently becomes "narrow window", so a desktop
browser at a narrow width takes the mobile path — and the mobile default sizes
cells for two columns. Redefine it deliberately: viewport width plus `hasTouch()`,
which is a capability rather than a guess.

`memoryPressure` polled a backend command that was never in the allowlist, so on
the web it 403'd into an empty catch and the viewer cache's pressure eviction
simply did not exist. One runtime now, so read one signal:
`navigator.deviceMemory`, sampled once, because it is a static device class.

**Grid invariants that must survive the port**, each of which fails silently:

- **Cells are keyed by path, never by index.** An index-keyed list rewrites every
  slot's `src` as the window shifts, which is per-row scroll flicker.
- **Prune per-path state surgically, never wholesale.** The URL-assignment effect
  has already run for that update; wiping surviving cells blanks the grid until
  the next scroll.
- **A cell's URL, its rung, and its in-flight swap are one unit.** Drop the URL
  but keep the rung and the cell is permanently un-evictable; drop the rung but
  leave the swap running and an abandoned image keeps fetching with nothing to
  commit to.
- **Two single-flight slots, not one.** The visible drain re-arms on completion;
  the warm slot deliberately does not, or the background crawl becomes
  continuous on the one bounded pool. Both reset in `finally`.
- **`warping` must not be cleared by `markSettled()`.** `scrollend` fires after
  every programmatic scroll, so mid-scrub it reopens the gate for one frame in
  every sixty — enough to assign a whole window of cells sixty times a second.
- **Speculation is never free.** It lands on the same bounded pool as visible
  cells, so gate it behind "nothing the viewport is waiting on is outstanding",
  and use a smaller batch than the visible drain — nothing preempts a batch once
  issued, so the batch size *is* the worst-case delay.
- **Served-original cells are gone.** The grid uses tiers only; `jh` covers high
  zoom. This removes the 256px quantization bucket and the never-warm rule.

**The file clipboard is ported but its precondition is gone.** The X11 backend
owns the selection on a background thread for the life of the process, and its
own comment notes that a Wayland session without XWayland fails at
`Clipboard::new()` — "fine in practice" only because the host forced
`GDK_BACKEND=x11` for WebKit. That variable is deleted with WebKit, and the host
may now have no display at all. Keep the module, make the failure explicit
rather than a panic, and let the frontend hide the action when the backend
reports it unavailable. It is an `Owner` command, so it is never offered
remotely regardless.

**Client caches**, kept with their bounds: service-worker Cache Storage for
thumbnails (2000 entries FIFO, 1-hour revalidation, **30-day hard ceiling**),
the sorted item list in IndexedDB (same ceiling against its own `savedAt`), and
the in-memory decoded-image cache. `networkFirstShell` serves the cached shell
**only when `navigator.onLine` is false**; network-up-but-origin-dead gets a
recovery page whose *Reset connection* button unregisters the worker and
reloads, so the next navigation is uncontrolled, reaches the network, and the
browser can finally render its certificate prompt — with cookies and Cache
Storage intact so the pairing survives.

The service worker is versioned (`lv-thumbs-${VERSION}`), and its cache branch
for `/thumbhash/*` goes with the route. **Bump the version in the same change**,
or a paired phone serves the old shell across the dark period and never picks up
the new one.

**Mobile defaults.** `thumbnail_size` is a cell size, not a column count, so the
200px that gives a desktop six columns gives a 390px phone one — the most
expensive possible layout. Derive a mobile default from the viewport's **short**
edge (so rotating does not change the answer) sized for two columns, and cap the
render scale so a 3× DPR phone does not push every cell to the top of the
ladder.

**Full-screen panels carry `.safe-panel`** — four `env(safe-area-inset-*)`
paddings on the panel root. Without it a close button renders under the status
bar on a notched phone: visible, tappable-looking, and completely dead. Padding
on the root specifically, so the panel still paints edge to edge.

---

## 4. The port table

**Anything in this table is copied, not reconsidered.** A rebuild's failure mode
is drifting into re-litigating decisions that were already settled correctly.
Reopening one of these needs a written reason, not a feeling that it could be
nicer.

### Ported near-verbatim

| From | Lines | Why it is not to be rewritten |
|---|---|---|
| `filter/` (ast, parser, evaluator) | 1,263 | the query language is the contract; zero `AppState` references |
| `sort/` (sorter, grouper) | 664 | correct, decoupled; the alias-qualification trap is already handled |
| `autocomplete/engine.rs` | 279 | the case-folding and character-count fixes are both bugs already found |
| `geocode/` (mod, countries) | 568 | the 25 km / 100 km ceilings are measured judgement |
| `companion/` (schema, reader, writer, migration) | 587 | the durable wire format; atomic write-and-rename |
| `provider/local.rs`, `util/` | 233 | scan, whole-file reads, data dir, fs-watch wrapper |
| `file_clipboard/` | 198 | self-contained per-platform selection ownership; see section 3.12 for the precondition that changes |
| `plugin/input.rs` | 1,034 | `PartTracker` + staleness rules — a year-old silent hang already fixed |
| `plugin/{runner,manifest,install}.rs` | 833 | subprocess/NDJSON, venv-relative interpreter rewriting |
| `pipeline/video.rs` | 810 | ffmpeg rotation, exact-dimension downscale, timeouts, ISO 6709 |
| `pipeline/{exif,heic_cache}.rs` | 273 | EXIF extraction; a 12-entry transcode LRU keyed on (path, mtime) |
| `pipeline/thumbnailer.rs` — `decode_image`, `fit_dims`, `generate_for_path_fit`, `fit_rgba`, `resize_rgba`, `compute_thumbhash`, WebP encode | ~600 of 1,149 | the one render path |
| `thumb_serve::get_or_generate` **and `cache/coalescer.rs`** | ~200 | enrol-before-recheck ordering, three-attempt bound — and the mechanism that matters more: **the generator slot is an RAII guard.** A dropped request future, which the grid's virtual scrolling causes constantly, must release the slot. The previous explicit-release design leaked the key and presented as "the server stops responding until restart". `cache/` is otherwise written fresh; this file is the exception. |
| `cache/duplicates.rs` dHash | 319 | the hash and the Hamming comparison |
| Frontend: `JustifiedGrid`, `MediaViewer`, `ThumbnailCell`, `ScrollBar`, `ContextMenu`, all `lib/` primitives | ~6,000 | measured, tuned, and untestable by `tsc` |

### Written fresh

| What | Replacing | Why |
|---|---|---|
| `cache/` | 2,516 lines | three tables not seven, no migrations, relative paths, new location |
| `server/` (routes + one command table) | `commands/` 6,557 + `http_server/` 3,800 | one adapter; the `*_impl` convention has nothing left to keep in step |
| `AppState` | `lib.rs` 479 | half its fields are Tauri, GPU, or dual-transport artifacts |
| `tagging/` | `tagging/` 1,446 + worker bin 1,678 | one job loop over two byte sources |
| `cli` | `main.rs` 429 + headless 434 | three modes, one binary |
| Sets in the duplicate finder | `not_duplicates` table | derived from co-membership |
| Trash | `commands/trash.rs` 563 | path-mirrored layout, no metadata file |
| `lib/ipc.ts` | 969 | every wrapper targets a command name and shape that changes |
| `lib/runtime.ts`, `lib/memoryPressure.ts` | 231 | one runtime; `isMobile()` and the pressure signal both need redefining |
| A directory-picker component + its `Owner` endpoint | `tauri-plugin-dialog` | a browser cannot return a filesystem path |

### One correction to this table

An earlier draft asserted that **nothing writes the `auto` tag namespace**. That
is false, and it is corrected here rather than quietly: the duplicate merge
unions `tags.auto` across every copy onto the survivor and writes it, with a
test asserting exactly that. It propagates rather than originates — no command
*creates* an auto tag — so the conclusion survives, but the reasoning offered
for it did not.

Two consequences the implementer needs, which the false claim would have hidden:
that merge test must be deleted along with the union, and **an old companion
carrying `tags.auto` will have those tags silently dropped from the index**,
because the tag-enumeration function will no longer emit them. Decided: drop
them. Folding them into `user::` would silently promote machine output to user
intent, which is the one boundary the companion format exists to keep.

This is recorded because the port table is the part of this plan an implementer
is told not to reconsider. A false fact there is more expensive than anywhere
else in the document.

### Deleted outright, with nothing replacing them

`pipeline/gpu_pipeline.rs` (450) and the `wgpu`/`pollster` deps and `gpu`
feature — unreachable once the square grid goes, since its only call site is
reachable only from a command that is not in the allowlist and whose only caller
is `GalleryGrid`. `gif_serve.rs` (187), `cache/gif_atlas.rs` (103) and
`GifCanvas.tsx` (171) — a workaround for a WebKitGTK animation bug that leaves
with the engine. `views.rs` (164) and the enabled-views setting. `commands/geo.rs`
(268). The `/thumbhash` route, protocol arm and PNG cache — the blob is inlined
into the items payload and decoded client-side; nothing ever fetched the route.
`RenderConfig`. The `hardware/` probes for storage type, filesystem and reflink,
which are logged and displayed and drive nothing; keep core count and RAM.
`decisions/` and the whole convention. Most of `docs/refactor.md`'s subject
matter.

---

## 5. Order of construction

Not a shipping sequence — nothing ships until it all does. A dependency order,
with what each step must produce.

| # | Step | Produces | Done when |
|---|---|---|---|
| 0 | **Branch and clear** | the old tree deleted in the same commit that adds the first new file, **plus a committed placeholder `dist/index.html`** | `cargo check` on an empty skeleton |
| 1 | **Pure modules** | `filter/`, `sort/`, `autocomplete/`, `geocode/`, `companion/`, `util/`, `provider/`, `file_clipboard/` moved across; `auto` removed and `set` added in `TagNamespace` **and its TypeScript mirror**, `#[serde(default)]` on the companion structs, quoted strings in the tokenizer | the ported tests pass **after their `auto::` cases are rewritten to `set::`** — the enum is serialized both directions, so this is a wire change, not only a parser change — plus new tests for quoting, `set::`, and an old sidecar parsing without `set` |
| 2 | **`cache/`** | three tables, relative paths, `format_version`, the path-keyed sweep and its test | a fresh open indexes a gallery; a version bump deletes and rebuilds |
| 3 | **Pipeline** | one render path, three tiers, the coalescer, the byte budget, the idle worker | tier bytes appear for a test gallery at all three edges |
| 4 | **Server + command table** | routes, the two trust levels, path confinement, TLS, pairing, the launch-token session, SSE, upload | `curl` exercises every route; an unauthenticated call is 401; an `Owner` command on a non-loopback bind is 403; **and on a loopback bind, redeeming a launch token and then calling the directory-listing endpoint succeeds** |
| 5 | **CLI** | the three modes | `lightview <dir>` opens a browser; `--serve` binds and pairs |
| 6 | **Frontend** | the ported SPA against the new API | the grid fills in headless Chromium |
| 7 | **Plugins + tagging** | one job loop; `remote.toml` (server url, cookie, `cert_sha256` TOFU pin, instance id and name, poll interval) and the `remote-pair` verb that writes it; then `--remote` | the example tagger completes a job locally, and a second process attached with `--remote` completes one against a self-signed server without disabling verification — **with a clip in the test gallery**, so `?frame=` is exercised and a video's companion gains a merged tag entry rather than being silently skipped |
| 8 | **Docs** | `docs/` rewritten; `_planning/rebuild/` and `refactor.md` deleted | every page describes what exists |

---

## 6. Verification

There is no frontend test harness and there will not be one in this change. The
verification that exists is worth more than it was, because after this the
browser is the *only* runtime.

**Rust tests to carry or write:**

- the path-keyed sweep test — **update it, do not delete it** with the four tier
  tables it currently covers
- filter parse/compile round-trips, including quoted strings and `set::`
- the sort alias qualification (a bare column name must not compile)
- companion round-trip, atomic write, and the read fallback
- geocode ceilings at 25 km and 100 km
- **the duplicate finder's new "never offer a pair that shares a `set::` tag"
  clause** — the one piece of genuinely new logic sitting where a user's
  judgement is stored, cheap to test and expensive to get wrong quietly
- trash: round-trip a nested path, refuse a restore onto an occupied
  destination, purge by age from the directory name alone
- the tier budget: hysteresis at 1.25×, warm seeding, drain-before-evict

**Both binds must be exercised, and the loopback one is the least-reviewed
surface in this document.** Section 6's browser recipe pairs a device and drives
`--serve`, which is `Device` throughout — so on its own it verifies that `Owner`
is *refused* and never that it works. Add a loopback pass: start `lightview
<dir>` with the browser launch suppressed, read the token from stdout, redeem it,
and call a directory listing, a copy into a temp destination, and `purge_trash`.
Without it the launch-token flow, the picker endpoint and the whole `Owner` half
of the trust table ship unverified.

**End-to-end, no display required.** Build the SPA (`npm run build` — `dist/`
is embedded at compile time, so the Rust build fails without it), start the
server on a throwaway gallery, and drive it with `curl`: pair, fetch a thumbnail
at each tier, watch the SSE stream while copying a file in, confirm an
unauthenticated call is 401. Then drive the real SPA in headless Chromium at
`/opt/pw-browsers/chromium` with the device cookie injected, and assert the grid
fills. This is the only way to exercise tier selection, eviction and decode
timing.

**The acceptance test for the whole change** is requirement 9: the finished tree
must be smaller. The baseline is 26,939 lines of Rust and 19,760 of TypeScript,
counted as: every `.rs` file under `src-tauri/src/` including inline
`#[cfg(test)]` modules and excluding `benches/`; every `.ts` and `.tsx` under
`src-solidjs/` excluding `.css`. Count the same way at the end, or the one
falsifiable global criterion is not falsifiable.

`benches/cache_db.rs` and `benches/thumbnailer.rs` reference the old schema and
will fail `cargo clippy --all-targets` from step 2 onward. Rewrite or delete
them in the step that breaks them; do not leave the prescribed lint command
failing.

---

## 7. Decisions already taken

Settled during review. A fresh session should treat these as given and reopen
one only with a written reason.

| Decision | Consequence if reversed |
|---|---|
| Browser-only; no native window | two UIs, or WebKitGTK back |
| Local mode is **selection-scoped**, not filesystem navigation | `path_in_gallery` needs a bypass — the one check between the server and the host filesystem |
| Trust is derived from the bind; no flag widens `Owner` | the security model becomes configuration |
| One grid (justified); no map, no canvas, no virtual folders | the tier collapse unwinds |
| Three tiers, one family, all WebP | four parallel generators come back |
| Scroll tuning is **not** re-measured after WebKitGTK leaves | speculative work on the part users feel most |
| Durable = photos + companions only | `sets.json`, `gallery.json`, and an exemption for trash |
| No migration code, anywhere, except the companion `migrate()` hook | permanent code that runs once |
| Sets are tags; `not_duplicates` is deleted | a table, a sweep exception, and 780 rows per burst |
| Sets are cheap and fluid — renaming rewrites members, trashing shrinks silently | a durable set object with an identity |
| Plugin input rounds **up** to a tier edge | a decode per image per job |
| One binary, three modes; one instance per role | a second binary and a release-skew story |
| A dark period is accepted; no compatibility shim | double the API surface for the duration |
| Rebuild in **this** repository, one branch | history lost for no benefit |
| `decisions/` deleted; reasoning lives in subsystem pages | eleven new records owed by this change alone |
| `AGENTS.md` is the one guidance file; `CLAUDE.md` symlinks to it | the drift that produced two different principle numberings |
| **Every bind authenticates, loopback included**; a one-time token in the launch URL, a per-install cookie name, and an `Origin` check | `open_with` reachable by any local process or any web page the user visits |
| **`tags.set` is a sibling of `tags.user`**, with `#[serde(default)]` on every field | old sidecars fail to parse, or sets get erased by plugin re-runs |
| **`purge_trash` and `merge_duplicates` are `Owner`; `restore_trash` is `Device`** | remote clients get permanent deletion, which requirement 2 forbids |
| **The bundled taggers declare 512, not 1024** | every tagging job generates `jm` per image on the server |
| **Plugin-proposed grouping is deferred and its result kind deleted, not stubbed** | a protocol a plugin can emit into with nothing receiving it — the video-tagging failure shape, and a plugin point with zero implementations |
| **The directory picker is an `Owner` listing endpoint**, not `rfd` | a new GTK/portal dependency on a possibly-headless process |
| **`auto` tags in old sidecars are dropped from the index, and preserved in the file** by a flattened extras map | the next write erases durable data the struct no longer models |
| **`auto` tags in old sidecars are dropped, not folded into `user::`** | machine output silently promoted to user intent |
| **A loopback client holds a process-lifetime session, not a device row**; token redeemed at `/auth/launch`, 60 s, single use | ambient authority on `127.0.0.1`, or a pairing flow where none is wanted |
| **`Origin` on loopback, `Sec-Fetch-Site: same-origin` on `--serve`** | a `0.0.0.0` bind has no fixed origin to name, which is why CORS is `Any` today |
| **Under `--serve`, nothing is `Owner`**; the host is administered by CLI and `server.toml` | a web UI that can move files on the server |
| **One process per gallery**, enforced by an advisory lock on the cache directory | two writers on one `cache.db` behind an in-process mutex |
| **The password is a CLI verb reading stdin**, argon2id, `--serve` only | hand-editing a hash into TOML |
| **Tag-write commands take a `namespace` of `user` or `set`** | a parallel command family for an identical operation |
| **One broadcast channel, not two** | a second channel whose only justification was the abolished subscriber-count signal |
| **The cache budget is enforced only by `lightview cache --prune`**; the LRU key is the cache file's mtime | a gallery discovering its thumbnails were evicted as it opens |

---

## 8. Deliberately not in this plan

Named so their absence reads as a decision rather than an oversight.

- **The map view, the infinite canvas, the virtual folder view.** The first is
  deleted; the other two were designed and unbuilt, and are incompatible with
  having one view. They leave the roadmap.
- **Plugin-proposed grouping** (a `groups` result kind, face clustering). See
  section 3.10 — deferred until a plugin exists that would use it, and deleted
  rather than left as a stub. Naming a group *from a selection* ships, because
  that is just a batch tag write.
- **Plugin findings** (`choice`/`confirm`/`label`, `pending::`). Deferred; no
  plugin needs them.
- **A decode-worker pool / canvas cells.** Would give up the browser's own image
  cache (measured: returning to visited positions costs +2 MB rather than
  +55 MB) in exchange for explicit `ImageBitmap.close()`. Two rationales on two
  platforms, neither measured here. Revisit only with a number.
- **A BK-tree for duplicate detection.** All-pairs is fine at this scale.
- **Per-device scopes.** All paired devices are equally trusted.
- **EXIF writing.** No path exists and acquiring one means rewriting image bytes.

**Open items carried forward** — real, and none of them blocking:

- Move the ML taggers to their own repository (section 3.10).
- A tagging job rebuilds the whole `tag_counts` table every 32 files. Cost scales
  with the *library*, not the batch, under the single writer. Wants a
  measurement on a large library before anything is built.
- Authentication reads under the writer lock in front of a read-only pool. Four
  cheap `SELECT`s, so whether the serialization is measurable depends on queue
  depth on a real phone. Get the number first.
