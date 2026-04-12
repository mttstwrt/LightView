// ---------------------------------------------------------------------------
// CanvasGrid — replaces DOM grid with a canvas-based renderer (Phase 7)
// ---------------------------------------------------------------------------
//
// Wraps a Canvas2D or WebGL GridRenderer inside a SolidJS component.
// Handles: image loading pipeline, mouse events via hit-testing,
// scroll-driven repaints, and selection overlays.

import { Show, createSignal, createEffect, on, onMount, onCleanup, batch, untrack } from "solid-js";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { settings, setSettings } from "../../stores/settingsStore";
import { ensureTierThumbnails, getThumbnailsBatch, precacheThumbnails, thumbUrl, thumbhashUrl, mediaUrl, type ThumbTier, type ThumbnailResult } from "../../lib/ipc";
import { thumbGenStarted, thumbGenProgress, thumbGenFinished } from "../../stores/thumbnailProgressStore";
import { initGPU } from "../../lib/gpu";
import { recordCacheMiss, setImageCountSource } from "../../lib/perfMonitor";
import { Canvas2DRenderer } from "../../lib/renderer/Canvas2DRenderer";
import { WebGLRenderer } from "../../lib/renderer/WebGLRenderer";
import { ImageLoader, type DecodePriority } from "../../lib/renderer/ImageLoader";
import { MemoryPressureMonitor } from "../../lib/memoryPressure";
import type { GridRenderer, GridLayout, GridItem } from "../../lib/renderer/types";
import type { RendererMode } from "../../lib/types";

interface CanvasGridProps {
  paths: string[];
  onItemClick: (index: number) => void;
  onItemSelect: (path: string) => void;
  onDragSelect?: (paths: string[]) => void;
  onBackgroundClick?: () => void;
  selectedPaths: Set<string>;
  onItemContextMenu?: (e: MouseEvent, path: string, index: number) => void;
  loading: boolean;
  onContentHeight?: (height: number) => void;
  rendererMode: RendererMode;
}

// --- Constants (same as GalleryGrid) ---
const BUFFER_ROWS = 3;
const BATCH_SIZE = 128;
const VELOCITY_SLOW = 500;
const VELOCITY_FAST = 3000;
const JUMP_FACTOR = 2;
const BG_BATCH_SIZE = 64;
const EVICT_ROWS = 15;
const SETTLE_MS = 150;
const SCROLL_DEBOUNCE_MS = 50;

// Ctrl+wheel thumbnail resize bounds (px). Each tick retargets the column
// count by ±1 so one scroll notch always produces a visible change.
const THUMB_SIZE_MIN = 80;
const THUMB_SIZE_MAX = 600;

// LOD tier thresholds — cellSize buckets used for URL construction.
// See docs/thumbnailStreamingResearch.md. Kept in sync with target_size()
// in src-tauri/src/cache/thumbnails.rs.
const TIER_MICRO_MAX = 160;
const TIER_STANDARD_MAX = 400;

function pickTier(cellPx: number): ThumbTier {
  if (cellPx <= TIER_MICRO_MAX) return "s";
  if (cellPx <= TIER_STANDARD_MAX) return "m";
  return "l";
}

const VIDEO_EXTS = new Set(["mp4", "mov", "avi", "mkv", "webm", "m4v", "wmv", "flv"]);

function getExt(path: string): string {
  const dot = path.lastIndexOf(".");
  return dot >= 0 ? path.slice(dot + 1).toLowerCase() : "";
}

function getBadge(path: string): "VID" | "GIF" | null {
  const ext = getExt(path);
  if (VIDEO_EXTS.has(ext)) return "VID";
  if (ext === "gif") return "GIF";
  return null;
}

export function CanvasGrid(props: CanvasGridProps) {
  const thumbSize = () => settings().display.thumbnail_size;
  const gap = () => settings().display.grid_gap;

  const [containerWidth, setContainerWidth] = createSignal(0);
  const [startRow, setStartRow] = createSignal(0);
  const [endRow, setEndRow] = createSignal(0);
  const [generation, setGeneration] = createSignal(0);

  // Track which paths have been assigned URLs (for optimistic loading)
  const assignedSet = new Set<string>();
  // Track URL versions for cache busting
  const urlVersions = new Map<string, number>();
  // Current assigned URL per path
  const urlMap = new Map<string, string>();

  let containerRef: HTMLDivElement | undefined;
  let canvasWrapperRef: HTMLDivElement | undefined;
  let renderer: GridRenderer | null = null;
  let imageLoader: ImageLoader | null = null;
  let pressureMonitor: MemoryPressureMonitor | null = null;
  let rafPending = false;
  let recalcRange: (() => void) | undefined;

  // Thumbnail generation state
  const needsGeneration = new Set<string>();
  const inFlightSet = new Set<string>();
  const failedSet = new Set<string>();
  const coalescedPaths = new Set<string>();
  const pathToIndex = new Map<string, number>();

  // ThumbHash placeholder state — tracks which paths have had their
  // blurred fallback requested. One fetch per path per session; failures
  // (no hash cached yet) are tracked in `hashFailedSet` so we don't retry.
  const hashRequestedSet = new Set<string>();
  const hashFailedSet = new Set<string>();

  /** Load a decoded ThumbHash PNG via the custom protocol and hand it to
   *  the renderer. Uses HTMLImageElement rather than fetch() because the
   *  `lightview://` scheme is intercepted at the image-loading layer on
   *  WebKitGTK, which `fetch()` does not always honour. */
  const requestThumbHash = (path: string) => {
    if (hashRequestedSet.has(path) || hashFailedSet.has(path)) return;
    if (!renderer || typeof renderer.thumbHashLoaded !== "function") return;
    hashRequestedSet.add(path);

    const img = new Image();
    img.decoding = "async";
    img.onload = () => {
      if (!renderer) return;
      renderer.thumbHashLoaded?.(path, img);
      scheduleRepaint();
    };
    img.onerror = () => {
      hashFailedSet.add(path);
    };
    img.src = thumbhashUrl(path);
  };
  let bgCursor = 0;
  let thumbGenTotal = 0;
  let thumbGenDone = 0;

  // GIF hover overlay state
  const [hoveredGif, setHoveredGif] = createSignal<{
    path: string;
    x: number;
    y: number;
    size: number;
  } | null>(null);

  // Drag-to-select state
  const [isDragging, setIsDragging] = createSignal(false);
  const [dragStartIndex, setDragStartIndex] = createSignal(-1);
  const [dragCurrentIndex, setDragCurrentIndex] = createSignal(-1);
  let dragBaseSelection = new Set<string>();
  let suppressClick = false;

  const dragSelectedPaths = () => {
    const si = dragStartIndex();
    const ci = dragCurrentIndex();
    if (si < 0 || ci < 0) return new Set<string>();
    const lo = Math.min(si, ci);
    const hi = Math.max(si, ci);
    const paths = new Set<string>();
    for (let i = lo; i <= hi; i++) {
      if (props.paths[i]) paths.add(props.paths[i]);
    }
    return paths;
  };

  const effectiveSelected = () => {
    if (!isDragging()) return props.selectedPaths;
    const dragged = dragSelectedPaths();
    const merged = new Set(dragBaseSelection);
    for (const p of dragged) merged.add(p);
    return merged;
  };

  // -----------------------------------------------------------------------
  // Grid geometry
  // -----------------------------------------------------------------------

  const cols = () => {
    const w = containerWidth();
    const min = thumbSize();
    const g = gap();
    if (w <= 0) return 1;
    return Math.max(1, Math.floor((w + g) / (min + g)));
  };

  const cellSize = () => {
    const w = containerWidth();
    const c = cols();
    const g = gap();
    return (w - (c - 1) * g) / c;
  };

  const rowHeight = () => cellSize() + gap();

  // Current LOD tier derived from the rendered cell size. Changes invalidate
  // the URL cache so visible items re-fetch at the new resolution.
  const tier = () => pickTier(cellSize());

  const totalRows = () => Math.ceil(props.paths.length / cols());

  const totalHeight = () => {
    const rows = totalRows();
    if (rows === 0) return 0;
    return rows * cellSize() + (rows - 1) * gap();
  };

  // -----------------------------------------------------------------------
  // Visible items
  // -----------------------------------------------------------------------

  const visibleItems = (): GridItem[] => {
    const sr = startRow();
    const er = endRow();
    const c = cols();
    const startIdx = sr * c;
    const endIdx = Math.min(er * c, props.paths.length);
    const sel = effectiveSelected();
    const items: GridItem[] = [];
    for (let i = startIdx; i < endIdx; i++) {
      const path = props.paths[i];
      items.push({
        index: i,
        path,
        thumbUrl: urlMap.get(path) ?? null,
        selected: sel.has(path),
        badge: getBadge(path),
      });
    }
    return items;
  };

  /** Height of the visible rendered area (for canvas sizing). */
  const renderedHeight = () => {
    const sr = startRow();
    const er = endRow();
    const rows = er - sr;
    if (rows <= 0) return 0;
    return rows * cellSize() + (rows - 1) * gap();
  };

  const sliceOffsetY = () => startRow() * rowHeight();

  // -----------------------------------------------------------------------
  // Renderer + image loader setup
  // -----------------------------------------------------------------------

  function createRenderer(mode: RendererMode): GridRenderer {
    if (mode === "webgl") {
      try {
        return new WebGLRenderer();
      } catch {
        console.warn("WebGL unavailable, falling back to Canvas2D");
      }
    }
    return new Canvas2DRenderer();
  }

  function scheduleRepaint() {
    if (rafPending) return;
    rafPending = true;
    requestAnimationFrame(() => {
      rafPending = false;
      repaint();
    });
  }

  function repaint() {
    if (!renderer || !canvasWrapperRef) return;

    const w = containerWidth() - gap() * 2; // account for left/right padding
    const h = renderedHeight();
    if (w <= 0 || h <= 0) return;

    const dpr = window.devicePixelRatio || 1;
    renderer.resize(w, h, dpr);

    const layout: GridLayout = {
      cols: cols(),
      cellSize: cellSize(),
      gap: gap(),
      totalItems: props.paths.length,
      scrollOffsetY: sliceOffsetY(),
      startRow: startRow(),
      endRow: endRow(),
    };

    renderer.render(layout, visibleItems());
  }

  // -----------------------------------------------------------------------
  // Thumbnail error handler (for 404s from protocol handler)
  // -----------------------------------------------------------------------

  const handleThumbError = (path: string) => {
    if (needsGeneration.has(path) || inFlightSet.has(path) || failedSet.has(path) || coalescedPaths.has(path)) return;
    recordCacheMiss();
    needsGeneration.add(path);
    coalescedPaths.add(path);
    thumbGenTotal++;
    if (thumbGenTotal === 1) {
      thumbGenStarted(thumbGenTotal);
    } else {
      thumbGenProgress(thumbGenDone, thumbGenTotal);
    }
  };

  // -----------------------------------------------------------------------
  // Mount: create renderer, image loader, scroll/resize handlers
  // -----------------------------------------------------------------------

  onMount(() => {
    initGPU();

    renderer = createRenderer(props.rendererMode);

    // Attach the canvas to the wrapper now that the renderer exists.
    // The ref callback runs during render (before onMount), so `renderer`
    // was still null at that point — we must attach here.
    if (canvasWrapperRef && renderer && !canvasWrapperRef.contains(renderer.canvas)) {
      canvasWrapperRef.appendChild(renderer.canvas);
    }

    imageLoader = new ImageLoader(
      // onReady: image decoded, hand to renderer
      (path, img) => {
        renderer?.imageLoaded(path, img);
        scheduleRepaint();
      },
      // onError: thumbnail not cached, request generation
      (path) => {
        handleThumbError(path);
      },
    );

    // Phase 3: pressure-aware memory management
    pressureMonitor = new MemoryPressureMonitor((state) => {
      if (!imageLoader) return;

      if (state.level === "emergency") {
        // Emergency purge: keep only visible items
        const sr = startRow();
        const er = endRow();
        const c = cols();
        const keepPaths = new Set<string>();
        for (let i = sr * c; i < Math.min(er * c, props.paths.length); i++) {
          keepPaths.add(props.paths[i]);
        }
        imageLoader.emergencyPurge(keepPaths);
        renderer?.destroy();
        renderer = createRenderer(props.rendererMode);
        if (canvasWrapperRef && renderer) {
          canvasWrapperRef.innerHTML = "";
          canvasWrapperRef.appendChild(renderer.canvas);
        }
        // Hash pool was wiped with the old renderer — drop the "already
        // requested" markers so visible items re-fetch on the next tick.
        hashRequestedSet.clear();
        hashFailedSet.clear();
        // Re-feed surviving cache entries to renderer
        for (const p of keepPaths) {
          const img = imageLoader.get(p);
          if (img) renderer?.imageLoaded(p, img);
        }
        scheduleRepaint();
      }

      imageLoader.setMaxCacheSize(state.targetCacheSize);
    });
    pressureMonitor.start();

    // Register getter so perf monitor can sample counts each tick
    setImageCountSource(() => ({
      visible: visibleItems().length,
      cached: imageLoader?.size ?? 0,
    }));

    if (containerRef) {
      setContainerWidth(containerRef.clientWidth);
    }

    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        setContainerWidth(entry.contentRect.width);
      }
    });
    if (containerRef) ro.observe(containerRef);

    // Scroll state
    let currentScrollY = window.scrollY;
    let currentVelocity = 0;
    let lastScrollY = window.scrollY;
    let lastTimestamp = performance.now();
    let lastFastScrollTime = 0;

    recalcRange = () => {
      const sy = currentScrollY;
      const vh = window.innerHeight;
      const offset = containerRef?.offsetTop ?? 0;
      const rh = rowHeight();
      const cs = cellSize();
      if (rh <= 0 || cs <= 0) return;

      const relativeTop = Math.max(0, sy - offset);
      const relativeBottom = relativeTop + vh;

      const newStart = Math.max(0, Math.floor(relativeTop / rh) - BUFFER_ROWS);
      const newEnd = Math.min(totalRows(), Math.ceil(relativeBottom / rh) + BUFFER_ROWS);

      if (newStart !== untrack(startRow) || newEnd !== untrack(endRow)) {
        batch(() => {
          setStartRow(newStart);
          setEndRow(newEnd);
        });
      }
    };

    let rafId = 0;
    const onScroll = () => {
      if (rafId) return;
      rafId = requestAnimationFrame((now) => {
        const y = window.scrollY;
        const dt = (now - lastTimestamp) / 1000;
        const dy = Math.abs(y - lastScrollY);

        currentVelocity = dt > 0 ? dy / dt : 0;
        currentScrollY = y;

        if (currentVelocity > VELOCITY_FAST) {
          lastFastScrollTime = now;
        }

        if (dy > window.innerHeight * JUMP_FACTOR) {
          setGeneration((g) => g + 1);
          assignedSet.clear();
          needsGeneration.clear();
          coalescedPaths.clear();
          inFlightSet.clear();
          failedSet.clear();
          bgCursor = 0;
          thumbGenTotal = 0;
          thumbGenDone = 0;
          imageLoader?.cancelAll();
        }

        lastScrollY = y;
        lastTimestamp = now;

        recalcRange?.();
        debouncedFetch();
        rafId = 0;
      });
    };

    window.addEventListener("scroll", onScroll, { passive: true });

    // WebKitGTK wheel handling
    let wheelAccumulator = 0;
    let wheelRafId = 0;
    const WHEEL_DECAY = 0.85;
    const WHEEL_THRESHOLD = 0.5;

    const drainWheel = () => {
      if (Math.abs(wheelAccumulator) < WHEEL_THRESHOLD) {
        wheelAccumulator = 0;
        wheelRafId = 0;
        return;
      }
      const step = wheelAccumulator * (1 - WHEEL_DECAY);
      wheelAccumulator -= step;
      window.scrollBy(0, step);
      wheelRafId = requestAnimationFrame(drainWheel);
    };

    const onWheel = (e: WheelEvent) => {
      // Ctrl+wheel resizes thumbnails instead of scrolling. cellSize snaps
      // to a whole-column layout, so we step the column count by 1 per tick
      // and pick a thumb_size that lands in the middle of the target bucket.
      if (e.ctrlKey || e.metaKey) {
        e.preventDefault();
        const w = containerWidth();
        const g = gap();
        if (w <= 0) return;
        const cur = settings().display.thumbnail_size;
        const curCols = Math.max(1, Math.floor((w + g) / (cur + g)));
        const targetCols = e.deltaY < 0 ? curCols - 1 : curCols + 1;
        if (targetCols < 1) return;
        const upper = (w + g) / targetCols - g;
        const lower = (w + g) / (targetCols + 1) - g;
        let next = Math.round((upper + lower) / 2);
        next = Math.max(THUMB_SIZE_MIN, Math.min(THUMB_SIZE_MAX, next));
        const newCols = Math.max(1, Math.floor((w + g) / (next + g)));
        if (newCols === curCols) return;
        setSettings((prev) => ({
          ...prev,
          display: { ...prev.display, thumbnail_size: next },
        }));
        return;
      }
      e.preventDefault();
      wheelAccumulator += e.deltaY;
      if (!wheelRafId) {
        wheelRafId = requestAnimationFrame(drainWheel);
      }
    };
    window.addEventListener("wheel", onWheel, { passive: false });

    recalcRange();

    // -------------------------------------------------------------------
    // Thumbnail fetch loop (same as GalleryGrid)
    // -------------------------------------------------------------------

    let fetchAbort = false;
    let inFlightFetch: Promise<void> | null = null;

    const evictFaraway = () => {
      const sr = startRow();
      const er = endRow();
      const c = cols();
      const keepStart = Math.max(0, (sr - EVICT_ROWS)) * c;
      const keepEnd = Math.min(props.paths.length, (er + EVICT_ROWS) * c);

      const toEvict: string[] = [];
      for (const p of assignedSet) {
        const idx = pathToIndex.get(p);
        if (idx !== undefined && (idx < keepStart || idx >= keepEnd)) {
          toEvict.push(p);
        }
      }

      for (const p of toEvict) {
        assignedSet.delete(p);
        urlMap.delete(p);
        renderer?.evictImage(p);
        renderer?.evictThumbHash?.(p);
        imageLoader?.evict(p);
        hashRequestedSet.delete(p);
      }
    };

    const scheduleFetch = () => {
      if (inFlightFetch) return;

      const now = performance.now();
      if (now - lastTimestamp > 150) currentVelocity = 0;
      if (currentVelocity > VELOCITY_FAST) return;
      if (now - lastFastScrollTime < SETTLE_MS) return;

      coalescedPaths.clear();
      const gen = generation();

      // Phase 1: Generate thumbnails that 404'd
      if (needsGeneration.size > 0) {
        const nearViewport: string[] = [];
        const farAway: string[] = [];
        const sr = startRow();
        const er = endRow();
        const c = cols();
        const nearStart = Math.max(0, (sr - EVICT_ROWS)) * c;
        const nearEnd = Math.min(props.paths.length, (er + EVICT_ROWS) * c);

        for (const p of needsGeneration) {
          const idx = pathToIndex.get(p);
          if (idx !== undefined && idx >= nearStart && idx < nearEnd) {
            if (nearViewport.length < BATCH_SIZE) nearViewport.push(p);
          } else {
            if (farAway.length < BATCH_SIZE) farAway.push(p);
          }
        }

        const toGenerate = nearViewport.slice(0, BATCH_SIZE);
        const remaining = BATCH_SIZE - toGenerate.length;
        if (remaining > 0) toGenerate.push(...farAway.slice(0, remaining));

        if (toGenerate.length > 0) {
          for (const p of toGenerate) {
            needsGeneration.delete(p);
            inFlightSet.add(p);
          }

          const activeTier = tier();
          inFlightFetch = (async () => {
            try {
              // Standard/Micro tiers go through get_thumbnails_batch, which
              // generates the 512 px standard thumbnail and derives the
              // 128 px micro (+ ThumbHash) in the same pass. Large and
              // Preview tiers are lazy — re-decode from source at the
              // tier's target size and return the count, not metadata.
              let resultPaths: Set<string>;
              if (activeTier === "l" || activeTier === "p") {
                await ensureTierThumbnails(toGenerate, activeTier);
                if (fetchAbort || generation() !== gen) return;
                resultPaths = new Set(toGenerate);
                for (const p of toGenerate) {
                  const v = (urlVersions.get(p) ?? 0) + 1;
                  urlVersions.set(p, v);
                  const url = `${thumbUrl(p, activeTier)}?v=${v}`;
                  urlMap.set(p, url);
                  imageLoader?.evict(p);
                  imageLoader?.request(p, url, 1);
                }
                thumbGenDone += toGenerate.length;
              } else {
                const results = await getThumbnailsBatch(toGenerate);
                if (fetchAbort || generation() !== gen) return;

                resultPaths = new Set(results.map((r) => r.path));
                for (const r of results) {
                  const v = (urlVersions.get(r.path) ?? 0) + 1;
                  urlVersions.set(r.path, v);
                  const url = `${thumbUrl(r.path, activeTier)}?v=${v}`;
                  urlMap.set(r.path, url);
                  // Re-request with priority 1 (buffer) — just generated
                  imageLoader?.evict(r.path);
                  imageLoader?.request(r.path, url, 1);
                }

                thumbGenDone += results.length;
              }
              for (const p of toGenerate) {
                inFlightSet.delete(p);
                if (!resultPaths.has(p)) failedSet.add(p);
              }

              if (needsGeneration.size === 0 && inFlightSet.size === 0) {
                thumbGenFinished(thumbGenDone);
                thumbGenTotal = 0;
                thumbGenDone = 0;
              } else {
                thumbGenProgress(thumbGenDone, thumbGenTotal);
              }
            } catch (e) {
              console.error("Thumbnail generation batch failed:", e);
              for (const p of toGenerate) {
                inFlightSet.delete(p);
                failedSet.add(p);
              }
            }
            inFlightFetch = null;
          })();
          return;
        }
      }

      // Phase 2: Background prefetch
      if (currentVelocity < VELOCITY_SLOW && bgCursor < props.paths.length) {
        const bgNeeded: string[] = [];
        const total = props.paths.length;
        while (bgNeeded.length < BG_BATCH_SIZE && bgCursor < total) {
          const p = props.paths[bgCursor];
          bgCursor++;
          if (p && !assignedSet.has(p) && !failedSet.has(p)) {
            bgNeeded.push(p);
          }
        }

        if (bgNeeded.length > 0) {
          inFlightFetch = (async () => {
            try {
              await precacheThumbnails(bgNeeded);
            } catch (e) {
              console.error("Background precache failed:", e);
            }
            inFlightFetch = null;
          })();
          return;
        }
      }

      // Phase 3: Eviction
      evictFaraway();
    };

    let scrollDebounceTimer: ReturnType<typeof setTimeout> | null = null;
    const debouncedFetch = () => {
      if (scrollDebounceTimer !== null) clearTimeout(scrollDebounceTimer);
      scrollDebounceTimer = setTimeout(scheduleFetch, SCROLL_DEBOUNCE_MS);
    };
    const bgIntervalId = setInterval(() => {
      if (!inFlightFetch) scheduleFetch();
    }, 500);

    createEffect(on(generation, () => {
      scheduleFetch();
    }));

    // -------------------------------------------------------------------
    // Listen for streamed thumbnail results
    // -------------------------------------------------------------------

    let unlistenStreamed: UnlistenFn | undefined;
    listen<ThumbnailResult>("thumb:streamed", (event) => {
      const r = event.payload;
      if (!inFlightSet.has(r.path)) return;
      const v = (urlVersions.get(r.path) ?? 0) + 1;
      urlVersions.set(r.path, v);
      const url = `${thumbUrl(r.path, tier())}?v=${v}`;
      urlMap.set(r.path, url);
      imageLoader?.evict(r.path);
      // Phase 5: priority 1 (buffer) for streamed results
      imageLoader?.request(r.path, url, 1);
    }).then((fn) => {
      unlistenStreamed = fn;
    });

    // Thumbnail invalidation
    const onInvalidate = () => {
      assignedSet.clear();
      needsGeneration.clear();
      inFlightSet.clear();
      failedSet.clear();
      hashRequestedSet.clear();
      hashFailedSet.clear();
      urlVersions.clear();
      urlMap.clear();
      bgCursor = 0;
      thumbGenTotal = 0;
      thumbGenDone = 0;
      imageLoader?.cancelAll();
      setGeneration((g) => g + 1);
    };
    window.addEventListener("lightview:thumbnails-invalidated", onInvalidate);

    // -------------------------------------------------------------------
    // Mouse event handling via hit-testing
    // -------------------------------------------------------------------

    const canvasToLocal = (e: MouseEvent): { x: number; y: number } => {
      const rect = renderer!.canvas.getBoundingClientRect();
      return { x: e.clientX - rect.left, y: e.clientY - rect.top };
    };

    const hitTestAt = (e: MouseEvent) => {
      if (!renderer) return { index: -1, path: null };
      const { x, y } = canvasToLocal(e);
      const layout: GridLayout = {
        cols: cols(),
        cellSize: cellSize(),
        gap: gap(),
        totalItems: props.paths.length,
        scrollOffsetY: sliceOffsetY(),
        startRow: startRow(),
        endRow: endRow(),
      };
      return renderer.hitTest(x, y, layout, visibleItems());
    };

    const onMouseDown = (e: MouseEvent) => {
      if (e.button !== 0) return;
      // Only start drag-select when Ctrl/Cmd is held
      if (!(e.ctrlKey || e.metaKey)) return;
      const hit = hitTestAt(e);
      if (hit.index < 0) return;
      e.preventDefault();
      dragBaseSelection = new Set(props.selectedPaths);
      setIsDragging(true);
      setDragStartIndex(hit.index);
      setDragCurrentIndex(hit.index);
    };

    const onMouseMove = (e: MouseEvent) => {
      // GIF hover overlay
      if (!isDragging()) {
        const hit = hitTestAt(e);
        if (hit.index >= 0 && hit.path && getExt(hit.path) === "gif") {
          const c = cols();
          const cs = cellSize();
          const g = gap();
          const sr = startRow();
          const row = Math.floor(hit.index / c) - sr;
          const col = hit.index % c;
          setHoveredGif({
            path: hit.path,
            x: col * (cs + g),
            y: row * (cs + g),
            size: cs,
          });
        } else {
          setHoveredGif(null);
        }
      }

      if (!isDragging()) return;
      const hit = hitTestAt(e);
      if (hit.index >= 0) {
        setDragCurrentIndex(hit.index);
        scheduleRepaint();
      }
    };

    const onMouseUp = () => {
      if (!isDragging()) return;
      const dragged = dragSelectedPaths();
      const wasMultiDrag = dragStartIndex() !== dragCurrentIndex();
      setIsDragging(false);

      if (!wasMultiDrag) {
        setDragStartIndex(-1);
        setDragCurrentIndex(-1);
        return;
      }

      suppressClick = true;
      const merged = new Set(dragBaseSelection);
      for (const p of dragged) merged.add(p);
      if (props.onDragSelect) {
        props.onDragSelect([...merged]);
      }
      setDragStartIndex(-1);
      setDragCurrentIndex(-1);
      scheduleRepaint();
    };

    const onClick = (e: MouseEvent) => {
      if (suppressClick) {
        suppressClick = false;
        return;
      }
      const hit = hitTestAt(e);
      if (hit.index < 0) {
        if (!e.ctrlKey && !e.metaKey) props.onBackgroundClick?.();
        return;
      }
      if (e.ctrlKey || e.metaKey) {
        props.onItemSelect(hit.path!);
      } else if (props.selectedPaths.size > 0) {
        // Clear selection first — don't open viewer until selection is gone
        props.onBackgroundClick?.();
      } else {
        props.onItemClick(hit.index);
      }
    };

    const onContextMenu = (e: MouseEvent) => {
      if (!props.onItemContextMenu) return;
      const hit = hitTestAt(e);
      if (hit.index < 0) return;
      e.preventDefault();
      props.onItemContextMenu(e, hit.path!, hit.index);
    };

    const onMouseLeave = () => {
      setHoveredGif(null);
    };

    const canvas = renderer.canvas;
    canvas.addEventListener("mousedown", onMouseDown);
    canvas.addEventListener("mousemove", onMouseMove);
    canvas.addEventListener("mouseleave", onMouseLeave);
    canvas.addEventListener("click", onClick);
    canvas.addEventListener("contextmenu", onContextMenu);
    window.addEventListener("mouseup", onMouseUp);

    // -------------------------------------------------------------------
    // Cleanup
    // -------------------------------------------------------------------

    onCleanup(() => {
      ro.disconnect();
      window.removeEventListener("scroll", onScroll);
      window.removeEventListener("wheel", onWheel);
      window.removeEventListener("mouseup", onMouseUp);
      window.removeEventListener("lightview:thumbnails-invalidated", onInvalidate);
      canvas.removeEventListener("mousedown", onMouseDown);
      canvas.removeEventListener("mousemove", onMouseMove);
      canvas.removeEventListener("mouseleave", onMouseLeave);
      canvas.removeEventListener("click", onClick);
      canvas.removeEventListener("contextmenu", onContextMenu);
      if (rafId) cancelAnimationFrame(rafId);
      if (wheelRafId) cancelAnimationFrame(wheelRafId);
      if (scrollDebounceTimer !== null) clearTimeout(scrollDebounceTimer);
      clearInterval(bgIntervalId);
      fetchAbort = true;
      unlistenStreamed?.();
      pressureMonitor?.stop();
      setImageCountSource(() => ({ visible: 0, cached: 0 }));
      renderer?.destroy();
      imageLoader?.destroy();
    });
  });

  // -----------------------------------------------------------------------
  // Optimistic URL assignment: request images as soon as items are visible
  // -----------------------------------------------------------------------

  createEffect(on(visibleItems, (items) => {
    const t = tier();
    for (const item of items) {
      // Request the ThumbHash placeholder for every visible item — cheap,
      // bounded by pool eviction, and gives us a blurred fallback while
      // the real thumbnail streams in.
      requestThumbHash(item.path);

      if (!assignedSet.has(item.path)) {
        assignedSet.add(item.path);
        const v = urlVersions.get(item.path);
        const url = v ? `${thumbUrl(item.path, t)}?v=${v}` : thumbUrl(item.path, t);
        urlMap.set(item.path, url);
        // Phase 5: priority 0 = viewport-visible (highest)
        imageLoader?.request(item.path, url, 0);
      } else if (imageLoader && renderer) {
        // Re-feed cached bitmaps whose GPU texture slots may have been
        // evicted by the renderer's internal pool management. The
        // renderer short-circuits if the slot still exists (O(1) Map check).
        const cached = imageLoader.get(item.path);
        if (cached) {
          renderer.imageLoaded(item.path, cached);
        }
      }
    }

    // Phase 3: update viewport context for distance-based eviction
    if (imageLoader) {
      const sr = startRow();
      const er = endRow();
      const c = cols();
      const centerIndex = Math.floor((sr + er) / 2) * c;
      imageLoader.setViewportContext(pathToIndex, centerIndex);
    }
    // Update visible count for emergency purge sizing
    pressureMonitor?.setVisibleCount(items.length);
  }));

  // Repaint when visible items or selection changes
  createEffect(on(
    () => [visibleItems(), effectiveSelected()] as const,
    () => scheduleRepaint(),
  ));

  // Reset state on path changes
  createEffect(on(
    () => props.paths,
    (paths) => {
      assignedSet.clear();
      needsGeneration.clear();
      inFlightSet.clear();
      failedSet.clear();
      hashRequestedSet.clear();
      hashFailedSet.clear();
      urlVersions.clear();
      urlMap.clear();
      bgCursor = 0;
      thumbGenTotal = 0;
      thumbGenDone = 0;
      imageLoader?.cancelAll();

      pathToIndex.clear();
      for (let i = 0; i < paths.length; i++) {
        pathToIndex.set(paths[i], i);
      }

      setGeneration((g) => g + 1);
      recalcRange?.();
    },
  ));

  // Recalc on container width change
  createEffect(on(containerWidth, () => {
    recalcRange?.();
  }));

  // Tier change (cellSize crossed a LOD boundary) — wipe assigned URLs,
  // renderer textures, and the image loader so visible items re-request
  // at the new tier. The first run on mount is a no-op because `on()`
  // with `defer: true` holds off until the signal actually changes.
  createEffect(on(tier, () => {
    assignedSet.clear();
    urlMap.clear();
    urlVersions.clear();
    needsGeneration.clear();
    inFlightSet.clear();
    failedSet.clear();
    // The renderer is destroyed below — its hash pool is wiped with it,
    // so we must re-request placeholders for the next render.
    hashRequestedSet.clear();
    hashFailedSet.clear();
    bgCursor = 0;
    imageLoader?.cancelAll();
    renderer?.destroy();
    renderer = createRenderer(props.rendererMode);
    if (canvasWrapperRef && renderer) {
      canvasWrapperRef.innerHTML = "";
      canvasWrapperRef.appendChild(renderer.canvas);
    }
    setGeneration((g) => g + 1);
    scheduleRepaint();
  }, { defer: true }));

  // Report content height
  createEffect(on(totalHeight, (h) => {
    props.onContentHeight?.(h);
  }));

  // -----------------------------------------------------------------------
  // Render
  // -----------------------------------------------------------------------

  return (
    <div ref={containerRef} class="w-full" style={{ "user-select": isDragging() ? "none" : undefined }}>
      <Show when={!props.loading && props.paths.length === 0}>
        <div class="flex items-center justify-center h-screen text-neutral-500 text-sm">
          No media files found
        </div>
      </Show>

      <Show when={props.loading}>
        <div class="flex items-center justify-center h-screen text-neutral-500 text-sm">
          Loading...
        </div>
      </Show>

      <Show when={props.paths.length > 0}>
        {/* Virtual scroll container: reserves full scroll height */}
        <div
          style={{
            position: "relative",
            height: `${totalHeight()}px`,
            overflow: "hidden",
          }}
        >
          {/* Inner: positioned at the offset of the first visible row */}
          <div
            ref={(el) => {
              canvasWrapperRef = el;
              if (renderer && el && !el.contains(renderer.canvas)) {
                el.appendChild(renderer.canvas);
                scheduleRepaint();
              }
            }}
            style={{
              position: "absolute",
              top: "0",
              left: `${gap()}px`,
              right: `${gap()}px`,
              transform: `translateY(${sliceOffsetY()}px)`,
              "will-change": "transform",
            }}
          >
            {/* GIF hover overlay: animated GIF on top of the static canvas thumbnail */}
            <Show when={hoveredGif()}>
              {(gif) => (
                <img
                  src={mediaUrl(gif().path)}
                  style={{
                    position: "absolute",
                    left: `${gif().x}px`,
                    top: `${gif().y}px`,
                    width: `${gif().size}px`,
                    height: `${gif().size}px`,
                    "object-fit": "cover",
                    "pointer-events": "none",
                  }}
                  draggable={false}
                  decoding="async"
                />
              )}
            </Show>
          </div>
        </div>
      </Show>
    </div>
  );
}
