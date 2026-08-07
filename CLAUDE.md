# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

LightView is a fast, plugin-extensible local media gallery built with **Tauri 2** (Rust backend) and **SolidJS** (TypeScript frontend). It opens a folder of images/videos, generates thumbnails, indexes metadata/tags into SQLite, and presents a browsable grid with filtering, sorting, and a full-resolution viewer.

**Deeper reference: [`docs/`](docs/README.md)** — this file is the orientation;
`docs/` carries the subsystem maps and the rules a change must not break. Start
at [`docs/architecture.md`](docs/architecture.md), then the subsystem README for
whatever you are touching — each one ends its header with the invariants callers
must uphold. Read [`docs/build-and-verify.md`](docs/build-and-verify.md) if
`cargo check` fails before it reaches your code (missing system libraries, or an
absent `dist/`).

## Build & Dev Commands

```bash
# Development (starts both Vite dev server and Tauri app)
cargo tauri dev

# Production build
cargo tauri build

# Frontend only (Vite dev server on port 5173)
npm run dev

# Rust only — check/build/test
cd src-tauri && cargo check
cd src-tauri && cargo build
cd src-tauri && cargo test

# Run a single Rust test
cd src-tauri && cargo test <test_name>

# Benchmarks (criterion)
cd src-tauri && cargo bench --bench thumbnailer
cd src-tauri && cargo bench --bench cache_db
```

### Headless test server (verify the web client without the GUI)

`lightview-headless` boots the same backend + axum HTTP server as the desktop
app but **without WebKitGTK**, so the LAN web client (and routes like
`/api/events`, `/api/upload`, `/api/invoke`) can be exercised with `curl` — no
display required. The desktop app starts the fs-watcher from its `open_gallery`
command; the headless binary calls `open_gallery_impl` and starts the watcher
itself, so both paths watch for disk changes.

```bash
# Build the binary (debug is fine for testing) and the SPA it serves from dist/
cd src-tauri && cargo build --bin lightview-headless
npm run build      # produces dist/ at the repo root; resolve_web_root() finds it

# Make a throwaway gallery
G=$(mktemp -d); for c in red green blue; do magick -size 64x64 xc:$c "$G/$c.jpg"; done

# Serve it (0.0.0.0, HTTPS with a self-signed cert, per-device cookie auth,
# SPA from dist/). Use `curl -k` — the cert is self-signed.
./target/debug/lightview-headless serve "$G" --port 8799 &

# Pair a device: mint a PIN, redeem it for the lv_device cookie
PIN=$(./target/debug/lightview-headless pair "$G" | sed -n 's/Pairing PIN: //p')
COOKIE=$(curl -sk -D- -o/dev/null -X POST https://localhost:8799/pair/redeem \
  -H 'Content-Type: application/json' -d "{\"code\":\"$PIN\",\"device_name\":\"test\"}" \
  | sed -nE 's/.*(lv_device=[^;]+).*/\1/p')

# Now hit auth-gated routes. Example: watch the SSE change stream, then add a file
curl -skN --cookie "$COOKIE" https://localhost:8799/api/events &  # streams `event: fs-changed`
cp "$G/red.jpg" "$G/new.jpg"                               # watcher → broadcast → SSE
```

Notes: the watcher only catches changes made *after* startup (a restart
re-indexes). `/api/events`, `/api/upload`, and `/api/invoke` all require the
`lv_device` cookie; unauthenticated requests get `401`. Default port is `8787`.
Remote serving is always HTTPS — browsers need a secure context for the async
Clipboard API and friends. The self-signed ECDSA cert persists at
`<exe_dir>/data/tls/` and regenerates when the LAN IP changes or expiry nears
(`src-tauri/src/http_server/tls.rs`). The desktop's loopback media server
stays plain HTTP (the webview won't accept a self-signed cert).

**Cert SANs behind NAT/Docker.** `detect_lan_ip()` sees only the interface this
process routes through — inside a container that's the bridge IP, never the host
address clients dial — so the cert fails hostname verification everywhere. Name
the reachable address explicitly with `--tls-san <ip-or-host>` (repeatable,
comma-separated) or `LIGHTVIEW_TLS_SAN`; docker-compose.yml wires the env var
through. Getting this wrong is quiet: desktop browsers keep working on a
click-through exception, but iOS drops that exception readily and a standalone
PWA can't render the prompt to re-accept it. Adding a SAN re-mints the cert,
re-prompting every paired browser once (and breaking any `lightview-worker`
cert pin — re-pair with `--trust-new`).

**Trusting the cert instead of clicking through.** A click-through exception is
per-origin and short-lived; the durable fix is installing the certificate. The
cert carries `basicConstraints CA:TRUE` + `keyCertSign` alongside its serverAuth
EKU (`CERT_SCHEMA` 2) precisely so iOS/macOS offer the full-trust toggle — they
only expose it for CA certs. `GET /cert` serves the PEM unauthenticated as
`application/x-x509-ca-cert` (it's public; every handshake hands it out anyway,
and it must be reachable *before* the browser trusts the connection enough to
pair), and `/pair` links to it. iOS: open the link → Settings → Profile
Downloaded → Install → General → About → Certificate Trust Settings → enable.

**Client-side cache lifetime, and why a dead server used to look alive.** Two
independent client caches, neither originally time-bounded: the service worker's
`lv-thumbs-*` Cache Storage (2000 entries, FIFO) and the whole sorted-item list
in IndexedDB (`loadBootSnapshot()`). When the server became unreachable, the
worker's network-first navigation fell back to the cached shell, the snapshot
repainted the entire grid, and cached thumbs rendered — so the app looked
connected while every `/api`, `/thumb`, and `/media` request failed. Worse, the
navigation never reached the network, so the browser had nothing to render its
certificate interstitial for: on iOS the only cure was clearing site data, which
also drops `lv_device` and forces a re-pair.

Fixed in three places. `networkFirstShell` now serves the cached shell only when
`navigator.onLine` is false (genuinely offline); network-up-but-origin-dead gets
a synthetic recovery page whose **Reset connection** button unregisters the
worker and reloads — the next navigation is uncontrolled, hits the network, and
the browser can finally show its cert prompt, with cookies and Cache Storage
left intact so the pairing survives. `?lv_offline=1` opts back into the cached
shell on demand. Cached thumbs get a 30-day hard ceiling (`THUMB_MAX_STALE_MS`)
on top of the 1-hour revalidation window, and the boot snapshot honours its
`savedAt` with the same ceiling. In-app, a failed `getBootState()` sets
`serverUnreachable` and shows `ConnectionBanner` with the same two actions, so
the cached-shell path is never silent either.

**Zoomed-in tier disk budget.** The `jm`/`jh` justified tiers cache whatever you
view zoomed in, so they're bounded by a byte budget (10% of free disk, floored
at 512 MiB, capped at 8 GiB) with LRU eviction keyed on an `accessed_at` column
(schema v15). Serves buffer access marks in `AppState::pending_tier_accesses` —
the read path holds a read-only connection and must not take the writer lock —
and `enforce_tier_budget` flushes them right before evicting. Both tier write
paths enforce the budget, and eviction only fires past 1.25× it, so passes stay
small; when only the batch path evicted, the table grew unbounded between calls
and then shed the whole overshoot in one multi-second `DELETE`. Override with
`LIGHTVIEW_TIER_BUDGET_MB` (per tier, MiB, bypasses the floor) to pin the cache
on a small box or in a test.

### Remote tagging worker (test the job queue end-to-end, no ML needed)

`lightview-worker` (feature-gated bin; see [`docs/remote/worker-tagging.md`](docs/remote/worker-tagging.md)) runs tagger
plugins on a capable machine against a headless server. The dependency-free
`plugins/example-auto-tagger` exercises the whole loop:

```bash
cd src-tauri && cargo build --bin lightview-worker --features worker

# Worker-local plugin install (data dir is exe-relative: target/debug/data/)
mkdir -p target/debug/data/plugins && cp -r ../plugins/example-auto-tagger target/debug/data/plugins/

# Pair the worker (TOFU-pins the server cert; PIN from lightview-headless pair).
# NOTE: pairings live in the gallery's cache.db — a new test gallery needs a re-pair.
./target/debug/lightview-worker pair --server https://localhost:8799 --pin "$PIN" --name test --yes
./target/debug/lightview-worker run &

# Enqueue from a paired "browser" device; watch `tagging-job` /
# `tagging-workers` SSE events on /api/events, then confirm nothing is left:
curl -sk --cookie "$COOKIE" -X POST https://localhost:8799/api/invoke -H 'Content-Type: application/json' \
  -d '{"command":"enqueue_tagging_job","args":{"pluginName":"example-image-tagger","filter":"type:image AND NOT has::plugin.example"}}'
curl -sk --cookie "$COOKIE" -X POST https://localhost:8799/api/invoke -H 'Content-Type: application/json' \
  -d '{"command":"apply_filter","args":{"query":"NOT has::plugin.example"}}'   # → []
```

**Remote jobs hang at exactly 64 images.** The worker keeps a bounded number
of downloaded files on disk (`MAX_PENDING_FILES`, a `Semaphore(64)`), and a
permit is released only when *that file's* result is matched back in the
`pending` map. So a plugin that skips a request, or answers with a path the
worker can't match (canonicalizing under a symlinked `TMPDIR` is enough),
leaks a slot each time — at 64 the downloader blocks forever, the plugin stops
receiving stdin, and no result ever arrives. Jobs under ~64 images finish
normally, which makes it read as "large batches fail." It is also *silent*:
the worker's heartbeat refreshes `updated_at` whether or not anything is being
tagged, so the 90 s stall reaper stays satisfied and the job shows `running`
forever. Mitigations, all three needed: `pending` is keyed on the temp file
name rather than the full path; the worker fails the job after 20 min with no
result; and the server tracks `progressed_at` separately from `updated_at` and
fails a job that goes 30 min without its counts moving. Grep the worker log for
`plugin result for unknown path`. Full contract in [`docs/remote/worker-tagging.md`](docs/remote/worker-tagging.md).

Notes: the *server itself* also runs jobs when plugins are installed in **its**
`data_dir()/plugins` (same `target/debug/data/plugins/` when server and worker
share an exe dir) — it registers as worker `local-server` / "Server (<host>)"
with `local: true`, so no `lightview-worker` is needed for a server-local run.
Pass `"workerId": "<id>"` to `enqueue_tagging_job` to pin a job to a specific
worker when several offer the same plugin. Plugins must stream results (never
buffer stdin to EOF) — see [`docs/remote/worker-tagging.md`](docs/remote/worker-tagging.md).

## Architecture

### Two-process Tauri model

- **Backend** (`src-tauri/`): Rust process managing all I/O, image processing, and state. Exposes functionality as `#[tauri::command]` functions registered in `src-tauri/src/main.rs`.
- **Frontend** (`src-solidjs/`): SolidJS SPA rendered in a webview. Communicates with Rust exclusively via Tauri's `invoke()` IPC. All IPC wrappers are in `src-solidjs/lib/ipc.ts`.

### Custom URI protocols

`main.rs` registers a `lightview://` protocol with two routes:
- `lightview://thumb/<path>` — serves cached thumbnails from SQLite (zero JSON overhead)
- `lightview://media/<path>` — serves full-resolution files (HEIC/HEIF auto-transcoded to JPEG)

### Backend modules (`src-tauri/src/`)

| Module | Purpose |
|---|---|
| `commands/` | Tauri command handlers (thin wrappers delegating to domain modules) |
| `provider/` | `FileProvider` trait + `ProviderRegistry` — file-access abstraction. Only `local` (`LocalProvider`) is implemented today; the trait exists to allow remote backends (SMB/SFTP/S3) later. |
| `companion/` | Sidecar `.lightview` JSON companion files for per-image metadata (schema, reader, writer, migration) |
| `cache/` | SQLite-backed cache: `db` (core), `thumbnails`, `index`, `counts`, `duplicates` |
| `pipeline/` | Thumbnail generation: CPU `thumbnailer`, optional GPU pipeline (`gpu_pipeline` via wgpu), `video` (ffmpeg/ffprobe frame extraction + probing) |
| `filter/` | Query language for filtering media: `ast` → `parser` → `evaluator` |
| `sort/` | Sorting + grouping: `sorter`, `grouper`, `timeline` |
| `autocomplete/` | In-memory tag autocomplete engine |
| `plugin/` | External plugin system: `manifest` (JSON) + `runner`. Spawns a plugin subprocess and exchanges NDJSON over stdin/stdout. Currently implements a single verb — image → tags — so in practice it's an auto-tagger runner, not a general extension host. See [`docs/plugins/`](docs/plugins/README.md). |
| `tagging/` | Remote-tagging job queue + worker registry (in-memory). Web clients enqueue jobs over `/api/invoke` (optionally pinned to one worker); a paired `lightview-worker` (bin, feature `worker`) claims them, runs the plugin on its own machine, and pushes tags back via `apply_plugin_tags`. `tagging/local.rs` is the in-process executor: the server registers itself as worker `local-server` and runs its own installed plugins directly (opt-in by installing plugins server-side). Job/worker changes stream to web clients as `tagging-job`/`tagging-workers` SSE events. See [`docs/remote/worker-tagging.md`](docs/remote/worker-tagging.md). |
| `hardware/` | Hardware detection (storage type, CPU, RAM, GPU) — drives adaptive performance tuning |
| `util/` | Helpers: `paths` (data dir), `hash`, `fs_watch` |

### Frontend structure (`src-solidjs/`)

| Path | Purpose |
|---|---|
| `stores/` | SolidJS signals for global state: `galleryStore`, `viewerStore`, `settingsStore`, `filterStore`, `tagStore`, `pluginStore`, `taggingStore` (remote-tagging workers/jobs, web), `thumbnailProgressStore` |
| `lib/ipc.ts` | All Tauri `invoke()` calls — the single IPC boundary |
| `lib/types.ts` | Shared TypeScript types |
| `components/gallery/` | Grid views: `GalleryGrid`, `ThumbnailCell`, `SelectionBar` |
| `components/viewer/` | Full-resolution `MediaViewer` |
| `components/topbar/` | `TopBar`, `FilterBar`, `SortMenu`, `SettingsMenu` |
| `components/debug/` | `DebugOverlay`, `Sparkline`, `DevtoolsApp` |

### Key design patterns

- **Shared state**: Rust `AppState` (in `lib.rs`) holds all cross-command state behind `Arc<RwLock<>>` / `Arc<Mutex<>>`. `rusqlite::Connection` uses `Mutex` (not `RwLock`) because it's `Send` but not `Sync`.
- **Thumbnail pipeline**: Hardware-adaptive — detects NVMe/SSD, discrete GPU, CPU cores. Uses a bounded `rayon::ThreadPool` (`thumb_pool`). Optional GPU path via `wgpu` (feature `gpu`) for fused crop+resize.
- **Dependencies in dev builds**: `[profile.dev.package."*"] opt-level = 2` — image codecs, SIMD resize, and SQLite are unusably slow at opt-level 0.
- **Video thumbnails need ffmpeg**: `pipeline/video.rs` shells out to `ffmpeg`/`ffprobe`; without them every clip falls back to a grey placeholder (checked once per process, so a missing binary doesn't cost two failed spawns per file). ffmpeg does the downscale in its filter graph at *exact* pixel dimensions, so the raw output length is known up front — a 4K clip crosses the pipe at ~1.8 MB instead of ~33 MB. Those dimensions come from the probe with the container's **display-matrix rotation applied**: phone clips are landscape on disk and portrait on screen, ffmpeg autorotates ahead of our scale filter, and a mismatch here is what makes `.MOV` thumbnails come back sideways or fail outright. Every invocation is timeout-bounded — a wedged subprocess would otherwise hold a `thumb_pool` thread forever.
- **Plugin system**: Plugins are directories with a `manifest.json`. Execution is CLI-based: the host spawns the plugin as a subprocess and streams NDJSON image paths in / tag results out (`plugin/runner.rs`). The host only implements the `tag` verb today, so every plugin is effectively an auto-tagger keyed by `tag_prefix`. Example: `plugins/wd-tagger/` (ML image tagger). Roadmap for true extensibility: [`docs/plugins/`](docs/plugins/README.md).

### Cargo features

- `gpu` (default): Enables `wgpu`/`pollster` for GPU-accelerated thumbnail pipeline
- `custom-protocol` (default): Tauri custom protocol for production builds

### Linux-specific

`main.rs` forces `GDK_BACKEND=x11` and `WEBKIT_DISABLE_DMABUF_RENDERER=1` to work around WebKitGTK/Wayland crashes.

## Tech Stack

- **Rust 2024 edition**, Tauri 2, rusqlite (bundled SQLite), rayon, tokio, wgpu, image, fast_image_resize, libheif-rs
- **TypeScript**, SolidJS, Tailwind CSS v4, Vite 5

## Engineering Principles

These are ordered. When they conflict, the earlier one wins.

### 1. Simplest thing that works

The simplest solution that satisfies the requirement is the correct solution.
Complexity must be earned by a demonstrated need, not an anticipated one.

- Prefer a function to a class, a class to a hierarchy, a hierarchy to a framework.
- Do not add abstraction layers, interfaces, or plugin points that have exactly one
  implementation. Write the concrete thing. Extract the abstraction on the second
  real use case, not the first hypothetical one.
- Do not add configuration options, feature flags, or parameters that were not asked
  for. Every knob is a permanent maintenance surface and a combinatorial test case.
- Do not add error handling for conditions that cannot occur, defensive checks for
  invariants the type system already guarantees, or retry logic where there is no
  transient failure mode.
- Prefer the standard library. Prefer an existing dependency to a new one. Justify any
  new dependency in terms of what it removes.
- Deleting code is a valid and preferred solution. If a change makes existing code
  unreachable, remove it in the same change; do not leave it commented out.

If you believe a more complex approach is warranted, state the specific requirement
that forces it before writing the code. If you cannot name the requirement, use the
simple version.

### 2. Performance is a design property, not a pass at the end

Think about cost at the level where it matters — algorithmic complexity, allocation
patterns, I/O and syscall boundaries, data layout and locality, work done per
iteration of a hot loop. Get these right the first time; they are expensive to change.

Do not micro-optimize. Do not restructure readable code for speculative gains, and do
not trade clarity for performance without a measurement showing the trade is real.
Unmeasured optimization is complexity without justification and violates principle 1.

When a fast path genuinely requires complexity, isolate it: keep the complex code in
one clearly marked place behind a simple interface, with a comment explaining the
measurement that motivated it.

### 3. Comments explain why

Comments carry the information that is not recoverable from the code itself.

- Explain rationale, constraints, and rejected alternatives — not mechanics. If a
  comment restates what the line does, delete it.
- Document non-obvious decisions: why this algorithm, why this ordering, why this
  buffer size, why this apparent inefficiency is deliberate.
- Document invariants, assumptions about caller behavior, and units/frames/coordinate
  conventions on anything numeric.
- Flag anything surprising. If a future reader would be tempted to "fix" the code,
  say why it is that way.
- Every module gets a module-level doc comment stating its purpose and boundaries.
  This is where per-file explanation lives — not in `docs/`.

### 4. Keep the docs current

`docs/` is a set of linked markdown pages describing how the system works and why.
It is part of every change, not a follow-up.

**Layout**

```
docs/
  README.md              entry point; map of the docs with links to each subsystem
  architecture.md        component map, data flow, dependency direction
  decisions/
    0001-<slug>.md       one decision per file, numbered, append-only
  <subsystem>/
    README.md            subsystem overview
    <topic>.md           only when a topic outgrows the README
```

**Granularity.** Pages describe subsystems, not files. Create a subsystem directory
when a component has its own responsibility and interface; do not create a page per
source file. Split a topic into its own page only when it is long enough that it
would dominate the subsystem README. Per-file explanation belongs in module doc
comments (principle 3).

**Every subsystem README covers:** what it is responsible for, what it explicitly is
not responsible for, its public interface, what it depends on, what depends on it,
and the invariants callers must uphold. State dependencies by name rather than
relying on directory nesting to imply them.

**Linking.** Use relative markdown links. Every page links back to `docs/README.md`
and to the subsystems it names. `architecture.md` and each subsystem README are hubs;
no page should be reachable only by browsing the filesystem.

**Decisions.** When a choice has alternatives worth recording, add
`docs/decisions/NNNN-<slug>.md` with: context, options considered, the choice, and
the consequences. Decision files are never edited after the fact. If a decision is
reversed, write a new one and add a superseding link to the old.

**Update rules.** In the same change, whenever you:
- add, remove, rename, or move a subsystem — update `architecture.md` and fix links
- change a data flow, interface contract, or file/wire format — update the affected
  subsystem READMEs on both sides of the boundary
- make a decision with real alternatives — add a decision file
- write code that contradicts something the docs currently state — fix the docs

Prose over bullet fragments. Do not paste code that will drift; link to it and explain
the shape. If a change makes a doc wrong, fixing it is not optional and not deferred.