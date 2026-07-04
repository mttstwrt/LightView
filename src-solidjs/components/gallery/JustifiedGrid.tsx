import { For, Show, createSignal, createEffect, createMemo, on, onMount, onCleanup, batch, untrack } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { safeListen as listen, isMobile, type UnlistenFn } from "../../lib/runtime";
import { settings, setSettings } from "../../stores/settingsStore";
import { durationByPath } from "../../stores/galleryStore";
import { ensureTierThumbnails, thumbUrl, mediaUrl, THUMB_REGENERATED_EVENT, type ThumbTier } from "../../lib/ipc";
import type { MediaMeta } from "../../stores/galleryStore";
import { thumbGenStarted, thumbGenProgress, thumbGenFinished } from "../../stores/thumbnailProgressStore";
import { recordCacheMiss } from "../../lib/perfMonitor";
import { computeJustifiedLayout, rowIndexAtOffset, portraitRowBoost } from "../../lib/justifiedLayout";
import { createDragSelect, createEdgeScroll } from "../../lib/galleryControls";
import { ThumbnailCell, markUrlLoaded } from "./ThumbnailCell";

interface JustifiedGridProps {
  paths: string[];
  /** Aspect ratio (w/h) per path; missing entries fall back to 1:1. */
  aspects: Map<string, number>;
  /** File size + media type per path; drives serve-original-vs-thumbnail at
   *  high zoom. Missing entries fall back to always thumbnailing. */
  itemMeta?: Map<string, MediaMeta>;
  /** Indices that begin a new group — force a row break before each. */
  groupStarts?: number[];
  onItemClick: (index: number) => void;
  onItemSelect: (path: string) => void;
  onDragSelect?: (paths: string[]) => void;
  onBackgroundClick?: () => void;
  selectedPaths: Set<string>;
  onItemContextMenu?: (e: MouseEvent, path: string, index: number) => void;
  loading: boolean;
  onContentHeight?: (height: number) => void;
}

// Rows of buffer rendered beyond the viewport, ahead of / behind scroll.
const BUFFER_AHEAD = 4;
const BUFFER_BEHIND = 2;
// Rows beyond the visible+buffer range before thumbnails are evicted.
const EVICT_ROWS = 12;
// Rows ahead of the viewport (in scroll direction) to warm the high (jh) tier
// when zoomed in, so cells aren't cold 404→generate round-trips on reveal.
const JH_PRECACHE_ROWS = 6;
// How many paths to generate per IPC batch.
const BATCH_SIZE = 96;
// Above this scroll speed (px/sec) we stop assigning new <img> srcs so the
// webview's main thread can keep up with scrolling instead of choking on a
// wall of image decodes. Srcs are assigned once scrolling settles.
const FAST_SCROLL_VELOCITY = 2500;
// Idle gap (ms) after the last fast-scroll frame before scrolling is considered
// settled and src assignment resumes (in case `scrollend` doesn't fire).
const SCROLL_SETTLE_MS = 120;
// Detail levels by zoom. "base" serves the 512px "j" tier. When zoomed in, the
// view serves the *original file* for cheap native-format images (sharp, no
// extra storage) and a 2560px "jh" thumbnail for large/non-native ones. The
// byte cutoff for "serve original" grows with zoom: at "mid" only smaller files
// stream as originals; at "high" most do. Thresholds compare the rendered row
// height in *physical* pixels (row height × DPR), so hi-DPI screens step up
// earlier — exactly where the 512px tier starts to look soft. The gap between
// each up/down pair is hysteresis to avoid thrashing at a boundary.
const MID_UP = 360;
const MID_DOWN = 320;
const HIGH_UP = 560;
const HIGH_DOWN = 500;
// "Serve original" byte cutoffs per level (native image formats only).
const MID_MAX_BYTES = 1.5 * 1024 * 1024;
const HIGH_MAX_BYTES = 3 * 1024 * 1024;
// How much larger than the displayed (physical) cell a source image's longest
// edge may be before we stop serving it as an original and use the jh tier
// instead. Decoding a source much bigger than shown stalls the webview's main
// thread, so we cap the over-decode at this factor.
const ORIGINAL_SRC_TOLERANCE = 2.5;
// Image formats the webview decodes natively, so the original can be served
// directly. HEIC/AVIF/RAW need transcoding/thumbnailing; videos/gifs keep their
// generated thumbnails.
const NATIVE_IMG_EXTS = new Set(["jpg", "jpeg", "png", "webp"]);
// Served-original cells request a backend resize to their longest edge via
// `?fit=`. Quantizing the requested edge up to this bucket keeps the URL stable
// across nearby cell sizes / small zoom steps, so the webview's HTTP cache hits
// instead of re-fetching a near-identical size on every layout tweak.
const FIT_BUCKET = 256;
// Target-row-height (zoom) bounds, mirroring the grid's thumb-size range.
const ROW_HEIGHT_MIN = 80;
const ROW_HEIGHT_MAX = 600;

type DetailLevel = "base" | "mid" | "high";

export function JustifiedGrid(props: JustifiedGridProps) {
  const gap = () => settings().display.grid_gap;

  // Aspect ratios recovered from loaded thumbnails, for paths whose indexed
  // dimensions aren't known yet (a just-added file is inserted with NULL
  // width/height and its dimensions are only written when its thumbnail is
  // generated — after the frontend has already fetched the sorted items). The
  // j/jm/jh tiers are all aspect-preserving, so a loaded cell's natural pixel
  // size gives the exact source aspect. Without this such cells lay out 1:1 and
  // the aspect-correct thumbnail is object-cover cropped to a square — looking
  // like a grid thumbnail until a restart re-reads the now-populated dimensions.
  const [measuredAspects, setMeasuredAspects] = createSignal<Map<string, number>>(new Map());
  // A path's aspect: indexed dimensions win; fall back to a measured value, then
  // 1:1. Reads measuredAspects() so consumers recompute when a cell is measured.
  const aspectOf = (path: string): number =>
    props.aspects.get(path) ?? measuredAspects().get(path) ?? 1;
  const recordMeasuredAspect = (path: string, w: number, h: number) => {
    if (w <= 0 || h <= 0) return;
    // The indexed value takes precedence and never needs a measured override.
    if (props.aspects.has(path)) return;
    const cur = measuredAspects();
    if (cur.has(path)) return;
    const next = new Map(cur);
    next.set(path, w / h);
    setMeasuredAspects(next);
  };

  // Measured width of the justified container; drives the mobile row-height
  // derivation below. Declared up here (before targetRowHeight) so the
  // detail-level effect, which reads targetRowHeight() during setup, doesn't
  // hit the temporal dead zone.
  const [containerWidth, setContainerWidth] = createSignal(0);

  // Target row height (px). On desktop this is the raw thumbnail_size (the
  // continuous zoom). On mobile, thumbnail_size is instead a *column-width*
  // target chosen by the settings "Columns" picker; using it raw as a row
  // height made the justified view ignore the selected column count and snap to
  // a stale default (~3 columns) until a width was re-picked. So on mobile we
  // derive the row height exactly the way the grid sizes its cells: turn the
  // column-width target into a whole column count against the measured width,
  // then use that cell width as the row height. Grid and justified then share
  // one effective size and the picker drives both.
  const targetRowHeight = () => {
    const ts = settings().display.thumbnail_size;
    if (!isMobile()) return ts;
    const g = gap();
    const cw = Math.max(0, containerWidth() - 2 * g);
    if (cw <= 0) return ts;
    const cols = Math.max(1, Math.floor((cw + g) / (ts + g)));
    return (cw - (cols - 1) * g) / cols;
  };

  // Current detail level from the (DPR-aware, hysteretic) zoom. "base" when
  // high-detail is disabled or zoomed out; "mid"/"high" step up as you zoom in.
  const [detailLevel, setDetailLevel] = createSignal<DetailLevel>("base");
  createEffect(() => {
    if (settings().display.justified_high_detail === false) {
      setDetailLevel("base");
      return;
    }
    const dpr = typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1;
    const px = targetRowHeight() * dpr;
    setDetailLevel((prev): DetailLevel => {
      if (prev === "base") return px > HIGH_UP ? "high" : px > MID_UP ? "mid" : "base";
      if (prev === "mid") return px > HIGH_UP ? "high" : px < MID_DOWN ? "base" : "mid";
      return px < MID_DOWN ? "base" : px < HIGH_DOWN ? "mid" : "high";
    });
  });

  // The thumbnail tier backing the current level (used for cells not served as
  // an original, and for the GIF atlas request). Each detail level maps to a
  // resolution sized for the cells it shows: base→512 ("j"), mid→1280 ("jm"),
  // high→2560 ("jh"). The mid tier exists so a mid-zoom cell doesn't decode the
  // 2560px high image at ~1/4 the displayed size.
  const thumbTier = (): ThumbTier =>
    detailLevel() === "base" ? "j" : detailLevel() === "mid" ? "jm" : "jh";

  const extOf = (p: string) => {
    const i = p.lastIndexOf(".");
    return i >= 0 ? p.slice(i + 1).toLowerCase() : "";
  };

  // Longest edge (physical px) a cell is expected to display at. Mirrors the
  // layout's per-row portrait boost (`portraitRowBoost`) so portrait cells —
  // which render taller than `targetRowHeight` — drive a matching resize/source
  // resolution instead of a soft, under-sized one. `aspect` is the cell's own
  // aspect, a per-image stand-in for its row's average (exact for the
  // portrait-only rows where the boost actually bites).
  const displayedLongEdge = (aspect: number): number => {
    const dpr = typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1;
    const h = targetRowHeight() * portraitRowBoost(aspect);
    return h * Math.max(1, aspect) * dpr;
  };

  // Whether `path` should be served via the media server's cell-fit resize
  // (`?fit=`) at the current level, rather than the pre-generated "jm"/"jh"
  // tier. Eligible: a native-format still image that isn't so large the on-
  // demand resize is wasteful versus the (precached) tier. Otherwise we fall
  // back to the tier.
  const servesOriginal = (path: string): boolean => {
    const lvl = detailLevel();
    if (lvl === "base") return false;
    const meta = props.itemMeta?.get(path);
    if (!meta || meta.media_type !== "image") return false;
    if (!NATIVE_IMG_EXTS.has(extOf(path))) return false;
    // Size ceiling: the resize itself is cheap (cell-sized, off the UI thread),
    // but a very large source decodes slowly on the backend *and* isn't covered
    // by the tier look-ahead precache. Keep big files on the precached tier so
    // their reveal stays instant; route the common, smaller files through the
    // crisper exact-fit resize.
    if (meta.size > (lvl === "high" ? HIGH_MAX_BYTES : MID_MAX_BYTES)) return false;
    // Resolution gate: a source far larger than the cell is expensive to decode
    // (even on the backend) and gains nothing over the tier — file bytes alone
    // don't capture that (a small-but-50MP JPEG is cheap to transfer, ruinous to
    // decode). Only fit-resize when the source's longest edge is within
    // tolerance of the displayed size; otherwise use the (precached, off-thread)
    // tier. Unknown dimensions fall back to the byte ceiling.
    if (meta.width != null && meta.height != null) {
      const physicalLongEdge = displayedLongEdge(aspectOf(path));
      const srcLongEdge = Math.max(meta.width, meta.height);
      if (srcLongEdge > physicalLongEdge * ORIGINAL_SRC_TOLERANCE) return false;
    }
    return true;
  };

  const [startRow, setStartRow] = createSignal(0);
  const [endRow, setEndRow] = createSignal(0);
  const [generation, setGeneration] = createSignal(0);
  // True while scrolling fast enough that we defer assigning new <img> srcs;
  // flips back to false when scrolling settles, re-running the assignment.
  const [fastScroll, setFastScroll] = createSignal(false);
  const [thumbMap, setThumbMap] = createStore<Record<string, string>>({});

  let containerRef: HTMLDivElement | undefined;

  // Shared pointer controls: Ctrl/Cmd-drag range select + click handling, and
  // edge-scroll while dragging. Identical behavior in GalleryGrid.
  const { isDragging, effectiveSelected, handleDragStart, handleDragEnter, handleItemClick, handleBackgroundClick } =
    createDragSelect(props);
  createEdgeScroll(isDragging);

  // -----------------------------------------------------------------------
  // Layout
  // -----------------------------------------------------------------------

  // Aspect ratios aligned to props.paths (fallback 1:1 for un-indexed items).
  const aspectArray = createMemo(() => {
    const measured = measuredAspects();
    return props.paths.map((p) => props.aspects.get(p) ?? measured.get(p) ?? 1);
  });

  // The wrapper reserves a `gap`-wide margin on each side, so the row content
  // area is the measured width minus those two margins.
  const contentWidth = () => Math.max(0, containerWidth() - 2 * gap());

  const layout = createMemo(() =>
    computeJustifiedLayout({
      aspects: aspectArray(),
      containerWidth: contentWidth(),
      targetRowHeight: targetRowHeight(),
      gap: gap(),
      groupStarts: props.groupStarts,
    }),
  );

  const totalHeight = () => layout().totalHeight;

  // Cells within the rendered row range, with absolute positions. A memo so the
  // several consumers below (geometry store, visible-path list, src assignment)
  // share one computation per change.
  const visibleCells = createMemo(() => {
    const lay = layout();
    const sr = startRow();
    const er = Math.min(endRow(), lay.rows.length);
    const out: { path: string; index: number; x: number; y: number; width: number; height: number }[] = [];
    for (let r = sr; r < er; r++) {
      const row = lay.rows[r];
      for (const cell of row.cells) {
        const path = props.paths[cell.index];
        if (path) out.push({ path, index: cell.index, x: cell.x, y: row.y, width: cell.width, height: cell.height });
      }
    }
    return out;
  });

  // Per-path cell geometry, kept in a fine-grained store so a cell whose
  // position is unchanged across a scroll step does NOT notify (reconcile
  // diffs leaves). This is what lets us render with a *path-keyed* <For>:
  // a cell that stays on screen keeps its exact DOM node and <img src>, so the
  // webview never re-decodes it — eliminating the per-row scroll flicker that
  // an index-keyed <Index> caused by rewriting every slot's src.
  type CellGeom = { x: number; y: number; width: number; height: number };
  const [geom, setGeom] = createStore<Record<string, CellGeom>>({});
  createEffect(() => {
    const cells = visibleCells();
    const next: Record<string, CellGeom> = {};
    for (const c of cells) next[c.path] = { x: c.x, y: c.y, width: c.width, height: c.height };
    setGeom(reconcile(next));
  });

  // Visible paths in render order. Strings are compared by value, so <For>
  // preserves the DOM node for any path that remains visible across a scroll.
  const visiblePaths = createMemo(() => visibleCells().map((c) => c.path));

  // -----------------------------------------------------------------------
  // Thumbnail streaming state (lazy "j" tier generation)
  // -----------------------------------------------------------------------

  const assignedSet = new Set<string>();
  const needsGeneration = new Set<string>();
  const inFlightSet = new Set<string>();
  const failedSet = new Set<string>();
  const urlVersions = new Map<string, number>();
  let versionEpoch = 0;
  const pathToIndex = new Map<string, number>();
  // Paths already requested for high-tier (jh) look-ahead precache, so we don't
  // re-issue IPC for them while zoomed in.
  const jhPrecached = new Set<string>();
  let bgCursor = 0;
  let recalcRange: (() => void) | undefined;

  let thumbGenTotal = 0;
  let thumbGenDone = 0;

  // Clear streaming state and force a re-stream at the current tier. With
  // `keepDisplayed`, the already-shown thumbnails (and their cache-bust state)
  // are left in place so cells don't blank to a skeleton — used on a zoom
  // (detail-level) change, where the re-stream double-buffers each cell's new
  // tier in behind the old image (see the assign effect). A hard invalidation
  // (bytes changed) clears everything so stale bytes can't linger.
  const resetStreaming = (keepDisplayed = false) => {
    if (!keepDisplayed) {
      setThumbMap(reconcile({}));
      urlVersions.clear();
      versionEpoch++;
    }
    assignedSet.clear();
    needsGeneration.clear();
    inFlightSet.clear();
    failedSet.clear();
    jhPrecached.clear();
    bgCursor = 0;
    thumbGenTotal = 0;
    thumbGenDone = 0;
    setGeneration((g) => g + 1);
  };

  // Re-stream when the detail level changes so visible cells refetch at the new
  // resolution / source. `keepDisplayed` so the old tier stays visible until the
  // new one decodes. Deferred so it doesn't fire on initial mount.
  createEffect(on(detailLevel, () => resetStreaming(true), { defer: true }));

  // Physical longest edge (px) a cell displays at, quantized up to FIT_BUCKET so
  // the `?fit=` URL stays cache-stable across small layout changes. This is the
  // resize target the media server fits the source to.
  const fitEdgeFor = (path: string): number => {
    const physicalLongEdge = displayedLongEdge(aspectOf(path));
    return Math.ceil(physicalLongEdge / FIT_BUCKET) * FIT_BUCKET;
  };

  const thumbSrcFor = (path: string) => {
    // Served-original cells get a cell-sized backend resize (`?fit=`) instead of
    // the full file: the webview decodes a small image off its main thread and
    // transfers far fewer bytes. Cache-stable per (path, fit bucket), so no
    // ?v= cache-buster and no thumbnail tier.
    if (servesOriginal(path)) return mediaUrl(path, fitEdgeFor(path));
    const v = (urlVersions.get(path) ?? 0) + versionEpoch;
    const tier = thumbTier();
    return v > 0 ? `${thumbUrl(path, tier)}?v=${v}` : thumbUrl(path, tier);
  };
  const bumpVersion = (path: string) => {
    urlVersions.set(path, (urlVersions.get(path) ?? 0) + 1);
  };

  const handleThumbError = (path: string) => {
    // Cells served their original file aren't thumbnail-backed — a load error
    // here isn't a cache miss, so don't queue thumbnail generation.
    if (servesOriginal(path)) return;
    if (needsGeneration.has(path) || inFlightSet.has(path) || failedSet.has(path)) return;
    recordCacheMiss();
    needsGeneration.add(path);
    thumbGenTotal++;
    if (thumbGenTotal === 1) thumbGenStarted(thumbGenTotal);
    else thumbGenProgress(thumbGenDone, thumbGenTotal);
  };

  // Regeneration / one-shot invalidation listener.
  const handleRegenerated = (p: string) => {
    bumpVersion(p);
    if (assignedSet.has(p)) setThumbMap(p, thumbSrcFor(p));
  };
  let unlistenRegenerated: UnlistenFn | undefined;
  onMount(() => {
    listen<{ path: string }>("thumb:regenerated", (event) => {
      handleRegenerated(event.payload.path);
    }).then((fn) => { unlistenRegenerated = fn; });
    // Web stand-in for the Tauri event (dispatched by ContextMenu after the
    // regenerate invoke resolves; `listen` above is a no-op off-desktop).
    const domRegen = (e: Event) => {
      const p = (e as CustomEvent<{ path: string }>).detail?.path;
      if (p) handleRegenerated(p);
    };
    window.addEventListener(THUMB_REGENERATED_EVENT, domRegen);
    onCleanup(() => window.removeEventListener(THUMB_REGENERATED_EVENT, domRegen));
  });
  onCleanup(() => unlistenRegenerated?.());

  // Point a cell at `url`. When the cell has no image yet (first paint, a
  // freshly scrolled-in row) the URL is assigned directly — a skeleton while it
  // loads is correct. When the cell *already shows* an image (a zoom swap to a
  // new tier), the new URL is decoded off-DOM first and only swapped in on
  // success, so the previous tier stays on screen with no skeleton flash; a cold
  // new-tier URL (404) leaves the old image up and queues generation, which the
  // fetch loop swaps in once warm. `markUrlLoaded` before the swap suppresses
  // the cell's fade-in, since the bytes are already decoded.
  const assignSrc = (path: string, url: string) => {
    const current = thumbMap[path];
    if (!current || current === url) {
      setThumbMap(path, url);
      return;
    }
    const gen = generation();
    const img = new Image();
    img.onload = () => {
      // Drop the swap if the cell was evicted or the path list changed meanwhile.
      if (generation() !== gen || !assignedSet.has(path)) return;
      markUrlLoaded(url);
      setThumbMap(path, url);
    };
    img.onerror = () => {
      if (generation() !== gen || !assignedSet.has(path)) return;
      handleThumbError(path);
    };
    img.src = url;
  };

  // Assign protocol URLs as soon as cells become visible. Cached thumbs load
  // instantly; uncached ones 404 → onError → queued for generation.
  createEffect(on([visibleCells, fastScroll], ([cells, fast]) => {
    // While flinging, leave new cells on their placeholder so the main thread
    // isn't buried under image decodes mid-scroll. When scrolling settles
    // (fast → false) this re-runs and assigns whatever's now on screen.
    if (fast) return;
    for (const cell of cells as ReturnType<typeof visibleCells>) {
      if (!assignedSet.has(cell.path)) {
        assignedSet.add(cell.path);
        assignSrc(cell.path, thumbSrcFor(cell.path));
      }
    }
  }));

  // -----------------------------------------------------------------------
  // Scroll + resize + wheel
  // -----------------------------------------------------------------------

  onMount(() => {
    if (containerRef) setContainerWidth(containerRef.clientWidth);

    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) setContainerWidth(entry.contentRect.width);
    });
    if (containerRef) ro.observe(containerRef);

    let lastScrollY = window.scrollY;
    let lastTimestamp = performance.now();
    let currentVelocity = 0;
    let scrollDirection: 1 | -1 = 1;

    recalcRange = () => {
      const lay = layout();
      if (lay.rows.length === 0) {
        if (untrack(startRow) !== 0 || untrack(endRow) !== 0) batch(() => { setStartRow(0); setEndRow(0); });
        return;
      }
      const sy = window.scrollY;
      const vh = window.innerHeight;
      const offset = containerRef?.offsetTop ?? 0;
      const relativeTop = Math.max(0, sy - offset);
      const relativeBottom = relativeTop + vh;

      const firstRow = rowIndexAtOffset(lay.rowTops, relativeTop);
      let lastRow = rowIndexAtOffset(lay.rowTops, relativeBottom);
      // rowIndexAtOffset returns the row whose top is <= offset; include it.
      lastRow = Math.min(lay.rows.length, lastRow + 1);

      const bufTop = scrollDirection === 1 ? BUFFER_BEHIND : BUFFER_AHEAD;
      const bufBottom = scrollDirection === 1 ? BUFFER_AHEAD : BUFFER_BEHIND;
      const newStart = Math.max(0, firstRow - bufTop);
      const newEnd = Math.min(lay.rows.length, lastRow + bufBottom);

      if (newStart !== untrack(startRow) || newEnd !== untrack(endRow)) {
        batch(() => { setStartRow(newStart); setEndRow(newEnd); });
      }
    };

    let rafId = 0;
    let settleTimer: ReturnType<typeof setTimeout> | undefined;
    const markSettled = () => {
      if (untrack(fastScroll)) setFastScroll(false);
    };
    const onScroll = () => {
      if (rafId) return;
      rafId = requestAnimationFrame((now) => {
        const y = window.scrollY;
        const dt = (now - lastTimestamp) / 1000;
        const dy = Math.abs(y - lastScrollY);
        currentVelocity = dt > 0 ? dy / dt : 0;
        scrollDirection = y >= lastScrollY ? 1 : -1;
        lastScrollY = y;
        lastTimestamp = now;
        // Defer src assignment while flinging; resume shortly after it settles.
        if (currentVelocity > FAST_SCROLL_VELOCITY) {
          if (!untrack(fastScroll)) setFastScroll(true);
          if (settleTimer) clearTimeout(settleTimer);
          settleTimer = setTimeout(markSettled, SCROLL_SETTLE_MS);
        }
        recalcRange?.();
        rafId = 0;
      });
    };
    window.addEventListener("scroll", onScroll, { passive: true });

    // WebKitGTK doesn't propagate wheel events to window scroll natively, so we
    // drive the scroll ourselves with momentum smoothing (mirrors GalleryGrid).
    let wheelTargetY = 0;
    let wheelCurrentY = 0;
    let wheelAnimating = false;
    let wheelRafId = 0;
    const WHEEL_DECAY = 0.8;
    const WHEEL_SETTLE = 0.5;

    const wheelDeltaPx = (e: WheelEvent) => {
      if (e.deltaMode === 1) return e.deltaY * (targetRowHeight() + gap());
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
        scheduleFetch();
        return;
      }
      wheelCurrentY += diff * (1 - WHEEL_DECAY);
      window.scrollTo(0, Math.round(wheelCurrentY));
      wheelRafId = requestAnimationFrame(drainWheel);
    };

    const onWheel = (e: WheelEvent) => {
      // Ctrl+wheel zooms (changes the target row height) instead of scrolling.
      // During a Ctrl+drag selection, fall through to scroll (mirrors GalleryGrid).
      if ((e.ctrlKey || e.metaKey) && !isDragging()) {
        e.preventDefault();
        const cur = settings().display.thumbnail_size;
        const step = Math.max(8, Math.round(cur * 0.12));
        const lo = settings().display.thumb_size_min ?? ROW_HEIGHT_MIN;
        const hi = settings().display.thumb_size_max ?? ROW_HEIGHT_MAX;
        const next = Math.max(lo, Math.min(hi, cur + (e.deltaY < 0 ? step : -step)));
        if (next !== cur) {
          setSettings((prev) => ({ ...prev, display: { ...prev.display, thumbnail_size: next } }));
        }
        return;
      }
      e.preventDefault();
      if (!wheelAnimating) {
        wheelCurrentY = window.scrollY;
        wheelTargetY = window.scrollY;
        wheelAnimating = true;
      }
      const maxScroll = Math.max(0, document.documentElement.scrollHeight - window.innerHeight);
      wheelTargetY = Math.max(0, Math.min(maxScroll, wheelTargetY + wheelDeltaPx(e)));
      if (!wheelRafId) wheelRafId = requestAnimationFrame(drainWheel);
    };
    window.addEventListener("wheel", onWheel, { passive: false });

    recalcRange();

    // -------------------------------------------------------------------
    // Fetch loop — lazy "j" tier generation + eviction
    // -------------------------------------------------------------------
    let fetchAbort = false;
    let inFlightFetch: Promise<void> | null = null;

    const evictFaraway = () => {
      const lay = layout();
      if (lay.rows.length === 0) return;
      const keepStartRow = Math.max(0, startRow() - EVICT_ROWS);
      const keepEndRow = Math.min(lay.rows.length, endRow() + EVICT_ROWS);
      const keepStart = lay.rows[keepStartRow]?.cells[0]?.index ?? 0;
      const lastKeepRow = lay.rows[keepEndRow - 1];
      const keepEnd = lastKeepRow ? lastKeepRow.cells[lastKeepRow.cells.length - 1].index + 1 : props.paths.length;

      const toEvict: string[] = [];
      for (const p of assignedSet) {
        const idx = pathToIndex.get(p);
        if (idx !== undefined && (idx < keepStart || idx >= keepEnd)) toEvict.push(p);
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

      evictFaraway();

      const gen = generation();

      if (needsGeneration.size > 0) {
        const toGenerate: string[] = [];
        for (const p of needsGeneration) {
          toGenerate.push(p);
          if (toGenerate.length >= BATCH_SIZE) break;
        }
        if (toGenerate.length > 0) {
          for (const p of toGenerate) {
            needsGeneration.delete(p);
            inFlightSet.add(p);
          }
          inFlightFetch = (async () => {
            try {
              await ensureTierThumbnails(toGenerate, thumbTier());
              if (fetchAbort || generation() !== gen) return;
              batch(() => {
                for (const p of toGenerate) {
                  bumpVersion(p);
                  if (assignedSet.has(p)) setThumbMap(p, thumbSrcFor(p));
                }
              });
              thumbGenDone += toGenerate.length;
              for (const p of toGenerate) inFlightSet.delete(p);
              if (needsGeneration.size === 0 && inFlightSet.size === 0) {
                thumbGenFinished(thumbGenDone);
                thumbGenTotal = 0;
                thumbGenDone = 0;
              } else {
                thumbGenProgress(thumbGenDone, thumbGenTotal);
              }
            } catch (e) {
              console.error("Justified tier generation failed:", e);
              for (const p of toGenerate) { inFlightSet.delete(p); failedSet.add(p); }
            }
            inFlightFetch = null;
          })();
          return;
        }
      }

      // Look-ahead precache of the high (jh) tier in the scroll direction when
      // zoomed in. Without it, every newly-revealed cell at mid/high detail is a
      // cold 404 → generate → re-request round-trip (the "loads too slowly"
      // symptom). Warming a bounded window ahead of the viewport means cells are
      // usually ready by the time they scroll in. Disk stays bounded because the
      // backend FIFO-evicts the jh tier. Skipped while flinging.
      if (detailLevel() !== "base" && currentVelocity < 1500) {
        const lay = layout();
        const from = scrollDirection === 1 ? endRow() : Math.max(0, startRow() - JH_PRECACHE_ROWS);
        const to = scrollDirection === 1
          ? Math.min(lay.rows.length, endRow() + JH_PRECACHE_ROWS)
          : startRow();
        const want: string[] = [];
        for (let r = from; r < to && want.length < BATCH_SIZE; r++) {
          const row = lay.rows[r];
          if (!row) continue;
          for (const cell of row.cells) {
            const p = props.paths[cell.index];
            if (!p || jhPrecached.has(p) || failedSet.has(p)) continue;
            // Originals aren't tier-backed — only jh-served cells need warming.
            if (servesOriginal(p)) continue;
            jhPrecached.add(p);
            want.push(p);
          }
        }
        if (want.length > 0) {
          const tier = thumbTier();
          inFlightFetch = (async () => {
            try { await ensureTierThumbnails(want, tier); }
            catch (e) { console.error("Justified jh look-ahead failed:", e); }
            inFlightFetch = null;
          })();
          return;
        }
      }

      // Background precache of upcoming items when idle. Only the base "j"
      // tier is precached across the whole gallery — the high "jh" tier is
      // warmed via the look-ahead above (visible + a window ahead) so its disk
      // cost stays bounded to what's actually viewed zoomed in.
      if (detailLevel() === "base" && currentVelocity < 500 && bgCursor < props.paths.length) {
        const bgNeeded: string[] = [];
        while (bgNeeded.length < BATCH_SIZE && bgCursor < props.paths.length) {
          const p = props.paths[bgCursor];
          bgCursor++;
          if (p && !assignedSet.has(p) && !failedSet.has(p)) bgNeeded.push(p);
        }
        if (bgNeeded.length > 0) {
          inFlightFetch = (async () => {
            try { await ensureTierThumbnails(bgNeeded, "j"); }
            catch (e) { console.error("Justified background precache failed:", e); }
            inFlightFetch = null;
          })();
        }
      }
    };

    const onScrollEnd = () => {
      markSettled();
      scheduleFetch();
    };
    window.addEventListener("scrollend", onScrollEnd);

    const bgIntervalId = setInterval(() => { if (!inFlightFetch) scheduleFetch(); }, 500);

    createEffect(on(generation, () => { scheduleFetch(); }));

    // Re-run the visible range whenever the layout changes (width / zoom).
    createEffect(on(layout, () => { recalcRange?.(); }));

    const onInvalidate = () => resetStreaming();
    window.addEventListener("lightview:thumbnails-invalidated", onInvalidate);

    // Keep the viewed image visible (arrow-key nav from the viewer).
    const onScrollToIndex = (e: Event) => {
      const index = (e as CustomEvent).detail as number;
      if (index < 0 || index >= props.paths.length) return;
      const lay = layout();
      const row = lay.rows.find((r) => r.cells.some((c) => c.index === index));
      if (!row) return;
      const offset = containerRef?.offsetTop ?? 0;
      const imageTop = offset + row.y;
      const imageBottom = imageTop + row.height;
      const viewTop = window.scrollY;
      const viewBottom = viewTop + window.innerHeight;
      if (imageTop < viewTop || imageBottom > viewBottom) {
        window.scrollTo(0, imageTop - (window.innerHeight - row.height) / 2);
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
      if (settleTimer) clearTimeout(settleTimer);
      clearInterval(bgIntervalId);
      fetchAbort = true;
    });
  });

  // Reset streaming state when the path list changes.
  createEffect(on(() => props.paths, (paths) => {
    setThumbMap(reconcile({}));
    assignedSet.clear();
    needsGeneration.clear();
    inFlightSet.clear();
    failedSet.clear();
    urlVersions.clear();
    jhPrecached.clear();
    setMeasuredAspects(new Map());
    bgCursor = 0;
    thumbGenTotal = 0;
    thumbGenDone = 0;
    pathToIndex.clear();
    for (let i = 0; i < paths.length; i++) pathToIndex.set(paths[i], i);
    setGeneration((g) => g + 1);
    recalcRange?.();
  }));

  createEffect(on(totalHeight, (h) => { props.onContentHeight?.(h); }));

  return (
    <div ref={containerRef} class="w-full" style={{ "user-select": isDragging() ? "none" : undefined }} onClick={handleBackgroundClick}>
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
        <div
          style={{
            position: "relative",
            height: `${totalHeight()}px`,
            "margin-left": `${gap()}px`,
            "margin-right": `${gap()}px`,
            contain: "strict",
          }}
        >
          <For each={visiblePaths()}>
            {(path) => {
              const g = () => geom[path];
              const index = () => pathToIndex.get(path) ?? -1;
              return (
                <Show when={g()}>
                  <div
                    style={{
                      position: "absolute",
                      top: `${g()!.y}px`,
                      left: `${g()!.x}px`,
                      width: `${g()!.width}px`,
                      height: `${g()!.height}px`,
                    }}
                  >
                    <ThumbnailCell
                      path={path}
                      thumbSrc={thumbMap[path] ?? null}
                      tier={thumbTier()}
                      freeSize={true}
                      durationSec={durationByPath().get(path) ?? null}
                      selected={effectiveSelected().has(path)}
                      onClick={(e: MouseEvent) => handleItemClick({ path, index: index() }, e)}
                      onMouseDown={(e: MouseEvent) => handleDragStart(index(), e)}
                      onMouseEnter={() => handleDragEnter(index())}
                      onContextMenu={(e) => props.onItemContextMenu?.(e, path, index())}
                      onError={handleThumbError}
                      onImageLoad={(w, h) => recordMeasuredAspect(path, w, h)}
                    />
                  </div>
                </Show>
              );
            }}
          </For>
        </div>
      </Show>
    </div>
  );
}
