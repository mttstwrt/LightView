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

**`provider/`** is the file-access seam: a `FileProvider` trait plus a
`ProviderRegistry` that maps a gallery root to the provider serving it.
`LocalProvider` is the only implementation, and the trait is small enough that
it has no page of its own — read `provider/mod.rs`.

**[`remote/`](remote/README.md)** (`http_server/`) is the axum server: static
SPA assets, media and thumbnail routes, an SSE change stream, device pairing,
TLS, and `/api/invoke` — an explicit allowlist that decides which backend
commands a paired device may reach. The desktop app additionally runs a second,
loopback-only instance of the same server purely so `<video>` elements have an
`http://` URL to load from.

**[`plugins/`](plugins/README.md)** (`plugin/`) spawns a plugin as a subprocess
and exchanges NDJSON with it. **`tagging/`** is the queue that decides *where*
that happens: in-process on the server, or on a paired `lightview-worker`
running on a machine with a GPU. See
[`remote/worker-tagging.md`](remote/worker-tagging.md).

**[`duplicates/`](duplicates/README.md)** spans `cache/duplicates.rs` and
`commands/duplicates.rs`: perceptual hashing, grouping, and the merge that
folds several copies' metadata onto one survivor before trashing the rest.

**`hardware/`** detects storage type, core count, RAM, and GPU once at startup.
Its output sizes the thumbnail thread pool and decides whether the GPU pipeline
is worth initializing — it is read, never written, after startup.

**`util/`** holds `paths` (the exe-relative data directory), `hash`, and
`fs_watch` (the notify-based watcher whose batches drive both the desktop's
`gallery:fs-changed` event and the web client's SSE stream).

**[`frontend/`](frontend/README.md)** is the SPA. `lib/ipc.ts` is the only
module that knows whether it is talking to Tauri or to HTTP; everything above
it is transport-agnostic.

## Data flow: opening a gallery

`open_gallery` is the one command with real orchestration, and its ordering is
load-bearing:

1. Register the directory with the `ProviderRegistry` and canonicalize the root
   once — every later path-confinement check compares against that value rather
   than canonicalizing the root per request.
2. Open `<gallery>/.lightview/cache.db`, run migrations, then **rebase the
   stored paths if the gallery directory moved**. This must happen before the
   index is populated, or the scan inserts bare rows under the new root that
   shadow the relocated history. See [`cache/`](cache/README.md).
3. Scan the tree and populate `media_meta`. The grid can render from this alone
   — no thumbnail or tag work has happened yet.
4. Re-index companion files whose mtime changed, rebuild `tag_counts`, and load
   the autocomplete engine from it.
5. Open the read-only connection pool, start the filesystem watcher, and start
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

- `cache/`, `companion/`, `filter/`, `sort/`, `autocomplete/`, `hardware/`, and
  `util/` know nothing about Tauri, axum, or `AppState`. They are ordinary
  libraries taking a connection or a struct.
- `pipeline/`, `provider/`, `plugin/`, and `tagging/` sit one level up: they
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

## Build shapes

Three binaries come out of `src-tauri/`:

| Binary | What it is | Needs |
|---|---|---|
| `lightview` | the desktop app | GTK/WebKitGTK, and `dist/` at compile time |
| `lightview-headless` | the same backend plus the HTTP server, no webview | nothing graphical |
| `lightview-worker` | pairs to a server, claims tagging jobs, runs plugins locally | feature `worker` |

`lightview-headless` exists so the entire remote surface can be exercised with
`curl` and a real browser on a machine with no display. See
[`build-and-verify.md`](build-and-verify.md).

Cargo features: `gpu` (default) pulls in `wgpu`/`pollster` for the fused
crop+resize path; `custom-protocol` (default) is Tauri's production protocol
handling; `worker` gates the worker binary.
