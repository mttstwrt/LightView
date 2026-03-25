# Photo Gallery App — Project Overview

<p style="color: #888; font-size: 1.1em; margin-top: -0.5em;">
A fast, plugin-extensible media gallery with tagging, filtering, and remote file support.
</p>

---

## 1. Design philosophy

The core app is deliberately thin. It does exactly three things well: **display media fast**, **manage tags**, and **parse companion data files**. Everything else — auto-tagging, backup, syncing, organization — lives in plugins that read and write to those companion files. The core never needs to know what a plugin does; it only needs to know how to read the data a plugin leaves behind.

This separation means the desktop app stays lean and fast, the plugin ecosystem can grow independently, and a future mobile app only needs to implement a companion file reader to get full tag/filter support for free.

The app supports images, videos, and GIFs as first-class media types, with the rendering pipeline branching based on media type while the tagging/filtering system treats them all uniformly.

---

## 2. Companion data file format

Every media file gets a sidecar `.lightview.json` file that lives alongside it. The schema is versioned and namespaced so plugins never clobber each other.

### 2.1 Schema (v1)

```json
{
  "schema_version": 1,
  "file": "clip.mp4",
  "file_hash": "sha256:a1b2c3d4...",
  "media_type": "video",
  "created": "2026-03-20T14:30:00Z",
  "modified": "2026-03-22T09:15:00Z",
  "tags": {
    "user": ["vacation", "family", "favorites"],
    "auto": [],
    "plugins": {
      "face-recognition": {
        "version": "1.2.0",
        "tags": ["person:alice", "person:bob"],
        "confidence": { "person:alice": 0.97, "person:bob": 0.84 }
      },
      "geo-tagger": {
        "version": "0.5.0",
        "tags": ["location:Paris", "country:France"],
        "data": { "lat": 48.8566, "lon": 2.3522 }
      }
    }
  },
  "meta": {
    "core": {
      "rating": 4,
      "color_label": "green",
      "notes": "Great sunset shot from the hotel balcony",
      "media": {
        "width": 1920,
        "height": 1080,
        "duration_seconds": 34.5,
        "codec": "h264",
        "has_audio": true,
        "fps": 30.0
      }
    },
    "plugins": {
      "backup-sync": {
        "version": "2.0.0",
        "last_synced": "2026-03-21T12:00:00Z",
        "remote_id": "abc123",
        "provider": "s3"
      }
    }
  }
}
```

### 2.2 Media type handling

The `media_type` field is one of `"image"`, `"video"`, or `"gif"`. The core uses this to select the correct rendering and thumbnail pipeline before opening the file. The `meta.core.media` block varies by type:

- **Image:** only `width` and `height` are populated; `duration_seconds`, `codec`, `has_audio`, and `fps` are null or absent.
- **GIF:** `width`, `height`, `duration_seconds`, and `fps` are populated; `has_audio` and `codec` are absent.
- **Video:** all fields are populated.

### 2.3 Schema design rules

| Rule | Rationale |
|------|-----------|
| `file_hash` is SHA-256 of the media bytes | Detects renamed/moved files without breaking associations |
| `media_type` at the root | Core selects rendering path and thumbnail strategy before opening the file |
| `tags.user` is a flat string array | Fast to read, easy to edit manually |
| `tags.plugins.<name>` is namespaced per plugin | Plugins never collide; you can filter "show only user tags" vs "show all" |
| Each plugin section carries its own `version` | Companion files remain parseable even when plugin versions change |
| `meta.core.media` holds format-specific properties | Video duration, codec, fps live here; plugins don't need to care about them |
| `meta.plugins.<name>` holds arbitrary plugin data | Plugins can store non-tag data (sync state, coordinates, etc.) without polluting the tag namespace |
| `schema_version` at the root | Enables future migrations; readers check this first |

### 2.4 File naming and placement

```
/photos/
  vacation/
    sunset.jpg
    sunset.jpg.lightview.json         ← companion file
    family-dinner.png
    family-dinner.png.lightview.json
    beach-clip.mp4
    beach-clip.mp4.lightview.json
  .lightview/
    cache.db                      ← SQLite thumbnail + index cache
    config.json                   ← per-gallery settings
```

The `.lightview.json` suffix (not replacing the extension) means companion files sort next to their media files in any file manager and won't collide with other sidecar formats. The `.lightview/` directory at the gallery root holds the local cache DB and gallery-level config.

---

## 3. Tech stack

### 3.1 Core choices

| Component | Choice | Rationale |
|-----------|--------|-----------|
| **App framework** | Tauri 2.x | Native performance, small binary (~5 MB vs Electron's ~150 MB), cross-platform, Rust backend |
| **Backend language** | Rust | Memory safety, zero-cost abstractions, `rayon` for parallel thumbnail generation, `image-rs` for decoding |
| **Frontend framework** | SolidJS | Fine-grained reactivity without virtual DOM diffing — critical for galleries with 10k+ thumbnails |
| **Styling** | Tailwind CSS | Utility-first, tree-shakes unused styles, fast iteration |
| **Local database** | SQLite (via `rusqlite`) | Thumbnail cache, tag index, tag counts, fast filtered queries |
| **IPC** | Tauri commands (Rust ↔ JS) | Type-safe, async, built into Tauri |
| **Image decoding** | `image-rs` + platform-native decoders | `image-rs` for common formats; delegate HEIF/RAW to OS APIs where available |
| **Video thumbnails** | `ffmpeg-next` (Rust FFmpeg bindings) | Frame extraction at ~2s mark for video thumbnails; GIF first-frame extraction |
| **GPU rendering** | WebGPU (via frontend) | Hardware-accelerated scaling and compositing in the viewer |

### 3.2 Key Rust crates

```toml
[dependencies]
tauri = { version = "2", features = ["shell-open-api"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
rusqlite = { version = "0.31", features = ["bundled"] }
image = "0.25"
sha2 = "0.10"
rayon = "1.10"
tokio = { version = "1", features = ["full"] }
notify = "6"                    # filesystem watcher
walkdir = "2"                   # recursive directory traversal
lru = "0.12"                    # LRU cache for decoded images
opendal = "0.47"                # unified storage abstraction (local, S3, SFTP, etc.)
ffmpeg-next = "7"               # video frame extraction for thumbnails
fuzzy-matcher = "0.3"           # fuzzy tag autocomplete matching
kamadak-exif = "0.5"            # EXIF metadata extraction (date, GPS, camera)
sysinfo = "0.30"                # CPU/RAM detection for hardware profile
nix = { version = "0.29", features = ["fs"] }  # statfs, ioctl for btrfs detection
```

---

## 4. Project file structure

```
lightview/
├── src-tauri/                          # Rust backend
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── src/
│   │   ├── main.rs                     # Tauri bootstrap + app setup
│   │   ├── lib.rs                      # Module declarations
│   │   │
│   │   ├── companion/                  # Companion file I/O
│   │   │   ├── mod.rs
│   │   │   ├── schema.rs               # Serde structs for .lightview.json
│   │   │   ├── reader.rs               # Parse + validate companion files
│   │   │   ├── writer.rs               # Atomic writes (write-tmp + rename)
│   │   │   └── migration.rs            # Schema version upgrades
│   │   │
│   │   ├── provider/                   # FileProvider abstraction
│   │   │   ├── mod.rs                  # FileProvider trait definition
│   │   │   ├── local.rs                # Local filesystem provider
│   │   │   ├── smb.rs                  # SMB/CIFS provider
│   │   │   ├── sftp.rs                 # SFTP provider
│   │   │   └── s3.rs                   # S3-compatible provider
│   │   │
│   │   ├── cache/                      # SQLite cache layer
│   │   │   ├── mod.rs
│   │   │   ├── db.rs                   # Connection pool + schema init
│   │   │   ├── thumbnails.rs           # Thumbnail blob storage + mtime checks
│   │   │   ├── index.rs                # Tag index for fast filtering
│   │   │   └── counts.rs              # Tag frequency counts + autocomplete
│   │   │
│   │   ├── pipeline/                   # Media processing pipeline
│   │   │   ├── mod.rs
│   │   │   ├── thumbnailer.rs          # Parallel thumbnail generation (rayon)
│   │   │   ├── video_thumbnailer.rs    # FFmpeg frame extraction for video/gif
│   │   │   ├── decoder.rs              # Full-res async decoding
│   │   │   ├── prefetcher.rs           # N+1/N+2 prefetch logic
│   │   │   └── gpu.rs                  # WebGPU texture upload helpers
│   │   │
│   │   ├── plugin/                     # Plugin system
│   │   │   ├── mod.rs
│   │   │   ├── manager.rs              # Discovery, registration, lifecycle
│   │   │   ├── runner.rs               # CLI plugin executor
│   │   │   ├── manifest.rs             # Plugin manifest parsing
│   │   │   └── wasm.rs                 # Future: WASM plugin host
│   │   │
│   │   ├── filter/                     # Query / filter engine
│   │   │   ├── mod.rs
│   │   │   ├── ast.rs                  # Filter expression AST
│   │   │   ├── parser.rs               # Query string → AST
│   │   │   └── evaluator.rs            # AST evaluation against companion data
│   │   │
│   │   ├── autocomplete/               # Tag autocomplete engine
│   │   │   ├── mod.rs
│   │   │   └── engine.rs               # In-memory tag cache + fuzzy matching
│   │   │
│   │   ├── sort/                        # Sorting and grouping engine
│   │   │   ├── mod.rs
│   │   │   ├── sorter.rs               # Sort by date/size/name/rating
│   │   │   ├── grouper.rs             # Group by time period/tag/type/size range
│   │   │   └── timeline.rs            # Date range extraction for scroll indicators
│   │   │
│   │   ├── hardware/                    # Hardware capability detection
│   │   │   ├── mod.rs
│   │   │   ├── storage.rs              # NVMe/SSD/HDD detection, filesystem type
│   │   │   └── gpu.rs                  # GPU compute shader capability check
│   │   │
│   │   ├── commands/                   # Tauri IPC command handlers
│   │   │   ├── mod.rs
│   │   │   ├── gallery.rs              # List/open/navigate galleries
│   │   │   ├── media.rs                # Thumbnail + full-res requests
│   │   │   ├── tags.rs                 # Tag CRUD operations
│   │   │   ├── filter.rs               # Filter execution
│   │   │   ├── autocomplete.rs         # Tag autocomplete queries
│   │   │   ├── sort.rs                # Sort/group operations
│   │   │   └── plugins.rs              # Plugin management commands
│   │   │
│   │   └── util/                       # Shared utilities
│   │       ├── mod.rs
│   │       ├── hash.rs                 # SHA-256 file hashing (lazy)
│   │       └── fs_watch.rs             # Filesystem change watcher
│   │
│   └── plugins/                        # Bundled plugin manifests
│       └── example-plugin/
│           └── manifest.json
│
├── src/                                # SolidJS frontend
│   ├── index.html
│   ├── index.tsx                       # App entry point
│   ├── App.tsx                         # Root component + routing
│   │
│   ├── components/
│   │   ├── gallery/
│   │   │   ├── GalleryGrid.tsx         # Virtualized square-crop thumbnail grid
│   │   │   ├── ThumbnailCell.tsx       # Single thumbnail (square crop + overlay)
│   │   │   ├── VideoThumbnail.tsx      # Video thumbnail with duration badge
│   │   │   └── GalleryShell.tsx        # Full-bleed container, no chrome
│   │   │
│   │   ├── viewer/
│   │   │   ├── MediaViewer.tsx         # Full-res viewer (image/video/gif switch)
│   │   │   ├── ImageViewer.tsx         # Image zoom/pan with WebGPU
│   │   │   ├── VideoPlayer.tsx         # Native <video> with controls
│   │   │   ├── ViewerToolbar.tsx       # Navigation, info panel toggle
│   │   │   └── InfoPanel.tsx           # Tags grouped by namespace, metadata
│   │   │
│   │   ├── topbar/
│   │   │   ├── TopBar.tsx              # Auto-hiding top bar (hover-reveal)
│   │   │   ├── FilterBar.tsx           # Filter input with autocomplete dropdown
│   │   │   ├── AutocompleteDropdown.tsx # Tag suggestions with namespace badges
│   │   │   └── SettingsButton.tsx      # Gear icon → settings panel
│   │   │
│   │   ├── settings/
│   │   │   ├── SettingsPanel.tsx       # Slide-out or modal settings
│   │   │   ├── DisplaySettings.tsx     # Thumbnail size, spacing, columns
│   │   │   ├── PerformanceSettings.tsx # Prefetch count, cache size
│   │   │   └── MaintenanceSettings.tsx # Re-index button, cache clear
│   │   │
│   │   ├── tags/
│   │   │   ├── TagEditor.tsx           # Add/remove user tags
│   │   │   ├── TagBadge.tsx            # Color-coded tag pill by namespace
│   │   │   └── TagGroup.tsx            # Collapsible namespace group
│   │   │
│   │   ├── context-menu/
│   │   │   ├── ContextMenu.tsx         # Right-click menu shell
│   │   │   └── ExternalApps.tsx        # "Open in..." submenu
│   │   │
│   │   └── shared/
│   │       ├── VirtualScroller.tsx     # Windowed grid for large galleries
│   │       ├── ScrollTimeline.tsx      # Scrollbar date indicator overlay
│   │       ├── DateSeparator.tsx       # Inline date group header row
│   │       ├── SortGroupMenu.tsx       # Sort/group options dropdown
│   │       ├── SkeletonCell.tsx        # Loading placeholder (square)
│   │       └── Modal.tsx
│   │
│   ├── stores/                         # SolidJS reactive stores
│   │   ├── galleryStore.ts             # Current gallery state
│   │   ├── viewerStore.ts              # Current media + prefetch state
│   │   ├── filterStore.ts              # Active filters + autocomplete state
│   │   ├── sortStore.ts                # Active sort/group settings + timeline data
│   │   ├── tagStore.ts                 # Tag operations
│   │   └── settingsStore.ts            # User preferences (persisted)
│   │
│   ├── hooks/
│   │   ├── useGallery.ts              # Gallery data fetching
│   │   ├── useMedia.ts                # Media loading + progressive swap
│   │   ├── useTags.ts                 # Tag mutations
│   │   ├── useAutocomplete.ts         # Debounced autocomplete queries
│   │   ├── useTopBar.ts               # Hover detection + show/hide logic
│   │   ├── useScrollVelocity.ts       # Scroll speed tracking for adaptive loading
│   │   ├── useTimeline.ts             # Scroll position → date range mapping
│   │   └── useContextMenu.ts          # Right-click handler
│   │
│   ├── lib/
│   │   ├── ipc.ts                     # Typed Tauri invoke wrappers
│   │   ├── types.ts                   # Shared TypeScript interfaces
│   │   └── gpu.ts                     # WebGPU canvas setup
│   │
│   └── styles/
│       ├── global.css
│       └── tailwind.config.ts
│
├── plugins/                            # Example / bundled plugins
│   └── example-auto-tagger/
│       ├── manifest.json
│       └── main.py                     # Example CLI plugin
│
├── shared/                             # Shared types (Rust ↔ TS codegen target)
│   └── types.rs                        # Source of truth → generates TS types
│
├── package.json
├── tsconfig.json
├── vite.config.ts
└── README.md
```

---

## 5. User interface specification

### 5.1 Design philosophy

The UI is **nothing but images**. No permanent chrome, no sidebars, no toolbars visible by default. The gallery fills the entire window edge-to-edge. All controls reveal on demand and disappear when not needed. The app should feel like looking at a wall of photos, not using software.

Dark background (`#0a0a0a` or similar very dark neutral) behind the grid so the images are the only source of color on screen. No borders, no card shadows, no frames around thumbnails.

### 5.2 Gallery grid (default view)

The default layout is a **square-crop grid** similar to Apple Photos and Google Photos:

- Images display as **uniform squares** in a responsive grid that fills the viewport width edge-to-edge.
- Each square is a center-crop of the image via CSS `object-fit: cover` on the thumbnail.
- The grid has **no outer padding** — images go right to the window edges.
- **Gap between images** is configurable in settings (default: 2px). This thin gap creates a subtle visual separation without wasting space.
- **Thumbnail size** is configurable (default: 200px). The grid auto-calculates column count: `columns = floor(viewport_width / thumbnail_size)`. Remaining space is distributed evenly across columns so images stretch slightly to fill the row.
- Scrolling is vertical. The virtual scroller only renders rows visible in the viewport plus a 2-row buffer above and below.

**Thumbnail states:**

- **Loading:** a flat dark gray skeleton square, no shimmer animation (keeps it minimal).
- **Loaded:** image fades in with a fast opacity transition (~150ms).
- **Hover:** subtle brightness boost (`filter: brightness(1.1)`) and a very faint scale (`transform: scale(1.02)`) with a 100ms transition. No borders or outlines.
- **Selected:** thin white outline (1px inset) and a small checkmark in the top-left corner. Multiple selection via shift+click or ctrl+click.

**Video/GIF indicators:**

- Videos show a small semi-transparent duration badge in the bottom-right corner (e.g., "0:34") using a compact monospace font. No play button icon — keep it clean.
- GIFs show a small "GIF" text badge in the same position.
- On hover, videos and GIFs could optionally play a short preview (configurable in settings, off by default to save resources).

### 5.3 Top bar (auto-hiding)

The top bar is **invisible by default**. It slides down when the cursor enters the top ~60px of the window, and slides back up when the cursor leaves the bar area. Transition: 200ms ease-out slide + fade.

The bar itself is a **translucent dark panel** (`background: rgba(10, 10, 10, 0.85)`, `backdrop-filter: blur(12px)`) that overlays the gallery grid. Height: ~48px. It contains exactly two elements:

**Left: Filter bar** — takes up most of the width. A single text input with a subtle border, placeholder text "Filter..." in muted color. As the user types:

- A dropdown appears below the input showing tag suggestions from the autocomplete engine.
- Each suggestion shows: the tag name, a small color-coded namespace badge (e.g., a teal pill reading "user", a coral pill for "face-recognition"), and a muted count number on the right.
- Arrow keys navigate suggestions, Enter/click selects.
- Selected tags appear as removable pills inside the input (before the cursor), each color-coded by namespace.
- The filter bar supports the full query syntax: boolean operators (AND, OR, NOT), namespace prefixes, rating and media type filters. For most users, just clicking suggested tags is enough.

**Right: Settings button** — a single gear icon (no label). Click opens the settings panel.

When the gallery is scrolled to the very top (scroll position 0), the top bar can optionally stay visible since there's no content it would obscure.

### 5.4 Settings panel

Opens as a **slide-out panel from the right edge** (~320px wide), same translucent dark style as the top bar. Closes by clicking outside, pressing Escape, or clicking the X.

Settings are organized into sections:

**Display**

| Setting | Control | Default | Range |
|---------|---------|---------|-------|
| Thumbnail size | Slider | 200px | 80–400px |
| Grid gap | Slider | 2px | 0–16px |
| Dark background | Color picker | #0a0a0a | Any dark color |
| Video hover preview | Toggle | Off | On/Off |
| GIF auto-play in grid | Toggle | Off | On/Off |

**Performance**

| Setting | Control | Default | Range |
|---------|---------|---------|-------|
| Images to preload | Slider | 3 | 1–10 |
| LRU cache size | Slider | 5 images | 1–20 |
| Thumbnail thread count | Slider | CPU count / 2 | 1–CPU count |
| Thumbnail quality | Slider | 80 | 40–100 |

**Maintenance**

| Setting | Control | Notes |
|---------|---------|-------|
| Full re-index | Button | Rebuilds SQLite tag index from all companion files |
| Rebuild thumbnails | Button | Clears and regenerates all thumbnails |
| Clear cache | Button | Deletes cache.db entirely |
| Gallery statistics | Read-only | Total media count, index size, cache size, tag count |

All settings persist to a local config file and take effect immediately (no restart required). Thumbnail size and grid gap changes are reflected live as the sliders move.

### 5.5 Image/media viewer

Clicking a thumbnail opens the **full-screen media viewer**. The gallery grid disappears and is replaced by the media filling the screen.

**Image viewing:**

- The image displays centered on the dark background, scaled to fit within the viewport (`object-fit: contain`).
- The cached thumbnail shows instantly, then the full-resolution image fades in on top when decoded (~50–200ms).
- Mouse scroll zooms. Click-drag pans when zoomed. Double-click resets to fit.
- Left/right arrow keys or swipe (for future touch) navigate between images. Navigation is instant because N+1 and N+2 are prefetched.

**Video viewing:**

- The native `<video>` element displays centered, same layout as images.
- Minimal custom controls appear on hover at the bottom: play/pause, timeline scrubber, volume, fullscreen. The controls match the translucent dark style.
- No autoplay — the video is paused on the first frame when opened.

**GIF viewing:**

- Renders as an `<img>` tag, auto-playing. Same centered layout.

**Viewer chrome:**

- Escape or clicking the background closes the viewer and returns to the gallery grid at the same scroll position.
- A small info panel can be toggled open from the right edge (keyboard shortcut: `I`). It shows: filename, resolution, file size, media type, and all tags grouped by namespace in collapsible sections. Each namespace section shows a header like "User tags (5)" or "Face recognition (12)" and expands to show the tag pills. Plugin namespaces default to collapsed.
- Right-click anywhere in the viewer opens the context menu.

### 5.6 Sorting, grouping, and timeline scrollbar

**Default sort: date (newest first).** Date is extracted from EXIF metadata when available, falling back to filesystem mtime. The sort control is accessible from the top bar (a small dropdown next to the filter bar) or from the settings panel.

**Sort options:**

| Sort by | Source | Notes |
|---------|--------|-------|
| Date | EXIF `DateTimeOriginal` → mtime fallback | Default, newest first |
| File size | Filesystem stat | Useful for finding large videos or RAW files |
| Filename | Alphabetical | A-Z or Z-A |
| Rating | `meta.core.rating` from companion file | Unrated items sort last |
| Media type | image → gif → video (or reverse) | Groups media types together |

**Grouping** inserts visual separator rows into the grid. Each group gets a header row — a thin, left-aligned label (e.g., "March 2026" or "Videos > 100 MB") spanning the full grid width, with subtle styling that doesn't break the image-forward feel. Groups are collapsible by clicking the header.

| Group by | Behavior |
|----------|----------|
| Time period (day/month/year) | Default grouping when sorted by date. Headers show "March 15, 2026" (day), "March 2026" (month), or "2026" (year). Auto-selects granularity based on gallery date range. |
| Media type | Three groups: Images, GIFs, Videos |
| File size range | Buckets: < 1 MB, 1–10 MB, 10–100 MB, 100 MB–1 GB, > 1 GB |
| Tag | Group by any specific tag — e.g., group by `plugin.geo-tagger:country:*` to get location-based albums |
| None | Flat list, no separators |

**Timeline scrollbar.** When sorted by date, the scrollbar gains a floating date indicator. As the user scrolls (or drags the scrollbar thumb), a small translucent label appears anchored to the scrollbar position showing the date range of the currently visible rows. The label updates in real-time during scroll. Format adapts to zoom level: "Mar 2026" for month-level, "Mar 15" for day-level, "2024" for year-level.

Additionally, subtle tick marks along the scrollbar track indicate date boundaries — denser ticks where more photos exist (e.g., a vacation week) and sparse ticks for quiet periods. This gives the user a visual sense of their photo distribution over time without any explicit UI chrome.

The timeline data is precomputed: on gallery open, the backend sends an ordered array of `(row_index, date)` pairs for each group boundary. The frontend maps scroll position to this array with binary search to find the current date label in O(log n).

### 5.7 Context menu

Right-click on a thumbnail or in the viewer shows a native-style context menu:

```
Open with            ▸  Gwenview
                        GIMP
                        System default
──────────────────────
Edit tags...
Copy tags from...
──────────────────────
Plugins              ▸  (dynamic list from installed plugins)
──────────────────────
Show in file manager
Copy path
File info
```

The "Open with" entries are user-configurable in settings. On Linux, apps are launched via direct binary invocation or `xdg-open`. Tauri's `shell-open-api` handles the platform-specific details.

### 5.8 Keyboard shortcuts

| Key | Action |
|-----|--------|
| `Escape` | Close viewer / close settings / clear filter |
| `←` `→` | Previous / next image in viewer |
| `I` | Toggle info panel in viewer |
| `F` | Focus filter bar (reveals top bar) |
| `T` | Open tag editor for selected image(s) |
| `/` | Focus filter bar (alternative) |
| `Delete` | Remove selected images from selection (not from disk) |
| `Ctrl+A` | Select all (in grid) |
| `Home` / `End` | Scroll to top / bottom of gallery |
| `+` / `-` | Increase / decrease thumbnail size |

---

## 6. Rust data structures

### 6.1 Companion file types (`companion/schema.rs`)

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanionFile {
    pub schema_version: u32,
    pub file: String,
    pub file_hash: String,
    pub media_type: MediaType,
    pub created: String,       // ISO 8601
    pub modified: String,
    pub tags: TagCollection,
    pub meta: MetaCollection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Image,
    Video,
    Gif,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagCollection {
    pub user: Vec<String>,
    pub auto: Vec<String>,
    pub plugins: HashMap<String, PluginTagEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginTagEntry {
    pub version: String,
    pub tags: Vec<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaCollection {
    pub core: Option<CoreMeta>,
    pub plugins: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreMeta {
    pub rating: Option<u8>,           // 0-5
    pub color_label: Option<String>,
    pub notes: Option<String>,
    pub media: Option<MediaInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    pub duration_seconds: Option<f64>,
    pub codec: Option<String>,
    pub has_audio: Option<bool>,
    pub fps: Option<f64>,
}
```

### 6.2 FileProvider trait (`provider/mod.rs`)

```rust
use async_trait::async_trait;
use bytes::Bytes;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u64,               // Unix timestamp
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GalleryInfo {
    pub path: String,
    pub total_media: usize,
    pub provider_type: ProviderType,
}

#[derive(Debug, Clone)]
pub enum ProviderType {
    Local,
    Smb,
    Sftp,
    S3,
}

#[async_trait]
pub trait FileProvider: Send + Sync {
    /// List all entries in a directory
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, ProviderError>;

    /// Read raw file bytes
    async fn read_file(&self, path: &str) -> Result<Bytes, ProviderError>;

    /// Read and parse a companion file
    async fn read_companion(&self, media_path: &str) -> Result<Option<CompanionFile>, ProviderError>;

    /// Write a companion file atomically (write-tmp then rename)
    async fn write_companion(&self, media_path: &str, data: &CompanionFile) -> Result<(), ProviderError>;

    /// Check if a file exists
    async fn exists(&self, path: &str) -> Result<bool, ProviderError>;

    /// Get file modification time (for cache invalidation)
    async fn mtime(&self, path: &str) -> Result<u64, ProviderError>;

    /// Provider type identifier
    fn provider_type(&self) -> ProviderType;
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("File not found: {0}")]
    NotFound(String),
    #[error("Permission denied: {0}")]
    PermissionDenied(String),
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Parse error: {0}")]
    Parse(String),
}
```

### 6.3 Filter AST (`filter/ast.rs`)

```rust
#[derive(Debug, Clone)]
pub enum FilterExpr {
    /// Match a specific tag: "user:vacation", "plugin.face-recognition:person:alice"
    Tag(TagQuery),
    /// Logical AND
    And(Box<FilterExpr>, Box<FilterExpr>),
    /// Logical OR
    Or(Box<FilterExpr>, Box<FilterExpr>),
    /// Logical NOT
    Not(Box<FilterExpr>),
    /// Rating filter: rating >= 4
    Rating(RatingOp),
    /// Media type filter: type:video, type:gif, type:image
    MediaType(MediaType),
    /// Has any tags in a namespace
    HasNamespace(TagNamespace),
    /// Color label match
    ColorLabel(String),
}

#[derive(Debug, Clone)]
pub struct TagQuery {
    pub namespace: TagNamespace,
    pub value: String,
}

#[derive(Debug, Clone)]
pub enum TagNamespace {
    User,
    Auto,
    Plugin(String),   // plugin name
    Any,              // search all namespaces
}

#[derive(Debug, Clone)]
pub enum RatingOp {
    Gte(u8),
    Lte(u8),
    Eq(u8),
}
```

### 6.4 Plugin manifest (`plugin/manifest.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,                      // unique identifier: "face-recognition"
    pub display_name: String,              // "Face Recognition"
    pub version: String,                   // semver
    pub description: String,
    pub author: Option<String>,
    pub license: Option<String>,

    pub execution: ExecutionConfig,
    pub capabilities: Vec<Capability>,
    pub tag_prefix: String,                // namespace in companion files

    pub ui: Option<PluginUiConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ExecutionConfig {
    #[serde(rename = "cli")]
    Cli {
        command: String,                   // e.g., "python3"
        args: Vec<String>,                // e.g., ["plugin.py", "--image", "{file}"]
        timeout_seconds: Option<u64>,
    },
    #[serde(rename = "wasm")]
    Wasm {
        module_path: String,              // path to .wasm file
        memory_limit_mb: Option<u32>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Capability {
    ReadCompanion,
    WriteCompanion,
    ReadImage,
    NetworkAccess,
    BatchProcess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginUiConfig {
    pub settings_schema: Option<serde_json::Value>,   // JSON Schema for settings UI
    pub context_menu_items: Vec<ContextMenuItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextMenuItem {
    pub label: String,
    pub action: String,     // action identifier passed to plugin
    pub icon: Option<String>,
}
```

### 6.5 Cache layer (`cache/db.rs`)

```rust
pub struct CacheDb {
    conn: rusqlite::Connection,
}

impl CacheDb {
    pub fn open(gallery_path: &Path) -> Result<Self> {
        let db_path = gallery_path.join(".lightview/cache.db");
        let conn = rusqlite::Connection::open(&db_path)?;
        conn.execute_batch("
            CREATE TABLE IF NOT EXISTS thumbnails (
                path         TEXT PRIMARY KEY,
                media_type   TEXT NOT NULL,       -- 'image', 'video', 'gif'
                mtime        INTEGER NOT NULL,
                width        INTEGER NOT NULL,
                height       INTEGER NOT NULL,
                thumbnail    BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tag_index (
                path        TEXT NOT NULL,
                namespace   TEXT NOT NULL,    -- 'user', 'auto', 'plugin.<name>'
                tag         TEXT NOT NULL,
                PRIMARY KEY (path, namespace, tag)
            );
            CREATE INDEX IF NOT EXISTS idx_tag_ns ON tag_index(namespace, tag);
            CREATE INDEX IF NOT EXISTS idx_tag_value ON tag_index(tag);

            CREATE TABLE IF NOT EXISTS tag_counts (
                namespace   TEXT NOT NULL,
                tag         TEXT NOT NULL,
                count       INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (namespace, tag)
            );
            CREATE INDEX IF NOT EXISTS idx_tag_counts_pop ON tag_counts(count DESC);

            CREATE TABLE IF NOT EXISTS file_hashes (
                path        TEXT PRIMARY KEY,
                hash        TEXT NOT NULL,
                mtime       INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_hash ON file_hashes(hash);

            CREATE TABLE IF NOT EXISTS index_state (
                path                TEXT PRIMARY KEY,
                companion_mtime     INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS media_meta (
                path          TEXT PRIMARY KEY,
                date_taken    INTEGER,            -- Unix timestamp: EXIF or mtime
                file_size     INTEGER NOT NULL,
                media_type    TEXT NOT NULL,       -- 'image', 'video', 'gif'
                width         INTEGER,
                height        INTEGER,
                duration      REAL                 -- seconds, null for images
            );
            CREATE INDEX IF NOT EXISTS idx_meta_date ON media_meta(date_taken DESC);
            CREATE INDEX IF NOT EXISTS idx_meta_size ON media_meta(file_size DESC);
            CREATE INDEX IF NOT EXISTS idx_meta_type ON media_meta(media_type);
        ")?;
        Ok(Self { conn })
    }
}
```

### 6.6 Tag autocomplete engine (`autocomplete/engine.rs`)

```rust
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize)]
pub struct TagSuggestion {
    pub namespace: String,
    pub tag: String,
    pub count: u32,
    pub score: i64,           // fuzzy match score (higher = better)
}

pub struct AutocompleteEngine {
    tags: Arc<RwLock<Vec<TagEntry>>>,
    matcher: SkimMatcherV2,
}

#[derive(Debug, Clone)]
struct TagEntry {
    namespace: String,
    tag: String,
    count: u32,
}

impl AutocompleteEngine {
    /// Load all unique tags from tag_counts table into memory.
    /// At 5,000 unique tags this is ~200-300 KB — trivial.
    pub async fn refresh_from_db(&self, db: &CacheDb) -> Result<()> {
        let entries = db.query_all_tag_counts()?;
        let mut tags = self.tags.write().await;
        *tags = entries;
        Ok(())
    }

    /// Query with optional namespace filter.
    /// Returns up to `limit` results sorted by fuzzy score × frequency.
    pub async fn query(
        &self,
        input: &str,
        namespace: Option<&str>,
        limit: usize,
    ) -> Vec<TagSuggestion> {
        let tags = self.tags.read().await;
        let input_lower = input.to_lowercase();

        let mut results: Vec<TagSuggestion> = tags
            .iter()
            .filter(|e| namespace.map_or(true, |ns| e.namespace == ns))
            .filter_map(|e| {
                self.matcher
                    .fuzzy_match(&e.tag.to_lowercase(), &input_lower)
                    .map(|score| TagSuggestion {
                        namespace: e.namespace.clone(),
                        tag: e.tag.clone(),
                        count: e.count,
                        score,
                    })
            })
            .collect();

        // Sort by: fuzzy score descending, then count descending as tiebreaker
        results.sort_by(|a, b| {
            b.score.cmp(&a.score)
                .then_with(|| b.count.cmp(&a.count))
        });
        results.truncate(limit);
        results
    }
}
```

### 6.7 Tauri IPC commands (`commands/autocomplete.rs`)

```rust
#[tauri::command]
async fn autocomplete_tags(
    state: tauri::State<'_, AppState>,
    query: String,
    namespace: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<TagSuggestion>, String> {
    let limit = limit.unwrap_or(15);
    state
        .autocomplete
        .query(&query, namespace.as_deref(), limit)
        .await
        .map_err(|e| e.to_string())
}
```

---

## 7. Performance strategy

### 7.1 Gallery open pipeline (multi-phase)

Opening a gallery of 20,000 images follows a phased pipeline where the user sees content within the first second:

```
Phase 1 — Fast directory scan (~200ms for 20k files)
  walkdir collects paths + mtimes + file sizes
  → sends total count to frontend immediately
  → frontend renders empty grid with skeleton cells at correct scroll height
  → populates media_meta table with date/size (EXIF extracted in parallel)

Phase 2 — Cache check + thumbnail streaming (ongoing)
  For each file, check SQLite: (path, mtime) match?
  HIT  → send thumbnail blob to frontend in batches of 100
  MISS → add to thumbnail work queue
  Priority: visible viewport rows first (see §7.6)

Phase 3 — Background thumbnail generation (parallel)
  Rayon thread pool processes work queue by priority tier
  Images:  image-rs decode → resize 400px → WebP encode
  Videos:  ffmpeg extract frame at ~2s → resize → WebP encode
  GIFs:    extract first frame → resize → WebP encode
  Each batch of completed thumbnails sent to frontend via Tauri event
  Writes batched in SQLite transactions (500 per transaction)

Phase 4 — Background indexing (parallel, low priority)
  For each companion file with changed mtime:
    Parse JSON → extract all tags → update tag_index
    Update tag_counts summary table
    Refresh autocomplete engine's in-memory cache
  Unchanged companion files (mtime matches index_state) are skipped

Phase 5 — Timeline computation (after Phase 1 completes)
  Query media_meta ordered by date_taken
  Compute group boundaries (month/year transitions)
  Send timeline index to frontend for scrollbar date labels
```

### 7.2 Thumbnail pipeline details

| Media type | Thumbnail source | Method | Fallback |
|------------|-----------------|--------|----------|
| Image | The image itself | `image-rs` decode → Lanczos3 resize to 400px longest edge → WebP quality 80 | Platform-native decoder for HEIF/RAW |
| Video | Frame at ~2 second mark | `ffmpeg-next` seek + decode single frame → same resize/encode pipeline | First frame if video is < 2s |
| GIF | First frame | `image-rs` decode first frame → resize → WebP | Same as image pipeline |

All thumbnails are stored as WebP blobs in SQLite (~15–30 KB each). At 20k media files, the cache DB is ~300–600 MB.

### 7.3 Full-resolution viewing

```
User clicks media item N:
  1. Show cached thumbnail instantly (already in frontend memory)
  2. Branch by media type:
     Image → begin async decode of full-res via image-rs
     Video → begin buffering via native <video> element
     GIF   → load full file into <img> tag
  3. Begin prefetch of items N+1 and N+2 (configurable, default 3)
  4. When N is decoded → upload to WebGPU texture → swap in viewer
  5. LRU cache holds configurable number of decoded full-res images (default 5)
  6. User navigates to N+1 → already decoded, instant swap
```

### 7.4 Memory budget

| Item | Budget |
|------|--------|
| Thumbnail cache (in-memory) | ~200 visible thumbnails × 30 KB ≈ 6 MB |
| Full-res LRU cache | 5 images × ~50 MB (for 50MP RAW) ≈ 250 MB |
| SQLite cache file | 300–600 MB for 20k media files |
| Frontend DOM | Virtualized — only ~50-100 DOM nodes regardless of gallery size |
| Tag autocomplete cache | ~5,000 unique tags × ~60 bytes ≈ 300 KB |
| Timeline index | ~20k entries × ~16 bytes ≈ 320 KB |

### 7.5 Large gallery optimizations

**Batched SQLite writes.** All thumbnail inserts and tag index updates are wrapped in transactions of 500 rows. This avoids per-row disk flushes and makes bulk operations 50–100x faster.

**Incremental indexing.** The `index_state` table tracks the last-indexed companion file mtime per path. On gallery open, only companion files whose mtime has changed are re-parsed and re-indexed. For a gallery where nothing has changed, the re-index phase is skipped entirely.

**Lazy file hashing.** SHA-256 hashing of media files is computationally expensive (especially for large videos). Hashes are computed on first access or as a lowest-priority background job — never as a blocking step during gallery open.

**Progressive frontend loading.** The frontend receives a total media count immediately and renders skeleton cells at the correct total scroll height. Thumbnails stream in as they become available, replacing skeletons with a fast opacity fade. The user can scroll and browse while thumbnails are still generating in the background.

**Gallery meta caching.** A `gallery_meta` record in SQLite stores the last scan timestamp and per-directory file counts. On subsequent opens, unchanged subdirectories are skipped entirely, reducing scan time from seconds to milliseconds.

### 7.6 Scroll-aware thumbnail loading

The thumbnail pipeline uses a **priority queue with three tiers** that adapts to the user's scroll position in real time:

| Priority | Zone | Behavior |
|----------|------|----------|
| 0 (immediate) | Visible viewport rows | Loaded first, always. When the user scrolls, these are recalculated immediately. |
| 1 (buffer) | 2–3 rows above and below the viewport | Loaded next, so scrolling a few rows is always instant. |
| 2 (background) | Everything else | Filled in during idle time, working outward from the viewport. |

**Scroll velocity gating.** The frontend tracks scroll velocity by measuring position deltas across `requestAnimationFrame` ticks. Three velocity regimes control loading behavior:

| Scroll velocity | Behavior |
|----------------|----------|
| Stopped or slow (< 500 px/s) | Normal loading: priority queue active, thumbnails stream in for visible rows. |
| Medium (500–3000 px/s) | Reduced loading: only load priority 0 (viewport) thumbnails, skip buffer and background. |
| Fast flick (> 3000 px/s) | Suspend loading entirely. Show skeleton cells. Don't waste decode cycles on rows that will be off-screen in 200ms. |

When velocity drops below the threshold (user is decelerating or has stopped), loading resumes immediately for the new viewport position.

**Jump-to handling.** When the user grabs the scrollbar and drags to a new position (a large discontinuous scroll), the system:

1. Increments a shared atomic **generation counter** on the backend
2. Cancels all in-flight thumbnail loads — worker threads check the counter before writing results and discard stale work
3. Flushes the priority queue
4. Rebuilds the queue around the new scroll position
5. Begins loading from priority 0 at the new location

This makes scrollbar dragging feel responsive even in galleries where most thumbnails haven't been generated yet.

**Frontend implementation.** The `useScrollVelocity` hook tracks scroll events and exposes a reactive `velocity` signal. The `VirtualScroller` component reads this signal and adjusts which thumbnail requests it sends to the backend:

```typescript
// Pseudocode for scroll-aware loading
const velocity = useScrollVelocity();
const visibleRange = computeVisibleRows(scrollTop, viewportHeight, rowHeight);

createEffect(() => {
  if (velocity() > 3000) return; // fast scroll: do nothing

  const buffer = velocity() > 500 ? 0 : 3; // reduce buffer at medium speed
  const loadRange = expandRange(visibleRange(), buffer);

  for (const row of loadRange) {
    if (!thumbnailLoaded(row)) {
      requestThumbnail(row, velocity() > 500 ? 'immediate' : 'normal');
    }
  }
});
```

### 7.7 Hardware-specific optimizations

The app detects hardware capabilities at startup and adjusts its pipeline accordingly. All detections are done once and cached in an in-memory `HardwareProfile` struct.

**Storage type detection (Linux).**

| Detection | Method | Effect |
|-----------|--------|--------|
| NVMe / SSD | Read `/sys/block/<dev>/queue/rotational` — 0 means solid-state | Increase thumbnail thread count (I/O is not the bottleneck), enable parallel companion file reads, bump default prefetch count |
| HDD (spinning disk) | `rotational` = 1 | Reduce thumbnail threads to 2 (avoid seek thrashing), serialize directory reads, reduce prefetch to 1 |
| Filesystem type | `statfs()` magic number or `/proc/mounts` | Enable btrfs/ZFS-specific optimizations when detected |

**Filesystem-specific optimizations.**

| Filesystem | Optimization |
|------------|-------------|
| btrfs | Use `FICLONE` (reflinks) for atomic companion file writes — instant copy-on-write instead of full data copy. Detect via `FS_IOC_FICLONE` ioctl. Also: btrfs transparent compression means reads may already be faster; adjust prefetch budgets up. |
| ZFS | Similar to btrfs: transparent compression, copy-on-write semantics. Adjust prefetch upward. |
| ext4 / XFS | Standard path, no special optimizations. |
| Network (NFS/CIFS) | Increase read buffer sizes, enable aggressive thumbnail caching, reduce companion file write frequency (batch more). |

**GPU compute detection.**

| Capability | Detection | Effect |
|-----------|-----------|--------|
| Discrete GPU with compute shaders | Query WebGPU adapter limits at startup (`maxComputeWorkgroupsPerDimension > 0`) | Offload thumbnail resizing to GPU: decode on CPU → upload full texture → compute shader downsample → read back WebP. Significantly faster for large images (50MP+). |
| Integrated GPU / no compute | Adapter reports limited compute capability | Use CPU Lanczos3 path (default). |
| GPU texture format support | Check for BC/ASTC compressed texture support | Store decoded images in GPU-compressed format in LRU cache for lower VRAM usage. |

**Rust struct for hardware capabilities:**

```rust
pub struct HardwareProfile {
    pub storage_type: StorageType,       // NVMe, SSD, HDD, Network
    pub filesystem: FsType,              // Btrfs, Zfs, Ext4, Xfs, Nfs, Cifs, Other
    pub supports_reflink: bool,          // FICLONE support
    pub cpu_cores: usize,
    pub gpu_compute: bool,               // WebGPU compute shader support
    pub gpu_compressed_textures: bool,   // BC/ASTC support
    pub total_ram_mb: u64,
}

pub enum StorageType { NVMe, SSD, HDD, Network, Unknown }
pub enum FsType { Btrfs, Zfs, Ext4, Xfs, Nfs, Cifs, Other }

impl HardwareProfile {
    /// Recommended thumbnail thread count based on storage and CPU
    pub fn thumbnail_threads(&self) -> usize {
        match self.storage_type {
            StorageType::NVMe => self.cpu_cores.min(12),
            StorageType::SSD => (self.cpu_cores / 2).max(2).min(8),
            StorageType::HDD => 2,
            StorageType::Network => (self.cpu_cores / 4).max(1).min(4),
            StorageType::Unknown => (self.cpu_cores / 2).max(2),
        }
    }

    /// Recommended prefetch count based on storage speed
    pub fn prefetch_count(&self) -> usize {
        match self.storage_type {
            StorageType::NVMe => 5,
            StorageType::SSD => 3,
            StorageType::HDD => 1,
            StorageType::Network => 2,
            StorageType::Unknown => 3,
        }
    }

    /// Recommended LRU cache size based on available RAM
    pub fn lru_cache_size(&self) -> usize {
        if self.total_ram_mb > 32_000 { 10 }
        else if self.total_ram_mb > 16_000 { 5 }
        else { 3 }
    }
}
```

These values serve as **defaults** that the user can override in settings. The settings panel shows the detected hardware profile and explains why certain defaults were chosen.

### 7.8 Sorting and filtered query performance

Sorted and filtered views combine the `tag_index` and `media_meta` tables:

```sql
-- Filtered + sorted by date: "vacation" photos, newest first
SELECT m.path, m.date_taken FROM media_meta m
INNER JOIN tag_index t ON m.path = t.path
WHERE t.namespace = 'user' AND t.tag = 'vacation'
ORDER BY m.date_taken DESC;

-- Filtered + sorted by file size: all videos over 100MB, largest first
SELECT m.path, m.file_size FROM media_meta m
WHERE m.media_type = 'video' AND m.file_size > 104857600
ORDER BY m.file_size DESC;

-- Complex: tagged "family" AND rated >= 4, sorted by date
SELECT m.path, m.date_taken FROM media_meta m
INNER JOIN tag_index t ON m.path = t.path
WHERE t.namespace = 'user' AND t.tag = 'family'
  AND m.path IN (SELECT path FROM media_meta WHERE path IN
    (SELECT path FROM tag_index WHERE namespace = 'user' AND tag = 'family'))
  AND m.path IN (SELECT ti.path FROM tag_index ti
    INNER JOIN ... ) -- rating is in companion, indexed separately
ORDER BY m.date_taken DESC;
```

For rating filters, since rating lives in the companion file, it needs its own index column in `media_meta` or a separate `ratings` table. Adding `rating INTEGER` to `media_meta` keeps it simple — updated whenever the companion file is re-indexed.

The `idx_meta_date` index on `date_taken DESC` makes date-sorted queries fast even at 20k+ rows. Changing sort order just requires a different query — the frontend sends the sort field and direction to the backend, which constructs the appropriate SQL.

---

## 8. Plugin system

### 8.1 Plugin directory structure

```
~/.config/lightview/plugins/
  face-recognition/
    manifest.json
    plugin.py            (or compiled binary, or .wasm)
    models/              (plugin's own data)
  geo-tagger/
    manifest.json
    geo-tag              (compiled binary)
```

### 8.2 Plugin execution flow (CLI mode)

```
1.  User triggers plugin (context menu, batch operation, auto-run)
2.  Core reads manifest.json → gets execution config
3.  Core invokes:  python3 plugin.py --image /path/to/photo.jpg
                                      --companion /path/to/photo.jpg.lightview.json
                                      --action <action_name>
4.  Plugin reads media file + existing companion data
5.  Plugin writes updated companion data to a temp file
6.  Plugin exits with status code 0 + temp file path on stdout
7.  Core validates the output against the companion schema
8.  Core atomically replaces the companion file (write-tmp → rename)
9.  Core re-indexes tags for that file in SQLite
10. Core updates tag_counts and refreshes autocomplete cache
11. Frontend receives update event → refreshes tag display
```

For batch operations across many files, the core accumulates results and commits tag index updates in a single transaction.

### 8.3 Plugin communication protocol

Plugins receive arguments as CLI flags and environment variables:

```bash
# Environment variables set by the core before invocation
LIGHTVIEW_IMAGE_PATH=/photos/sunset.jpg
LIGHTVIEW_COMPANION_PATH=/photos/sunset.jpg.lightview.json
LIGHTVIEW_PLUGIN_DATA_DIR=~/.config/lightview/plugins/face-recognition/data/
LIGHTVIEW_PLUGIN_CONFIG=<json string of plugin settings>
LIGHTVIEW_ACTION=scan
LIGHTVIEW_TEMP_DIR=/tmp/lightview-plugins/

# Plugin writes result to:
# $LIGHTVIEW_TEMP_DIR/<plugin_name>-result.json
# Then prints the path to stdout and exits 0
```

---

## 9. Filter system

### 9.1 Query syntax

```
# Simple tag match (searches all namespaces)
vacation

# Namespace-qualified tag
user:vacation
auto:outdoor
plugin.face-recognition:person:alice

# Boolean operators
user:vacation AND user:family
user:vacation OR user:travel
NOT auto:indoor

# Complex queries
(user:vacation OR user:travel) AND plugin.face-recognition:person:alice AND NOT auto:indoor

# Rating, color, and media type filters
rating>=4
color:green
type:video
type:gif
rating>=3 AND user:favorites AND type:image

# Namespace-level queries
has:user         (has any user tags)
has:plugin.geo   (has any geo-tagger tags)
```

### 9.2 Autocomplete integration

The filter bar's autocomplete engine runs entirely in-memory against a cache of unique tags loaded from the `tag_counts` SQLite table. The flow:

```
1. User types in filter bar (e.g., "vac")
2. Frontend debounces at 150ms, then calls autocomplete_tags via Tauri IPC
3. Rust backend runs fuzzy match against in-memory tag cache (~5,000 entries)
4. Results sorted by: fuzzy match score descending, then frequency descending
5. Top 15 results returned with namespace, tag name, and count
6. Frontend renders dropdown with color-coded namespace badges
7. User selects "user:vacation (342)" → tag appended to filter as a pill
```

The in-memory cache refreshes whenever the tag index is updated (plugin runs, user edits tags, re-index completes). At ~5,000 unique tags the cache is ~300 KB and fuzzy matching completes in under 1ms.

For empty input (filter bar focused but nothing typed), the autocomplete shows the user's 10 most recently used filter tags, stored in the settings store.

### 9.3 Evaluation strategy

For small galleries (< 5,000 media files), evaluate filters by scanning companion files in memory. For larger galleries, the SQLite tag index handles it. All filter queries are combined with the active sort order via the `media_meta` table:

```sql
-- Find images tagged "vacation" by user AND "person:alice" by face-recognition
-- sorted by date (default)
SELECT DISTINCT m.path, m.date_taken FROM media_meta m
INNER JOIN tag_index t1 ON m.path = t1.path
INNER JOIN tag_index t2 ON m.path = t2.path
WHERE t1.namespace = 'user' AND t1.tag = 'vacation'
  AND t2.namespace = 'plugin.face-recognition' AND t2.tag = 'person:alice'
ORDER BY m.date_taken DESC;

-- Same filter, sorted by file size instead
SELECT DISTINCT m.path, m.file_size FROM media_meta m
INNER JOIN tag_index t1 ON m.path = t1.path
INNER JOIN tag_index t2 ON m.path = t2.path
WHERE t1.namespace = 'user' AND t1.tag = 'vacation'
  AND t2.namespace = 'plugin.face-recognition' AND t2.tag = 'person:alice'
ORDER BY m.file_size DESC;
```

Complex boolean queries with many AND clauses require multiple joins. The query planner is guided to start with the most selective clause (lowest count in `tag_counts`).

---

## 10. Context menu and external app integration

Right-click on a thumbnail or in the viewer shows a context menu:

```
Open with            ▸  Gwenview
                        GIMP
                        System default
──────────────────────
Edit tags...
Copy tags from...
──────────────────────
Plugins              ▸  (dynamic list from installed plugins)
──────────────────────
Show in file manager
Copy path
File info
```

The "Open with" entries are user-configurable in settings. On Linux, apps are launched via direct binary invocation or `xdg-open`. Tauri's `shell-open-api` handles the platform-specific details.

---

## 11. Mobile strategy

### 11.1 What ships on mobile

The mobile app is a **read-only viewer** with no plugin execution. It ships:

- Companion file reader (JSON parser, same schema)
- Gallery browser (local photos + remote via FileProvider)
- Tag display and filtering (same filter engine + autocomplete, ported)
- Thumbnail viewer and full-res media viewer (images, video, GIF)

### 11.2 Code sharing plan

| Layer | Desktop | Mobile | Shared? |
|-------|---------|--------|---------|
| Companion schema + parser | Rust | Kotlin/Swift (or Rust via FFI) | Schema is shared, implementation may differ |
| FileProvider trait | Rust | Kotlin/Swift (or Rust via FFI) | Trait contract shared, implementations separate |
| Filter AST + evaluator | Rust | Rust via UniFFI → Kotlin/Swift bindings | Fully shared |
| Autocomplete engine | Rust | Rust via UniFFI → Kotlin/Swift bindings | Fully shared |
| Gallery UI | SolidJS (Tauri webview) | SwiftUI / Jetpack Compose | Separate (native) |
| Thumbnail pipeline | Rust (rayon + ffmpeg-next) | Platform-native (Photos framework / Glide) | Separate |

The key sharing mechanism is **UniFFI** — Rust code compiles to a shared library and UniFFI generates Kotlin/Swift bindings automatically. The filter engine, autocomplete engine, and companion parser are the highest-value modules to share.

### 11.3 Remote gallery on mobile

Mobile connects to the same remote locations (SMB, SFTP, S3) using the FileProvider abstraction. The flow:

```
1. User adds remote gallery in settings (URL + credentials)
2. App lists directory via FileProvider
3. App downloads thumbnails on demand (with local disk cache)
4. Full-res media streams on tap (progressive loading for images, buffered for video)
5. Companion files downloaded alongside thumbnails for tag display
6. All filtering happens locally against downloaded companion data
```

---

## 12. Build order

| Phase | Milestone | Key deliverables |
|-------|-----------|-----------------|
| **1** | Foundation | Companion file schema (finalized + documented), Tauri project scaffold, Rust module structure, hardware detection module |
| **2** | Core read path | LocalProvider, companion reader, SQLite cache schema (thumbnails + tag_index + tag_counts + index_state + media_meta), thumbnail pipeline with video/GIF support, hardware-adaptive thread/prefetch defaults |
| **3** | Basic UI | Full-bleed square-crop gallery grid (virtualized), auto-hiding top bar shell, thumbnail display with progressive loading, skeleton cells, scroll-velocity-aware loading |
| **4** | Media viewer | Full-screen image viewer with zoom/pan + WebGPU, video player with native `<video>`, GIF display, prefetching (N+1/N+2), LRU cache |
| **5** | Sorting + timeline | Default date sort with EXIF extraction, sort options (date/size/name/rating/type), grouping with collapsible headers, timeline scrollbar with date indicator, date separator rows |
| **6** | Tags + filtering | Tag display in info panel (grouped by namespace), user tag CRUD, filter AST + parser, filter bar with autocomplete + fuzzy matching, SQLite tag indexing, combined filter+sort queries |
| **7** | Settings | Settings panel (display, performance, maintenance), live thumbnail size/gap adjustment, re-index and cache rebuild buttons, detected hardware profile display |
| **8** | Plugins (v1) | Plugin manifest format, CLI plugin runner, plugin discovery, batch execution with transactional index updates, example auto-tagger plugin |
| **9** | Context menu | Right-click menu, "Open in..." external app launching, plugin context items |
| **10** | Remote files | SFTP or SMB FileProvider, remote gallery browsing, remote thumbnail caching |
| **11** | Hardware optimization | GPU compute shader thumbnail resizing (where supported), btrfs reflink writes, NVMe-aggressive parallel loading, storage-type adaptive tuning |
| **12** | Polish | Keyboard shortcuts, multi-select, GPU-accelerated viewer refinement, gallery meta caching for fast reopens |
| **13** | Mobile prep | UniFFI bindings for filter engine + autocomplete + sort, companion file reader library extracted, remote FileProvider tested on mobile network conditions |

---

## 13. Appendix: Full type reference

### TypeScript interfaces (frontend, generated from Rust types)

```typescript
// Companion file types (mirrors Rust structs)
interface CompanionFile {
  schema_version: number;
  file: string;
  file_hash: string;
  media_type: 'image' | 'video' | 'gif';
  created: string;
  modified: string;
  tags: TagCollection;
  meta: MetaCollection;
}

interface TagCollection {
  user: string[];
  auto: string[];
  plugins: Record<string, PluginTagEntry>;
}

interface PluginTagEntry {
  version: string;
  tags: string[];
  [key: string]: unknown;
}

interface MetaCollection {
  core?: CoreMeta;
  plugins: Record<string, unknown>;
}

interface CoreMeta {
  rating?: number;
  color_label?: string;
  notes?: string;
  media?: MediaInfo;
}

interface MediaInfo {
  width: number;
  height: number;
  duration_seconds?: number;
  codec?: string;
  has_audio?: boolean;
  fps?: number;
}

// Gallery types
interface GalleryMediaItem {
  path: string;
  name: string;
  media_type: 'image' | 'video' | 'gif';
  thumbnail_url: string;       // blob URL from cache
  companion: CompanionFile | null;
  size: number;
  mtime: number;
}

interface GalleryState {
  path: string;
  items: GalleryMediaItem[];
  filtered_items: GalleryMediaItem[];
  active_filter: FilterExpr | null;
  sort_by: 'date' | 'size' | 'name' | 'rating' | 'media_type';
  sort_order: 'asc' | 'desc';
  group_by: GroupBy;
  total_count: number;         // total items in gallery (for scroll height)
  thumbnails_ready: number;    // how many thumbnails have loaded
  indexing_progress: number;   // 0.0 - 1.0
  loading: boolean;
}

// Sort and group types
type GroupBy =
  | { type: 'time_period'; granularity: 'day' | 'month' | 'year' }
  | { type: 'media_type' }
  | { type: 'size_range' }
  | { type: 'tag'; namespace: string; tag_prefix: string }
  | { type: 'none' };

interface GroupHeader {
  label: string;               // "March 2026", "Videos", "> 100 MB"
  start_index: number;         // first item index in this group
  count: number;               // items in this group
  collapsed: boolean;
}

// Timeline scrollbar types
interface TimelineIndex {
  entries: TimelineEntry[];    // ordered by position in the sorted list
}

interface TimelineEntry {
  row_index: number;           // row in the virtual grid
  date: string;                // ISO 8601 date
  label: string;               // pre-formatted: "Mar 2026", "Dec 14", etc.
}

// Hardware detection types
interface HardwareProfile {
  storage_type: 'nvme' | 'ssd' | 'hdd' | 'network' | 'unknown';
  filesystem: string;          // "btrfs", "ext4", "zfs", etc.
  supports_reflink: boolean;
  cpu_cores: number;
  gpu_compute: boolean;
  total_ram_mb: number;
  recommended_thumbnail_threads: number;
  recommended_prefetch_count: number;
  recommended_lru_size: number;
}

// Filter types
type FilterExpr =
  | { type: 'tag'; namespace: TagNamespace; value: string }
  | { type: 'and'; left: FilterExpr; right: FilterExpr }
  | { type: 'or'; left: FilterExpr; right: FilterExpr }
  | { type: 'not'; expr: FilterExpr }
  | { type: 'rating'; op: 'gte' | 'lte' | 'eq'; value: number }
  | { type: 'media_type'; value: 'image' | 'video' | 'gif' }
  | { type: 'has_namespace'; namespace: TagNamespace }
  | { type: 'color_label'; value: string };

type TagNamespace = 'user' | 'auto' | `plugin.${string}` | 'any';

// Autocomplete types
interface TagSuggestion {
  namespace: string;
  tag: string;
  count: number;
  score: number;
}

interface AutocompleteState {
  query: string;
  suggestions: TagSuggestion[];
  selected_index: number;       // keyboard navigation
  recent_tags: string[];        // shown when input is empty
  loading: boolean;
}

// Settings types
interface AppSettings {
  display: {
    thumbnail_size: number;     // 80-400px, default 200
    grid_gap: number;           // 0-16px, default 2
    background_color: string;   // default "#0a0a0a"
    video_hover_preview: boolean;
    gif_autoplay_grid: boolean;
  };
  performance: {
    preload_count: number;      // 1-10, default 3
    lru_cache_size: number;     // 1-20 images, default 5
    thumbnail_threads: number;  // 1 to CPU count, default CPU/2
    thumbnail_quality: number;  // 40-100, default 80
  };
  external_apps: {
    label: string;
    command: string;
    args: string[];             // {file} is replaced with media path
  }[];
}

// Plugin types
interface PluginManifest {
  name: string;
  display_name: string;
  version: string;
  description: string;
  author?: string;
  execution: CliExecution | WasmExecution;
  capabilities: string[];
  tag_prefix: string;
  ui?: PluginUiConfig;
}

interface CliExecution {
  type: 'cli';
  command: string;
  args: string[];
  timeout_seconds?: number;
}

interface WasmExecution {
  type: 'wasm';
  module_path: string;
  memory_limit_mb?: number;
}
```
