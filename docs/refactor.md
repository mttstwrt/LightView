# LightView as it is, and what it should become

[← docs index](README.md)

**Status:** proposal, not a decision. Part 1 is a complete inventory of the
system as of this writing; Part 2 argues for a smaller one. Nothing here has
been built. Where a claim is a measurement it says so; where it is a judgement
it says that too.

**Why this exists.** LightView set out to be a fast, minimal photo gallery and
management tool. It is fast. It is not minimal — 26,900 lines of Rust, 19,800
of TypeScript, 5,900 of documentation, three binaries, two UI transports, five
declared views, seven thumbnail tiers, eighty backend commands of which
forty-eight are reachable remotely, and roughly thirty-five places where
describing the system correctly requires the word *except*. Most of those
exceptions are individually defensible; each one is documented and most were
written to fix a real failure. The problem is the count.

The measure this document optimises for is the one that was asked for: **how
many times you have to say "except" to describe the system truthfully.**

---

# Part 1 — The system as it stands

## 1. Shape

| | Lines | Notes |
|---|---|---|
| Rust (`src-tauri/src`) | 26,939 | 3 binaries out of one crate |
| TypeScript/TSX (`src-solidjs`) | 19,760 | one SPA, two runtimes |
| Docs (`docs/`) | 5,858 | 9 subsystem pages, 15 decision records, a 616-line todo |
| Tests | 189 | Rust unit tests; no frontend test harness |
| Plugins (`plugins/`) | 4 | 3 ML taggers + 1 dependency-free example |

Three binaries:

- **`lightview`** — the Tauri 2 desktop app. GTK/WebKitGTK window, SolidJS SPA
  in a webview, `invoke()` IPC to the Rust backend. Ships as `Gallery`
  (`tauri.conf.json` `productName`), which is not the name anything else uses.
- **`lightview-headless`** — the same backend plus the axum HTTP server, no
  webview. Subcommands `serve`, `pair`, `views`.
- **`lightview-worker`** — feature-gated (`worker`). Pairs to a server, claims
  tagging jobs, runs plugins on a capable machine. Subcommands `pair`, `run`,
  `install`, `plugins`, `status`.

Cargo features: `gpu` (default, `wgpu` + `pollster`), `custom-protocol`
(default, Tauri production protocol), `worker`.

## 2. Storage

Everything a gallery knows lives in two places, and the split is by *format*
rather than by lifetime — which is the root of several problems below.

### `<gallery>/.lightview/cache.db`

One SQLite file per gallery, WAL mode, schema version 17 derived from a
17-entry migration list. Described as "a cache you can delete", and it is not:
it holds ratings mirrors, view history, dedup verdicts, device pairings, and
every per-gallery setting that is not a display preference.

Path-keyed tables, all swept together by `path_keyed_tables()`:

- `media_meta` — the index: path, media type, size, `date_taken`, `date_added`,
  `last_viewed`, `last_rated`, rating, dimensions, duration, GPS, colour label
- `tag_index` — `(path, namespace, tag)`, rebuilt from companion files
- `index_state` — companion mtime per path, so re-indexing skips unchanged files
- `gif_atlas` — pre-rendered GIF sprite sheets keyed `(path, tier)`
- seven thumbnail tables, one of which (`thumbnails`) also carries the `phash`
  column duplicate detection reads

Not path-keyed:

- `gallery_meta` — key/value: `schema_version`, `gallery_root`, remote-access
  settings, default filter, upload config, trash retention, enabled views,
  location tagger version, per-gallery cookie id
- `tag_counts` — `(namespace, tag) → count`, feeds autocomplete
- `not_duplicates` — user-confirmed non-duplicate pairs, `path_a < path_b`
- `remote_devices`, `remote_pairing` — device cookie hashes and enrollment codes

**Every path-keyed row stores an absolute path.** Moving the gallery directory
therefore orphans the whole cache, which is why `adopt_gallery_root`,
`rebase_root` and `infer_old_root` exist: compare the root against
`gallery_meta.gallery_root`, rewrite the prefix across every table in one
transaction, and — for caches predating root tracking — infer the old root by
matching on-disk files against cached rows by suffix, requiring all five samples
to agree.

### `<gallery>/.lightview/` — everything else on disk

- `companions/` — `.lightview.json` sidecars, when the gallery's companion
  location is `LightviewFolder`; otherwise they sit alongside the media
- `settings.toml` — display settings, hand-editable, hot-reloaded by the watcher
- `trash/<epoch_ms>_<seq>/` — self-describing trash entries: the media file, up
  to two companion copies (one per original location), and `meta.json` with the
  gallery-relative original path

### `<exe_dir>/data/`

TLS certificate and key, server-side plugins, `worker.toml`, recent galleries,
render config.

### Companion files

`<media>.lightview.json`, the **record of intent**:

```
{ schema_version, file, file_hash, media_type, created, modified,
  tags: { user: [...], auto: [...], plugins: { "<name>": {...} } },
  meta: { core: {...},                plugins: { "<name>": {...} } } }
```

`user` is never overwritten by anything but the user; a plugin writes only under
its own key, so a re-run replaces that plugin's output wholesale. `meta.core`
holds `rating`, `date_rated`, `color_label`, `notes`, `media` (dimensions,
duration, codec) and `location` (decimal degrees WGS-84, optional altitude).
`rating` and `color_label` are **mirrored** into `media_meta` columns, because
filtering runs in SQLite and never opens a sidecar.

Two locations (`Alongside`, `LightviewFolder`) with a read fallback to the other
and no write fallback. Writes are atomic: temp file in the target directory,
renamed into place. `schema_version` is stamped on write, checked on read, and
`migrate` is called unconditionally — currently the identity function.

## 3. Opening a gallery

`open_gallery` is the one command with real orchestration, and its ordering is
load-bearing:

1. Construct `LocalProvider`, canonicalize the root once
2. Open `cache.db`, migrate, then **rebase stored paths if the directory moved**
   — before the index is populated, or the scan inserts bare rows that shadow
   the relocated history
3. Scan the tree and populate `media_meta`; the grid can render from this alone
4. Backfill EXIF GPS, then reverse-geocode and write place names into companion
   files — before the index pass, so they land in the same sweep
5. Re-index companions whose mtime changed, rebuild `tag_counts`, load
   autocomplete
6. Open the read-only connection pool, start the fs watcher, start the idle
   backfill worker

## 4. Thumbnails

Seven cached resolutions in two families:

| Tier | Seg | Target | Shape | Format | Bounded |
|---|---|---|---|---|---|
| Micro | `s` | 128 | square crop | JPEG | no |
| Standard | `m` | 512 | square crop | JPEG | no |
| Large | `l` | 1024 | square crop | WebP | no |
| Preview | `p` | 1600 | square crop | WebP | no |
| Justified | `j` | 512 | fit | WebP | no |
| JustifiedMid | `jm` | 1280 | fit | WebP | **LRU** |
| JustifiedHigh | `jh` | 2560 | fit | WebP | **LRU** |

A fit tier cannot be derived from a square one — the crop threw the pixels away
— so the two families are independent decode paths. `Standard` is the hub: the
only tier with a `resize_filter` column, the only one carrying the ThumbHash
blob, the only one carrying `phash`, and the only one other tiers derive from.

**Four generation entry points**, all funnelling onto one bounded rayon pool
(`thumb_pool`, sized from the hardware profile): `get_thumbnails_batch` (desktop
IPC, optionally GPU), `precache_thumbnails_impl` (speculative),
`ensure_tier_thumbnails_impl` (any non-Standard tier, batched), and
`generate_and_store_tier` (one path, one tier, from the serve path on a miss,
coalesced). `regenerate_thumbnail_impl` is a fifth, maintenance-only.

**Two serving transports, one implementation.** `lightview://thumb/<tier>/<path>`
on the desktop and `GET /thumb/{tier}/{*rel}` on the web both call
`thumb_serve::get_or_generate`, which reads through a read-only connection pool
and on a miss elects one generator while others wait on a `Notify`, bounded to
three attempts. Micro short-circuits: if the Standard row exists, derive the
128px from those bytes rather than decoding the original.

**Disk budget.** Only `jm` and `jh` are bounded: 10% of free disk, floored at
512 MiB, capped at 8 GiB, per tier, overridable with `LIGHTVIEW_TIER_BUDGET_MB`.
Eviction is byte-budgeted with a window function, keyed on an `accessed_at`
column, with 1.25× hysteresis. Access marks are buffered in
`AppState::pending_tier_accesses` because the read path holds a read-only
connection, and drained immediately before an eviction pass.

**Idle backfill.** `pipeline/idle.rs` grinds the backlog when no SSE subscriber
is connected and no user thumbnail request landed in 60 s, newest-first, warming
whichever tiers the gallery's *enabled views* ask for (`views::prewarm_tiers`,
re-read each poll). It also computes perceptual hashes.

**Sources.** CPU decode/resize via `image` + `fast_image_resize`; an optional
fused crop+resize `wgpu` path; `ffmpeg`/`ffprobe` for video frames, with the
display-matrix rotation applied and every invocation timeout-bounded; `libheif`
for HEIC with a 12-entry in-memory transcode cache; EXIF via `kamadak-exif`;
ThumbHash placeholders; GIF frame atlases as PNG sprite sheets.

## 5. Query

**Filter language** — tokenizer → parser → AST → SQL `WHERE` fragment plus bound
parameters, evaluated entirely in SQLite over `media_meta` with correlated
`EXISTS` subqueries for tag terms.

```
vacation                    bare word, any namespace
user::vacation              namespaced
plugin.face::person:alice   plugin namespace
Japan  Kyoto                place names, from geocoded GPS
NOT auto::indoor            negation; AND / OR / parentheses
rating>=4                   rating comparison
type:video                  media type
has::user     has:geo       namespace non-empty / has coordinates
color:red     color:none    colour label, or its absence
date>=2024-01-01  date=2024 dates: taken / added / viewed
width>=1920  size>=10mb     dimensions, file size
```

Plus a `GeoBbox` term used only by the map view. A single colon is part of a tag
value; namespaces use `::` so `character:` and `rating:general` stay tag values.
**The tokenizer splits on whitespace and has no quoting**, so a tag containing a
space cannot be named by any query — it fails outright rather than matching
poorly.

**Sort and group.** `get_sorted_items` selects from `media_meta` with a
`LEFT JOIN thumbnails` purely to inline the ~25-byte ThumbHash into the payload;
every column is `m.`-qualified because the join brings ambiguous names into
scope. A filtered list is bound as one JSON parameter and expanded with
`json_each` so one prepared statement serves every filter size. Grouping is a
separate in-memory pass over the already-sorted list, comparing `(year, month,
day)` triples and formatting a label once per group.

**Autocomplete.** Every unique tag held in memory (~300 KB at 5,000 tags) with
its lowercase form precomputed, refreshed from `tag_counts` at gallery open and
after any tag write. Four-tier scoring — exact, prefix, substring, subsequence —
deduplicated across namespaces, with namespace suggestions under a sentinel.

## 6. Geocoding

`reverse_geocoder` (GeoNames cities1000, ~144,000 places, 7.9 MB embedded,
lazily built behind a `OnceLock`). Nearest-neighbour in a k-d tree, with two
ceilings: past 25 km the city tag is dropped, past 100 km nothing is emitted.
Country and region are dependable; the city tag is best read as "the nearest
named place" — measured against twelve landmarks, country was right 12/12,
region 11/12, city 4/11.

Names are written into companion files as `tags.plugins["location"]`, spaces
underscored, so `Japan` works as a bare filter word and `grep -rl Kyoto` finds
it without LightView. `TAGGER_VERSION` in `gallery_meta` forces one re-resolve
pass when the output would change, and is stamped only if every write succeeded.

## 7. Duplicates

A 64-bit dHash computed from the *cached Standard thumbnail* — already decoded,
already in the database, so hashing a gallery costs no source decodes. Stored in
a `phash` column on `thumbnails`, so it dies with the thumbnail it describes.
`find_duplicates(threshold)` is an all-pairs Hamming comparison, quadratic in
the number of hashed files.

Rejected pairs go into `not_duplicates` as `(path_a, path_b)` with
`path_a < path_b`. That table is deliberately outside `path_keyed_tables()`
because its paths are not in a `path` column, so every sweep handles it
separately.

**Merge** folds several copies' metadata onto one survivor and trashes the rest:
user tags union (editable), auto/plugin tags union (automatic), rating / colour
/ notes / companion location per-field picks, file mtime stamped with
`filetime`, and embedded EXIF GPS promotable into the keeper's companion. Image
bytes are never rewritten — LightView has no EXIF write path, and GPS is the one
exception precisely because it can be captured without touching them.

## 8. Trash and file operations

**Trash** is app-managed, not the OS trash, so a remote client can delete and
restore. Each entry is a self-describing directory under
`<gallery>/.lightview/trash/`; nothing about it lives in the cache, so entries
survive a cache rebuild and are portable across mounts. Auto-purge on gallery
open, retention in `gallery_meta`, default 30 days.

**File operations** are desktop-only and absent from the remote allowlist:
`copy_files`, `move_files` (reporting in-gallery moves as old→new pairs so cells
re-key, and out-of-gallery moves as removals), `copy_files_to_clipboard` (a
self-contained X11 module), and `open_with`.

## 9. Plugins

A plugin is a directory with a `manifest.json`: `name`, `display_name`,
`version`, `api_version`, `execution`, `capabilities`, `tag_prefix`, optional
`input`, optional `ui`.

The host spawns it as a subprocess and speaks NDJSON:

- request `{"action":"tag","path":"/abs/path.jpg"}`
- result `{"path":"...","tags":[...],"meta":{...}}` or `{"path":"...","error":"..."}`

`api_version` **selects behaviour**: version 0 (absent) gets original paths and
the whole request list up front; version 1 gets stills pre-scaled to
`input.max_edge` and videos already split into `input.video_frames` still
requests, and must consume incrementally. A version 0 plugin is refused by
`lightview-worker` (where reading stdin to EOF deadlocks) and allowed on the
desktop. `LIGHTVIEW_JOB_TOTAL` replaces the read-to-EOF sizing pattern.

`plugin/input.rs` is the single implementation of what a plugin receives, shared
by all three drivers, and it merges a video's per-frame results back into one:
a union of tag sets, plus a redone argmax for `rating:` because a rating is one
choice rather than a set.

What the protocol *cannot* express, and the stubs that imply otherwise: one verb
(`tag`); `tag_prefix` mandatory; `ExecutionConfig::Wasm` present in the enum and
returning `WasmNotSupported`; `capabilities` (`ReadImage`, `NetworkAccess`)
advisory with no sandbox; `ui.settings_schema` parsed and never rendered;
`ui.context_menu_items` where only "tag" resolves. A findings system
(`api_version: 2`, three host-drawn shapes, a `pending::` filter term, two new
tables) is designed in `plugins/findings-and-ui.md` and not built.

## 10. Remote tagging

An in-memory job queue and worker registry in `tagging/`. Web clients enqueue
over `/api/invoke`; a worker claims, runs the plugin locally, and pushes tags
back through `apply_plugin_tags`. The server never receives or executes code — a
job carries a plugin *name*.

Three executors run the same protocol: the desktop's `run_plugin_batch` (which
bypasses the queue entirely and reports through Tauri events), the server's
in-process `tagging/local.rs` (registering as the reserved worker id
`local-server`, rejected in the API dispatch so a device cannot impersonate it),
and `lightview-worker` over HTTP.

Constants: worker TTL 45 s, announce every 15 s, job stall 90 s, no-progress
30 min, last 50 finished jobs retained. Stalled jobs are requeued lazily inside
announce/claim/enqueue/status **and heartbeat** — the heartbeat because a worker
inside a wedged job produces no other traffic. Liveness and progress are
separate clocks on purpose.

The worker keeps at most 64 downloaded files on disk (`Semaphore(64)`), and a
permit returns only when *that request's* result is matched back — keyed on the
temp file **name**, because a plugin canonicalizing under a symlinked `TMPDIR`
echoes back a different string. `PartTracker::take_stale` abandons a request
once the plugin has answered 128 *other* requests since it was sent, or once
both it and the plugin have been idle five minutes. Measured: a 200-image job
with a plugin answering every other request parks at 64/200, reclaims at ~5 min,
and completes 100 tagged / 100 failed in 331 s.

## 11. Remote access

`http_server/` is axum. The desktop additionally runs a *second*, loopback-only
instance with no auth and no static files, purely because WebKitGTK refuses
non-`http(s)` schemes for `<video>`.

Route groups and their layers:

```
/healthz                                       no auth
/cert  /pair/redeem  /auth/password  /auth/status   bootstrap, cannot be behind auth
/media /thumb /gif-atlas /thumbhash
/api/invoke /api/events /api/upload             device cookie
everything else                                 SPA + cache policy
```

The SPA is embedded with `rust-embed` so a deployment is one file;
`--web-root <dir>` overrides it with `ServeDir` for development, and debug
builds read `dist/` off disk either way.

**Auth.** A paired device holds `lv_device_<galleryid>=<device_id>.<secret>`.
The server stores a SHA-256 hash of the secret — deliberately not argon2,
because the secret is 32 random bytes and verification runs on every thumbnail
request. The cookie name carries a per-gallery id because cookies are scoped by
host and **not** by port, so two galleries on one machine shared a jar entry and
pairing with the second un-paired the first. A bare `lv_device` is still
accepted forever, for devices paired before this existed. Enrollment is a
short-lived single-use `remote_pairing` row — a 6-digit PIN or a 32-byte hex
token in a QR code. An optional gallery-wide password layers on top, re-proved
after an inactivity window, answered as `401` with `WWW-Authenticate:
LV-Password`.

**The `/api/invoke` allowlist** is a 48-arm match in `api.rs`. Anything not named
is 403. Host operations (file copy/move, plugin execution, render config) are
simply absent. Delete-shaped commands sit behind an additional per-gallery
`remote.allow_delete` gate checked in match guards. The verbosity is the
security property.

**Path confinement.** Every route resolving a filesystem path calls
`path_in_gallery`, which canonicalizes per request against the root canonicalized
once at open, and returns **404, not 403**. Uploads enforce it three times over:
basename with traversal rejected, extension resolving to a known `MediaType`,
and destination confirmed inside the root before any byte is written.

**TLS.** Always HTTPS remotely, because browsers gate the Clipboard API and
friends behind a secure context. Self-signed ECDSA, persisted at
`<exe_dir>/data/tls/`, regenerated when the LAN IP changes or expiry nears, and
carrying `basicConstraints CA:TRUE` + `keyCertSign` alongside its serverAuth EKU
so iOS and macOS offer the full-trust toggle. `detect_lan_ip()` sees only the
interface this process routes through, so behind NAT or Docker the SANs must be
named explicitly with `--tls-san` or `LIGHTVIEW_TLS_SAN` — and getting it wrong
fails quietly, because desktop browsers survive on a click-through exception
that iOS drops readily.

**Change notification.** The fs-watcher publishes batches to `fs_change_tx`; the
desktop receives Tauri events, a browser receives them as SSE on `/api/events`.
`tagging-job` and `tagging-workers` events are merged in from a *separate*
broadcast, because `fs_change_tx`'s subscriber count doubles as the "is anyone
watching?" signal for the idle worker.

## 12. Frontend

One SolidJS bundle in two runtimes. `lib/ipc.ts` (969 lines) is the only module
that knows which: Tauri `invoke()` or `POST /api/invoke`. It also absorbs
password challenges (concurrent 401s share one pending promise) and unpaired
redirects.

**Views** — five declared, three built: `GalleryGrid` (fixed square cells, 1025
lines), `JustifiedGrid` (aspect-preserving rows, 1128 lines), `MapView` (273
lines, leaflet, behind a `lazy()` split at 153 kB of a 445 kB bundle). Two
unbuilt: an infinite canvas and a virtual folder hierarchy. Per-gallery
enablement comes from `views.rs` and drives both what the switcher offers and
which tiers the idle worker warms.

**The grid machine** — both grids run the same seven parts: a virtual range
recomputed per frame but signalled only on change; two nested asymmetric windows
(outer cheap-tier look-ahead, inner full resolution); a resolution ladder that
holds new cells on the cheap rung during flings; 404-driven generation as a
*recovery* path (measured: 880 thumbnail responses over a cold 1200-image
gallery, zero 404s); drain-time prioritization against the *current* window; two
single-flight slots (`inFlightFetch` re-arms on completion, `inFlightWarm`
deliberately does not); and eviction that shrinks the DOM but cannot free the
browser's own image cache.

Fourteen extracted primitives in `lib/`: `scrollDynamics`, `loadPriority`,
`thumbSwap`, `thumbProgress`, `galleryControls`, `wheelScroll`,
`thumbRegeneration`, `justifiedLayout`, `gridLayout`, `urlVersions`,
`pathIndex`, `thumbQueue`, `fetchLoop`, `cellSources`, `loadedUrls`.

**Measured costs** (Chromium, 390×664 at DPR 3, 5,000 items): loading and
dropping 1200 thumbnails costs ~28 MB at `s`, ~104 MB at `m`, ~409 MB at `l` —
four times per rung. Removing `will-change: transform` *doubled* growth
(630 MB → 1232 MB). Scrollbar handling cut a sixty-landing burst from +254 MB to
+186 MB and removed the largest tier from the gesture entirely (281 `l` loads →
0). The scrub gate cut three full-gallery scrubs from 4442 requests / +528 MB to
88 / +124 MB.

**Chrome** — one command list rendered two ways (desktop dropdown where the gear
was, mobile FAB where the upload button was): upload, auto-tagging, manage tags,
find duplicates, trash, open a folder, settings. `SettingsMenu` (1333 lines)
keeps only configuration in seven source-ordered sections: Display, Views,
Thumbnails, Remote Access, Connection, Default Filter, Storage.

**Panels** — `TagManagerPanel`, `DuplicatesPanel`, `MergeDialog`, `TrashPanel`,
`AutoTagPanel` (branching once on `isWeb()`: installed plugins on the desktop,
worker roster and job list on the web), `UploadSheet`, `InfoPanel`,
`ContextMenu`, `SelectionBar`, `ScrollBar` with date/name/size/rating markers,
`ConnectionBanner`, `PairApp`, `PasswordModal`, `WindowResizeGrips`, `TitleBar`.

**Viewer** — `MediaViewer` (1553 lines) with keyboard navigation, ratings, tags,
info panel, `VideoPlayer`, `GifCanvas` playing atlases on a canvas, an in-memory
decoded-image cache evicted under memory pressure, and a viewer transition that
hands off from the grid cell.

**Diagnostics** — a second HTML entry point (`devtools.html`), `DebugOverlay`,
`Sparkline`, `DevtoolsApp`, `perfMonitor`, `metricRows`: ~865 lines. The second
entry is why Rollup's shared chunk is named after `Sparkline` and why
`Sparkline-*.css` is actually the whole Tailwind stylesheet — a documented
non-issue that exists only because of it.

**Client caches** — three, each of which can make a dead server look alive: the
service worker's `lv-thumbs-*` Cache Storage (2000 entries FIFO, 1 h
revalidation, 30-day hard ceiling), the whole sorted item list in IndexedDB
(same ceiling, honoured against `savedAt`), and the in-memory viewer cache.
`networkFirstShell` serves the cached shell only when `navigator.onLine` is
false; network-up-but-origin-dead gets a synthetic recovery page whose **Reset
connection** button unregisters the worker so the next navigation can reach the
network and the browser can finally render its certificate prompt.

## 13. The exception ledger

Every one of these is documented and most were written to fix a real failure.
That is the point: individually justified, collectively the thing the refactor
is against.

| # | Except… |
|---|---|
| 1 | …two transports for the same commands, and the desktop uses a custom protocol for thumbnails but HTTP for media |
| 2 | …the desktop runs a second HTTP server purely for `<video>` |
| 3 | …there are 80 backend commands but 48 remote ones, and the difference is implicit |
| 4 | …`not_duplicates` is outside `path_keyed_tables()` and every sweep handles it separately |
| 5 | …there are two thumbnail families, and a fit tier cannot derive from a square one |
| 6 | …only `jm` and `jh` are LRU-bounded |
| 7 | …Micro derives from Standard; no other tier has a fast path |
| 8 | …the viewer's `p` tier is square-cropped, so the code crops back to the right shape |
| 9 | …`api_version: 0` is refused by the worker and allowed on the desktop |
| 10 | …desktop plugin runs bypass the job queue and have their own toast and cancel path |
| 11 | …`local-server` is a reserved worker id rejected in exactly one place |
| 12 | …settings live in four places: `settings.toml`, `gallery_meta`, `RenderConfig`, and browser local storage |
| 13 | …every host setting is a Tauri command, so a headless deployment cannot reach any of them |
| 14 | …the cookie name carries a per-gallery id, and a bare `lv_device` is accepted forever |
| 15 | …companion reads fall back to the other location and writes do not |
| 16 | …a tag containing a space cannot be filtered at all, and autocomplete offers it anyway |
| 17 | …colour labels are settable, filterable, invisible in the justified grid, and unsortable |
| 18 | …`rating:x` is a tag and `rating>=x` is a comparison; `::` is a namespace and `:` is not |
| 19 | …`reindex_gallery` does not regenerate thumbnails |
| 20 | …the tier route coalesces and the `?fit=` route does not — removed rather than fixed, by dropping the grid's use of `?fit=` (change 9) |
| 21 | …`auth_layer` takes the writer lock in front of the read-only pool that exists to avoid it |
| 22 | …the service worker serves the cached shell only when genuinely offline, unless `?lv_offline=1` |
| 23 | …two persisted client caches have a 30-day ceiling, and the boot snapshot is never a source of truth |
| 24 | …grid cells are keyed by path and pruned surgically, never wholesale |
| 25 | …the drain slot re-arms on completion and the warm slot deliberately does not |
| 26 | …`warping` must not be cleared by `markSettled()`, because `scrollend` fires mid-scrub |
| 27 | …paths are absolute, so moving a gallery needs `rebase_root` and `infer_old_root` |
| 28 | …`dist/` must exist before any `cargo` command, including the library's |
| 29 | …`cargo fmt --check` fails on ~70 files, so the advertised gate cannot pass and `clippy --fix` is a trap |
| 30 | …`lightview.desktop` names a binary the build does not produce (`productName` is `Gallery`) |
| 31 | …`cache.db` is "a cache you can delete", except it holds pairings, verdicts and settings |
| 32 | …behind NAT or Docker the certificate SANs must be named by hand or everything fails quietly |
| 33 | …speculation shares the one bounded pool with visible cells, so it is gated on "nothing outstanding" |
| 34 | …`.safe-panel` sets all four paddings and silently overrides `p-*`, so two dialogs opt out |
| 35 | …a tagging job rebuilds the whole `tag_counts` table every 32 files |

---

# Part 2 — What it should become

Seven changes, ordered so each is independently shippable and the tree works
after every one. Together they remove roughly 13,000 lines and about half the
ledger above.

## Change 1 — One runtime: the SPA in a browser, the backend always an HTTP server

**The decision that unblocks everything else.** Delete Tauri. `lightview <dir>`
starts the same axum server bound to loopback on an ephemeral port and opens the
system browser at it; `lightview --serve <dir>` binds `0.0.0.0` with TLS and
pairing. One binary, one transport, one frontend.

**Why not Iced.** Iced gives a native window, real GPU rendering, and — the
genuinely attractive part — explicit control over decoded-image memory, which is
the one lever `grid-loading.md` says the web client does not have. But
`--serve` requires a web client regardless, so Iced means building and
maintaining **two complete user interfaces forever**. That is the exact opposite
of the stated goal, and it is a much larger commitment than the WebKitGTK
problem justifies. Going browser-only solves the same problem — WebKitGTK is
gone — at negative cost.

**What browser-only actually costs, checked rather than assumed.** Every
privileged operation already lives in the Rust backend and is reached over IPC.
The backend stays a native process, so `copy_files`, `move_files`,
`copy_files_to_clipboard` (X11), `open_with` and trash all keep working exactly
as they do — the *frontend* becomes a browser, not the backend. The only genuine
loss is the native folder-picker dialog, and local mode already has exploratory
filesystem access by design, so a served directory chooser replaces it in about
150 lines and removes `tauri-plugin-dialog`. The custom `lightview://` protocol
was already a second path to code the HTTP route shares.

**Trust becomes a property of the bind, not of the build.** One command table,
each entry carrying the minimum trust it requires — and there are two levels,
not three:

| Level | Reachable from | Covers |
|---|---|---|
| `Device` | any paired client | browse, metadata writes, trash, upload, enqueue tagging |
| `Owner` | loopback bind only | copy, move, clipboard, open-with, gallery open, plugin install |

The bootstrap routes — `/healthz`, `/cert`, `/pair/redeem`, `/auth/*` — are
unauthenticated by necessity, since there would otherwise be no way past the auth
layer the first time. That is a route group, not a trust level: no *command* is
ever reachable unauthenticated, so giving it a name in the same table would
imply a third tier of command that does not exist.

That table *is* the answer to "local mode has filesystem controls, serve mode has
only move to trash." It is one list you can read top to bottom, rather than 80
commands in one place, 48 in another, and the difference held in your head. The
rule that keeps it honest is a single one: **`Owner` is granted by a loopback
bind and by nothing else — there is no flag that widens it.**

**Local mode is selection-scoped, and that keeps path confinement universal.**
`Owner` covers the operations that already exist — copy, move, clipboard,
open-with, applied to a selection — and not filesystem navigation. The
distinction matters more than it sounds: `path_in_gallery` stays on **every**
route with no exception, because every path a command *reads* is still a
gallery member. What `Owner` widens is the *destination* of a copy or move,
which was never confined and never can be. Sources confined always, destinations
confined never, and one trust level deciding who may name a destination at all.

A file-manager-shaped local mode would have broken that. It would need paths
outside the root to be readable, which means `path_in_gallery` gains a bypass —
and a bypass on the one check standing between the server and the host
filesystem is the last place to want a conditional. Ruling it out is what lets
the confinement rule stay a rule.

One consequence for the folder picker: the only two places that name a directory
are opening a gallery and choosing a copy/move destination. Both are "pick a
directory", both are `Owner`, so they are one served component rather than the
two the native dialog was doing.

Removes ledger items 1, 2, 3, 9, 10, 13. Deletes `main.rs` (429), the Tauri
command registry, the `*_impl` wrapper convention, `initMediaServer`, every
`isTauri()` branch, `safeListen`, the dual-default `capabilitiesStore`, the
Tauri-events-vs-SSE duality, and the `@tauri-apps/*` dependencies.

**The grid's scroll tuning is deliberately left alone.** `frontend/README.md`
says outright that WebKitGTK's main-thread image decode is the premise behind
the decode gate, the staged tier upgrade and most `isTauri()` branches, and that
no harness here can measure that platform. Dropping the engine therefore removes
the *justification* for machinery that has never been measured — but the
scroller works, it is the part a user feels most directly, and a re-measurement
pass would be speculative work on a component that is not currently a problem.
Decided: ship the move, and revisit only if scrolling is worse afterwards. The
`isTauri()` branches themselves still go, since there is no Tauri to branch on;
what stays is the tuning they were guarding.

## Change 2 — One view, three tiers

Delete `GalleryGrid` (1025), `MapView` (273), `ViewSwitcher` (113),
`gridLayout.ts`, `commands/geo.rs` (268), the `GeoBbox` filter term, `views.rs`
(164), the `views` subcommand, the Views settings section, and `leaflet`.

**Keep geocoding.** Place *names* as ordinary tags are the valuable half, they
cost a lazily-built k-d tree, and they work in the filter bar with no map and no
network. The map view was 153 kB of leaflet plus online tiles for a feature that
is not used; the tags stay. `has:geo` stays; `GeoBbox` goes with the view that
was its only caller.

**The square tiers cannot simply be deleted** — `m` carries the ThumbHash blob
and the `phash` column, `s` backs the tag panel, `p` backs the viewer. Move the
derived data onto the fit family and the whole square family goes:

| After | Target | Shape | Role |
|---|---|---|---|
| `j` | 512 | fit | the hub: grid cells, ThumbHash, `phash`, GIF atlas, tag panel |
| `jm` | 1280 | fit | zoomed cells, and the viewer's progressive underlay |
| `jh` | 2560 | fit | high zoom |

Seven tables become three. Four fewer entries in every path-keyed sweep, one
family instead of two, and the "a fit tier cannot be derived from a square one"
paragraph disappears along with the second decode path. The viewer's underlay
gets *better*, because `jm` already has the right aspect ratio — ledger item 8
is not worked around, it stops existing. Existing perceptual hashes must be
recomputed against `j` bytes, which is free: they are regenerable by definition,
and the dHash downsamples to 9×8 regardless.

`prewarm_tiers` collapses to "warm `j`". Removes ledger items 5, 8, and the
enablement half of 12.

### One pipeline, and what that actually means

The tier collapse is worth more than four fewer tables, and the reason is not
visible from the tier list. There are two *generator families* in
`pipeline/thumbnailer.rs` today, and they are not symmetric:

- The **square** family is four parallel implementations. `generate_for_path`
  dispatches to `generate_image_thumbnail` — itself a three-way split into
  `generate_jpeg_thumbnail`, `generate_heic_thumbnail` and
  `generate_generic_thumbnail` — or to `generate_video_thumbnail`. Each does its
  own decode, crop, resize and encode.
- The **fit** family is one function. `generate_for_path_fit` calls
  `decode_image`, which pushes the source-type dispatch *below* the pipeline, and
  everything after that — `fit_dims`, `resize_rgba`, encode — is single-copy.
  `fit_rgba` is its tail, factored out for callers that already hold pixels.

So deleting the square tiers deletes **four parallel implementations of the same
operation**, and what is left is:

```
decode_image(path, edge)  →  fit_dims + resize_rgba  →  encode WebP
  dispatch on format           one implementation        one encoder
```

**The `?fit=` route was never a second pipeline.** Change 9 below treats it as
one and is wrong to: `http_server/routes.rs`, `plugin/input.rs` and both tier
paths in `commands/media.rs` all call `generate_for_path_fit` already, at
`ThumbFormat::Webp`, differing only in the edge they ask for and whether the
result is stored. The right move is therefore not to drop the route but to put
the cache-and-coalesce wrapper around the one function, so **a tier is just a
cached edge** and the route inherits coalescing rather than needing a second one.
B1 is then solved by construction instead of dropped.

**One encoder, and B5 stops existing.** All three surviving tiers and the
`?fit=` route are WebP today. With no square family there is no JPEG output, so
`rgba_to_rgb`, `encode_rgb_to_jpeg` and `encode_rgba_to_jpeg` go — and B5, which
is that two of four `(format, source layout)` combinations convert a whole
buffer before resizing rather than after, has no combinations left to be
inconsistent about. A todo removed by deletion rather than by measurement.

**The derivation helpers go.** `store_derived_extras`, `derive_standard_extras`,
`derive_micro_from_standard` and `derive_micro_for_cached` all exist to keep
Micro and ThumbHash in step with Standard, under a transaction invariant that
"a Standard row implies a ThumbHash blob and a Micro row". With Micro gone and
the ThumbHash computed during `j` generation, that is a side output of one
function rather than four helpers and an invariant to uphold.

**What stays branched, and why none of it is a second failure mode.** Four
decoders inside `decode_image` — JPEG with scale-on-decode, HEIC via `libheif`,
video via `ffmpeg`, everything else via the `image` crate. Different container
formats genuinely need different decoders; this is dispatch that converges on
RGBA immediately, not duplication. `fit_rgba` is the shared tail for the one
caller holding its own pixels — a video frame at a chosen timestamp, which no
path-based entry can express. The HEIC transcode cache is an optimization in
front of one decoder, worth keeping at ~500 ms a decode, and it is a cache
rather than a path.

**The invariant to state once and defend:** every cached thumbnail is
`generate_for_path_fit(path, edge)` at one of three edges. Anything that makes
that untrue — a tier derived from a larger tier instead of decoded, a second
encoder, a GPU fast path — is a new failure mode and has to be argued for rather
than slipped in as an optimization.

The obvious candidate is worth refusing in advance: deriving `jm` from a cached
`jh`. The Micro-from-Standard fast path it would imitate was worth its branch
because it saved a 16× decode on the rung the grid hammers hardest. `jm` from
`jh` saves 4×, on a tier that is LRU-bounded and rarely cold, in exchange for a
second way for a thumbnail to be wrong. Decode from source, always.

## Change 3 — Split storage by lifetime, not by format

This is the answer to "process a folder of two hundred images and never open it
again, or be the stable place for thousands." Today one SQLite file holds both,
inside the user's photo folder, and nothing ever cleans it up.

**Durable → the photos and their companion files. Nothing else.**

That is the whole rule, and it is stronger than the first draft of this section,
which also wanted a `gallery.json` and a `sets.json`. Neither survives contact
with the rule: sets become tags in the companions (Change 4), and everything
`gallery.json` was going to hold is either a deleted setting (companion
location), server configuration (below), or cache-keying detail that should not
be user data at all. `trash/` stays under `.lightview/` because a trashed file
*is* a photo and its companion — see below for why it needs no metadata file
either.

So a gallery on disk is: the media, the sidecars, and a trash folder. Delete
everything else and re-open it, and you are back — slower, having re-thumbnailed,
but with nothing lost. That is the property, and it is worth more than any
individual thing that could have been stored alongside.

**Derived, disposable, machine-local → `<data_dir>/galleries/<hash-of-root>/cache.db`**

Everything regenerable: `media_meta`, `tag_index`, `tag_counts`, `index_state`,
`gif_atlas`, the three thumbnail tables, `phash`. Three consequences, all of
them things currently missing:

1. **The photo folder stops accumulating an opaque multi-hundred-megabyte
   blob.** A throwaway gallery leaves a few kilobytes behind.
2. **The derived caches are all in one place, so they can have a budget.** One
   total-size ceiling with least-recently-opened eviction across galleries, and
   a `lightview cache` subcommand to show and prune it. That is the direct answer
   to the transient-versus-durable question, and it is not expressible today
   because the caches are scattered across the filesystem.
3. **Paths become gallery-relative**, because the root is no longer implied by
   the file's location. `rebase_root`, `infer_old_root` and structural
   observation F1 are deleted outright.

A read-only gallery also improves: derived data goes to the local data dir, and
only the durable half degrades, rather than nothing working at all.

**Where the rule costs something, stated so it is a choice.** Keying the cache
by a hash of the canonical root means **moving a gallery re-thumbnails it**.
Today `rebase_root` preserves the cache across a move, and an id in a
`.lightview/gallery.json` would too, for about two lines. It is not in the design
because an id is not something the user needs — it exists only to save the
machine work — and the rule earns its power by having no exceptions. If moving
large galleries turns out to be a real habit rather than a rare event, adding
that file later is a small, additive change; building it now on the guess is
what the rule is against.

### Trash needs no exemption

`meta.json` looked like the one thing in a gallery that is neither a photo nor a
companion: restore has to know where a file came from. Mirroring the
gallery-relative path *inside* the trash removes it — a file at
`2026/january/photo.jpeg` is trashed to `trash/…/2026/january/photo.jpeg`, and
the path is the provenance.

Two things stop a bare mirror from working, and one segment fixes both:

**Deletion time has nowhere to live.** Purge needs `deleted_at`. The file's
mtime cannot carry it — a rename preserves mtime, and preserving it is the
point: the duplicate merge deliberately stamps a keeper's mtime, and restoring a
file with a rewritten one would be silent data loss. `ctime` is neither portable
nor exposed reliably.

**Relative paths are not unique over time.** Trash `2026/january/photo.jpeg`,
restore it, edit it, trash it again, and the second copy overwrites the first.
Today's `<epoch_ms>_<seq>` directory guarantees uniqueness; a bare mirror gives
that up.

So: **`trash/<epoch_ms>/<gallery-relative path>`.** The timestamp segment is the
deletion time and the uniqueness key; everything after it is the original path.

That does better than merely replacing `meta.json`:

- **Purge gets cheaper, not just simpler.** `purge_entries` currently reads a
  `meta.json` per entry to learn `deleted_at`. With the timestamp in the
  directory name it is one `read_dir`, a numeric parse, a compare, and
  `remove_dir_all` — no file reads at all. `list_trash` loses the same per-entry
  read.
- **One delete is one directory**, so "undo that delete" becomes a natural unit.
  Restoring a whole operation is restoring a directory, where today the caller
  has to remember which entry ids belonged together.
- **It is browsable.** `ls .lightview/trash/` shows what was deleted and when,
  with no application involved — the same property the companion files have, for
  the same reason.
- **The two companion slots go.** An entry currently carries both
  `companion.lightview_folder.json` and `companion.alongside.json` because it
  cannot know which location the original used. Inside a mirrored trash the
  companion sits alongside the media uniformly, and restore writes it to the one
  current write location. `COMPANION_ENTRIES` and its loop disappear.

What carries over untouched: refusing a restore when something already occupies
the destination, `create_dir_all` for a parent directory that no longer exists,
and `.lightview` already being skipped by the media scan, the companion indexer
and the fs-watcher.

Two costs, both small and worth naming. Restoring leaves empty directories
behind (`trash/<ts>/2026/january/`), so restore prunes upward to the timestamp
directory — a few lines. And the prefix adds roughly thirty-five characters to
every path, which matters only in a gallery already close to the system limit.

**The rule therefore holds with no amendment:** everything durable in a gallery
is a photo, a companion, or a path.

### No migration code

The database is now *purely* derived, and that unlocks the thing a versioned
schema was protecting: **delete it and rebuild instead of migrating it.**

`cache.db` gets a single format integer. If it does not match the build's,
`fs::remove_file` and re-index. That deletes the seventeen-entry `MIGRATIONS`
list, `run_migrations`, the `const fn` deriving `SCHEMA_VERSION` from it, the
"strictly increasing versions" and "idempotent re-run" tests that guard it, the
warning about a database stamped ahead of this build, and
[decision 0003](decisions/0003-derive-schema-version-from-migrations.md) along
with the failure it records. Every future schema change becomes a bump of one
integer and no thought at all about what an older database looks like.

This is only safe *because* of the split above. A file holding device pairings,
dedup verdicts and per-gallery settings cannot be deleted on a version mismatch;
a file holding only thumbnails and indexes can. The two changes are one change.

**The companion file keeps its version, and this is the exception that proves
the rule.** A sidecar is a wire format other LightView installations read and
write; it holds intent that exists nowhere else, and deleting a user's tags
because a number did not match is not a trade anyone would take. `schema_version`
stays stamped on write and checked on read, and `migration::migrate` stays the
one function a version 2 would change. It is currently the identity function and
about twenty lines — cheap insurance on the only data that matters.

The cost of all this is re-thumbnailing on a format bump, which is exactly the
cost the derived cache exists to be able to pay.

**Server configuration → a TOML file in the data dir**, read at startup and on
change: bind address, port, TLS SANs, password hash and inactivity window,
upload enable and scheme, remote-delete flag. Device pairings move there too —
they are a property of *this machine serving*, not of the gallery, which is why
the per-gallery cookie-id mint exists at all.

**Configuration is a file; commands are actions.** That one rule deletes ten
Tauri commands, the Remote Access and Connection settings sections (~20 signals,
the largest surviving chunk of `SettingsMenu`), and todo item E2 — a headless
deployment configures itself by editing a file, which is what a headless
deployment expects. Pairing stays a command (`lightview pair` prints a PIN)
because minting a code is an action, not a setting.

Removes ledger items 12, 13, 27, 31, and most of 14 — and turns 31 from "removed"
into *true*: `cache.db` becomes a cache you can genuinely delete.

## Change 4 — A set is a tag

The complaint is exact: pairwise negative facts are the wrong shape. They are
quadratic in a group, invisible in the UI, keyed on absolute paths, and cannot
express "these three belong together."

An earlier draft answered that with a new durable record in a new file,
`sets.json`. The rule in Change 3 — *the photos and their companions are the
only durable data* — rules that out, and forcing the answer through the rule
produces a much smaller one. **Set membership is a tag.** A `set` namespace
alongside `user` and `plugin.<name>`, one tag per member:

```
set::vacation-burst-3      a burst that is not forty duplicates
set::kellys-comic          a work that exists as several images
set::alice                 a face cluster, once a person has named it
```

That is the entire data model. No new file, no new table, no new wire format, no
new filter syntax — `set::kellys-comic` and `has::set` are the two shapes the
language already has for every other tag. The tag index, `tag_counts`,
autocomplete, grouping and the `/api/invoke` tag-write commands all apply
unchanged, and the whole thing is reconstructable from companions because it *is*
companion content.

**"Not a duplicate" stops being stored at all.** It becomes a derived fact: two
files that share any `set::` tag are never offered as a duplicate pair. One
`EXISTS` clause in the duplicate finder replaces the `not_duplicates` table, its
`path_a < path_b` canonicalization, and its standing exception from
`path_keyed_tables()` — the table is not relocated, it is *deleted*, and ledger
item 4 goes with it. Forty burst frames cost forty tag rows instead of 780
pairwise ones, and the user sees a name rather than a list of negations.

**Order comes from the gallery's own sort.** A comic strip's pages are
`page01.jpg`, `page02.jpg`; the sort that already orders the grid orders the set.
Storing an explicit ordinal per member would mean a second thing to keep in step
with the filename, for a case the filename already answers. If a set genuinely
needs an order its filenames do not carry, that is the moment to add an ordinal —
not before.

**One kind, and no `source` field.** An earlier draft had three kinds
(`variants`, `related`, `cluster`) and then two fields; both were wrong for the
same reason. A confirmed *variants* group does not persist — the merge trashes
the extras, so one file survives and there is no set left. An unconfirmed one is
a *candidate*, recomputed from perceptual hashes on demand exactly as today. What
remains is one relation: these belong together. Whether a person, a plugin, or a
timestamp heuristic proposed it changes nothing about what is stored, and a
plugin that wants its proposal attributed already has `meta.plugins[<name>]`.

Two things fall out for free:

- **Filtering and grouping**, with no new code: `set::kellys-comic` narrows,
  `has::set` finds everything grouped, and `compute_groups` can already group by
  a tag.
- **Stacking.** A set is a collapsible unit in the grid — the burst of forty
  frames renders as one cell you can expand. That is the feature the concept was
  worth building for anyway, and it needs the set to be queryable, which it now
  is by construction.

The duplicates panel becomes "these look alike; are they the same file, or a
set?" — merge, or name a set — rather than a screen with its own vocabulary.

**No migration code**, here or anywhere. Existing `not_duplicates` rows are not
translated. The tier collapse in Change 2 changes what perceptual hashes are
computed from, so a translation would have to reason about which old verdicts
still describe pairs the new hash groups together — logic that runs once and is
then dead weight forever. Re-marking a handful of duplicates is cheaper than
owning that code, and the same reasoning is what deletes the schema migrations
in Change 3.

Note the one thing this gives up against the `sets.json` design: a set cannot
name a file that has no companion. In practice every file LightView knows about
acquires one the moment anything is said about it, and being in a set is saying
something about it — so the companion is created, exactly as adding a tag
already does.

**One scaling note, not a recommendation:** detection is all-pairs Hamming,
quadratic in hashed files. At ten thousand images that is fifty million
comparisons and fine. At a hundred thousand it is five billion and will not be.
Leave it until it hurts; the fix (a BK-tree, or banding on hash prefixes) is
contained and wants a measurement first.

## Change 5 — Plugins: one executor, one queue, one new output kind

**Three executors become one.** Once the desktop *is* the server (Change 1),
`run_plugin_batch` and `tagging/local.rs` are the same code, and there is no
reason for a local run to bypass the queue. Every plugin run goes through the
job queue: one code path, one progress display, one cancel. `cancel_plugin_batch`,
the Tauri plugin toast, and the duplicate progress store all go. The remote
executor stays as the second one — its file-window machinery is measured and
correct — but Change 8 folds it into the same binary and, with it, into the same
job loop, so "three executors become one" is literal rather than approximate.

**Delete the stubs.** `ExecutionConfig::Wasm`, advisory `capabilities`, and
`ui.context_menu_items` promise things that do not exist. A stub that errors is
worse than an honest absence. `api_version` keeps only version 1 — a version 0
plugin is refused everywhere rather than refused in one place and allowed in
another.

**Add exactly one output kind: `groups`.** This is the answer to "something more
complex like facial clustering." A plugin emits proposed groupings of paths; the
user confirms and names one; naming it writes `set::alice` on every member. That
is the whole feature, and after Change 4 it needs **no new storage at all** —
the confirmation is a batch tag write, which `add_user_tag_batch` already is.

What `findings-and-ui.md` deferred as "genuinely new state — a `plugin_groups`
table, a merge and rename surface, and an answer to what happens when a re-run
reshapes a cluster the user already named" mostly dissolves. Merging two clusters
is renaming a tag; splitting one is retagging a selection; a re-run that reshapes
a cluster cannot disturb the confirmed name, because that name lives in
`tags.user`-class storage and the plugin's own bucket is what gets replaced. The
one genuinely new piece is where an *unconfirmed* proposal lives while it waits
for an answer, and that is scaffolding in the derived cache — regenerable by
re-running the plugin, which is exactly the `not_duplicates` precedent applied
to something that deserves it.

**Plugin input is quantized up to a cached tier edge.** A plugin declares the
longest edge it wants; the host serves the smallest tier that is at least that
big — up to 512 gets `j`, up to 1280 gets `jm`, up to 2560 gets `jh`, and
anything larger decodes from source. Round **up**, never down: a model handed a
smaller image than it trained on has lost information it cannot recover, while
one handed a larger image downsizes internally, which is what it does with any
input anyway. The bundled taggers declare 1024 and so get `jm`; a 448-pixel
model gets `j`.

The payoff is the whole reason to do it. Today every tagging job pays one full
source decode per image, on both the local and the remote path — that is what
`input.max_edge` bought, and it only avoided decoding a 60-megapixel original at
full size rather than avoiding the decode. Quantized to tier edges, **a job over
a warmed gallery does no decoding at all**: the idle worker has already produced
`j`, and `jm` and `jh` are one request each through the cached, coalesced tier
route. On the N100 that this feature exists for, that is the difference between
a job that costs hours of host CPU and one that costs none.

It also collapses a route. The plugin host stops asking for `/media?fit=<edge>`
and starts asking for `/thumb/<tier>/<path>` — the same URL a browser asks for,
through the same cache and the same coalescer. `?fit=` survives only for edges
above the top tier, which no plugin has asked for.

Videos are the exception, and an irreducible one: a frame at a chosen timestamp
is not a tier and never will be, so `?frame=i&frames=n` still decodes. Clips are
a small fraction of a library and sample five frames each.

**Defer findings.** The `choice`/`confirm`/`label` shapes, `pending::`, and the
two extra tables are a good design for a plugin that does not exist yet. `sets`
covers both motivating cases that do (duplicates, clustering). Build the shapes
when a third case demands them, not before — which is the same argument the
plugins page makes about general renderers, applied to itself.

**Move the ML taggers out** (todo A6). Keep `example-auto-tagger`, which is
load-bearing for the headless test recipe.

Removes ledger items 9, 10, 11.

## Change 6 — Frontend cleanup that follows from 1 and 2

- Delete the diagnostics subsystem — `DebugOverlay`, `Sparkline`, `DevtoolsApp`,
  `perfMonitor`, `metricRows`, `devtools.html`, `get_perf_snapshot` (~1,000
  lines across both sides). Removing the second HTML entry also removes the
  chunk-naming confusion that has its own documented section. Checked: the
  overlay was largely non-functional already, so this is a deletion rather than
  a trade — the headless Playwright harness is the measurement surface, and it
  is the more trustworthy one.
- `SettingsMenu` 1333 → roughly 300: Display, Thumbnails, Default Filter.
- Merge `pluginStore`, `taggingStore` and `thumbnailProgressStore` into one
  activity store, now that there is one execution path.
- `memoryPressure.ts` reads one signal instead of branching on runtime.
- Keep every `lib/` primitive. They are earned, measured, and are what a future
  view would inherit.

## Change 7 — Close the honesty gaps in the same pass

Small, but each is an existing feature that lies:

- **`cargo fmt` the tree** (E1), in its own commit, so the advertised gate is
  real and `clippy --fix` stops being a trap.
- **Quoted strings in the filter tokenizer** (C4) — autocomplete currently
  offers tags that return a 500 when clicked.
- **Colour labels in the justified grid, and a `color` sort field** (C1), or
  remove the feature. A label you can set, search for, and not see is worse than
  no label.
- **Rename the binary** — `productName: "Gallery"` versus a `.desktop` file that
  execs `lightview` (E3).
- **`reindex_gallery` regenerates thumbnails** (B2).

B1 — the missing coalescer on `?fit=` — is deliberately not on this list. See
"the served-original path" in Change 9: dropping the grid's use of that route
removes the need for the coalescer rather than fixing it.

Removes ledger items 16, 17, 19, 29, 30.

## Change 8 — One binary, three roles

`lightview-worker` exists for one reason: the server is an N100 that cannot run
ML models, so a desktop with a GPU runs them against the server's gallery. That
requirement is real. A second binary is not the only way to meet it.

**The rule, stated once:** an instance offers whatever plugins are installed on
it to whatever gallery it is attached to — its own, or a remote one. Three modes
fall out, and any machine can be any combination:

| Invocation | Role |
|---|---|
| `lightview <dir>` | serve `<dir>` on loopback and open a browser at it |
| `lightview --serve <dir>` | serve `<dir>` on `0.0.0.0` with TLS and pairing |
| `lightview --remote <url>` | attach to a remote instance and offer this machine's plugins to it |

The third is today's `lightview-worker run`, and the pairing it needs is the
*same* device pairing a browser needs: the worker already redeems a PIN at
`/pair/redeem` and stores the resulting cookie. Under the two trust levels from
Change 1 a plugin host is an ordinary `Device` — it writes tags and claims jobs,
and it cannot touch the filesystem. No new trust level, no new enrollment flow.

**One instance per role.** A machine that both serves its own gallery and hosts
plugins for a remote one runs two processes, each with its own config, and that
is the constraint rather than a limitation to work around. The alternative — one
process holding a list of attachments — means a config file with a repeated
section, a lifecycle where one attachment failing must not take down the server,
and a log where two unrelated jobs interleave. Two processes get all of that from
the operating system for free. It also keeps `--remote` and `--serve` mutually
exclusive, which is one fewer combination to reason about.

**`--remote` must not open a browser**, and this is worth stating because the
natural reading of "launch the app as a client" is that it should. Attaching a
GPU machine to a NAS is a long-running background job — a systemd unit on a
headless desktop, grinding for hours. Coupling it to a foreground process with a
browser window would take that away for nothing, because the viewer role needs
no binary at all: browsing a remote gallery is a browser pointed at its URL, and
the server already serves the SPA. Keep the mode single-purpose.

### What is actually deleted, and what only moves

The honest accounting, because the headline number and the line count disagree.

**Deleted.** `bin/lightview-worker/main.rs` (539) — its subcommands become modes
on the one binary, and `install` / `plugins` are already thin wrappers over the
`plugin::install` code the desktop commands share. `bin/lightview-worker/config.rs`
(65) — `worker.toml` folds into the single server config file Change 3
introduces. The `worker` cargo feature and its `required-features` bin
declaration. The separate release artifact, and with it the premise of
[decision 0014](decisions/0014-ship-the-worker-with-the-release.md): "ship the
worker with the release" exists because a separately-built worker can be months
stale against its server, and one binary cannot be stale against itself.

**Moved, not deleted.** `http.rs` (424) and `job.rs` (650) are the claim loop,
the bounded download window, `PartTracker` and the staleness rules. That is the
actual work and it relocates into the crate rather than evaporating — but it
lands next to `tagging/local.rs` (380), and the two are already most of the way
to being one thing: both drive `plan_parts`, `InputPolicy`, `PartTracker` and
`MergedItem` from `plugin::input`, and differ only in **how bytes are obtained**
(an HTTP fetch versus a local read) and **where tags go** (`apply_plugin_tags`
over HTTP versus `apply_plugin_tags_impl` directly). One job loop parameterized
on a byte source and a result sink replaces both, which is the same shape
`plugin::input` already uses for preparing input.

So roughly 2,050 lines of executor code become roughly 1,000, and three binaries
become one. The line saving is modest; the concept saving is the point.

**The cost, stated plainly.** `reqwest` becomes an unconditional dependency
rather than a feature-gated one, so every build carries an HTTP client it may
not use. Against a binary that already links `axum`, `rustls` and `hyper`, the
marginal cost is small — but it is a real cost and it is the price of the
feature going away.

**One thing gets slightly worse, and it is worth naming.** `PluginInfo.api_version`
and the reported worker binary version exist to answer "what is that machine
actually running?", which is how a rebuilt worker next to a year-old plugin copy
survived as a configuration. One binary removes half that question — there is no
separate worker version to skew — but a **stale plugin install** is still
possible on any machine, so the `api_version` in the announce keeps earning its
place. Keep it; drop only the binary-version field.

## Change 9 — Dead weight, and what falls out of the changes above

A second pass over the tree, looking specifically for code that is already
unreachable or about to become so. Most of this is not a decision — it is
bookkeeping that changes 1 and 2 create and someone has to actually do.

### Already dead, regardless of anything else

**The `/thumbhash` route and the `lightview://thumbhash/` protocol arm serve
nobody.** The ThumbHash blob is inlined into the sorted-items payload — that is
the entire reason `get_sorted_items` carries a `LEFT JOIN thumbnails` — and the
frontend decodes it client-side in `lib/thumbhashPlaceholder.ts`. Nothing in
`lib/ipc.ts` builds a thumbhash URL; there is no `thumbhashUrl` to build one
with. The service worker even has a cache branch for `/thumbhash/*`, matching
requests that are never made. Delete the route, the protocol arm,
`AppState::thumbhash_png_cache`, `ThumbhashOutcome`, and the service-worker
branch.

**`storage.companion_location` is a setting that does nothing.** It has a radio
control in `SettingsMenu`, a field in `AppSettings`, a `CompanionLocation` type
in `lib/types.ts`, and a line in the persisted `settings.toml`. Every write path
in the Rust tree reaches disk through `modify_companion` → `write_companion` →
`write_companion_at(…, CompanionLocation::default())`. The parameterized forms
exist and are never passed anything but the default, so **changing the setting
moves no file.** Delete the setting and the control; keep the read fallback,
which costs nothing and means a gallery holding `Alongside` sidecars from an
older build keeps resolving them with no migration pass — the same reasoning as
the dedup verdicts.

**`tags.auto` is a namespace nothing writes.** It is defined in the companion
schema, unioned by the duplicate merge, and parseable as `auto::` in the filter
— but no command sets it. Plugins write `tags.plugins.<name>`; the user writes
`tags.user`. It is a third of the tag model carrying nothing. Worth confirming
against a real gallery before deleting the read path, since a companion written
by an older build could still hold entries; deleting the *namespace* from the
filter and the merge is safe either way.

### Falls out of Change 2, for free

**The GPU pipeline becomes unreachable.** `state.gpu_pipeline` has exactly one
call site, in `generate_thumbnails_batch_impl`. That is reachable only from
`get_thumbnails_batch`, which is a Tauri command **absent from the remote
allowlist**, whose only caller in the frontend is `GalleryGrid.tsx`. Delete the
square grid and the whole chain is dead code: `pipeline/gpu_pipeline.rs` (450
lines), the `wgpu` and `pollster` dependencies, the `gpu` cargo feature, and the
GPU probe in `hardware/`.

That is a stronger argument than the one worth making on its own terms — there
is no measurement anywhere in this repository showing the GPU path beats
`fast_image_resize`'s SIMD path, and it accelerates one of four generation
entry points rather than the serve path, the tier warm, or the idle worker. But
the reachability argument needs no measurement at all.

**Four generation entry points become two.** `get_thumbnails_batch` and
`precache_thumbnails` are called only from `GalleryGrid` and from one
maintenance button in `SettingsMenu`; `JustifiedGrid` uses
`ensure_tier_thumbnails` exclusively. With one grid and one tier family, what is
left is `ensure_tier_thumbnails` (batch warm) and `generate_and_store_tier`
(the serve path's miss), and the maintenance button calls the former.

**Most of `hardware/` stops earning its keep.** `storage_type`, `filesystem` and
`supports_reflink` are probed at startup, logged once, and displayed in the
debug panel Change 6 deletes. They drive no decision anywhere. `cpu_cores` sizes
the thumbnail pool and `total_ram_mb` feeds the memory-pressure signal; the GPU
probe goes with the pipeline above. What remains is two numbers, which is a
function rather than a subsystem.

### Falls out of Change 1, for free

**The GIF atlas exists solely to work around a WebKitGTK bug**, and both module
doc comments say so: WebKitGTK 2.52 animates `<img>` GIFs several times too fast
and leaks a decoded copy of every frame on each loop. The workaround is a
backend-rendered PNG sprite sheet played on a canvas — `gif_serve.rs` (187),
`cache/gif_atlas.rs` (103), `GifCanvas.tsx` (171), a cache table, an HTTP route,
a tier parameter, and a display setting. Every other browser plays a GIF from an
`<img>` correctly. Change 1 removes the engine; this goes with it.

Worth stating the risk plainly, because it is the one item here that is not pure
subtraction: the atlas also happens to give bounded, explicitly-closed memory per
animated file, which an `<img>` does not. If animated GIFs turn out to be a
memory problem on a phone, the answer is the same one Change 6 leaves open for
thumbnails — `createImageBitmap` and an explicit `close()` — and not a
resurrected sprite-sheet pipeline.

**`RenderConfig` stops existing.** `GDK_BACKEND`, `WEBKIT_DISABLE_DMABUF_RENDERER`
and the GPU-acceleration override are process-level settings that describe
WebKitGTK, stored in their own file in the data directory because they cannot
take effect after GTK init. That is one of the four configuration homes Change 3
counts, and it empties itself.

### Two that are decisions, not consequences

**Drop the served-original path in the justified grid.** At mid and high zoom a
native-format still can bypass the tier *cache* for a backend resize of the
original, `GET /media?fit=<px>`. The frontend documentation calls it "the one
thing that is genuinely different", and it carries its own machinery: a 256px
quantization bucket to keep the URL cache-stable, and an explicit rule that such
cells are never warmed ahead of the viewport.

The reason it is never warmed is the reason to drop it: each one is a full
source decode inside the request, measured in seconds, landing on the same
bounded pool as the visible cells — so a look-ahead cannot win the race. `jh` at
2560px is cached, warmed, and bounded. Give up a little sharpness at maximum
zoom and the grid has one way to get pixels rather than two.

Note what this is *not*, since an earlier draft of this section had it wrong:
`?fit=` is not a second pipeline. It calls the same `generate_for_path_fit` the
tiers do (see "One pipeline" under Change 2), so what is being dropped is a
second *caching policy* in the grid, not a second implementation. The route
itself stays, for the plugin download path, and gains coalescing by going
through the same wrapper the tiers use — which is why B1 is not on Change 7's
list.

**Cut the display knobs.** `AppSettings.display` carries fifteen, six of them
about autoplay alone — `video_hover_preview`, `video_autoplay_loop`,
`gif_autoplay_grid`, `video_autoplay_grid`, `video_autoplay_max_seconds`,
`video_autoplay_viewer` — plus `scroll_blur`, `start_at_bottom`,
`justified_high_detail`, `mobile_filter_sheet`, and `map_dark_mode`, which dies
with the map. `performance.thumbnail_threads` duplicates a value `hardware/`
already detects better.

This is the repository's own first principle applied to itself: *do not add
configuration options that were not asked for; every knob is a permanent
maintenance surface and a combinatorial test case.* Two settings — "animate in
the grid" and "autoplay in the viewer" — cover what the six do, and the detected
thread count should simply win.

## What this adds up to

| | Now | After | Removed |
|---|---|---|---|
| Rust | 26,900 | ~18,500 | Tauri host, GPU pipeline, GIF atlas, geo commands, `views.rs`, 4 tier tables, settings commands, one executor |
| TypeScript | 19,800 | ~13,000 | square grid, map, view switcher, `GifCanvas`, diagnostics, dual-runtime branching, most of settings |
| Binaries | 3 | **1** | `lightview` + `lightview-headless` merge; the worker becomes `--remote` |
| Views | 5 declared / 3 built | 1 | |
| Thumbnail tiers | 7 in 2 families | 3 in 1 | |
| Command surfaces | 80 + 48, implicitly related | 1 table, 2 trust levels | |
| Config homes | 4 | 2 (gallery file, server file) | `RenderConfig` empties itself with WebKitGTK |
| Ledger entries | ~35 | ~13 | items 4 and 31 now removed outright rather than mitigated |

The fifteen that survive are the ones that are *inherent* rather than
accumulated: speculation shares one bounded pool; grid cells must be keyed by
path; the scrub gate must not be cleared by `scrollend`; a self-signed
certificate behind NAT needs its SANs named; an offline-capable web client has
caches that can lie. Those are properties of the problem. The other twenty are
properties of the history.

## How this gets built

**This is a rebuild, not a refactor**, in **this repository**, and the decision is
worth stating first because it changes what every section above means in
practice.

The nine changes rewrite roughly 60% of the tree. At that ratio "incremental"
stops buying what it usually buys: each step would have to negotiate with the
shape it is replacing — keeping the `*_impl` split alive through the tier
collapse, threading seven tiers through a storage move that only wants three,
carrying `AppState`'s twenty-five fields through a change that dissolves half of
them. Every constraint that would justify paying that cost has been removed:
there is one user, no continuity requirement, and no need for intermediate
commits to be runnable.

Same repository, one branch, the old tree deleted in the same commit that adds
the new one. Git carries the history, which is all "record keeping" needed.

### There is no data migration, and probably no script either

The durable format does not change. A companion file written by the current
build is readable by the new one: the schema is the same, `set::` is a namespace
old files simply have none of, and `auto` is a namespace nothing ever wrote.
Everything else on disk is derived and gets rebuilt on first open.

So the old binary keeps working on the old deployment for as long as the rebuild
takes. There is no cutover, no downtime, and no window where a gallery is
unreadable — the two builds read the same photos and the same sidecars. The only
thing deliberately abandoned is the `not_duplicates` table, which was already
decided.

### What ports, and what is written fresh

The existing architecture already draws the line, and it draws it in the right
place. Seven modules — `filter/`, `sort/`, `autocomplete/`, `geocode/`,
`companion/`, `provider/`, `util/` — total 3,594 lines and contain **zero**
references to `AppState`. They are ordinary libraries taking a connection or a
struct, and none of the nine changes touches their subject matter.

| Ported near-verbatim | Why |
|---|---|
| `filter/`, `sort/`, `autocomplete/`, `companion/`, `geocode/` (~3,400) | correct, decoupled, and unaffected by every change here |
| `plugin/input.rs`, `runner.rs`, `manifest.rs`, `install.rs` (~1,900) | `PartTracker` and the staleness rules are a year-old silent bug already found; do not re-find it |
| `pipeline/video.rs`, `exif.rs`, `heic_cache.rs`, `decode_image`, `fit_dims` (~1,200) | ffmpeg rotation handling and HEIC decode are knowledge, not code |
| `thumb_serve::get_or_generate`'s coalescer | the enrol-before-recheck ordering is subtle and was arrived at by failure |
| The justified grid, the viewer, and the fourteen `lib/` primitives (~6,000 TS) | measured, tuned, and untestable by `tsc` — the riskiest thing to retype |

| Written fresh | Why |
|---|---|
| `cache/` | three tables, no migrations, relative paths, a new location |
| `commands/` + `http_server/` → one dispatch | the `*_impl` split exists only to keep two adapters in step |
| `AppState` | half its fields are Tauri, GPU, or dual-transport artifacts |
| `tagging/` + the worker | one job loop over two byte sources |
| The CLI | three modes replacing three binaries |

The pattern is not a coincidence: **the layer that ports is exactly the layer
that already knew nothing about its callers.** A rebuild is cheap here because
the architecture was right about where to put its seams, even where it was wrong
about what sat on top of them.

### The honest risk

A rebuild has no natural stopping point. Incremental work is bounded by each
step; a rewrite can drift into re-litigating decisions that were already settled
correctly — the justified grid's zoom hysteresis, the tier budget's warm
seeding, the scrub gate. The mitigation is the port table above: **anything in
the left column is copied, not reconsidered.** Reopening one of those needs a
reason written down, not a feeling that it could be nicer.

The second risk is that "rebuild" becomes licence to add. Every change in this
document subtracts. The finished tree should be smaller than 34,000 lines, and
if it is not, something was added that nobody asked for.

### Order of construction

Not a shipping sequence — nothing ships until it all does — but a dependency
order:

1. **The pure modules**, moved across unchanged. They compile against nothing.
2. **`cache/`**, since everything above it needs its shape: three tables,
   relative paths, a format integer instead of a migration list.
3. **The pipeline**, ported around the one `generate_for_path_fit` path.
4. **The server and its one command table**, with trust derived from the bind.
5. **The frontend**, ported: justified grid, viewer, primitives, minus the square
   grid, the map, the diagnostics, and the dual-runtime branches.
6. **Plugins and tagging**, one job loop, then `--remote`.
7. **The docs**, rewritten against what exists.

## The decision records go

`decisions/` is being deleted rather than extended, and the reasoning belongs
here because it is the same reasoning as everything else in this document.

The format has a failure mode it cannot avoid: append-only means monotonic
growth, and "a reversal is a new file that supersedes the old" means the truth
about any one topic is spread across however many files happen to touch it, in
an order the reader has to reconstruct. Fifteen files today, eleven more implied
by this refactor, and the answer to "why is the cache where it is?" would live in
two of them. That is baggage in exactly the sense the rest of this document is
against.

What the records hold that is worth keeping is the *why* — and the right home
for that is the subsystem page describing the thing, as prose, next to what it
explains. "This was tried and rejected because X" reads better beside the code
it constrains than in a numbered annex, and it stops the reader having to know
that an annex exists. Git holds the rest; a decision's history is its commit.

So: fold the surviving reasoning into the subsystem pages, delete the directory
and the convention, and drop the "add a decision file" clause from the
engineering principles. The eleven records this refactor would otherwise have
demanded are never written, which is a saving on top of the fifteen deleted.

**Done in `CLAUDE.md` ahead of the rebuild**, since the rule was live and would
otherwise have applied to the rebuild's own commits: the `decisions/` entry is
out of the docs layout, the "Decisions." paragraph is replaced by *reasoning
lives beside what it constrains*, and the update rule now says to record a choice
in the subsystem page it affects.

`AGENTS.md` turned out never to have carried the rule, and the two guidance
files had drifted badly — different principle counts, different numbering (so
each file's internal "principle N" cross-references pointed at the other's wrong
rule), a `docs/_planning/` convention in one and a decision log in the other.

**Resolved rather than deferred**, since a rebuild guided by two contradictory
documents is worse than one guided by either. `AGENTS.md` is now the single file
and `CLAUDE.md` is a symlink to it, so there is no second copy that *can* drift.
Principle 1 was rewritten from "plan before you build" to **work out the
architecture before you write code**: the plan's first three questions are now
placement (which module, and which way the dependencies point), contract (what
format or invariant changes, and who is on the other side), and cost in concepts
— which asks explicitly whether deleting something would meet the requirement
instead, and requires any new case needing the word *except* to be named in the
plan. Two self-checks came with it: name the second consumer before writing an
abstraction, and treat "this change is hard to place" as the architecture
reporting a bad seam rather than as an obstacle to route around.

## Where this proposal was weakest, and what is left

The first draft listed four. Three are now closed, and saying so plainly matters
more than preserving a balanced-looking list of caveats.

**Giving up a native window is not a loss.** The concern was that a browser tab
has no window chrome, no native menu, and an `--app`-mode launch that differs per
browser. Weighed against the thing it was being traded for, that was the wrong
frame: the native window was *measurably slower than a browser on the same
machine*. WebKitGTK is not a cost being paid for polish; it is a cost being paid
for nothing. Change 1 stops being a trade and becomes a straight improvement, and
the `tao`/`wry` shell floated as a compromise is not needed.

**Where user data lives is settled, and the rule got stricter.** The worry was
that moving the cache would strand galleries and surprise anyone copying a folder
between machines. The answer is not a better migration — it is that **the photos
and their companion files are the only durable data**, full stop. Everything else
is regenerable by definition, so there is nothing to strand. That rule then went
further than the concern did: it deleted `sets.json` and `gallery.json` from the
design, and it is what makes deleting the migration machinery safe. The residual
cost is named in Change 3 — moving a gallery re-thumbnails it — and it is a
deliberate choice rather than an oversight.

**Deleting the diagnostics overlay costs nothing.** The stated risk was losing
the only in-app measurement surface, since several decisions in this codebase
rest on numbers it helped produce. Checked: it was largely non-functional
already. That turns a trade-off into a straight deletion, and the headless
Playwright harness — the more trustworthy of the two — is what remains.

**What is actually left.** One item, and it is smaller than it was.

Sets replace a working feature. `not_duplicates` is small, correct and tested,
and its replacement is new code in the place a user's judgement is stored. Making
a set a *tag* rather than a new record shrinks the exposure considerably — the
storage, indexing, querying and write paths are all machinery that already exists
and is already exercised — but the finder's "never offer a pair that shares a
set" clause is new, and if it is wrong the symptom is a duplicate prompt you
already answered coming back. That is annoying rather than destructive, and
re-marking is the accepted price throughout this document; it is worth a test
that outlives the change.

## What the refactor leaves behind

Three pieces of work that no individual change owns, and that are therefore the
ones most likely to be skipped.

### The decision records — deleted, not rewritten

The first version of this section counted eleven decision files the refactor
would owe: six superseding the ones it reverses (0001, 0002, 0003, 0005, 0008,
0014) and five for choices made here. That debt is cancelled by deleting the
convention instead — see [The decision records go](#the-decision-records-go).
The reasoning worth keeping moves into the subsystem pages; git holds the rest.

What survives as real work is the subsystem READMEs themselves, which describe a
system that will no longer exist. That is not optional and not deferrable:
documentation confidently describing the wrong system is the condition this
refactor is against, in the place a reader trusts most.

### `todo.md` nearly empties, and that is the scorecard

Of the twenty open items, roughly seventeen close as a side effect rather than
by being worked:

| Closed by | Items |
|---|---|
| Change 2 and the pipeline collapse | B1, B3, B5 |
| Change 3 | E2, F1 |
| Change 5 | A4's deferred half, and A7's group case |
| Change 7 | B2, C1, C4, E1, E3 |
| Change 8 | A3 |
| Change 9 | D2 |

**Two are cancelled rather than closed, and that should be explicit.** C2 (the
infinite scrolling canvas) and C3 (the virtual folder view) are designed,
unbuilt, and incompatible with one view. They leave the roadmap; the design notes
stay in the decision history as things considered and dropped.

**Three survive untouched**, and it is worth knowing which: **A6** (move the ML
taggers to their own repository — named in Change 5, still a real task), **A8**
(a tagging job rebuilds the whole `tag_counts` table every 32 files, which no
change here addresses and which still wants a measurement on a large library),
and **B4** (`auth_layer` takes the writer lock in front of the read-only pool,
which also still wants a number before code).

### The tests sit opposite the risk

189 Rust unit tests, no frontend harness. The changes with the least coverage are
the ones where being wrong is least recoverable, and two deserve naming.

`remove_media_rows_clears_every_path_keyed_table` (`cache/db.rs`) guards the
invariant that every path-keyed table is swept together — the failure it prevents
is a multi-megabyte blob keyed to a path that can never be reached again. Four of
those tables are being deleted. That test must be **updated**, not removed with
them.

The duplicate finder's new "never offer a pair that shares a `set::` tag" clause
is the one piece of genuinely new logic sitting where a user's judgement is
stored. It is cheap to test and expensive to get wrong quietly, so it should have
a test that outlives the change.

The grid remains untestable by `tsc` and covered only by driving the real SPA
against the headless server. That does not change here, and after Change 1 it is
the *only* runtime, so the harness is worth more than it was.

## Decisions taken during review

Recorded here because they are the answers that shaped the sections above, and a
reader who disagrees should know they were decided rather than assumed.

- **Local mode is selection-scoped**, not filesystem navigation — which is what
  keeps `path_in_gallery` universal (Change 1).
- **The grid's scroll tuning is not re-measured** after WebKitGTK leaves. Ship
  the move; revisit only if scrolling regresses (Change 1).
- **Plugin input rounds up to a cached tier edge**, so a warmed gallery tags
  without decoding (Change 5).
- **One instance per role.** A machine serving its own gallery and hosting
  plugins for another runs two processes (Change 8).
- **No migration code anywhere**, for the cache schema or for dedup verdicts.
  The companion file keeps its version, because it is the only thing that cannot
  be regenerated (Changes 3 and 4).
- **This is a rebuild in the same repository**, landing as one branch rather
  than nine shippable steps, with a port table deciding what is copied and what
  is written fresh.
- **`decisions/` is deleted**, convention and all; the reasoning folds into the
  subsystem pages. The rule is already out of `CLAUDE.md`.
- **A dark period is accepted.** The backend is not built against the existing
  SPA as a compatibility target; the first working thing is the whole system.
- **Re-marking duplicates and re-thumbnailing are accepted costs**, repeatedly
  and deliberately, in exchange for code that does not carry its own history.
