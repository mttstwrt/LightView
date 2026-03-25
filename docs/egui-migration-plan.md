# LightView: Tauri+SolidJS → egui+wgpu Migration Plan

## Motivation

Replace the Tauri (WebKitGTK) + SolidJS frontend with a pure Rust egui application. This eliminates:

- IPC serialization overhead (JSON encode/decode on every thumbnail, transform, tag operation)
- CPU↔GPU roundtrips for display (thumbnails currently: GPU render → CPU readback → JPEG encode → base64 → IPC → blob URL → `<img>`)
- WebKitGTK dependency (source of Wayland crashes, GDK_BACKEND workarounds, DMA-BUF issues)
- Node.js toolchain (npm, Vite, TypeScript, Tailwind)
- The entire `src-solidjs/` directory (~2,000 lines TypeScript)

After migration, thumbnails stay as GPU textures and render directly — no readback, no encoding, no serialization.

## Architecture Overview

```
Before:                                    After:
┌─────────────┐  IPC (JSON)  ┌──────────┐  ┌──────────────────────────────┐
│  SolidJS    │◄────────────►│  Tauri   │  │  egui app (single binary)   │
│  (WebKitGTK)│              │  (Rust)  │  │                              │
│  <img> tags │              │  AppState│  │  winit window                │
│  blob URLs  │              │  wgpu    │  │  egui UI ──► wgpu surface   │
└─────────────┘              └──────────┘  │  AppState (direct access)   │
                                            │  GpuPipeline (shared device)│
                                            └──────────────────────────────┘
```

## What Stays (Zero/Minimal Changes)

All core library code is already pure Rust with no Tauri dependency:

| Module | Files | Notes |
|--------|-------|-------|
| `provider/` | `mod.rs`, `local.rs` | FileProvider trait, LocalProvider |
| `cache/` | `db.rs`, `thumbnails.rs`, `atlas.rs`, `index.rs`, `counts.rs` | SQLite, BC7 atlas, tag index |
| `companion/` | `schema.rs`, `reader.rs`, `writer.rs`, `migration.rs` | Sidecar metadata |
| `filter/` | `ast.rs`, `parser.rs`, `evaluator.rs` | Query language |
| `sort/` | `sorter.rs`, `grouper.rs`, `timeline.rs` | Sort/group/timeline |
| `autocomplete/` | `engine.rs` | Fuzzy tag matching |
| `pipeline/` | `gpu_pipeline.rs`, `thumbnailer.rs` | GPU compute, CPU fallback |
| `hardware/` | `mod.rs` | Hardware detection |
| `plugin/` | `manifest.rs` | Plugin discovery |
| `util/` | `hash.rs`, `fs_watch.rs` | Hashing, file watch |

## What Gets Deleted

```
src-solidjs/                     # Entire frontend directory
package.json                     # Node.js config
package-lock.json
tsconfig.json
vite.config.ts
src-tauri/tauri.conf.json        # Tauri config
src-tauri/capabilities/          # Tauri ACL
src-tauri/src/commands/          # All IPC wrappers (they just call library code)
src-tauri/src/pipeline/gpu.rs    # WebGPU capability detection endpoint
```

## What Gets Rewritten

### `main.rs` — App Entry Point

Replace Tauri builder with winit event loop + egui-wgpu renderer.

```rust
fn main() {
    env_logger::init();

    let event_loop = winit::event_loop::EventLoop::new().unwrap();
    let window = winit::window::WindowBuilder::new()
        .with_title("LightView")
        .with_inner_size(winit::dpi::LogicalSize::new(1280, 800))
        .build(&event_loop)
        .unwrap();

    let mut app = LightViewApp::new(&window);
    // ... event loop
}
```

### `lib.rs` — AppState

Remove `tauri::State` wrappers. `AppState` becomes a plain struct owned by the app:

```rust
pub struct AppState {
    // Same fields, no Arc<RwLock<>> needed for single-threaded egui access
    // (keep Arc for fields shared with background tasks)
    pub providers: ProviderRegistry,
    pub cache_db: Option<CacheDb>,
    pub thumb_atlas: Option<ThumbAtlas>,
    pub autocomplete: AutocompleteEngine,
    pub hardware: HardwareProfile,
    pub current_gallery: Option<String>,
    pub gpu_pipeline: Option<Arc<GpuPipeline>>,  // Arc: shared with background thumbnail gen
    pub thumb_pool: rayon::ThreadPool,
    // ... same as before minus Tauri-specific wrapping
}
```

## New Dependencies

Add to `Cargo.toml`:

```toml
[dependencies]
# Window + event loop
winit = "0.30"

# UI framework (wgpu-backed)
egui = "0.31"
egui-wgpu = "0.31"
egui-winit = "0.31"

# Native file dialogs
rfd = "0.15"

# Image display in egui
image = "0.25"  # already have this
```

Remove from `Cargo.toml`:

```toml
# All of these go away
tauri = "..."
tauri-build = "..."
tauri-plugin-shell = "..."
tauri-plugin-dialog = "..."
tauri-plugin-fs = "..."
```

Remove from project root:

```
package.json, tsconfig.json, vite.config.ts, node_modules/
```

## Implementation Phases

### Phase 1: Skeleton App (winit + egui + wgpu surface)

Create a minimal egui app that opens a window with the shared wgpu device.

**New files:**
- `src/app.rs` — `LightViewApp` struct implementing `eframe::App` or manual egui-wgpu integration
- `src/ui/mod.rs` — UI module root

**Key detail:** The `GpuPipeline` and the egui renderer must share the same `wgpu::Device` and `wgpu::Queue`. Initialize wgpu first, pass device/queue to both `GpuPipeline::new()` and `egui_wgpu::Renderer::new()`.

```rust
// Shared device
let instance = wgpu::Instance::new(...);
let adapter = instance.request_adapter(...).await.unwrap();
let (device, queue) = adapter.request_device(...).await.unwrap();

let device = Arc::new(device);
let queue = Arc::new(queue);

// Both use the same device
let gpu_pipeline = GpuPipeline::from_device(device.clone(), queue.clone());
let egui_renderer = egui_wgpu::Renderer::new(&device, surface_format, None, 1, false);
```

**Modify:** `GpuPipeline::new()` → add `GpuPipeline::from_device(device, queue)` constructor that accepts an existing device instead of creating its own.

**Goal:** Window opens, renders "LightView" text, closes on Esc.

---

### Phase 2: Gallery Open + Thumbnail Grid

Port the gallery grid with virtual scrolling.

**New files:**
- `src/ui/gallery_grid.rs` — Virtual-scrolled thumbnail grid
- `src/ui/thumbnail_cache.rs` — Manages egui `TextureHandle`s for visible thumbnails

**How virtual scrolling works in egui:**

```rust
egui::ScrollArea::vertical().show_rows(ui, row_height, total_rows, |ui, row_range| {
    for row in row_range {
        ui.horizontal(|ui| {
            for col in 0..items_per_row {
                let idx = row * items_per_row + col;
                if let Some(path) = paths.get(idx) {
                    render_thumbnail(ui, path, &mut thumb_cache);
                }
            }
        });
    }
});
```

**Thumbnail display — the key win:**

Instead of GPU → CPU readback → JPEG encode → base64 → IPC → blob URL → `<img>`, the flow becomes:

1. CPU decodes image (same as now, rayon pool)
2. GPU crop+resize (same `GpuPipeline` shader)
3. Result stays as GPU buffer → upload as `egui::TextureHandle`
4. egui renders it directly via wgpu

No readback. No encoding. No serialization. The thumbnail never leaves the GPU after step 2.

```rust
// After GPU resize, result is already on the device
let texture = ui.ctx().load_texture(
    path,
    egui::ColorImage::from_rgba_unmultiplied([w as _, h as _], &rgba_data),
    egui::TextureOptions::LINEAR,
);
// Or even better: create wgpu texture directly and register with egui renderer
let tex_id = egui_renderer.register_native_texture(&device, &texture_view, filter);
```

**Port from SolidJS `GalleryGrid.tsx`:**
- Scroll velocity detection (throttle loading during fast scroll)
- Background thumbnail generation (channel-based: gen thread → UI thread)
- LRU texture eviction (can't hold 20k textures in VRAM)
- Stale work cancellation via atomic generation counter

**Dialog:** Replace `@tauri-apps/plugin-dialog` with `rfd::FileDialog::new().pick_folder()`.

---

### Phase 3: Top Bar + Filter

**New files:**
- `src/ui/top_bar.rs` — Menu bar with open button, filter, settings
- `src/ui/filter_bar.rs` — Filter input with autocomplete dropdown

**Port from SolidJS `FilterBar.tsx`:**
- Text input with 150ms debounce (use `egui::TextEdit`)
- Autocomplete dropdown (use `egui::popup_below_widget`)
- Filter pills (horizontal layout with X buttons)
- Arrow key navigation in dropdown

The filter/autocomplete logic is already pure Rust (`filter/parser.rs`, `autocomplete/engine.rs`) — call it directly instead of through IPC.

---

### Phase 4: Media Viewer (Lightbox)

**New files:**
- `src/ui/viewer.rs` — Full-screen image viewer with transforms
- `src/ui/transform_controls.rs` — Rotation, exposure, saturation, contrast sliders

**Image display:** Load full-res image as egui texture. For transforms, run the GPU transform shader and update the texture in-place — no JPEG encode/decode round-trip.

```rust
// Transform is instant — shader runs, result goes straight to screen
let transformed = gpu_pipeline.transform_image(&rgba, w, h, dst_w, dst_h, params);
texture.set(ColorImage::from_rgba_unmultiplied([dst_w, dst_h], &transformed));
```

**Port from SolidJS `MediaViewer.tsx`:**
- Image/video display (video: use platform decoder or `ffmpeg` crate)
- Navigation (arrow keys, click sides)
- Transform controls (sliders, rotation buttons)
- Info panel overlay

**Video playback** is the hardest part. Options:
1. `egui_video` crate (ffmpeg-based, renders frames as textures)
2. Spawn external player (mpv/vlc) via `std::process::Command`
3. Custom ffmpeg decode → wgpu texture upload loop

Option 2 is simplest and sidesteps WebKitGTK video crashes. Option 1 gives inline playback.

---

### Phase 5: Info Panel + Tag Editor

**New files:**
- `src/ui/info_panel.rs` — Side panel with metadata, tags, rating

**Port from SolidJS `MediaViewer.tsx` InfoPanel:**
- Metadata display (file size, dimensions, date)
- Star rating (clickable stars)
- Tag list with add/remove
- Tag input with autocomplete

All tag/metadata operations become direct function calls:

```rust
// Before (Tauri IPC):  invoke<void>("add_user_tag", { path, tag })
// After (direct call):  companion_writer.add_user_tag(&path, &tag)?;
```

---

### Phase 6: Context Menu + Settings

**New files:**
- `src/ui/context_menu.rs` — Right-click menu
- `src/ui/settings.rs` — Settings panel

**Context menu:** `ui.menu_button()` or custom popup on right-click.

**Settings persistence:** Replace `localStorage` with `serde_json` to `~/.config/lightview/settings.json` (same data, different storage).

---

### Phase 7: Cleanup + Delete Old Code

- Delete `src-solidjs/` directory
- Delete `package.json`, `tsconfig.json`, `vite.config.ts`
- Delete `src-tauri/tauri.conf.json`, `src-tauri/capabilities/`
- Delete `src-tauri/src/commands/` (all IPC wrappers)
- Delete `src-tauri/src/pipeline/gpu.rs` (WebGPU capability detection)
- Remove tauri dependencies from `Cargo.toml`
- Rename `src-tauri/` → `src/` (no longer a Tauri sub-project)
- Update `Cargo.toml` paths
- Flatten project structure (single Rust crate, no Node.js)

## Plugin Architecture Changes

The current plugin system has two execution modes defined in the manifest:

```rust
enum ExecutionConfig {
    Cli { command, args, timeout_seconds },
    Wasm { module_path, memory_limit_mb },
}
```

Plugins also define UI contributions:

```rust
struct PluginUiConfig {
    settings_schema: Option<serde_json::Value>,  // JSON Schema for settings form
    context_menu_items: Vec<ContextMenuItem>,     // { label, action, icon }
}
```

**What changes:**

1. **CLI plugins** — No change. `std::process::Command` works identically. Currently unimplemented (`run_plugin` returns an error), so this gets implemented fresh in egui regardless.

2. **WASM plugins** — No change. `wasmtime`/`wasmer` runtime is pure Rust. Same sandboxing model. Also currently unimplemented.

3. **Plugin UI (settings forms)** — Currently the `settings_schema` is a JSON Schema intended for a web-based form renderer. In egui, you'd render it as native widgets:
   - `"type": "string"` → `egui::TextEdit`
   - `"type": "number"` → `egui::DragValue` or `egui::Slider`
   - `"type": "boolean"` → `egui::Checkbox`
   - `"enum"` → `egui::ComboBox`

   This is actually simpler than building a dynamic form renderer in SolidJS. A ~50-line recursive function can walk the JSON Schema and emit egui widgets.

4. **Context menu items** — Plugins declare `ContextMenuItem { label, action, icon }`. In egui, these render as `ui.button(label)` inside the context menu popup. Simpler than the web version.

5. **New opportunity: GPU plugins** — With shared wgpu device access, plugins could register custom compute shaders (e.g., AI upscaling, denoising, style transfer). This isn't possible in the Tauri architecture where the frontend has a separate WebGPU context. A plugin could provide a `.wgsl` shader that gets compiled into a compute pipeline on the shared device.

**Summary:** Plugin architecture carries over cleanly. The manifest format doesn't change. UI rendering switches from hypothetical web forms to egui widgets. The migration is an opportunity to actually implement `run_plugin` since it was never finished.

## Shared wgpu Device Architecture

The most important architectural detail. Currently `GpuPipeline` creates its own `wgpu::Device`. After migration, the egui renderer and the GPU pipeline share one device:

```
                    ┌─────────────────────┐
                    │  wgpu::Device (Arc)  │
                    │  wgpu::Queue  (Arc)  │
                    └────────┬────────────┘
                             │
              ┌──────────────┼──────────────┐
              │              │              │
     ┌────────▼──────┐ ┌────▼─────┐ ┌──────▼───────┐
     │  GpuPipeline  │ │  egui    │ │  Future GPU  │
     │  (compute)    │ │  renderer│ │  plugins     │
     │               │ │  (render)│ │  (compute)   │
     │  crop+resize  │ │  UI draw │ │  custom WGSL │
     │  BC7 encode   │ │  calls   │ │  shaders     │
     │  transforms   │ │          │ │              │
     └───────────────┘ └──────────┘ └──────────────┘
```

**Thumbnail zero-copy path:**

```
1. CPU decode (rayon)        → RGBA in RAM
2. GPU crop+resize           → RGBA in VRAM (wgpu buffer)
3. GPU BC7 encode (optional) → BC7 in VRAM (for atlas cache)
4. Create wgpu::TextureView  → stays in VRAM
5. Register with egui        → egui::TextureId
6. egui renders to screen    → direct GPU blit

No CPU readback. No JPEG encode. No base64. No IPC.
```

Compare to current Tauri path:
```
1. CPU decode        → RGBA in RAM
2. GPU crop+resize   → RGBA in VRAM
3. CPU readback      → RGBA back to RAM (640KB per thumb)
4. CPU JPEG encode   → JPEG in RAM
5. Base64 encode     → string in RAM
6. Tauri IPC         → JSON serialization
7. WebKitGTK         → deserialize, create blob URL
8. <img> element     → browser decodes JPEG again
9. Browser composites → finally on screen
```

Steps 3-8 are eliminated entirely.

## State Management

Replace SolidJS reactive stores with plain Rust structs. egui is immediate-mode — it redraws every frame, so there's no need for a reactivity system.

```rust
struct UiState {
    // Gallery (was galleryStore.ts)
    gallery_path: Option<String>,
    display_paths: Vec<String>,
    groups: Vec<GroupHeader>,
    timeline: Vec<TimelineEntry>,
    loading: bool,
    selected: HashSet<String>,

    // Filter (was filterStore.ts)
    filter_pills: Vec<FilterPill>,
    filter_query: String,
    ac_query: String,
    ac_suggestions: Vec<TagSuggestion>,
    ac_open: bool,

    // Viewer (was viewerStore.ts)
    viewer_open: bool,
    viewer_index: usize,
    info_panel_open: bool,

    // Transforms (was in MediaViewer.tsx)
    rotation: f32,
    exposure: f32,
    saturation: f32,
    contrast: f32,

    // Settings (was settingsStore.ts, localStorage)
    settings: Settings,  // serde to ~/.config/lightview/settings.json

    // Sort
    sort_field: SortField,
    sort_order: SortOrder,
    group_by: GroupBy,
}
```

## Performance Considerations

### What Gets Faster
- Thumbnail display: ~8 fewer data transformations per thumbnail
- Image transforms: GPU result → screen in one frame (no IPC round-trip)
- Tag operations: direct function call vs async IPC
- Startup: no WebKitGTK initialization, no Vite dev server

### What Needs Care
- **egui repaints every frame** — use `ctx.request_repaint()` only when state changes, not continuously. Set `egui::ViewportBuilder::with_active(false)` when idle.
- **Texture memory** — LRU evict textures for off-screen thumbnails. Budget: ~200MB VRAM for thumbnail textures (500 thumbnails at 400x400 RGBA).
- **Background work** — Use channels (`std::sync::mpsc` or `crossbeam`) to send results from rayon/tokio threads to the UI thread. egui's `ctx.request_repaint()` wakes the event loop when new data arrives.
- **Scroll performance** — egui's `ScrollArea::show_rows` does row-level virtualization. For 20k+ images this is sufficient (only ~20-40 rows rendered at any time).

## Final Project Structure

```
lightview/
├── Cargo.toml
├── src/
│   ├── main.rs                    # winit event loop + egui init
│   ├── app.rs                     # LightViewApp, frame loop
│   ├── lib.rs                     # AppState, module tree
│   ├── ui/
│   │   ├── mod.rs
│   │   ├── gallery_grid.rs        # Virtual-scrolled thumbnail grid
│   │   ├── thumbnail_cache.rs     # TextureHandle LRU cache
│   │   ├── top_bar.rs             # Menu bar
│   │   ├── filter_bar.rs          # Filter input + autocomplete
│   │   ├── viewer.rs              # Full-screen image viewer
│   │   ├── transform_controls.rs  # Rotation/exposure/saturation/contrast
│   │   ├── info_panel.rs          # Metadata + tags sidebar
│   │   ├── context_menu.rs        # Right-click menu
│   │   └── settings.rs            # Settings panel
│   ├── provider/                  # (unchanged)
│   ├── cache/                     # (unchanged)
│   ├── companion/                 # (unchanged)
│   ├── filter/                    # (unchanged)
│   ├── sort/                      # (unchanged)
│   ├── autocomplete/              # (unchanged)
│   ├── pipeline/                  # (unchanged, minus gpu.rs)
│   ├── plugin/                    # (unchanged)
│   ├── hardware/                  # (unchanged)
│   └── util/                      # (unchanged)
├── docs/
│   ├── project-overview.md
│   ├── performance-guide.md
│   ├── egui-migration-plan.md     # This document
│   └── todo.md
└── plugins/
    └── example-auto-tagger/
```

Single binary. No Node.js. No WebKitGTK. No IPC.

## Verification Checklist

- [ ] Phase 1: Window opens, egui renders text, GPU pipeline initializes on shared device
- [ ] Phase 2: Gallery opens via `rfd`, thumbnails render in virtual-scrolled grid
- [ ] Phase 2: Thumbnail generation uses zero-copy GPU → egui texture path
- [ ] Phase 2: 20k+ image gallery scrolls smoothly (< 16ms frame time)
- [ ] Phase 3: Filter bar with autocomplete, pills, keyboard navigation
- [ ] Phase 4: Lightbox viewer with navigation, GPU transforms render in real-time
- [ ] Phase 4: Video playback works (external player or inline)
- [ ] Phase 5: Info panel shows metadata, tags editable, star rating works
- [ ] Phase 6: Right-click context menu, settings persist to disk
- [ ] Phase 7: `src-solidjs/` deleted, no Node.js deps, single `cargo build` produces final binary
- [ ] Benchmark: gallery open time ≤ current Tauri version
- [ ] Benchmark: thumbnail display latency measurably lower (no IPC overhead)
- [ ] Binary size: should be smaller (no WebKitGTK runtime)
