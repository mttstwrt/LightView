# LightView

A fast local media gallery with pluggable auto-tagging. Point it at a folder of images
and videos and LightView indexes the contents, generates thumbnails, extracts metadata,
and gives you a browsable grid with filtering, sorting, grouping, a full-resolution
viewer, a map view, auto-tagging plugins, and optional access from your phone over
the LAN.

It is a desktop app built with **Tauri 2** (Rust backend) and **SolidJS** (TypeScript
frontend). All heavy lifting — file I/O, image decoding, thumbnailing, SQLite — happens
in the Rust process; the UI is a webview that talks to it over IPC.

---

## Features at a glance

- **Grid, justified, and map views** of any local folder
- **Full-resolution viewer** with keyboard navigation, ratings, tags, and an info panel
- **Thumbnail cache** in SQLite, hardware-adaptive, with optional **GPU** acceleration
- **Filter query language** over tags, ratings, dates, dimensions, media type, and color labels
- **Sort & group** by date, name, size, rating, view history, media type, and more
- **Companion sidecar files** (`.lightview` JSON) holding tags, ratings, notes, and metadata
- **Auto-tagging plugins** (ML taggers like WD/Camie/PixAI) via a simple CLI protocol
- **Duplicate detection**
- **Remote web access** — browse and upload from a phone on the same network, with device pairing and an optional password
- **Per-gallery settings**, persisted in the gallery's own `.lightview/settings.toml`
- Wide format support, including **HEIC/HEIF auto-transcoding** and a **GIF frame atlas** for smooth playback

### Supported formats

**Images:** JPEG, PNG, WebP, AVIF, HEIC/HEIF, BMP, TIFF, GIF
**Videos:** MP4, WebM, MKV, MOV, AVI, M4V

HEIC/HEIF files are transcoded to JPEG on the fly when served at full resolution.

---

## Building & running

Prerequisites: a Rust toolchain (2024 edition), Node.js, and the Tauri 2 system
dependencies for your platform. On Linux you also need WebKitGTK.

```bash
# Install frontend deps
npm install

# Run the app in development (Vite dev server + Tauri window)
cargo tauri dev

# Production build
cargo tauri build

# Frontend only (Vite dev server on :5173)
npm run dev
```

Rust-side checks, tests, and benchmarks (run from `src-tauri/`):

```bash
cargo check
cargo test
cargo bench --bench thumbnailer
cargo bench --bench cache_db
```

### Cargo features

| Feature | Default | Purpose |
|---|---|---|
| `gpu` | ✅ | GPU-accelerated thumbnail pipeline via `wgpu` (fused crop+resize) |
| `custom-protocol` | ✅ | Tauri custom URI protocol, required for production builds |

### Linux note

`main.rs` forces `GDK_BACKEND=x11` and `WEBKIT_DISABLE_DMABUF_RENDERER=1` to work
around WebKitGTK/Wayland crashes. You can override the GTK backend from the in-app
render settings (see below); a restart is required for it to take effect.

---

## How it works under the hood

### Two-process model

LightView is split across two processes that share nothing but an IPC channel:

- **Backend (`src-tauri/`, Rust):** owns all I/O and CPU-heavy work — opening galleries,
  walking the filesystem, decoding images, generating thumbnails, running the SQLite
  cache, evaluating filters, and spawning plugins. Each capability is a
  `#[tauri::command]` registered in `src-tauri/src/main.rs`.
- **Frontend (`src-solidjs/`, SolidJS):** a single-page app rendered in the webview.
  It holds UI state in SolidJS signals (`stores/`) and reaches the backend *only*
  through the wrappers in `src-solidjs/lib/ipc.ts`. That file is the single IPC boundary.

### Serving pixels: custom protocols and a local HTTP server

Thumbnails and full-resolution stills are served through a custom `lightview://`
URI protocol, which avoids JSON-encoding image bytes:

- `lightview://thumb/<path>` — a cached thumbnail straight from SQLite
- `lightview://media/<path>` — the full-resolution file (HEIC/HEIF transcoded to JPEG)

Video is the exception. WebKitGTK refuses to play `<video>` from a custom scheme,
so videos (and the GIF frame atlas) are served from a small local **axum** HTTP server
running in the same process.

### The cache database

Each gallery gets a SQLite database (bundled `rusqlite`) that stores generated
thumbnails, the searchable index, per-folder counts, duplicate-detection state, and
the GIF frame atlas. Thumbnails are served with a one-hour cache lifetime, so any
code path that changes thumbnail bytes must bump a version query parameter to avoid
serving stale images.

The connection is held behind a `Mutex` (SQLite's `Connection` is `Send` but not
`Sync`); the rest of the shared `AppState` lives behind `Arc<RwLock<>>`/`Arc<Mutex<>>`.

### The thumbnail pipeline

Thumbnailing is hardware-adaptive. On open, LightView detects your storage type
(NVMe/SSD/HDD), CPU core count, RAM, and discrete GPU, and tunes a bounded
`rayon` thread pool accordingly. With the `gpu` feature and a suitable GPU, a `wgpu`
path performs a fused crop-and-resize.

Thumbnails are generated in **tiers**: a small grid tier plus higher-resolution tiers
that are produced lazily for cells you actually view zoomed in (the justified view's
"high detail" mode serves a ~1600px aspect-preserving tier on demand), so disk cost
scales with use rather than library size.

### Companion files (sidecar metadata)

Per-image metadata — user tags, auto tags, plugin tags, rating, color label, notes,
and core media info (dimensions, duration, codec, fps, audio) — lives in a small
`.lightview` JSON **companion file**. These are the source of truth for editable
metadata; the SQLite index is a derived, rebuildable cache. Companion files can be
stored either in the gallery's `.lightview` folder or directly **alongside** each
media file (see the `companion_location` setting).

### Filtering, sorting, grouping

The filter bar accepts a small query language (`filter/ast` → `parser` → `evaluator`).
Examples:

```
vacation                               # match the term in any namespace
user::vacation                         # only user tags
plugin.wd-tagger::1girl                # a specific plugin namespace
user::vacation AND user::family        # boolean AND
user::vacation OR user::travel         # boolean OR
NOT video                              # negation
rating>=4                              # rating comparison (>=, <=, =)
date>=2024-01-01                       # capture date on/after
date=2024                              # captured during 2024
added<=2024-06                         # date added to the library
viewed>=2023                           # last viewed
width>=1920  height<=1080              # pixel dimensions
filesize>10mb                          # file size (kb / mb / gb)
image  /  video  /  gif                # media type
```

Sorting and grouping are handled by the `sort/` module. Sort fields: **date, name,
size, rating, media type, last viewed, date added, last rated**, each ascending or
descending, with an optional secondary sort. Grouping options: time period
(day/month/year), media type, size range, tag prefix, or none. A timeline index lets
you jump around large date-sorted libraries quickly.

### Plugins

Plugins are directories containing a `manifest.json`. Execution is CLI-based: LightView
spawns the configured command as a subprocess, streams it image paths as NDJSON on
stdin, and reads tags back on stdout, storing them under the plugin's `tag_prefix`
namespace. The bundled `plugins/` folder includes several ML auto-taggers (WD, Camie,
PixAI) plus a minimal `example-auto-tagger` that documents the protocol. Plugins can be
installed, listed, run on a single file, or run as a cancellable batch.

> **Scope today:** the host implements exactly one plugin verb — *image → tags* — so
> every plugin is in practice an auto-tagger. Plugins cannot yet add views, panels,
> commands, or other UI, and the built-in grid / justified / map views are native, not
> plugins. A design for broadening this into real extensibility (more plugin verbs,
> per-gallery enablement, and a sandboxed view surface) lives in
> [`docs/pluginExtensibility.md`](docs/pluginExtensibility.md).

### Remote web access

You can expose the open gallery to other devices on your LAN. The same axum server
binds `0.0.0.0`, devices **pair** by scanning a QR code (or entering a pairing code),
and each paired device gets its own cookie. Access can be protected with a password
and is gated by an inactivity timeout (default 6 hours). Paired devices can also
**upload** media into the gallery, filed into a configurable folder scheme
(`Uploads/<year>/`, `Uploads/<year>/<month>/`, or `Uploads/<year>/<album>/`).

---

## Settings & knobs

Settings are **per-gallery**: they live in that gallery's `.lightview/settings.toml`,
are loaded when the gallery opens, and a fresh gallery starts from the built-in
defaults. Editing the TOML by hand while the app is open is picked up live via a
filesystem watcher. The table below lists every configurable value, its default, and
what it does.

### Display

| Setting | Default | Description |
|---|---|---|
| `thumbnail_size` | `200` | Grid thumbnail / justified row target size, in px |
| `thumb_size_min` | `120` | Smallest size the zoom control allows (px) |
| `thumb_size_max` | `700` | Largest size the zoom control allows (px) |
| `grid_gap` | `2` | Gap between grid cells (px) |
| `background_color` | `#0a0a0a` | Gallery background color |
| `video_hover_preview` | `false` | Preview videos on hover in the grid |
| `video_autoplay_loop` | `false` | Loop video playback in the viewer |
| `gif_autoplay_grid` | `false` | Autoplay GIFs in the grid |
| `video_autoplay_grid` | `false` | Autoplay short videos in the grid (muted, looping) |
| `video_autoplay_max_seconds` | `30` | Max video duration eligible for grid autoplay (s) |
| `scroll_blur` | `false` | Blur thumbnails while scrolling fast |
| `map_dark_mode` | `true` | Use the dark map tiles in the map view |
| `justified_high_detail` | `true` | Serve a 1600px tier in the justified view when zoomed, instead of upscaling the 512px tier (generated only for visible cells) |

### Performance

| Setting | Default | Description |
|---|---|---|
| `preload_count` | `3` | How many neighbors to preload around the viewer's current image |
| `lru_cache_size` | `5` | Number of full-resolution images kept in the in-memory LRU |
| `thumbnail_threads` | `6` | Worker threads in the thumbnailing pool |

### Storage

| Setting | Default | Description |
|---|---|---|
| `companion_location` | `lightview_folder` | Where companion files go: `lightview_folder` (in `.lightview/`) or `alongside` (next to each media file) |

### Default filter

| Setting | Default | Description |
|---|---|---|
| `default_filter.enabled` | `false` | Apply a filter automatically when the gallery opens (desktop and web) |
| `default_filter.query` | `""` | The query to apply (same syntax as the filter bar) |

### External apps

`external_apps` is a list of "Open with…" targets shown in the context menu. Each entry
is `{ label, command, args }`, where `{file}` in `args` is replaced with the media path.
Defaults ship with Gwenview and GIMP:

```jsonc
[
  { "label": "Gwenview", "command": "gwenview", "args": ["{file}"] },
  { "label": "GIMP",     "command": "gimp",     "args": ["{file}"] }
]
```

### Render config (process-level)

Separate from per-gallery settings, the render configuration controls how the webview
itself is created and is read at process start:

| Setting | Description |
|---|---|
| `gpu_acceleration` | Toggle GPU compositing for the webview (restart required) |
| `gtk_backend` | Override the GTK backend, e.g. `x11` (Linux; restart required) |

### Remote access & uploads

Configured from the in-app remote-access panel rather than the settings file:

- Enable/disable LAN access (binds `0.0.0.0`)
- Generate pairing codes / QR codes for new devices; revoke or delete paired devices
- Set or clear a password
- Set the inactivity timeout (default 6 hours)
- Upload config: enable/disable, and choose the folder scheme — `Year`,
  `YearMonth`, or `YearAlbum`

---

## Keyboard shortcuts

**Viewer**

| Key | Action |
|---|---|
| `←` / `→` | Previous / next image |
| `Esc` | Close viewer |
| `i` | Toggle the info panel |
| `Tab` | Focus the tag input (when the info panel is open) |
| `0`–`5` | Set rating on the current image |

**Gallery**

| Key | Action |
|---|---|
| `Esc` | Clear selection |
| `Ctrl/Cmd + A` | Select all |
| `F11` | Toggle fullscreen |
| `F12` | Open the debug overlay |

---

## Project layout

```
src-tauri/        Rust backend (Tauri)
  src/
    commands/     Tauri command handlers (thin wrappers over domain modules)
    provider/     FileProvider trait + registry (local only today; SMB/SFTP/S3 are future targets the trait leaves room for)
    companion/    .lightview sidecar files (schema, reader, writer, migration)
    cache/        SQLite cache: db, thumbnails, index, counts, duplicates
    pipeline/     Thumbnail generation (CPU thumbnailer + optional GPU pipeline)
    filter/       Filter query language: ast → parser → evaluator
    sort/         Sorting, grouping, timeline
    autocomplete/ In-memory tag autocomplete
    plugin/       External plugin system (manifest, runner, daemon)
    hardware/     Hardware detection → adaptive performance tuning
    http_server/  Local axum server: video, GIF atlas, remote LAN access, uploads
    util/         paths, hashing, fs watching

src-solidjs/      SolidJS frontend
  stores/         Global UI state (gallery, viewer, settings, filter, tags, plugins)
  lib/ipc.ts      The single IPC boundary — every invoke() call lives here
  lib/types.ts    Shared TypeScript types (mirror the Rust structs)
  components/      gallery/, viewer/, topbar/, map/, debug/

plugins/          Bundled plugins (wd-tagger, camie-tagger, pixai-tagger, example)
```

---

## License

See individual plugin manifests for their licenses. Project license: add as appropriate.
