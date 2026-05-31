import { Index, Show, createSignal, createEffect, on, onMount, onCleanup, batch, untrack } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { safeListen as listen, isMobile, hasTouch, type UnlistenFn } from "../../lib/runtime";
import { pointerDistance, pointerMidpoint, type Point } from "../../lib/touch";
import { settings, setSettings } from "../../stores/settingsStore";
import { ensureTierThumbnails, getThumbnailsBatch, precacheThumbnails, thumbUrl, type ThumbTier, type ThumbnailResult } from "../../lib/ipc";
import { thumbGenStarted, thumbGenProgress, thumbGenFinished } from "../../stores/thumbnailProgressStore";
import { initGPU } from "../../lib/gpu";
import { recordCacheMiss } from "../../lib/perfMonitor";
import { ThumbnailCell } from "./ThumbnailCell";
import { CanvasGrid } from "./CanvasGrid";

interface GalleryGridProps {
  paths: string[];
  onItemClick: (index: number) => void;
  onItemSelect: (path: string) => void;
  onDragSelect?: (paths: string[]) => void;
  onBackgroundClick?: () => void;
  selectedPaths: Set<string>;
  onItemContextMenu?: (e: MouseEvent, path: string, index: number) => void;
  loading: boolean;
  onContentHeight?: (height: number) => void;
}

// Asymmetric buffer: more rows ahead of scroll direction, fewer behind.
const BUFFER_AHEAD = 5;
const BUFFER_BEHIND = 2;

// How many paths to send per IPC batch call.
const BATCH_SIZE = 128;

// Velocity thresholds (px/s)
const VELOCITY_SLOW = 500;
const VELOCITY_FAST = 3000;

// Jump detection: scroll delta > 2x viewport height triggers generation bump.
const JUMP_FACTOR = 2;

// How many paths to send per background precache call.
const BG_BATCH_SIZE = 64;

// Rows beyond the visible+buffer range before thumbnails are evicted from memory.
const EVICT_ROWS = 15;


// Ctrl+wheel thumbnail resize bounds (px). Each tick retargets the column
// count by ±1 so one scroll notch always produces a visible change.
const THUMB_SIZE_MIN = 80;
const THUMB_SIZE_MAX = 600;

// LOD tier thresholds — cellSize buckets used for URL construction.
// See docs/thumbnailStreamingResearch.md.
const TIER_MICRO_MAX = 160;
const TIER_STANDARD_MAX = 400;

// Cap on the DPR multiplier so a 4×-DPR phone doesn't always punt to the
// largest tier (which would dominate bandwidth on cellular).
const MAX_DPR_SCALE = 3;

function pickTier(cellPx: number): ThumbTier {
  // On mobile, a 130px CSS cell paints ~390 device pixels on a high-DPR phone;
  // serving the "s" tier there is what makes thumbnails look blurry. Scale by
  // DPR so the tier matches the actual rendered resolution. Desktop is left
  // alone so its bandwidth profile doesn't change.
  const dpr =
    isMobile() && typeof window !== "undefined"
      ? Math.min(window.devicePixelRatio || 1, MAX_DPR_SCALE)
      : 1;
  const effective = cellPx * dpr;
  if (effective <= TIER_MICRO_MAX) return "s";
  if (effective <= TIER_STANDARD_MAX) return "m";
  return "l";
}

export function GalleryGrid(props: GalleryGridProps) {
  const mode = () => settings().display.renderer_mode ?? "dom";

  return (
    <Show
      when={mode() === "dom"}
      fallback={
        <CanvasGrid
          paths={props.paths}
          onItemClick={props.onItemClick}
          onItemSelect={props.onItemSelect}
          onDragSelect={props.onDragSelect}
          onBackgroundClick={props.onBackgroundClick}
          selectedPaths={props.selectedPaths}
          onItemContextMenu={props.onItemContextMenu}
          loading={props.loading}
          onContentHeight={props.onContentHeight}
          rendererMode={mode()}
        />
      }
    >
      <DOMGrid {...props} />
    </Show>
  );
}

function DOMGrid(props: GalleryGridProps) {
  const thumbSize = () => settings().display.thumbnail_size;
  const gap = () => settings().display.grid_gap;

  // Measured container width (updated by ResizeObserver).
  const [containerWidth, setContainerWidth] = createSignal(0);

  // Visible row range — only updated when it actually changes.
  const [startRow, setStartRow] = createSignal(0);
  const [endRow, setEndRow] = createSignal(0);

  // Generation counter — incremented on large jumps or path changes.
  const [generation, setGeneration] = createSignal(0);

  // Store for protocol URLs (lightview://thumb/...).
  const [thumbMap, setThumbMap] = createStore<Record<string, string>>({});

  let containerRef: HTMLDivElement | undefined;
  // The virtual-scroll layer we apply the live pinch `scale()` to (so the grid
  // zooms smoothly under the fingers before committing a new column count).
  let scrollContainerRef: HTMLDivElement | undefined;

  // Pinch-to-zoom state. `pinchActive` widens the render buffer so scaling the
  // slice down (zoom out) doesn't reveal blank rows; `pinchScale` is the live
  // gesture scale, read by recalcRange to size that buffer.
  const [pinchActive, setPinchActive] = createSignal(false);
  const [pinchScale, setPinchScale] = createSignal(1);

  // -----------------------------------------------------------------------
  // Drag-to-select state
  // -----------------------------------------------------------------------
  const [isDragging, setIsDragging] = createSignal(false);
  const [dragStartIndex, setDragStartIndex] = createSignal(-1);
  const [dragCurrentIndex, setDragCurrentIndex] = createSignal(-1);
  // Snapshot of selection when drag started (for additive Ctrl+drag)
  let dragBaseSelection = new Set<string>();
  // Suppress the next click after a multi-item drag completes
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

  const handleDragStart = (index: number, e: MouseEvent) => {
    // Only left mouse button, only drag-select with Ctrl/Cmd
    if (e.button !== 0) return;
    if (!(e.ctrlKey || e.metaKey)) return;
    e.preventDefault(); // Prevent text selection during drag
    dragBaseSelection = new Set(props.selectedPaths);
    setIsDragging(true);
    setDragStartIndex(index);
    setDragCurrentIndex(index);
  };

  const handleDragEnter = (index: number) => {
    if (!isDragging()) return;
    setDragCurrentIndex(index);
  };

  const handleItemClick = (item: { path: string; index: number }, e: MouseEvent) => {
    if (suppressClick) {
      suppressClick = false;
      return;
    }
    if (e.ctrlKey || e.metaKey) {
      props.onItemSelect(item.path);
    } else if (props.selectedPaths.size > 0) {
      // Clear selection first — don't open viewer until selection is gone
      props.onBackgroundClick?.();
    } else {
      props.onItemClick(item.index);
    }
  };

  const handleBackgroundClick = (e: MouseEvent) => {
    // Only fire when clicking the background, not on a thumbnail
    const target = e.target as HTMLElement;
    if (!target.closest(".thumb-cell") && !e.ctrlKey && !e.metaKey) {
      props.onBackgroundClick?.();
    }
  };

  // Global mouseup to end drag
  onMount(() => {
    const onMouseUp = () => {
      if (!isDragging()) return;
      const dragged = dragSelectedPaths();
      const wasMultiDrag = dragStartIndex() !== dragCurrentIndex();
      setIsDragging(false);

      if (!wasMultiDrag) {
        // Single click (no real drag) — let onClick handle it
        setDragStartIndex(-1);
        setDragCurrentIndex(-1);
        return;
      }

      // Suppress the click event that fires after mouseup
      suppressClick = true;

      // Merge base selection with drag selection
      const merged = new Set(dragBaseSelection);
      for (const p of dragged) merged.add(p);

      if (props.onDragSelect) {
        props.onDragSelect([...merged]);
      }
      setDragStartIndex(-1);
      setDragCurrentIndex(-1);
    };

    window.addEventListener("mouseup", onMouseUp);
    onCleanup(() => window.removeEventListener("mouseup", onMouseUp));
  });

  // Compute effective selection (base + drag range) for display during drag
  const effectiveSelected = () => {
    if (!isDragging()) return props.selectedPaths;
    const dragged = dragSelectedPaths();
    const merged = new Set(dragBaseSelection);
    for (const p of dragged) merged.add(p);
    return merged;
  };

  // -----------------------------------------------------------------------
  // Thumbnail streaming state
  // -----------------------------------------------------------------------

  /** Paths that currently have a protocol URL in thumbMap. */
  const assignedSet = new Set<string>();
  /** Paths where the protocol handler returned 404 — need IPC generation. */
  const needsGeneration = new Set<string>();
  /** Paths currently in-flight for IPC generation. */
  const inFlightSet = new Set<string>();
  /** Paths that permanently failed (won't be retried). */
  const failedSet = new Set<string>();
  /** Coalesced paths: accumulated during in-flight fetch, merged into next batch. */
  const coalescedPaths = new Set<string>();
  /** Cache-bust version per path — incremented after IPC generation. */
  const urlVersions = new Map<string, number>();
  /** Reverse index: path → position in props.paths for O(1) eviction lookups. */
  const pathToIndex = new Map<string, number>();

  let bgCursor = 0;
  let recalcRange: (() => void) | undefined;

  // Thumbnail generation progress tracking
  let thumbGenTotal = 0;
  let thumbGenDone = 0;

  /** Called by ThumbnailCell when the protocol handler returns 404. */
  const handleThumbError = (path: string) => {
    if (needsGeneration.has(path) || inFlightSet.has(path) || failedSet.has(path) || coalescedPaths.has(path)) return;
    recordCacheMiss();
    // If a fetch is in-flight, coalesce into the next batch instead of needsGeneration
    // to allow merging with other pending requests.
    needsGeneration.add(path);
    coalescedPaths.add(path);
    thumbGenTotal++;
    if (thumbGenTotal === 1) {
      thumbGenStarted(thumbGenTotal);
    } else {
      thumbGenProgress(thumbGenDone, thumbGenTotal);
    }
  };

  // Initialize WebGPU on mount (non-blocking, caches result).
  onMount(() => {
    initGPU();
  });

  // Listen for streamed thumbnail results — update URLs as each arrives
  // rather than waiting for the full batch to complete.
  let unlistenStreamed: UnlistenFn | undefined;
  onMount(() => {
    listen<ThumbnailResult>("thumb:streamed", (event) => {
      const r = event.payload;
      if (!inFlightSet.has(r.path)) return;
      const v = (urlVersions.get(r.path) ?? 0) + 1;
      urlVersions.set(r.path, v);
      setThumbMap(r.path, `${thumbUrl(r.path, tier())}?v=${v}`);
    }).then((fn) => {
      unlistenStreamed = fn;
    });
  });
  onCleanup(() => unlistenStreamed?.());

  // Listen for one-shot regeneration. Unlike thumb:streamed there is no
  // in-flight batch to gate on — just bump the URL version so WebKit's
  // image cache fetches the fresh bytes from the protocol handler.
  let unlistenRegenerated: UnlistenFn | undefined;
  onMount(() => {
    listen<ThumbnailResult>("thumb:regenerated", (event) => {
      const r = event.payload;
      const v = (urlVersions.get(r.path) ?? 0) + 1;
      urlVersions.set(r.path, v);
      if (assignedSet.has(r.path)) {
        setThumbMap(r.path, `${thumbUrl(r.path, tier())}?v=${v}`);
      }
    }).then((fn) => {
      unlistenRegenerated = fn;
    });
  });
  onCleanup(() => unlistenRegenerated?.());

  // -----------------------------------------------------------------------
  // Grid geometry — derived from container width, thumb size, gap
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

  // LOD tier derived from rendered cell size. Tier changes invalidate the
  // URL cache below so visible items refetch at the new resolution.
  const tier = () => pickTier(cellSize());

  const totalRows = () => Math.ceil(props.paths.length / cols());

  // Total height of the virtual content area.
  const totalHeight = () => {
    const rows = totalRows();
    if (rows === 0) return 0;
    return rows * cellSize() + (rows - 1) * gap();
  };

  // -----------------------------------------------------------------------
  // Visible items — derived from startRow/endRow signals (stable references)
  // -----------------------------------------------------------------------

  const visibleItems = () => {
    const sr = startRow();
    const er = endRow();
    const c = cols();
    const startIdx = sr * c;
    const endIdx = Math.min(er * c, props.paths.length);
    const items: { path: string; index: number }[] = [];
    for (let i = startIdx; i < endIdx; i++) {
      items.push({ path: props.paths[i], index: i });
    }
    return items;
  };

  // Y offset for the rendered slice of cells.
  const sliceOffsetY = () => startRow() * rowHeight();

  // -----------------------------------------------------------------------
  // Optimistic URL assignment: set protocol URLs as soon as items are visible.
  // Cached thumbnails load instantly via the protocol handler (no IPC delay).
  // Uncached ones 404 → onerror → queued for IPC generation.
  // -----------------------------------------------------------------------

  createEffect(on(visibleItems, (items) => {
    const t = tier();
    const updates: [string, string][] = [];
    for (const item of items) {
      if (!assignedSet.has(item.path)) {
        assignedSet.add(item.path);
        const v = urlVersions.get(item.path);
        const url = v ? `${thumbUrl(item.path, t)}?v=${v}` : thumbUrl(item.path, t);
        updates.push([item.path, url]);
      }
    }
    if (updates.length > 0) {
      batch(() => {
        for (const [path, url] of updates) {
          setThumbMap(path, url);
        }
      });
    }
  }));

  // -----------------------------------------------------------------------
  // Scroll + resize: compute visible range, update signals only when changed
  // -----------------------------------------------------------------------

  onMount(() => {
    if (containerRef) {
      setContainerWidth(containerRef.clientWidth);
    }

    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        setContainerWidth(entry.contentRect.width);
      }
    });
    if (containerRef) ro.observe(containerRef);

    // Scroll state (raw, not reactive — avoids triggering effects on every pixel)
    let currentScrollY = window.scrollY;
    let currentVelocity = 0;
    let lastScrollY = window.scrollY;
    let lastTimestamp = performance.now();
    let scrollDirection: 1 | -1 = 1; // 1 = down, -1 = up

    /** Recompute the visible row range from current scroll position and update signals if changed. */
    recalcRange = () => {
      const sy = window.scrollY;
      const vh = window.innerHeight;
      const offset = containerRef?.offsetTop ?? 0;
      const rh = rowHeight();
      const cs = cellSize();
      if (rh <= 0 || cs <= 0) return;

      const relativeTop = Math.max(0, sy - offset);
      const relativeBottom = relativeTop + vh;

      let bufferTop = scrollDirection === 1 ? BUFFER_BEHIND : BUFFER_AHEAD;
      let bufferBottom = scrollDirection === 1 ? BUFFER_AHEAD : BUFFER_BEHIND;

      // While pinching, the slice is visually scaled by `pinchScale` about the
      // focal point. Scaling down (zoom out) makes the viewport show ~1/scale
      // more rows, so render that many extra rows on both sides to avoid blanks.
      if (untrack(pinchActive)) {
        const s = Math.max(0.2, untrack(pinchScale));
        const extra = Math.min(60, Math.ceil((vh / rh) * (1 / s)) + 2);
        bufferTop = extra;
        bufferBottom = extra;
      }

      const newStart = Math.max(0, Math.floor(relativeTop / rh) - bufferTop);
      const newEnd = Math.min(totalRows(), Math.ceil(relativeBottom / rh) + bufferBottom);

      // Only update signals when the range actually changes — this is the key
      // optimization that prevents reactive recomputation on every scroll pixel.
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
        scrollDirection = y >= lastScrollY ? 1 : -1;

        // Jump detection
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
        }

        lastScrollY = y;
        lastTimestamp = now;

        recalcRange?.();
        rafId = 0;
      });
    };

    window.addEventListener("scroll", onScroll, { passive: true });

    // WebKitGTK doesn't propagate wheel events to window scroll natively.
    // Intercept wheel events and translate them into window.scrollBy calls
    // with momentum-like smoothing for a natural feel.
    let wheelAccumulator = 0;
    let wheelRafId = 0;
    const WHEEL_DECAY = 0.85;
    const WHEEL_THRESHOLD = 0.5;

    const drainWheel = () => {
      if (Math.abs(wheelAccumulator) < WHEEL_THRESHOLD) {
        wheelAccumulator = 0;
        wheelRafId = 0;
        scheduleFetch(); // Wheel scroll settled — fetch now
        return;
      }
      const step = wheelAccumulator * (1 - WHEEL_DECAY);
      wheelAccumulator -= step;
      window.scrollBy(0, step);
      wheelRafId = requestAnimationFrame(drainWheel);
    };

    // Commit a new thumbnail size, keeping the image under the anchor screen
    // point pinned in place. Shared by Ctrl+wheel (anchor = cursor), the column
    // stepper below, and the touch pinch commit (anchor = two-finger midpoint).
    // No-ops if the resulting column count wouldn't change.
    const applyThumbSizeAnchored = (rawNext: number, anchorX: number, anchorY: number) => {
      const w = containerWidth();
      const g = gap();
      if (w <= 0) return;
      const next = Math.max(THUMB_SIZE_MIN, Math.min(THUMB_SIZE_MAX, Math.round(rawNext)));
      const cur = settings().display.thumbnail_size;
      const curCols = Math.max(1, Math.floor((w + g) / (cur + g)));
      const newCols = Math.max(1, Math.floor((w + g) / (next + g)));
      if (newCols === curCols) return;

      // --- Anchor: keep the anchored image at the same screen position ---
      const containerTop = containerRef?.offsetTop ?? 0;
      const curCellSize = (w - (curCols - 1) * g) / curCols;
      const curRowHeight = curCellSize + g;

      const anchorPageY = anchorY + window.scrollY;
      const anchorInGrid = anchorPageY - containerTop;
      const anchorRow = Math.max(0, Math.floor(anchorInGrid / curRowHeight));

      const gridLeft = (containerRef?.getBoundingClientRect().left ?? 0) + g;
      const anchorInRow = anchorX - gridLeft;
      const anchorCol = Math.max(0, Math.min(curCols - 1, Math.floor(anchorInRow / (curCellSize + g))));
      const anchorIndex = Math.min(props.paths.length - 1, anchorRow * curCols + anchorCol);

      // Where the anchored image's row currently is on screen
      const imagePageY = containerTop + anchorRow * curRowHeight;
      const imageClientY = imagePageY - window.scrollY;

      setSettings((prev) => ({
        ...prev,
        display: { ...prev.display, thumbnail_size: next },
      }));

      // Compute new position of that image and adjust scroll
      const newCellSize = (w - (newCols - 1) * g) / newCols;
      const newRowHeight = newCellSize + g;
      const newRow = Math.floor(anchorIndex / newCols);
      const newImagePageY = containerTop + newRow * newRowHeight;
      window.scrollTo(0, newImagePageY - imageClientY);
    };

    // Step to `targetCols` columns (Ctrl+wheel): pick a thumb_size in the middle
    // of the target bucket so the layout snaps cleanly to that column count.
    const resizeByCols = (targetCols: number, anchorX: number, anchorY: number) => {
      const w = containerWidth();
      const g = gap();
      if (w <= 0 || targetCols < 1) return;
      const upper = (w + g) / targetCols - g;
      const lower = (w + g) / (targetCols + 1) - g;
      applyThumbSizeAnchored((upper + lower) / 2, anchorX, anchorY);
    };

    const onWheel = (e: WheelEvent) => {
      // Ctrl+wheel resizes thumbnails instead of scrolling, stepping the column
      // count by 1 per tick. During a Ctrl+drag selection, fall through to
      // scroll so the user can extend the drag past the viewport.
      if ((e.ctrlKey || e.metaKey) && !isDragging()) {
        e.preventDefault();
        const w = containerWidth();
        const g = gap();
        if (w <= 0) return;
        const curCols = Math.max(1, Math.floor((w + g) / (settings().display.thumbnail_size + g)));
        resizeByCols(e.deltaY < 0 ? curCols - 1 : curCols + 1, e.clientX, e.clientY);
        return;
      }
      e.preventDefault();
      wheelAccumulator += e.deltaY;
      if (!wheelRafId) {
        wheelRafId = requestAnimationFrame(drainWheel);
      }
    };
    window.addEventListener("wheel", onWheel, { passive: false });

    // -----------------------------------------------------------------------
    // Touch pinch-to-zoom: two fingers smoothly scale the grid (a cheap CSS
    // transform on the scroll layer, anchored at the pinch midpoint), then the
    // new column count is committed once on release. Stepping the columns live
    // (as Ctrl+wheel does) felt janky and could blank/crash on zoom-out; this
    // mirrors how Apple Photos scales the grid during the gesture. One-finger
    // touch still scrolls natively (the container is `touch-action: pan-y`).
    // -----------------------------------------------------------------------
    const gridPointers = new Map<number, Point>();
    let pinchStartDist = 0;
    let pinchMidX = 0;
    let pinchMidY = 0;
    let pinchOriginX = 0;
    let pinchOriginY = 0;

    const applyPinchTransform = (s: number) => {
      if (!scrollContainerRef) return;
      if (s === 1) {
        scrollContainerRef.style.transform = "";
        scrollContainerRef.style.transformOrigin = "";
        scrollContainerRef.style.willChange = "";
      } else {
        scrollContainerRef.style.willChange = "transform";
        scrollContainerRef.style.transformOrigin = `${pinchOriginX}px ${pinchOriginY}px`;
        scrollContainerRef.style.transform = `scale(${s})`;
      }
    };

    const endPinch = () => {
      if (!untrack(pinchActive)) return;
      const s = untrack(pinchScale);
      const cur = settings().display.thumbnail_size;
      applyPinchTransform(1);
      setPinchActive(false);
      setPinchScale(1);
      // Commit the zoom: a scale of s means the user wants thumbnails s× the
      // current size (→ fewer/more columns). Anchored at the start midpoint.
      applyThumbSizeAnchored(cur * s, pinchMidX, pinchMidY);
      recalcRange?.();
    };

    const onGridPointerDown = (e: PointerEvent) => {
      if (e.pointerType !== "touch") return;
      gridPointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
      if (gridPointers.size === 2) {
        const [a, b] = [...gridPointers.values()];
        pinchStartDist = pointerDistance(a, b) || 1;
        const m = pointerMidpoint(a, b);
        pinchMidX = m.x;
        pinchMidY = m.y;
        // transform-origin is relative to the scroll layer's own box.
        const rect = scrollContainerRef?.getBoundingClientRect();
        pinchOriginX = m.x - (rect?.left ?? 0);
        pinchOriginY = m.y - (rect?.top ?? 0);
        setPinchScale(1);
        setPinchActive(true);
        recalcRange?.(); // widen the buffer before scaling begins
        // Capture both pointers so a re-render (recycled cells under the
        // fingers) doesn't fire pointercancel and abort the pinch, and so the
        // browser won't claim the two-finger move as a scroll.
        for (const id of gridPointers.keys()) {
          try { containerRef?.setPointerCapture(id); } catch {}
        }
      }
    };
    const onGridPointerMove = (e: PointerEvent) => {
      if (e.pointerType !== "touch" || !gridPointers.has(e.pointerId)) return;
      gridPointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
      if (!untrack(pinchActive) || gridPointers.size < 2) return;
      e.preventDefault();
      const [a, b] = [...gridPointers.values()];
      const s = Math.max(0.35, Math.min(3, (pointerDistance(a, b) || 1) / pinchStartDist));
      setPinchScale(s);
      applyPinchTransform(s);
      recalcRange?.(); // grow the buffer as the visible area scales
    };
    const onGridPointerEnd = (e: PointerEvent) => {
      const wasTracked = gridPointers.delete(e.pointerId);
      try { containerRef?.releasePointerCapture(e.pointerId); } catch {}
      if (wasTracked && untrack(pinchActive) && gridPointers.size < 2) {
        endPinch();
      }
    };

    if (containerRef) {
      containerRef.addEventListener("pointerdown", onGridPointerDown);
      containerRef.addEventListener("pointermove", onGridPointerMove);
      containerRef.addEventListener("pointerup", onGridPointerEnd);
      containerRef.addEventListener("pointercancel", onGridPointerEnd);
      onCleanup(() => {
        containerRef?.removeEventListener("pointerdown", onGridPointerDown);
        containerRef?.removeEventListener("pointermove", onGridPointerMove);
        containerRef?.removeEventListener("pointerup", onGridPointerEnd);
        containerRef?.removeEventListener("pointercancel", onGridPointerEnd);
      });
    }

    recalcRange(); // initial

    // -----------------------------------------------------------------------
    // Thumbnail fetch loop — generation, background precache, and eviction
    // -----------------------------------------------------------------------

    let fetchAbort = false;
    let inFlightFetch: Promise<void> | null = null;

    /** Remove thumbMap entries for paths far from the current viewport. */
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

      if (toEvict.length > 0) {
        batch(() => {
          for (const p of toEvict) {
            setThumbMap(p, undefined as any);
            assignedSet.delete(p);
          }
        });
      }
    };

    const scheduleFetch = () => {
      if (inFlightFetch) return;

      const now = performance.now();
      if (now - lastTimestamp > 150) currentVelocity = 0;
      if (currentVelocity > VELOCITY_FAST) return;

      // Drain coalesced paths into needsGeneration (they're already there,
      // but clear the coalesced set so new items can accumulate during next fetch).
      coalescedPaths.clear();

      const gen = generation();

      // Phase 1: Generate thumbnails that 404'd.
      // Prioritize near-viewport, drop far-away items to prevent redundant work.
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
            // Drop out-of-range items from needsGeneration — they were
            // queued during a scroll that has since moved past them.
            // They'll re-queue if the user scrolls back.
            if (farAway.length < BATCH_SIZE) farAway.push(p);
          }
        }

        // Near-viewport first, then fill remaining slots with off-screen paths
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
              let resultPaths: Set<string>;
              if (activeTier === "l" || activeTier === "p") {
                await ensureTierThumbnails(toGenerate, activeTier);
                if (fetchAbort || generation() !== gen) return;
                resultPaths = new Set(toGenerate);
                batch(() => {
                  for (const p of toGenerate) {
                    const v = (urlVersions.get(p) ?? 0) + 1;
                    urlVersions.set(p, v);
                    setThumbMap(p, `${thumbUrl(p, activeTier)}?v=${v}`);
                  }
                });
                thumbGenDone += toGenerate.length;
              } else {
                const results = await getThumbnailsBatch(toGenerate);
                if (fetchAbort || generation() !== gen) return;

                resultPaths = new Set(results.map((r) => r.path));
                batch(() => {
                  for (const r of results) {
                    const v = (urlVersions.get(r.path) ?? 0) + 1;
                    urlVersions.set(r.path, v);
                    setThumbMap(r.path, `${thumbUrl(r.path, activeTier)}?v=${v}`);
                  }
                });

                thumbGenDone += results.length;
              }
              for (const p of toGenerate) {
                inFlightSet.delete(p);
                if (!resultPaths.has(p)) failedSet.add(p);
              }

              // Report progress or completion
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

      // Phase 2: Background prefetch (silent, no progress tracking)
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

    // scrollend fires when scrolling stops — replaces manual debounce.
    // Also covers native scrollbar drags and programmatic scrollTo().
    const onScrollEnd = () => scheduleFetch();
    window.addEventListener("scrollend", onScrollEnd);

    // Background interval for precache/eviction when not actively scrolling.
    const bgIntervalId = setInterval(() => {
      if (!inFlightFetch) scheduleFetch();
    }, 500);

    // Also fire immediately on generation change (jump/path change)
    createEffect(
      on(generation, () => {
        scheduleFetch();
      }),
    );

    // Listen for thumbnail invalidation (e.g. after rebuild)
    const onInvalidate = () => {
      setThumbMap(reconcile({}));
      assignedSet.clear();
      needsGeneration.clear();
      inFlightSet.clear();
      failedSet.clear();
      urlVersions.clear();
      bgCursor = 0;
      thumbGenTotal = 0;
      thumbGenDone = 0;
      setGeneration((g) => g + 1);
    };
    window.addEventListener("lightview:thumbnails-invalidated", onInvalidate);

    // Scroll grid to keep the viewed image visible (e.g. arrow key navigation)
    const onScrollToIndex = (e: Event) => {
      const index = (e as CustomEvent).detail as number;
      if (index < 0 || index >= props.paths.length) return;
      const c = cols();
      const rh = rowHeight();
      const offset = containerRef?.offsetTop ?? 0;
      const targetRow = Math.floor(index / c);
      const imageTop = offset + targetRow * rh;
      const imageBottom = imageTop + cellSize();
      const viewTop = window.scrollY;
      const viewBottom = viewTop + window.innerHeight;
      if (imageTop < viewTop || imageBottom > viewBottom) {
        // Center the target row in the viewport
        window.scrollTo(0, imageTop - (window.innerHeight - cellSize()) / 2);
      }
    };
    window.addEventListener("lightview:scroll-to-index", onScrollToIndex);

    onCleanup(() => {
      ro.disconnect();
      window.removeEventListener("scroll", onScroll);
      window.removeEventListener("scrollend", onScrollEnd);
      window.removeEventListener("wheel", onWheel);
      window.removeEventListener("lightview:thumbnails-invalidated", onInvalidate);
      window.removeEventListener("lightview:scroll-to-index", onScrollToIndex);
      if (rafId) cancelAnimationFrame(rafId);
      if (wheelRafId) cancelAnimationFrame(wheelRafId);
      clearInterval(bgIntervalId);
      fetchAbort = true;
    });
  });

  // Reset state when paths change
  createEffect(
    on(
      () => props.paths,
      (paths) => {
        setThumbMap(reconcile({}));
        assignedSet.clear();
        needsGeneration.clear();
        inFlightSet.clear();
        failedSet.clear();
        urlVersions.clear();
        bgCursor = 0;
        thumbGenTotal = 0;
        thumbGenDone = 0;

        pathToIndex.clear();
        for (let i = 0; i < paths.length; i++) {
          pathToIndex.set(paths[i], i);
        }

        setGeneration((g) => g + 1);
        recalcRange?.();
      },
    ),
  );

  // Also recalc when container width changes (e.g. resize, initial measure).
  createEffect(
    on(containerWidth, () => {
      recalcRange?.();
    }),
  );

  // Recalc when layout geometry changes (zoom via Ctrl+wheel).
  // thumbSize/cols changes don't trigger scroll events, so recalcRange
  // must be called directly to update startRow/endRow.
  createEffect(
    on(cols, () => {
      recalcRange?.();
    }),
  );

  // Tier change (cellSize crossed an LOD boundary) — re-point visible items
  // at the new-tier URL in place. We deliberately don't reconcile({}) the
  // map, because that would set every <img> src to null and blank the grid
  // until the next visibleItems re-run. Updating src on a live <img> lets
  // the browser keep the previously-decoded bitmap on screen until the new
  // one is fetched and decoded.
  createEffect(
    on(
      tier,
      () => {
        assignedSet.clear();
        needsGeneration.clear();
        inFlightSet.clear();
        failedSet.clear();
        urlVersions.clear();
        bgCursor = 0;
        const newTier = tier();
        const items = visibleItems();
        batch(() => {
          for (const item of items) {
            assignedSet.add(item.path);
            setThumbMap(item.path, thumbUrl(item.path, newTier));
          }
        });
        setGeneration((g) => g + 1);
      },
      { defer: true },
    ),
  );

  // Report content height to parent for custom scrollbar
  createEffect(
    on(totalHeight, (h) => {
      props.onContentHeight?.(h);
    }),
  );

  // -----------------------------------------------------------------------
  // Render
  // -----------------------------------------------------------------------

  return (
    <div ref={containerRef} class="w-full" style={{ "user-select": isDragging() ? "none" : undefined, "touch-action": hasTouch() ? "pan-y" : undefined }} onClick={handleBackgroundClick}>
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
        {/* Virtual scroll container: reserves full scroll height. Also the
            layer we apply the live pinch `scale()` to (set imperatively). */}
        <div
          ref={scrollContainerRef}
          style={{
            position: "relative",
            height: `${totalHeight()}px`,
            overflow: "hidden",
            contain: "strict",
          }}
        >
          {/* Inner: positioned at the offset of the first visible row */}
          <div
            style={{
              position: "absolute",
              top: "0",
              left: `${gap()}px`,
              right: `${gap()}px`,
              transform: `translateY(${sliceOffsetY()}px)`,
              "will-change": "transform",
            }}
          >
            <div
              style={{
                display: "grid",
                "grid-template-columns": `repeat(${cols()}, 1fr)`,
                gap: `${gap()}px`,
              }}
            >
              <Index each={visibleItems()}>
                {(item) => (
                  <ThumbnailCell
                    path={item().path}
                    thumbSrc={thumbMap[item().path] ?? null}
                    selected={effectiveSelected().has(item().path)}
                    onClick={(e: MouseEvent) => handleItemClick(item(), e)}
                    onMouseDown={(e: MouseEvent) => handleDragStart(item().index, e)}
                    onMouseEnter={() => handleDragEnter(item().index)}
                    onContextMenu={(e) => props.onItemContextMenu?.(e, item().path, item().index)}
                    onError={handleThumbError}
                  />
                )}
              </Index>
            </div>
          </div>
        </div>
      </Show>
    </div>
  );
}
