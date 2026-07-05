import { For, Show, createSignal, createEffect, createMemo, on, onMount, onCleanup, batch, untrack } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { safeListen as listen, hasTouch, isTauri, type UnlistenFn } from "../../lib/runtime";
import { pointerDistance, pointerMidpoint, type Point } from "../../lib/touch";
import { settings, setSettings } from "../../stores/settingsStore";
import { durationByPath } from "../../stores/galleryStore";
import { ensureTierThumbnails, getThumbnailsBatch, precacheThumbnails, thumbUrl, THUMB_REGENERATED_EVENT, type ThumbTier, type ThumbnailResult } from "../../lib/ipc";
import { thumbGenStarted, thumbGenProgress, thumbGenFinished } from "../../stores/thumbnailProgressStore";
import { recordCacheMiss } from "../../lib/perfMonitor";
import { createDragSelect, createEdgeScroll } from "../../lib/galleryControls";
import { createScrollDynamics, CHEAP_RUNG_DURING_GATE } from "../../lib/scrollDynamics";
import { createThumbSwapper } from "../../lib/thumbSwap";
import { pickByPriority } from "../../lib/loadPriority";
import { ThumbnailCell, markUrlLoaded } from "./ThumbnailCell";

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

// Two-zone asymmetric render buffer, more rows ahead of scroll direction than
// behind. The outer window renders cells at the cheap "s" tier — small enough
// to keep a deep look-ahead loaded — and the inner window carries the target
// tier; cells upgrade as rows cross into it (off-screen, so invisibly during
// normal scrolling).
const BUFFER_AHEAD = 12;
const BUFFER_BEHIND = 4;
const FULL_AHEAD = 5;
const FULL_BEHIND = 2;

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

// LOD tier native pixel sizes (longest edge of the square crop).
const TIER_PX = { s: 128, m: 512, l: 1024 } as const;
// Tier ordering for upgrade decisions — during a fling a cell may hold a
// lower ("cheap") tier than pickTier() wants, upgraded once scrolling settles.
const TIER_RANK: Partial<Record<ThumbTier, number>> = { s: 0, m: 1, l: 2 };
// How much upscale a tier may cover before bumping to the next one up. Slight
// upscaling of a downsampled thumbnail is visually minor, but decoding a 2×-
// larger tier than the cell can show is the dominant per-cell cost on
// WebKitGTK — so we tolerate a little softness to avoid it. 1.25 keeps the
// micro boundary near the old 160px and lets the 512px tier cover cells up to
// ~640px instead of punting to 1024.
const TIER_UPSCALE_TOLERANCE = 1.25;

// Cap on the DPR multiplier so a 4×-DPR phone doesn't always punt to the
// largest tier (which would dominate bandwidth on cellular).
const MAX_DPR_SCALE = 3;

function pickTier(cellPx: number): ThumbTier {
  // Match the decoded image to the *rendered* pixel size (cell × DPR), on
  // desktop too. Serving a far larger tier than the cell shows forces the
  // webview to decode + downscale a big bitmap per cell — see
  // docs/thumbnailStreamingResearch.md. DPR is capped so a 4×-DPR phone doesn't
  // always punt to the largest tier.
  const dpr =
    typeof window !== "undefined"
      ? Math.min(window.devicePixelRatio || 1, MAX_DPR_SCALE)
      : 1;
  const needed = cellPx * dpr;
  if (needed <= TIER_PX.s * TIER_UPSCALE_TOLERANCE) return "s";
  if (needed <= TIER_PX.m * TIER_UPSCALE_TOLERANCE) return "m";
  return "l";
}

export function GalleryGrid(props: GalleryGridProps) {
  const thumbSize = () => settings().display.thumbnail_size;
  const gap = () => settings().display.grid_gap;

  // Measured container width (updated by ResizeObserver).
  const [containerWidth, setContainerWidth] = createSignal(0);

  // Rendered row range (the outer, cheap-tier window) — only updated when it
  // actually changes.
  const [startRow, setStartRow] = createSignal(0);
  const [endRow, setEndRow] = createSignal(0);
  // Inner full-resolution window (viewport + FULL_* buffers) within the
  // rendered range; rows inside it get / upgrade to the target tier.
  const [fullStartRow, setFullStartRow] = createSignal(0);
  const [fullEndRow, setFullEndRow] = createSignal(0);

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

  // Shared pointer controls: Ctrl/Cmd-drag range select + click handling, and
  // edge-scroll while dragging. Identical behavior in JustifiedGrid.
  const { isDragging, effectiveSelected, handleDragStart, handleDragEnter, handleItemClick, handleBackgroundClick } =
    createDragSelect(props);
  createEdgeScroll(isDragging);

  // -----------------------------------------------------------------------
  // Thumbnail streaming state
  // -----------------------------------------------------------------------

  /** Paths that currently have a protocol URL in thumbMap → the tier that URL
   *  serves. During a fling new cells get the cheap "s" rung; the assignment
   *  effect upgrades any entry below the target tier once scrolling settles. */
  const assignedRung = new Map<string, ThumbTier>();
  /** Paths where the protocol handler returned 404 — need IPC generation. */
  const needsGeneration = new Set<string>();
  /** Paths currently in-flight for IPC generation. */
  const inFlightSet = new Set<string>();
  /** Paths that permanently failed (won't be retried). */
  const failedSet = new Set<string>();
  /** Coalesced paths: accumulated during in-flight fetch, merged into next batch. */
  const coalescedPaths = new Set<string>();
  /** Paths already sent to the backend by landing-zone warming during a
   *  fling, so successive drains of the same fling don't re-issue them. */
  const landingWarmed = new Set<string>();
  /** Cache-bust version per path — incremented after IPC generation. */
  const urlVersions = new Map<string, number>();
  /** Cache-bust epoch for full invalidations (rebuilds). Thumbnail responses
   *  are served cacheable, so a rebuild must change every URL — bumping this
   *  shifts the effective version of every path at once. */
  let versionEpoch = 0;
  /** Reverse index: path → position in props.paths for O(1) eviction lookups. */
  const pathToIndex = new Map<string, number>();

  let bgCursor = 0;
  let recalcRange: (() => void) | undefined;

  /** Build the (possibly cache-busted) protocol URL for a path. */
  const thumbSrcFor = (path: string, t: ThumbTier) => {
    const v = (urlVersions.get(path) ?? 0) + versionEpoch;
    return v > 0 ? `${thumbUrl(path, t)}?v=${v}` : thumbUrl(path, t);
  };

  /** Bump a path's version so its next URL points at fresh bytes. */
  const bumpVersion = (path: string) => {
    urlVersions.set(path, (urlVersions.get(path) ?? 0) + 1);
  };

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

  // Off-DOM decode-then-swap for tier upgrades on cells already showing an
  // image, so the cheap rung stays on screen (no skeleton flash) until the
  // target tier is ready. Cancelled per-path on eviction, wholesale on resets.
  const swapper = createThumbSwapper({
    generation,
    isCurrent: (path, gen) => generation() === gen && assignedRung.has(path),
    apply: (path, url) => {
      markUrlLoaded(url); // bytes already decoded — suppress the fade-in
      setThumbMap(path, url);
    },
    onMiss: handleThumbError,
  });
  onCleanup(() => swapper.cancelAll());

  // Listen for streamed thumbnail results — update URLs as each arrives
  // rather than waiting for the full batch to complete.
  let unlistenStreamed: UnlistenFn | undefined;
  onMount(() => {
    listen<ThumbnailResult>("thumb:streamed", (event) => {
      const r = event.payload;
      if (!inFlightSet.has(r.path)) return;
      bumpVersion(r.path);
      if (assignedRung.has(r.path)) assignedRung.set(r.path, tier());
      setThumbMap(r.path, thumbSrcFor(r.path, tier()));
    }).then((fn) => {
      unlistenStreamed = fn;
    });
  });
  onCleanup(() => unlistenStreamed?.());

  // Listen for one-shot regeneration. Unlike thumb:streamed there is no
  // in-flight batch to gate on — just bump the URL version so WebKit's
  // image cache fetches the fresh bytes from the protocol handler.
  const handleRegenerated = (path: string) => {
    bumpVersion(path);
    if (assignedRung.has(path)) {
      assignedRung.set(path, tier());
      setThumbMap(path, thumbSrcFor(path, tier()));
    }
  };
  let unlistenRegenerated: UnlistenFn | undefined;
  onMount(() => {
    listen<ThumbnailResult>("thumb:regenerated", (event) => {
      handleRegenerated(event.payload.path);
    }).then((fn) => {
      unlistenRegenerated = fn;
    });
    // Web stand-in for the Tauri event (dispatched by ContextMenu after the
    // regenerate invoke resolves; `listen` above is a no-op off-desktop).
    const domRegen = (e: Event) => {
      const path = (e as CustomEvent<{ path: string }>).detail?.path;
      if (path) handleRegenerated(path);
    };
    window.addEventListener(THUMB_REGENERATED_EVENT, domRegen);
    onCleanup(() => window.removeEventListener(THUMB_REGENERATED_EVENT, domRegen));
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

  // Scroll velocity/direction tracking + the WebKitGTK decode gate (never
  // engages on the web client). Owns the window scroll listener; each frame
  // runs jump detection and the range recalc.
  const dynamics = createScrollDynamics({
    rowHeight,
    cellsPerRow: cols,
    cellCostPx: () => {
      const px = TIER_PX[tier() as keyof typeof TIER_PX] ?? TIER_PX.m;
      return px * px;
    },
    onFrame: ({ dy }) => {
      // Jump detection: a huge delta means the user warped (scrollbar drag,
      // scroll-to-index) — drop all queued/in-flight work for the old spot.
      if (dy > window.innerHeight * JUMP_FACTOR) {
        setGeneration((g) => g + 1);
        assignedRung.clear();
        swapper.cancelAll();
        needsGeneration.clear();
        coalescedPaths.clear();
        inFlightSet.clear();
        failedSet.clear();
        landingWarmed.clear();
        bgCursor = 0;
        thumbGenTotal = 0;
        thumbGenDone = 0;
      }
      recalcRange?.();
    },
  });
  onCleanup(() => dynamics.dispose());

  // -----------------------------------------------------------------------
  // Visible items — derived from startRow/endRow signals (stable references)
  // -----------------------------------------------------------------------

  const visibleItems = createMemo(() => {
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
  });

  // Visible paths in grid order. Rendered with a path-keyed <For> so a cell
  // that stays on screen across a scroll keeps its DOM node and <img src> — no
  // re-decode, no flicker (an index-keyed <Index> rewrote every slot's src as
  // the window shifted). CSS grid flow places nodes by order, and <For> moves
  // existing nodes to keep that order on reorder.
  const visiblePaths = createMemo(() => visibleItems().map((it) => it.path));

  // Y offset for the rendered slice of cells.
  const sliceOffsetY = () => startRow() * rowHeight();

  // -----------------------------------------------------------------------
  // Optimistic URL assignment: set protocol URLs as soon as items are visible.
  // Cached thumbnails load instantly via the protocol handler (no IPC delay).
  // Uncached ones 404 → onerror → queued for IPC generation.
  // -----------------------------------------------------------------------

  createEffect(on(
    [visibleItems, dynamics.decodeGate, dynamics.settled, fullStartRow, fullEndRow],
    ([items, gated, settled, fullStart, fullEnd]) => {
    // While the WebKitGTK decode gate is up, leave new cells on their skeleton
    // so the main thread isn't buried under image decodes mid-scroll. When it
    // releases this effect re-runs and assigns whatever's now on screen.
    // Already-assigned cells keep their src, so visible content stays.
    if (gated && !CHEAP_RUNG_DURING_GATE) return;
    const target = tier();
    // Resolution ladder: a cell gets the tiny "s" tier when it's outside the
    // inner full-res window (deep look-ahead stays cheap) or while a fling is
    // in progress (content flying past isn't worth full decodes), and is
    // upgraded to the target tier when it sits in the inner window with
    // scrolling settled — usually off-screen, so the swap isn't seen. On the
    // desktop webview the hard gate covers flings, so the fling rung only
    // applies there behind the experiment flag.
    const cheap = isTauri() ? gated : !settled;
    const canDegrade = (TIER_RANK[target] ?? 0) > 0;
    const c = cols();
    const fullStartIdx = fullStart * c;
    const fullEndIdx = fullEnd * c;
    const updates: [string, string][] = [];
    const upgrades: string[] = [];
    for (const item of items as ReturnType<typeof visibleItems>) {
      const inFull = item.index >= fullStartIdx && item.index < fullEndIdx;
      const cur = assignedRung.get(item.path);
      if (cur === undefined) {
        const rung: ThumbTier = canDegrade && (cheap || !inFull) ? "s" : target;
        assignedRung.set(item.path, rung);
        updates.push([item.path, thumbSrcFor(item.path, rung)]);
      } else if (
        inFull &&
        !cheap &&
        (TIER_RANK[cur] ?? 0) < (TIER_RANK[target] ?? 0)
      ) {
        assignedRung.set(item.path, target);
        upgrades.push(item.path);
      }
    }
    if (updates.length > 0) {
      batch(() => {
        for (const [path, url] of updates) {
          setThumbMap(path, url);
        }
      });
    }
    // Upgrades decode off-DOM and swap in on success, so the cheap rung stays
    // visible with no skeleton flash.
    for (const path of upgrades) {
      swapper.swap(path, thumbSrcFor(path, target));
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

      let bufferTop = dynamics.direction() === 1 ? BUFFER_BEHIND : BUFFER_AHEAD;
      let bufferBottom = dynamics.direction() === 1 ? BUFFER_AHEAD : BUFFER_BEHIND;
      let fullTop = dynamics.direction() === 1 ? FULL_BEHIND : FULL_AHEAD;
      let fullBottom = dynamics.direction() === 1 ? FULL_AHEAD : FULL_BEHIND;

      // While pinching, the slice is visually scaled by `pinchScale` about the
      // focal point. Scaling down (zoom out) makes the viewport show ~1/scale
      // more rows, so render that many extra rows on both sides to avoid blanks
      // — all at full resolution, since they're about to be on screen.
      if (untrack(pinchActive)) {
        const s = Math.max(0.2, untrack(pinchScale));
        const extra = Math.min(60, Math.ceil((vh / rh) * (1 / s)) + 2);
        bufferTop = extra;
        bufferBottom = extra;
        fullTop = extra;
        fullBottom = extra;
      }

      const firstRow = Math.floor(relativeTop / rh);
      const lastRow = Math.ceil(relativeBottom / rh);
      const newStart = Math.max(0, firstRow - bufferTop);
      const newEnd = Math.min(totalRows(), lastRow + bufferBottom);
      const newFullStart = Math.max(0, firstRow - fullTop);
      const newFullEnd = Math.min(totalRows(), lastRow + fullBottom);

      // Only update signals when the range actually changes — this is the key
      // optimization that prevents reactive recomputation on every scroll pixel.
      if (
        newStart !== untrack(startRow) ||
        newEnd !== untrack(endRow) ||
        newFullStart !== untrack(fullStartRow) ||
        newFullEnd !== untrack(fullEndRow)
      ) {
        batch(() => {
          setStartRow(newStart);
          setEndRow(newEnd);
          setFullStartRow(newFullStart);
          setFullEndRow(newFullEnd);
        });
      }
    };

    // WebKitGTK doesn't propagate wheel events to window scroll natively, so we
    // intercept them and drive the scroll ourselves with momentum smoothing.
    // Two WebKitGTK quirks (vs. Chromium in the web client) need handling:
    //   1. Traditional mouse wheels arrive as DOM_DELTA_LINE (deltaY ±1 per
    //      notch), not pixels — so we normalize by deltaMode, mapping one line
    //      to one grid row (= one column width, cells are square).
    //   2. Fractional window.scrollBy() steps get rounded away every frame, so
    //      relative stepping silently drops the tail of each gesture by an
    //      amount that varies with frame timing. We instead animate an absolute
    //      float target and keep our own float position, immune to rounding.
    let wheelTargetY = 0; // absolute scroll target (float)
    let wheelCurrentY = 0; // our float view of scrollY, immune to engine rounding
    let wheelAnimating = false;
    let wheelRafId = 0;
    const WHEEL_DECAY = 0.8; // fraction of remaining distance covered per frame
    const WHEEL_SETTLE = 0.5; // px from target at which we snap and stop

    // Convert a wheel delta to pixels. Line mode maps one line to one row so a
    // single notch advances exactly one column width; page mode → viewport.
    const wheelDeltaPx = (e: WheelEvent) => {
      if (e.deltaMode === 1) return e.deltaY * rowHeight();
      if (e.deltaMode === 2) return e.deltaY * window.innerHeight;
      return e.deltaY;
    };

    const drainWheel = () => {
      const diff = wheelTargetY - wheelCurrentY;
      if (Math.abs(diff) < WHEEL_SETTLE) {
        wheelCurrentY = wheelTargetY;
        window.scrollTo(0, Math.round(wheelTargetY));
        wheelAnimating = false;
        wheelRafId = 0;
        scheduleFetch(); // Wheel scroll settled — fetch now
        return;
      }
      wheelCurrentY += diff * (1 - WHEEL_DECAY);
      window.scrollTo(0, Math.round(wheelCurrentY));
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
      const lo = settings().display.thumb_size_min ?? THUMB_SIZE_MIN;
      const hi = settings().display.thumb_size_max ?? THUMB_SIZE_MAX;
      const next = Math.max(lo, Math.min(hi, Math.round(rawNext)));
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
      // At the start of a gesture, re-sync our float position from the DOM so
      // we pick up any scrollbar drag / keyboard scroll that happened between
      // gestures. While animating we keep accumulating into wheelTargetY.
      if (!wheelAnimating) {
        wheelCurrentY = window.scrollY;
        wheelTargetY = window.scrollY;
        wheelAnimating = true;
      }
      const maxScroll = Math.max(
        0,
        document.documentElement.scrollHeight - window.innerHeight,
      );
      wheelTargetY = Math.max(
        0,
        Math.min(maxScroll, wheelTargetY + wheelDeltaPx(e)),
      );
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
      for (const p of assignedRung.keys()) {
        const idx = pathToIndex.get(p);
        if (idx !== undefined && (idx < keepStart || idx >= keepEnd)) {
          toEvict.push(p);
        }
      }

      if (toEvict.length > 0) {
        batch(() => {
          for (const p of toEvict) {
            setThumbMap(p, undefined as any);
            assignedRung.delete(p);
            swapper.cancel(p);
          }
        });
      }
    };

    // A fling has a predictable destination — warm thumbnails around the
    // projected landing scroll position (± one viewport) so they're cached by
    // the time the scroll arrives. Backend warm via precacheThumbnails; on
    // the web client additionally prime the browser's HTTP cache with the
    // cheap-rung URLs (responses are cacheable for 1h). Reuses the
    // single-flight inFlightFetch slot — one batch per drain, recomputed from
    // the live projection each time, so a redirected fling self-corrects and
    // wasted warms stay bounded.
    const warmLandingZone = () => {
      const rh = rowHeight();
      const c = cols();
      if (rh <= 0 || c <= 0) return;
      const offset = containerRef?.offsetTop ?? 0;
      const vh = window.innerHeight;
      const landingTop = dynamics.projectedLandingY() - offset;
      const firstRow = Math.max(0, Math.floor((landingTop - vh) / rh));
      const lastRow = Math.min(totalRows(), Math.ceil((landingTop + 2 * vh) / rh));
      const startIdx = firstRow * c;
      const endIdx = Math.min(props.paths.length, lastRow * c);

      const want: string[] = [];
      for (let i = startIdx; i < endIdx && want.length < BG_BATCH_SIZE; i++) {
        const p = props.paths[i];
        if (!p || assignedRung.has(p) || inFlightSet.has(p) || failedSet.has(p) || landingWarmed.has(p)) continue;
        landingWarmed.add(p);
        want.push(p);
      }
      if (want.length === 0) return;

      inFlightFetch = (async () => {
        try {
          await precacheThumbnails(want);
          // Browser cache warm — low-priority so it can't compete with the
          // visible cells' loads. `priority` is a progressive enhancement
          // (ignored where unsupported; absent from TS 5.4's RequestInit).
          if (!isTauri()) {
            for (const p of want) {
              fetch(thumbSrcFor(p, "s"), { priority: "low" } as RequestInit).catch(() => {});
            }
          }
        } catch (e) {
          console.error("Landing-zone precache failed:", e);
        }
        inFlightFetch = null;
      })();
    };

    const scheduleFetch = () => {
      if (inFlightFetch) return;
      // During a fling, near-viewport generation is wasted work — those cells
      // fly past unseen. Warm the projected landing zone instead; queued
      // 404-generation drains on settle (with stale entries dropped).
      if (dynamics.velocity() > VELOCITY_FAST) {
        warmLandingZone();
        return;
      }

      // Drain coalesced paths into needsGeneration (they're already there,
      // but clear the coalesced set so new items can accumulate during next fetch).
      coalescedPaths.clear();

      // Evict far-from-viewport thumbnails up front. Generation/precache
      // batches below return early, so running eviction last would starve
      // it for as long as there's work queued — and it's cheap.
      evictFaraway();

      const gen = generation();

      // Phase 1: Generate thumbnails that 404'd. Priorities are computed at
      // drain time from the current windows (full-res first, then rendered
      // buffer by distance); leftovers outside the rendered window are
      // dropped — they were queued during a scroll that has since moved past
      // them, and re-queue via 404 if the user scrolls back.
      if (needsGeneration.size > 0) {
        const c = cols();
        const { picked: toGenerate, stale } = pickByPriority(
          needsGeneration,
          (p) => pathToIndex.get(p),
          {
            viewStart: fullStartRow() * c,
            viewEnd: fullEndRow() * c,
            renderStart: startRow() * c,
            renderEnd: endRow() * c,
          },
          BATCH_SIZE,
        );
        if (stale.length > 0) {
          for (const p of stale) needsGeneration.delete(p);
          thumbGenTotal = Math.max(thumbGenDone, thumbGenTotal - stale.length);
          // Dropping may have emptied the queue — close out the progress UI
          // (stale items were counted when queued, so it is showing).
          if (needsGeneration.size === 0 && inFlightSet.size === 0) {
            thumbGenFinished(thumbGenDone);
            thumbGenTotal = 0;
            thumbGenDone = 0;
          }
        }

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
                    bumpVersion(p);
                    if (assignedRung.has(p)) assignedRung.set(p, activeTier);
                    setThumbMap(p, thumbSrcFor(p, activeTier));
                  }
                });
                thumbGenDone += toGenerate.length;
              } else {
                const results = await getThumbnailsBatch(toGenerate);
                if (fetchAbort || generation() !== gen) return;

                resultPaths = new Set(results.map((r) => r.path));
                batch(() => {
                  for (const r of results) {
                    bumpVersion(r.path);
                    if (assignedRung.has(r.path)) assignedRung.set(r.path, activeTier);
                    setThumbMap(r.path, thumbSrcFor(r.path, activeTier));
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
      if (dynamics.velocity() < VELOCITY_SLOW && bgCursor < props.paths.length) {
        const bgNeeded: string[] = [];
        const total = props.paths.length;
        while (bgNeeded.length < BG_BATCH_SIZE && bgCursor < total) {
          const p = props.paths[bgCursor];
          bgCursor++;
          if (p && !assignedRung.has(p) && !failedSet.has(p)) {
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
        }
      }
    };

    // scrollend fires when scrolling stops — replaces manual debounce.
    // Also covers native scrollbar drags and programmatic scrollTo().
    const onScrollEnd = () => {
      dynamics.markSettled();
      scheduleFetch();
    };
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

    // Listen for thumbnail invalidation (e.g. after rebuild). Thumbnail
    // responses are cacheable, so bump the epoch: every rebuilt URL must
    // differ from the one the webview may have cached.
    const onInvalidate = () => {
      setThumbMap(reconcile({}));
      assignedRung.clear();
      swapper.cancelAll();
      needsGeneration.clear();
      inFlightSet.clear();
      failedSet.clear();
      landingWarmed.clear();
      urlVersions.clear();
      versionEpoch++;
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
      window.removeEventListener("scrollend", onScrollEnd);
      window.removeEventListener("wheel", onWheel);
      window.removeEventListener("lightview:thumbnails-invalidated", onInvalidate);
      window.removeEventListener("lightview:scroll-to-index", onScrollToIndex);
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
        assignedRung.clear();
        swapper.cancelAll();
        needsGeneration.clear();
        inFlightSet.clear();
        failedSet.clear();
        landingWarmed.clear();
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
        assignedRung.clear();
        swapper.cancelAll();
        needsGeneration.clear();
        inFlightSet.clear();
        failedSet.clear();
        landingWarmed.clear();
        urlVersions.clear();
        bgCursor = 0;
        const newTier = tier();
        const items = visibleItems();
        batch(() => {
          for (const item of items) {
            assignedRung.set(item.path, newTier);
            setThumbMap(item.path, thumbSrcFor(item.path, newTier));
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
              <For each={visiblePaths()}>
                {(path) => {
                  const index = () => pathToIndex.get(path) ?? -1;
                  return (
                    <ThumbnailCell
                      path={path}
                      thumbSrc={thumbMap[path] ?? null}
                      tier={tier()}
                      durationSec={durationByPath().get(path) ?? null}
                      selected={effectiveSelected().has(path)}
                      onClick={(e: MouseEvent) => handleItemClick({ path, index: index() }, e)}
                      onMouseDown={(e: MouseEvent) => handleDragStart(index(), e)}
                      onMouseEnter={() => handleDragEnter(index())}
                      onContextMenu={(e) => props.onItemContextMenu?.(e, path, index())}
                      onError={handleThumbError}
                    />
                  );
                }}
              </For>
            </div>
          </div>
        </div>
      </Show>
    </div>
  );
}
