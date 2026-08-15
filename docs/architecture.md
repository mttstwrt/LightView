# Architecture

[← docs index](README.md)

LightView is two processes that share one address space's worth of trust: a
Rust backend that owns every byte of I/O, and a SolidJS single-page app
rendered in a webview that owns every pixel. The same SPA bundle also runs in
an ordinary browser on the LAN, talking to the same backend over HTTP instead
of over Tauri's IPC. Everything below follows from that.

## The component map

```
                     ┌──────────────────────────────────────────┐
   desktop webview   │  SolidJS SPA  (src-solidjs/)             │   LAN browser
        │            │  stores/ · components/ · lib/            │        │
        │            └──────────────────────────────────────────┘        │
        │  invoke()          │  lib/ipc.ts is the only boundary   │  fetch()
        ▼                    ▼                                   ▼
 ┌───────────────┐                                   ┌────────────────────────┐
 │  commands/    │◀──────── same *_impl functions ───▶│  http_server/          │
 │  (Tauri)      │                                    │  routes · api · auth   │
 └───────────────┘                                   └────────────────────────┘
        │                                                        │
        └──────────────────────────┬─────────────────────────────┘
                                   ▼
        ┌──────────────────────────────────────────────────────────┐
        │  AppState  (lib.rs) — every piece of cross-command state  │
        └──────────────────────────────────────────────────────────┘
             │            │            │            │           │
             ▼            ▼            ▼            ▼           ▼
        ┌────────┐  ┌──────────┐  ┌────────┐  ┌─────────┐  ┌──────────┐
        │ cache/ │  │ pipeline/│  │ query  │  │ tagging/│  │ provider/│
        │ SQLite │  │ thumbs   │  │ filter │  │ plugin/ │  │ local fs │
        └────────┘  └──────────┘  │ sort   │  └─────────┘  └──────────┘
                                  │ auto…  │
                                  └────────┘
                                       │
                                       ▼
                                 ┌────────────┐
                                 │ companion/ │  .lightview sidecars on disk
                                 └────────────┘
```

## The pieces

**[`cache/`](cache/README.md)** owns the per-gallery SQLite database — the
media index, the tag index, the thumbnail blobs, dedup state, and device
pairings. Everything else reads through it. It depends on nothing but
`rusqlite` and the tier definitions it exports.

**[`pipeline/`](pipeline/README.md)** turns a file on disk into thumbnail
bytes: CPU decode and resize, an optional `wgpu` path, `ffmpeg` for video
frames, EXIF extraction, and the idle worker that grinds the backlog when
nobody is looking. It writes to `cache/` and reads through `provider/`.

**[`query/`](query/README.md)** is `filter/`, `sort/`, and `autocomplete/`
together: parse a filter string to an AST, compile the AST to SQL, order and
group the result, and suggest tags as the user types. It depends on `cache/`
for the tables it queries and on `companion/` for the media-type vocabulary.

**[`companion/`](companion/README.md)** reads and writes the `.lightview` JSON
sidecar next to each media file. That file, not the database, is the record of
user intent — tags, ratings, notes. The cache indexes it for query speed and
can always be rebuilt from it.

**[`geocode/`](geocode/README.md)** turns the GPS coordinates `pipeline/`
extracted into country, region, and city names, so a filter query can be the
word `Kyoto` rather than a bounding box. It is a pure lookup over an embedded
gazetteer, depending on nothing else in the tree; `commands/gallery.rs` writes
its output into companion files, because the name — unlike the coordinate — is
not recoverable from the media file itself.

**`provider/`** reads the gallery's files: a recursive scan at open, and
whole-file reads for the viewer. It was a `FileProvider` trait plus a registry,
sized for the SMB/SFTP/S3 backends that were never written; it is now the one
concrete struct, since an interface with a single implementation and five
unreachable methods bought nothing. Re-introducing the trait is the right move
on the second backend, not the first hypothetical one.

**[`remote/`](remote/README.md)** (`http_server/`) is the axum server: static
SPA assets, media and thumbnail routes, an SSE change stream, device pairing,
TLS, and `/api/invoke` — an explicit allowlist that decides which backend
commands a paired device may reach. The desktop app additionally runs a second,
loopback-only instance of the same server purely so `<video>` elements have an
`http://` URL to load from.

**[`plugins/`](plugins/README.md)** (`plugin/`) spawns a plugin as a subprocess
and exchanges NDJSON with it. It also decides what the plugin *receives*:
`plugin/input.rs` scales a still to the size the manifest asked for, splits a
video into sampled frames, and merges those frames' results back into one, so a
plugin never handles video and behaves identically wherever it runs.
`plugin/install.rs` is how a plugin gets into an install directory — shared by
the desktop command and `lightview-worker install`, because a hand-copied plugin
nobody updates is what produced the deadlock `api_version` now catches.
**`tagging/`** is the queue that decides *where* execution happens: in-process
on the server, or on a paired `lightview-worker` running on a machine with a
GPU. See [`remote/worker-tagging.md`](remote/worker-tagging.md).

**[`duplicates/`](duplicates/README.md)** spans `cache/duplicates.rs` and
`commands/duplicates.rs`: perceptual hashing, grouping, and the merge that
folds several copies' metadata onto one survivor before trashing the rest.

**`views.rs`** records which layouts a gallery offers, in `gallery_meta`, and
maps them to the thumbnail tiers the idle worker should pre-warm. It is a single
small module because enablement and generation are one decision: a view nobody
can open should not be spending gigabytes pre-generating cells. Read by
[`pipeline/`](pipeline/README.md) and by both clients; written only from the
desktop. Together with a `lazy()` split on the one view that carries its own
rendering library, it is also the whole of what a view-module API would have
bought — see [decision 0008](decisions/0008-no-view-module-api.md).

**`hardware/`** detects storage type, core count, RAM, and GPU once at startup.
Its output sizes the thumbnail thread pool and decides whether the GPU pipeline
is worth initializing — it is read, never written, after startup.

**`util/`** holds `paths` (the exe-relative data directory) and `fs_watch` (a
thin non-blocking wrapper over `notify`; the coalescing that turns an event
storm into one refresh is policy, and lives in `commands/gallery.rs`).

**[`frontend/`](frontend/README.md)** is the SPA. `lib/ipc.ts` is the only
module that knows whether it is talking to Tauri or to HTTP; everything above
it is transport-agnostic.

## Data flow: opening a gallery

`open_gallery` is the one command with real orchestration, and its ordering is
load-bearing:

1. Construct the `LocalProvider` for the directory and canonicalize the root
   once — every later path-confinement check compares against that value rather
   than canonicalizing the root per request.
2. Open `<gallery>/.lightview/cache.db`, run migrations, then **rebase the
   stored paths if the gallery directory moved**. This must happen before the
   index is populated, or the scan inserts bare rows under the new root that
   shadow the relocated history. See [`cache/`](cache/README.md).
3. Scan the tree and populate `media_meta`. The grid can render from this alone
   — no thumbnail or tag work has happened yet.
4. Backfill EXIF GPS into `media_meta` for anything missing it, then
   reverse-geocode those coordinates and write the resulting place names into
   companion files — before the index pass below, so they land in the same
   sweep. See [`geocode/`](geocode/README.md).
5. Re-index companion files whose mtime changed, rebuild `tag_counts`, and load
   the autocomplete engine from it.
6. Open the read-only connection pool, start the filesystem watcher, and start
   the idle backfill worker.

## Data flow: a thumbnail reaching the screen

A grid cell points an `<img>` optimistically at a tier URL. A cached thumbnail
comes back immediately; an uncached one 404s, and the frontend queues the path
for batch generation. Both transports converge on the same code:

```
lightview://thumb/<tier>/<path>   (desktop custom protocol, main.rs)
GET /thumb/<tier>/<path>          (web, http_server/routes.rs)
        └──────────────┬──────────────┘
                       ▼
            thumb_serve::get_or_generate
                       │  read via ThumbProtocolPool (read-only, N conns)
                       │  miss → elect one generator, others wait
                       ▼
            pipeline::thumbnailer  on the bounded rayon thumb_pool
                       ▼
            cache::thumbnails      write + enforce the tier budget
```

The read path never takes the writer lock. That is why LRU access marks are
buffered in `AppState::pending_tier_accesses` and flushed by whoever next runs
an eviction pass, rather than written through per request.

## Dependency direction

The rule is that the domain modules do not know about their callers:

- `cache/`, `companion/`, `filter/`, `sort/`, `autocomplete/`, `geocode/`,
  `hardware/`, `provider/`, and `util/` know nothing about Tauri, axum, or
  `AppState`. They are ordinary libraries taking a connection or a struct.
- `pipeline/`, `plugin/`, and `tagging/` sit one level up: they
  take `AppState` or pieces of it, but no HTTP or IPC types.
- `commands/` and `http_server/` are the two adapters at the top. **Neither may
  contain logic the other needs.** The convention that enforces this is the
  `*_impl` split: `#[tauri::command] async fn foo(state: State<AppState>)` is a
  three-line wrapper around `foo_impl(&AppState)`, and `http_server/api.rs`
  dispatches to the same `foo_impl`. When a command exists only on the desktop
  (file copy, plugin execution, render config), it simply is not in the
  allowlist — the split still holds.

`thumb_serve.rs` and `gif_serve.rs` live at the top level for exactly this
reason: they are the shared bodies of the desktop protocol handler and the HTTP
route, and belong to neither adapter.

## Concurrency and shared state

All cross-command state is in `AppState` behind `Arc`. The choice of lock per
field is deliberate and documented at each field; the two that matter most:

- `cache_db` is a `tokio::Mutex`, not an `RwLock`, because
  `rusqlite::Connection` is `Send` but not `Sync`. It is the serialization
  point for every write in the process, so nothing expensive may be done while
  holding it — decode, encode, and `ffprobe` all run first, and the lock is
  taken only to commit.
- `thumb_protocol_db` is a `std::sync::RwLock` around an `Arc<ThumbProtocolPool>`
  because its readers are synchronous and hold the outer lock only long enough
  to clone the `Arc`.

CPU-bound thumbnail work runs on one bounded `rayon::ThreadPool`
(`thumb_pool`), sized from the hardware profile. Speculative work — look-ahead,
landing-zone warms, the idle backfill — lands on that same pool, which is why
the frontend gates speculation behind "nothing visible is outstanding". There
is no second pool to escape to.

## Shutdown

Both binaries run `commands::gallery::close_gallery_impl` on the way out — the
desktop from Tauri's `RunEvent::Exit`, `lightview-headless` from a SIGTERM or
SIGINT handler (SIGTERM being what `docker stop` sends).

It is worth knowing what that does and does not buy, because most of the
obvious answers are already handled elsewhere:

- **Companion files need nothing.** Writes go to a temp file in the target
  directory and are renamed into place, so no reader sees a partial one. Dying
  between writing a companion and re-indexing it is also safe: `index_state`
  still holds the old mtime, so the next gallery open re-indexes that file.
- **The WAL needs nothing.** SQLite recovers it on the next open. The checkpoint
  is tidiness, not correctness.
- **Buffered tier-access marks do need it.** The thumbnail serve path holds a
  read-only connection and cannot write, so "this row was served" accumulates in
  `AppState::pending_tier_accesses` and is otherwise landed only by
  `enforce_tier_budget`, which runs when a capped tier is written. Exiting
  without flushing sends the zoom thumbnails you were just looking at back to
  the next eviction pass looking maximally cold.

The shutdown path deliberately flushes without evicting: exit is the wrong
moment to spend seconds in a `DELETE`, and the next write enforces the budget
anyway.

## Build shapes

Three binaries come out of `src-tauri/`:

| Binary | What it is | Needs |
|---|---|---|
| `lightview` | the desktop app | GTK/WebKitGTK, and `dist/` at compile time |
| `lightview-headless` | the same backend plus the HTTP server, no webview | nothing graphical |
| `lightview-worker` | pairs to a server, claims tagging jobs, runs plugins locally | feature `worker` |

`lightview-worker` is skipped by an ordinary build and by the Tauri bundle
(`required-features`), so it is built and released explicitly — see
[decision 0014](decisions/0014-ship-the-worker-with-the-release.md).

`lightview-headless` exists so the entire remote surface can be exercised with
`curl` and a real browser on a machine with no display. See
[`build-and-verify.md`](build-and-verify.md).

Cargo features: `gpu` (default) pulls in `wgpu`/`pollster` for the fused
crop+resize path; `custom-protocol` (default) is Tauri's production protocol
handling; `worker` gates the worker binary.
