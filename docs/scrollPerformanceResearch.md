# Scroll Performance Research

The seamless "infinite scroll" experience in apps like Apple or Google Photos is about clever memory management and "cheating" the user’s perception.

---

## Part 1: General Principles (Apple Photos, Google Photos)

### 1. The Virtualized List (Cell Reuse)
These apps never actually render thousands of images at once. They use **View Virtualization**.
* **The "Pool":** The app only creates enough UI "cells" to fill the screen plus a small buffer (e.g., 20 cells).
* **Recycling:** As a cell scrolls off the top of the screen, it isn’t destroyed. Instead, it is moved to the bottom, its old image is cleared, and a new image is "injected" into it.
* **Static Offsets:** The scroll bar isn’t tied to the physical height of thousands of images; the app calculates a "virtual height" so the scroll thumb moves accurately even though only a dozen images exist in memory.

### 2. Multi-Stage Pipeline (The Loading "Hand-off")
To prevent those white or grey boxes during fast scrolls, the apps use a three-step visual fallback system:
1.  **Dominant Color:** Before an image is even fetched, the cell is filled with a solid color representing the average hue of that photo. This maintains the "rhythm" of the grid.
2.  **BlurHash / Micro-Thumb:** A tiny (e.g., 10x10 pixel) string of data stored in the local database is decoded instantly into a blurred representation of the photo. This takes almost zero CPU time.
3.  **The High-Res Swap:** The actual thumbnail is decoded on a background thread and swapped in once ready.


### 3. Asynchronous Decoding & "The GPU Jump"
One of the biggest bottlenecks in scrolling is **JPEG/HEIC decoding**. Doing this on the main thread causes "jank" (dropped frames).
* **Background Wrangling:** Apps decode the compressed image bytes into raw bitmap data on a background thread.
* **Zero-Copy Transfers:** Once decoded, the bitmap is uploaded to the GPU as a texture. Modern systems use specialized APIs (like `DirectComposition` on Windows or `Core Animation` on iOS) to ensure the GPU handles the actual drawing while the CPU stays free to handle your touch inputs.

### 4. Predictive Prefetching (The "Crystal Ball")
The app tracks the **velocity** of your scroll.
* **Slow Scroll:** It fetches images just a few rows ahead.
* **Fast Flick:** If you "flick" the screen, the app realizes it can’t keep up. It stops trying to load high-res thumbnails for the images flying by and instead only fetches the **BlurHash** or **Micro-Thumbs**.
* **Targeting:** It predicts where the scroll will likely stop (based on friction and velocity) and begins pre-loading the high-quality thumbnails for *that* specific landing zone before you even get there.

### 5. Memory-Mapped Databases
To know *which* image to show at index #5,402 instantly, these apps use highly optimized databases like **SQLite** or custom binary formats.
* They use **Memory Mapping (mmap)**, which maps the database file directly into the app’s virtual memory.
* This allows the OS to handle the heavy lifting of reading from the disk, making metadata lookups (like "find the file path for the 500th photo") feel like they are happening in RAM.

### Technical Performance Summary
| Technique | Problem Solved | Impact |
| :--- | :--- | :--- |
| **Cell Recycling** | High RAM usage | Constant memory footprint regardless of library size. |
| **Off-thread Decoding** | UI Stutter (Jank) | Maintains 60Hz/120Hz/144Hz scroll smoothness. |
| **BlurHash/LPP** | "Empty" white boxes | Visual continuity during high-velocity scrolls. |
| **Velocity Tracking** | Network/IO Clogging | Prioritizes loading the "landing zone" over the "passing zone." |

---

## Part 2: How Specific Apps Handle It

### Google Photos Web (from [Building the Google Photos Web UI](https://medium.com/google-design/google-photos-45b714dfbed1))

**Architecture:** Three-level hierarchy — **Sections** (months) → **Segments** (days) → **Tiles** (photos). This lets them detach entire groups at once instead of individual photos.

**Layout is separated from scroll.** The justified-row grid layout is computed once on load and again on resize. The scroll handler only does a binary search to find which rows are visible — no layout work happens during scroll.

**requestAnimationFrame for scroll coalescing:**
> "In the scroll and resize handlers we use [rAF] to schedule a single callback instead of immediately updating."

This prevents layout thrashing since scroll events fire multiple times per frame.

**Resize handling:** They "delay updating for half a second until the user has settled on the final window size." Non-visible sections fall back to estimated heights rather than full FlexLayout recalculation.

**CSS `contain` property:** Sections and segments are annotated with `contain` to indicate independent layout scope, so the browser doesn’t re-layout the entire page when one section changes.

**Scale:** Layout for 1 million photos takes ~1.5s. The scroll handler itself does almost no work.

### Immich (Svelte/SvelteKit, from [Immich Web Interface](https://deepwiki.com/immich-app/immich/2.3-web-interface))

**DOM virtualization, not canvas.** Only visible DOM nodes are rendered. Off-screen month groups are replaced with spacer `<div>`s of known height.

**TimelineManager class** maintains a buffered viewport of assets organized by month/day. Tracks which asset groups intersect the viewport (plus buffer) and only fetches/renders those groups.

**Layout recomputation is separated from scroll:** Resize triggers a full layout recalculation of row heights and group positions, then the viewport is re-evaluated **without requiring a scroll event.** This is the key difference from LightView’s current approach.

**Directional buffering:** The buffer is asymmetric — larger in the scroll direction.

---

## Part 3: Canvas-Specific Patterns (Applicable to LightView)

### The Core Bug Pattern: Layout-Gated Rendering

In canvas-based virtual grids, the visible range (`startRow`/`endRow`) is typically computed from scroll position. If a **layout change** (zoom, resize, column count change) alters what’s visible but doesn’t trigger a scroll event, the visible range goes stale and new items never appear.

**The fix pattern:** Compute visible range from **current state** (scroll position + layout geometry), not only in response to scroll events. Any change to layout geometry must also trigger `recalcRange()`.

### Passive Scroll + rAF (Gold Standard for Canvas)

```js
let scrollY = 0, ticking = false;
element.addEventListener(‘scroll’, (e) => {
  scrollY = element.scrollTop;
  if (!ticking) {
    requestAnimationFrame(() => { render(scrollY); ticking = false; });
    ticking = true;
  }
}, { passive: true });
```

The scroll handler captures position and sets a dirty flag. The rAF callback reads the position and renders. This naturally coalesces multiple scroll events into one render per frame.

### Three-Tier Work Scheduling

| Tier | API | Work |
| :--- | :--- | :--- |
| **Immediate** | `requestAnimationFrame` | Reposition canvas, redraw from cached textures |
| **Deferred** | `scrollend` event | Upgrade to full-quality thumbnails, trigger generation |
| **Idle** | `requestIdleCallback` | Background prefetch, evict distant textures from GPU |

### `scrollend` Event (Now Baseline)

`scrollend` fires once when scrolling completes. It replaces fragile `setTimeout`/debounce hacks for "scroll settled" detection. Supported in Chrome 114+, Firefox 109+, Safari 26.2+.

**Pattern:** During scroll, show ThumbHash/cached textures only. On `scrollend`, upgrade to full-resolution thumbnails. This eliminates the `SETTLE_MS`/`SCROLL_DEBOUNCE_MS` constants entirely.

### IntersectionObserver vs Scroll Listeners

- **IntersectionObserver** runs on a separate internal thread and doesn’t block the main thread. Ideal for **DOM-based** grids where each thumbnail is an element.
- **For canvas grids (LightView):** IntersectionObserver is irrelevant since there are no individual DOM elements to observe. Scroll listeners with the passive+rAF pattern are the correct approach.
- **Exception:** A single IntersectionObserver on the canvas element itself can detect when the grid enters/exits the viewport, useful for pausing/resuming the render loop.

### `content-visibility: auto`

Not applicable to canvas grids. Provides "free" browser-managed virtualization for DOM-based rendering. Requires `contain-intrinsic-size` to maintain scroll height. Known issue: janky fast-scrollbar dragging.

### Canvas-Specific: Pixel Shifting

Use `drawImage(canvas, 0, deltaY)` to shift existing pixels on scroll, then only draw newly revealed rows. Avoids redrawing the entire canvas each frame. (LightView already avoids this by using instanced WebGL drawing which is fast enough to redraw fully each frame.)

### OffscreenCanvas for Decode

Move thumbnail decode and texture preparation to a Web Worker via `OffscreenCanvas`. (LightView already does off-thread decode via `imageDecodeWorker.ts`.)

---

## Part 4: LightView-Specific Analysis & Recommendations

### Current Bug: Zoom-Out Stale Viewport

**Root cause:** `startRow`/`endRow` are only recomputed inside `recalcRange()`, which is only called by:
1. The `onScroll` handler (needs a scroll event)
2. The `containerWidth` effect (width doesn’t change during zoom)

After Ctrl+wheel zoom, the `window.scrollTo()` at the end of `onWheel` should trigger `onScroll`, but:
- If scroll position doesn’t meaningfully change (e.g., near the top), no scroll event fires
- Even when it does fire, `currentScrollY` is a closure variable updated inside the rAF callback, adding a frame delay

**Fix:** Call `recalcRange()` directly from the zoom handler (after `setSettings()`), and also add it to a `createEffect` watching layout-dependent signals (`thumbSize`, `cols`, `cellSize`). This decouples viewport computation from scroll events, matching how Google Photos and Immich separate layout from scroll.

### What LightView Already Does Well

- **WebGL instanced rendering** — single draw call for entire grid ✓
- **Texture pool with LRU eviction** — GPU memory management ✓
- **ThumbHash fallback pipeline** — blurred placeholder while loading ✓
- **Off-thread decode via Web Worker** — main thread stays free ✓
- **Velocity-based scroll behavior** — skips fetches during fast scroll ✓
- **Priority-based loading** (viewport=0, buffer=1, background=2) ✓

### Potential Future Improvements

1. **Replace `SCROLL_DEBOUNCE_MS` with `scrollend` event** — eliminates the 50ms timer, uses the browser’s native "scroll settled" detection instead.

2. **Replace `SETTLE_MS` velocity decay with `scrollend`** — the 150ms settle heuristic becomes unnecessary; `scrollend` fires at exactly the right time.

3. **Asymmetric buffering** — like Google Photos, make `BUFFER_ROWS` larger in the scroll direction. Track scroll direction from `lastScrollY` delta.

4. **CSS `contain: strict` on the scroll container** — tells the browser the grid’s layout is independent, preventing full-page layout recalculations.

---

### Sources
- [Building the Google Photos Web UI — Antin Harasymiv (Medium/Google Design)](https://medium.com/google-design/google-photos-45b714dfbed1)
- [Immich Web Interface — DeepWiki](https://deepwiki.com/immich-app/immich/2.3-web-interface)
- [Next Generation Virtual Scrolling — SitePen](https://www.sitepen.com/blog/next-generation-virtual-scrolling)
- [scrollend event — MDN](https://developer.mozilla.org/en-US/docs/Web/API/Element/scrollend_event)
- [Optimizing Canvas — MDN](https://developer.mozilla.org/en-US/docs/Web/API/Canvas_API/Tutorial/Optimizing_canvas)
- [Safari Adds scrollend Event Support — InfoQ](https://www.infoq.com/news/2026/04/safari-scrollend-support/)
