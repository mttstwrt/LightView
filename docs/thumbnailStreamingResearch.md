# Thumbnail Streaming Research — LightView vs. State of the Art

## How top-tier apps handle it

**Apple Photos** — a full **thumbnail pyramid** (micro ~64px, small ~256px, medium ~512px, preview ~screen-size, original). `PHImageManager.requestImage(targetSize:)` picks the right tier and can return a fast/low-quality result first, then a high-quality one ("opportunistic" delivery). Pinch-to-zoom GPU-scales the currently-bound tier and **cross-fades** in the next tier once it's ready.

**Google Photos** — cloud-first, but the model is the same: the image server is URL-addressed with size/crop params (`=w400-h400-c`), so the client requests exactly the tier matching the cell's physical pixels. Formats are WebP/AVIF. Grid cells fall back to a dominant-color background until the tile arrives, with a tiny base64 blur-up as an intermediate stage.

**Immich** — materializes **three fixed tiers per asset**:
- `thumbhash` — a blurred placeholder (~25–30 bytes, stored inline in the DB)
- `thumbnail.webp` — 250px WebP @ Q80, for the grid
- `preview.jpeg` — 1440px JPEG @ Q80, for the viewer
- plus the original on disk

Files live under `thumbs/{ownerId}/…/{id}_thumbnail.webp` / `_preview.jpeg` / `_fullsize.jpeg`. The timeline component uses a justified layout and serves the 250px tier; pressing into the viewer swaps to the 1440px preview.

## What LightView already does well

Mapping `docs/gridScaleResearch.md`, `docs/scrollPerformanceResearch.md`, and `docs/WebGL.md` against the code, most of the **scroll-performance** techniques are in place:

| Technique | Where |
|---|---|
| Virtualized row window + buffer | `GalleryGrid.tsx:377` (`BUFFER_ROWS`, `startRow`/`endRow`) |
| Viewport-distance + recency LRU | `ImageLoader.ts:266` (`findEvictionCandidate`) |
| Off-thread decode (Web Worker, `createImageBitmap`) | `ImageLoader.ts:70`, `imageDecodeWorker.ts` |
| Texture pool with `texSubImage2D`, no reallocation | `WebGLRenderer.ts:454` |
| Instanced drawing, single `drawElementsInstanced` | `WebGLRenderer.ts:384` |
| Velocity-aware fetch gating (`VELOCITY_FAST`/`SETTLE_MS`) | `GalleryGrid.tsx:402`, `518` |
| Zero-JSON IPC via `lightview://thumb/<path>` custom protocol | `main.rs:312` |
| mmap-backed BC7 atlas for zero-copy GPU uploads | `cache/atlas.rs` |
| DCT-scaled JPEG decode | `pipeline/thumbnailer.rs:118` |
| Priority tiers on decode worker (viewport/buffer/background) | `ImageLoader.ts:26` |
| Generation counter + jump detection | `GalleryGrid.tsx:407` |

## The actual gap: there is no LOD pyramid

LightView currently caches **exactly one size per media** (`DEFAULT_THUMB_WIDTH = 400`, `cache/thumbnails.rs` schema is `path`-keyed, not `(path, size)`-keyed). The grid already has Ctrl+wheel resize clamped to `THUMB_SIZE_MIN=80 … THUMB_SIZE_MAX=600` (`GalleryGrid.tsx:51`). That means:

- At **80 px** cells (dense grid, hundreds of DOM nodes / instances on screen), every cell still pulls a 400×400 JPEG and uploads a 512×512 pool slot. You pay 25× the pixels you'll ever display — decode cost, GPU memory, and atlas bandwidth all suffer.
- At **600 px** cells (large grid, few items on screen), the cell is *upscaled* from 400 px and looks blurry, exactly when you have the GPU/memory headroom to do better.
- The viewer has the same problem in reverse: `lightview://media/<path>` serves the original, so first-paint in the viewer waits on a full decode.

Everything else in the docs is implemented; the pyramid is the missing piece.

## Recommended tiers for LightView

Adapt Immich's three-tier model to LightView's atlas-centric pipeline:

| Tier | Size | Format | Storage | Purpose |
|---|---|---|---|---|
| **T0 — placeholder** | ThumbHash (~25 B) | inline bytes | new column on `thumbnails` table, or on the index row | Paint a meaningful blurred tile instantly while T1/T2 stream in; also the "fast flick" fallback |
| **T1 — micro** | 128×128 | BC7 in atlas | existing `ThumbAtlas` (second slot per asset) | Dense grids (`thumbnail_size` ≤ 160); `THUMB_SIZE_MIN=80` should land here |
| **T2 — standard** | 512×512 | BC7 in atlas (or WebP in SQLite) | existing atlas | Medium grids (160–400); matches current 512 pool slot |
| **T3 — large** | 1024×1024 | WebP Q80 in SQLite blob | `thumbnails` table | Large grids (400+) and the "zoomed-in" pre-view state, so cells don't blur |
| **T4 — preview** | ~1600 px longest edge | WebP/JPEG Q85 | SQLite | Fullscreen viewer first-paint before the original arrives |

Store T0 alongside the existing index (it's tiny and essentially free). T1/T2 fit naturally in the BC7 atlas — add a second `AtlasEntry` key shape like `(path, tier)`. T3/T4 are called infrequently enough that SQLite blobs are fine and let you reuse WebP without a BC7 encode.

## Concrete integration points

1. **Schema** — evolve `AtlasIndex.entries: HashMap<String, AtlasEntry>` (`cache/atlas.rs:27`) into `HashMap<(String, Tier), AtlasEntry>`, and add `tier` to `cache/thumbnails.rs` (the existing mtime-based invalidation still works). Migrate by treating the current entries as T2.

2. **Pipeline** — generate T1 and T2 in one pass: after `fast_image_resize` produces the 512 px RGBA, run a second downsample to 128 px from the same intermediate. `pipeline/thumbnailer.rs` already walks RGBA; this is ~free compared to the source decode. T0 (ThumbHash) is trivially computed from the 128 px buffer. Generate T3/T4 lazily on first miss — do not block the bulk ingest on them.

3. **Protocol** — extend the handler in `main.rs:312` to accept `lightview://thumb/<tier>/<path>` (`thumb/s/…`, `thumb/m/…`, `thumb/l/…`). Keep the existing `lightview://thumb/<path>` as an alias for `m/`. The frontend picks the tier from `cellSize()`:

   ```ts
   const tier = cs <= 160 ? "s" : cs <= 400 ? "m" : "l";
   ```

4. **Cross-fade on grid resize** — when `thumbnail_size` changes and `tier` crosses a bucket, keep the old texture slot bound, request the new tier, and have the shader `mix()` between old/new UVs over ~150 ms (a `u_fade` uniform per instance). The WebGL renderer's instance buffer already has two spare floats in the `a_flags` vec2 — you can pass fade progress there. This is the "pinch-to-zoom" behavior from `gridScaleResearch.md §2`.

5. **ThumbHash skeleton** — in `WebGLRenderer.ts` the current fallback is a flat `u_skeletonColor` (line 349). Replace `hasImage=0` rendering with a 2nd pool texture holding decoded 32×32 ThumbHash bitmaps (or a tiny SSBO/texture array of 4×4 DCT coefficients decoded in the fragment shader). Keep the flat color as a third fallback while the ThumbHash itself is still fetching. This solves the "white boxes during fast flick" from `scrollPerformanceResearch.md §2`.

6. **Velocity-aware tier downgrade** — you already gate generation on `VELOCITY_FAST` (`GalleryGrid.tsx:523`). Extend that: during a fast flick, only request **T0 + T1**, never T2/T3. The "landing zone" prediction (`lastFastScrollTime + SETTLE_MS`) then upgrades the resting cells to the proper tier. This matches `scrollPerformanceResearch.md §4`.

7. **Texture pool slot size** — `SLOT_SIZE=512` in `WebGLRenderer.ts:15` is pinned. Once you have tiers, split the pool into two sub-pools (one 128 px, one 512 px) so dense grids use ~16× less GPU memory. Slot allocation is already keyed by path — keying by `(path, tier)` lets both sub-pools coexist and swap when the user pinch-zooms.

8. **WebP support** — `ThumbFormat` (`pipeline/thumbnailer.rs:17`) only has `Jpeg` / `Rgba`. Add `WebP` for T3/T4. Immich's numbers (WebP Q80 at 250 px) land around 10–15 KB per thumbnail vs 25–40 KB JPEG — meaningful for the SQLite blob tier. Not worth it for T1/T2 because BC7 bypasses decode entirely.

9. **Viewer preview hand-off** — `lightview://media/<path>` currently serves the original. Add a `lightview://preview/<path>` that serves T4 (or falls through to the original), and have `MediaViewer` paint T4 immediately while the original decodes. This kills the white-frame-on-open stutter without touching the decode pipeline.

## Priority order if implemented incrementally

1. **ThumbHash in the index row + shader skeleton.** One column, no pipeline changes to the existing atlas, and you immediately get rid of the grey boxes during fast scroll. Highest value, lowest risk.
2. **Second tier (T1 128px) alongside existing T2.** Cuts dense-grid GPU memory by 16× and eliminates the decode-work-per-cell at `thumbnail_size ≤ 160`.
3. **T3 large tier, lazy-generated.** Fixes the blur at `thumbnail_size ≥ 500`.
4. **Cross-fade on tier swap** via `u_fade` in the instanced shader.
5. **T4 preview tier for viewer** — small change, big perceptual win.
6. **WebP format option** — orthogonal; worth doing when disk footprint becomes a concern.

The first three map cleanly onto existing modules (`cache/atlas.rs`, `cache/thumbnails.rs`, `pipeline/thumbnailer.rs`, the protocol handler in `main.rs`, and `WebGLRenderer.ts`) — no architectural change, just `(path)` → `(path, tier)` throughout and one extra downsample in the ingest pipeline.

## Sources
- [System Settings | Immich](https://docs.immich.app/administration/system-settings/)
- [Media Processing — immich-app/immich | DeepWiki](https://deepwiki.com/immich-app/immich/3.4-media-processing)
- [Asset Management — immich-app/immich | DeepWiki](https://deepwiki.com/immich-app/immich/4.1-asset-management)
- [Architecture | Immich](https://immich.app/docs/developer/architecture/)
- [Three sized thumbnails discussion · immich-app/immich#10108](https://github.com/immich-app/immich/discussions/10108)
- [Small thumbnail size request · immich-app/immich#4455](https://github.com/immich-app/immich/discussions/4455)
- [ThumbHash: A very compact image placeholder](https://evanw.github.io/thumbhash/)
- [evanw/thumbhash on GitHub](https://github.com/evanw/thumbhash)
- [BlurHash](https://blurha.sh/) / [woltapp/blurhash](https://github.com/woltapp/blurhash)
- [A clear look at blurry image placeholders on the web | Mux](https://www.mux.com/blog/blurry-image-placeholders-on-the-web)
- [How Medium does progressive image loading | José M. Pérez](https://jmperezperez.com/blog/medium-image-progressive-loading-placeholder/)
- [Dominant color lazy loading (Pinterest/Google-style)](https://github.com/Lorti/dominant-colors-lazy-loading-wordpress-plugin)
