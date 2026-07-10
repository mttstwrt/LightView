# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

LightView is a fast, plugin-extensible local media gallery built with **Tauri 2** (Rust backend) and **SolidJS** (TypeScript frontend). It opens a folder of images/videos, generates thumbnails, indexes metadata/tags into SQLite, and presents a browsable grid with filtering, sorting, and a full-resolution viewer.

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

### Remote tagging worker (test the job queue end-to-end, no ML needed)

`lightview-worker` (feature-gated bin; see `docs/workerTagging.md`) runs tagger
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

Notes: the *server itself* also runs jobs when plugins are installed in **its**
`data_dir()/plugins` (same `target/debug/data/plugins/` when server and worker
share an exe dir) — it registers as worker `local-server` / "Server (<host>)"
with `local: true`, so no `lightview-worker` is needed for a server-local run.
Pass `"workerId": "<id>"` to `enqueue_tagging_job` to pin a job to a specific
worker when several offer the same plugin. Plugins must stream results (never
buffer stdin to EOF) — see `docs/workerTagging.md`.

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
| `pipeline/` | Thumbnail generation: CPU `thumbnailer`, optional GPU pipeline (`gpu_pipeline` via wgpu) |
| `filter/` | Query language for filtering media: `ast` → `parser` → `evaluator` |
| `sort/` | Sorting + grouping: `sorter`, `grouper`, `timeline` |
| `autocomplete/` | In-memory tag autocomplete engine |
| `plugin/` | External plugin system: `manifest` (JSON) + `runner`. Spawns a plugin subprocess and exchanges NDJSON over stdin/stdout. Currently implements a single verb — image → tags — so in practice it's an auto-tagger runner, not a general extension host. See `docs/pluginExtensibility.md`. |
| `tagging/` | Remote-tagging job queue + worker registry (in-memory). Web clients enqueue jobs over `/api/invoke` (optionally pinned to one worker); a paired `lightview-worker` (bin, feature `worker`) claims them, runs the plugin on its own machine, and pushes tags back via `apply_plugin_tags`. `tagging/local.rs` is the in-process executor: the server registers itself as worker `local-server` and runs its own installed plugins directly (opt-in by installing plugins server-side). Job/worker changes stream to web clients as `tagging-job`/`tagging-workers` SSE events. See `docs/workerTagging.md`. |
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
- **Plugin system**: Plugins are directories with a `manifest.json`. Execution is CLI-based: the host spawns the plugin as a subprocess and streams NDJSON image paths in / tag results out (`plugin/runner.rs`). The host only implements the `tag` verb today, so every plugin is effectively an auto-tagger keyed by `tag_prefix`. Example: `plugins/wd-tagger/` (ML image tagger). Roadmap for true extensibility: `docs/pluginExtensibility.md`.

### Cargo features

- `gpu` (default): Enables `wgpu`/`pollster` for GPU-accelerated thumbnail pipeline
- `custom-protocol` (default): Tauri custom protocol for production builds

### Linux-specific

`main.rs` forces `GDK_BACKEND=x11` and `WEBKIT_DISABLE_DMABUF_RENDERER=1` to work around WebKitGTK/Wayland crashes.

## Tech Stack

- **Rust 2024 edition**, Tauri 2, rusqlite (bundled SQLite), rayon, tokio, wgpu, image, fast_image_resize, libheif-rs
- **TypeScript**, SolidJS, Tailwind CSS v4, Vite 5
