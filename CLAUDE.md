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
cd src-tauri && cargo bench --bench atlas
```

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
| `provider/` | `FileProvider` trait + `ProviderRegistry` — abstraction over local/SMB/SFTP/S3 file access |
| `companion/` | Sidecar `.lightview` JSON companion files for per-image metadata (schema, reader, writer, migration) |
| `cache/` | SQLite-backed cache: `db` (core), `thumbnails`, `index`, `counts`, `atlas` (BC7 mmap), `duplicates` |
| `pipeline/` | Thumbnail generation: CPU `thumbnailer`, optional GPU pipeline (`gpu_pipeline` via wgpu) |
| `filter/` | Query language for filtering media: `ast` → `parser` → `evaluator` |
| `sort/` | Sorting + grouping: `sorter`, `grouper`, `timeline` |
| `autocomplete/` | In-memory tag autocomplete engine |
| `plugin/` | External plugin system: `manifest` (JSON), `runner`, `daemon` (long-running process plugins) |
| `hardware/` | Hardware detection (storage type, CPU, RAM, GPU) — drives adaptive performance tuning |
| `util/` | Helpers: `paths` (data dir), `hash`, `fs_watch` |

### Frontend structure (`src-solidjs/`)

| Path | Purpose |
|---|---|
| `stores/` | SolidJS signals for global state: `galleryStore`, `viewerStore`, `settingsStore`, `filterStore`, `tagStore`, `pluginStore`, `thumbnailProgressStore` |
| `lib/ipc.ts` | All Tauri `invoke()` calls — the single IPC boundary |
| `lib/types.ts` | Shared TypeScript types |
| `components/gallery/` | Grid views: `GalleryGrid`, `ThumbnailCell`, `SelectionBar` |
| `components/viewer/` | Full-resolution `MediaViewer` |
| `components/topbar/` | `TopBar`, `FilterBar`, `SortMenu`, `SettingsMenu` |
| `components/debug/` | `DebugOverlay`, `Sparkline`, `DevtoolsApp` |

### Key design patterns

- **Shared state**: Rust `AppState` (in `lib.rs`) holds all cross-command state behind `Arc<RwLock<>>` / `Arc<Mutex<>>`. `rusqlite::Connection` uses `Mutex` (not `RwLock`) because it's `Send` but not `Sync`.
- **Thumbnail pipeline**: Hardware-adaptive — detects NVMe/SSD, discrete GPU, CPU cores. Uses a bounded `rayon::ThreadPool` (`thumb_pool`). Optional GPU path via `wgpu` (feature `gpu`) for resize + BC7 encoding. BC7 atlas is mmap-backed for GPU-direct reads.
- **Dependencies in dev builds**: `[profile.dev.package."*"] opt-level = 2` — image codecs, SIMD resize, BC7 encoding, and SQLite are unusably slow at opt-level 0.
- **Plugin system**: Plugins are directories with a `manifest.json`. Execution is CLI-based (spawned process or long-running daemon). Example: `plugins/wd-tagger/` (ML image tagger).

### Cargo features

- `gpu` (default): Enables `wgpu`/`pollster` for GPU-accelerated thumbnail pipeline
- `custom-protocol` (default): Tauri custom protocol for production builds

### Linux-specific

`main.rs` forces `GDK_BACKEND=x11` and `WEBKIT_DISABLE_DMABUF_RENDERER=1` to work around WebKitGTK/Wayland crashes.

## Tech Stack

- **Rust 2024 edition**, Tauri 2, rusqlite (bundled SQLite), rayon, tokio, wgpu, image, fast_image_resize, libheif-rs
- **TypeScript**, SolidJS, Tailwind CSS v4, Vite 5
