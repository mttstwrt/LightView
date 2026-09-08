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
| 20 | …the tier route coalesces and the `?fit=` route does not |
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

## Change 3 — Split storage by lifetime, not by format

This is the answer to "process a folder of two hundred images and never open it
again, or be the stable place for thousands." Today one SQLite file holds both,
inside the user's photo folder, and nothing ever cleans it up.

**Durable, portable, small → `<gallery>/.lightview/`**

- companion sidecars, unchanged
- `gallery.json` — a generated gallery **id**, the companion location, the
  default filter, trash retention
- `sets.json` — see Change 4
- `trash/`, unchanged

Kilobytes to low megabytes. Safe to copy, sync, or read with `grep`.

**Derived, disposable, machine-local → `<data_dir>/galleries/<gallery-id>/cache.db`**

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
   the file's location. `rebase_root`, `infer_old_root`, and structural
   observation F1 are deleted outright — and because the cache is keyed by the
   *id* stored in `.lightview/gallery.json`, moving the folder keeps the cache
   anyway. Strictly better than both of today's behaviours.

A read-only gallery also improves: derived data goes to the local data dir, and
only the durable half degrades, rather than nothing working at all.

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

Removes ledger items 12, 13, 27, 31, and most of 14.

## Change 4 — Sets replace pairwise non-duplicates

The complaint is exact: pairwise negative facts are the wrong shape. They are
quadratic in a group, invisible in the UI, keyed on absolute paths, and cannot
express "these three belong together."

Replace `not_duplicates` with one positive concept. A **set** is a small durable
record: an id, a name, an optional `source` naming whatever proposed it, and a
member list keyed by gallery-relative path plus the companion's existing
`file_hash` so a rename is recoverable.

**There is only one kind of set**, and the first draft of this proposal was wrong
to give it three (`variants`, `related`, `cluster`). Check what each would
actually have done. A confirmed *variants* group does not persist — the merge
trashes the extras, so one file survives and there is no set left to store; an
unconfirmed one is a candidate, recomputed from perceptual hashes on demand
exactly as it is today. That leaves "these belong together and are not duplicates
of each other," which is the same record whether a person made it from a rejected
candidate group, a plugin proposed it, or a burst was grouped by time. The
difference between a burst and a face cluster is the **name**, plus who proposed
it — which is a field, not a type. One kind, no enum, no arm per kind anywhere
downstream.

A pair co-occurring in any set is never offered as a duplicate again — which is
exactly what `not_duplicates` does, at one record per *set* rather than per
*pair*, with a name, in a file you can read. It is not in the companions, so
nothing clutters per-image metadata; it is not buried in SQLite, so nothing is
obscured. `<gallery>/.lightview/sets.json`, written with the same atomic
write-and-rename as companions, indexed into the derived cache for query speed
the same way companions already feed `tag_index`.

Two things fall out of it for free:

- **A filter term**, and it needs no new syntax: `set:alice` names one,
  `has::set` matches any — the same two shapes the language already has for
  tags.
- **Stacking.** A set is a collapsible unit in the grid — the burst of forty
  frames renders as one cell you can expand. That is the feature the concept was
  worth building for anyway.

Removes ledger item 4, and the duplicates panel becomes "resolve these candidate
sets" rather than a separate screen with its own vocabulary.

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
worker stays as the second executor, unchanged — it earns its complexity, and
the file-window machinery is measured and correct.

**Delete the stubs.** `ExecutionConfig::Wasm`, advisory `capabilities`, and
`ui.context_menu_items` promise things that do not exist. A stub that errors is
worse than an honest absence. `api_version` keeps only version 1 — a version 0
plugin is refused everywhere rather than refused in one place and allowed in
another.

**Add exactly one output kind: `sets`.** This is the answer to "something more
complex like facial clustering." A face cluster *is* a set — the same record,
the same file, the same UI, the same naming interaction as a confirmed duplicate
group. The plugin emits proposed groupings; the user confirms and names them;
the name becomes an ordinary `tags.user` entry on every member. What
`findings-and-ui.md` deferred as "genuinely new state — a `plugin_groups` table,
a merge and rename surface, and an answer to what happens when a re-run reshapes
a cluster" is machinery Change 4 has to build anyway.

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
  chunk-naming confusion that has its own documented section.
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
- **A coalescer on `?fit=`** (B1), which every remote tagging job now goes
  through.
- **`reindex_gallery` regenerates thumbnails** (B2).

Removes ledger items 16, 17, 19, 20, 29, 30.

## What this adds up to

| | Now | After | Removed |
|---|---|---|---|
| Rust | 26,900 | ~20,000 | Tauri host, geo commands, `views.rs`, 4 tier tables, settings commands, one executor |
| TypeScript | 19,800 | ~13,500 | square grid, map, view switcher, diagnostics, dual-runtime branching, most of settings |
| Binaries | 3 | 2 | `lightview` and `lightview-headless` become one |
| Views | 5 declared / 3 built | 1 | |
| Thumbnail tiers | 7 in 2 families | 3 in 1 | |
| Command surfaces | 80 + 48, implicitly related | 1 table, 2 trust levels | |
| Config homes | 4 | 2 (gallery file, server file) | |
| Ledger entries | ~35 | ~15 | |

The fifteen that survive are the ones that are *inherent* rather than
accumulated: speculation shares one bounded pool; grid cells must be keyed by
path; the scrub gate must not be cleared by `scrollend`; a self-signed
certificate behind NAT needs its SANs named; an offline-capable web client has
caches that can lie. Those are properties of the problem. The other twenty are
properties of the history.

## Sequence

Each step leaves a working tree, and each is worth doing even if the next one
never happens.

1. **Change 7's formatting commit**, while nothing is in flight — it blocks
   clean diffs on everything else.
2. **Delete the map** (part of Change 2). Smallest, self-contained, immediate.
3. **Collapse to one grid and three tiers** (rest of Change 2). Touches the
   schema; do it before the storage move so there is less to move.
4. **Drop Tauri** (Change 1). The largest conceptual win, and the prerequisite
   for the single executor.
5. **Split storage** (Change 3). Deletes `rebase_root`, E2, and the settings
   sprawl.
6. **Sets** (Change 4).
7. **Plugins** (Change 5), then the remaining frontend and honesty items
   (Changes 6 and 7).

## Where this proposal is weakest

Stated so it can be argued with rather than discovered later.

- **Change 1 gives up a native window.** A browser tab is a browser tab: no
  window chrome you control, no native menu, and an `--app`-mode launch that
  behaves differently per browser. If "feels like a desktop app" matters more
  than the code it costs, the honest alternative is a minimal native shell
  (`tao` + `wry`, or Iced hosting a webview) that loads the same loopback URL —
  which keeps one frontend and re-adds only a window. That is the compromise
  worth considering; Iced *replacing* the frontend is not.
- **Change 3 changes where a user's data lives.** Galleries that already carry a
  populated `cache.db` need a one-time migration, and someone who has been
  copying a folder between machines expecting the thumbnails to travel will
  notice. The gallery-id key makes moves work; copies to a second machine will
  re-thumbnail.
- **Change 4 replaces a working feature.** `not_duplicates` is small, correct,
  and tested. Sets are strictly more capable but are new code in the place a
  user's judgement is stored, and getting them wrong loses decisions that cannot
  be recomputed.
- **Deleting the diagnostics overlay removes the only in-app measurement
  surface.** Several decisions in this codebase rest on numbers that overlay
  helped produce. If it goes, the replacement is the headless Playwright harness,
  which is already the more trustworthy of the two — but it is not the same
  thing as watching a live counter while scrolling.
