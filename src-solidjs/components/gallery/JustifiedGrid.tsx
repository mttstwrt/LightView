import { For, Show, createSignal, createEffect, createMemo, on, onMount, onCleanup, batch, untrack } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { safeListen as listen, isMobile, isTauri, type UnlistenFn } from "../../lib/runtime";
import { settings, setSettings } from "../../stores/settingsStore";
import { durationByPath } from "../../stores/galleryStore";
import { ensureTierThumbnails, thumbUrl, mediaUrl, THUMB_REGENERATED_EVENT, type ThumbTier } from "../../lib/ipc";
import type { MediaMeta } from "../../stores/galleryStore";
import { thumbGenStarted, thumbGenProgress, thumbGenFinished } from "../../stores/thumbnailProgressStore";
import { recordCacheMiss } from "../../lib/perfMonitor";
import { computeJustifiedLayout, rowIndexAtOffset, portraitRowBoost } from "../../lib/justifiedLayout";
import { createDragSelect, createEdgeScroll } from "../../lib/galleryControls";
import { createScrollDynamics, constrainedNetwork, CHEAP_RUNG_DURING_GATE } from "../../lib/scrollDynamics";
import { createThumbSwapper } from "../../lib/thumbSwap";
import { pickByPriority } from "../../lib/loadPriority";
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
  /** Tap-to-toggle multi-select mode (mobile Select button). */
  selectionMode?: boolean;
  onItemContextMenu?: (e: MouseEvent, path: string, index: number) => void;
  loading: boolean;
  onContentHeight?: (height: number) => void;
}

// Two-zone render buffer beyond the viewport, ahead of / behind scroll. The
// outer window renders cells at the cheap base "j" tier (at mid/high detail);
// the inner FULL_* window carries the level's real source (jm/jh/fit),
// upgrading as rows cross into it. At base detail the zones coincide in
// practice ("j" is already the target).
const BUFFER_AHEAD = 12;
const BUFFER_BEHIND = 4;
const FULL_AHEAD = 4;
const FULL_BEHIND = 2;
// Ceiling for the velocity×latency-adaptive ahead buffer: BUFFER_AHEAD grows
// by the rows a scroll covers in one measured image-load round-trip
// (dynamics.bufferAheadRows), capped here to bound DOM growth. Eviction
// margins are relative to the rendered range, so they track the growth.
const BUFFER_AHEAD_MAX = 20;
// Rows beyond the visible+buffer range before thumbnails are evicted.
const EVICT_ROWS = 12;
// Rows ahead of the viewport (in scroll direction) to warm the high (jh) tier
// when zoomed in, so cells aren't cold 404→generate round-trips on reveal.
const JH_PRECACHE_ROWS = 6;
// How many paths to generate per IPC batch.
const BATCH_SIZE = 96;
// Scroll velocity (px/s) above which the fetch loop treats the scroll as a
// fling: near-viewport generation is skipped (cells fly past unseen) and the
// projected landing zone is warmed instead. Matches GalleryGrid.
const VELOCITY_FAST = 3000;
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
  // Inner full-resolution row window within [startRow, endRow).
  const [fullStartRow, setFullStartRow] = createSignal(0);
  const [fullEndRow, setFullEndRow] = createSignal(0);
  const [generation, setGeneration] = createSignal(0);
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
    const out: { path: string; index: number; row: number; x: number; y: number; width: number; height: number }[] = [];
    for (let r = sr; r < er; r++) {
      const row = lay.rows[r];
      for (const cell of row.cells) {
        const path = props.paths[cell.index];
        if (path) out.push({ path, index: cell.index, row: r, x: cell.x, y: row.y, width: cell.width, height: cell.height });
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

  // Paths with a URL in thumbMap → which rung that URL serves. "cheap" is the
  // base "j" tier assigned during flings at mid/high detail; "full" is
  // whatever thumbSrcFor picks for the current detail level (j / jm / jh /
  // fit-original). Cheap cells upgrade once scrolling settles.
  type Rung = "cheap" | "full";
  const assignedRung = new Map<string, Rung>();
  const needsGeneration = new Set<string>();
  const inFlightSet = new Set<string>();
  const failedSet = new Set<string>();
  const urlVersions = new Map<string, number>();
  let versionEpoch = 0;
  const pathToIndex = new Map<string, number>();
  // Paths already requested for high-tier (jh) look-ahead precache, so we don't
  // re-issue IPC for them while zoomed in. The backend evicts from these tiers
  // to stay inside a disk budget, so this memo can go stale — every warm call
  // reports what it dropped and `forgetEvicted` takes those back out. Without
  // that, an evicted cell is never re-warmed: the look-ahead still believes it's
  // cached, so the cell falls through to the slow per-cell generate-on-serve.
  const jhPrecached = new Set<string>();
  const forgetEvicted = (evicted: string[] | undefined) => {
    if (!evicted?.length) return;
    for (const p of evicted) jhPrecached.delete(p);
  };
  // Paths already sent to the backend by landing-zone warming during a fling,
  // so successive drains of the same fling don't re-issue them.
  const landingWarmed = new Set<string>();
  let bgCursor = 0;
  let recalcRange: (() => void) | undefined;

  let thumbGenTotal = 0;
  let thumbGenDone = 0;

  // Scroll velocity/direction tracking + the WebKitGTK decode gate (never
  // engages on the web client). Owns the window scroll listener; each frame
  // re-derives the visible row range. Cells-per-row and per-cell decode cost
  // are estimates (avg ~square aspect; tier target size²) — the gate only
  // needs the right order of magnitude.
  const dynamics = createScrollDynamics({
    rowHeight: () => targetRowHeight() + gap(),
    cellsPerRow: () => {
      const rh = targetRowHeight();
      return rh > 0 ? Math.max(1, contentWidth() / rh) : 1;
    },
    cellCostPx: () => {
      const t = thumbTier();
      const px = t === "jh" ? 2560 : t === "jm" ? 1280 : 512;
      return px * px;
    },
    onFrame: () => recalcRange?.(),
  });
  onCleanup(() => dynamics.dispose());

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
    assignedRung.clear();
    swapper.cancelAll();
    needsGeneration.clear();
    inFlightSet.clear();
    failedSet.clear();
    jhPrecached.clear();
    landingWarmed.clear();
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

  const versionedThumbUrl = (path: string, tier: ThumbTier) => {
    const v = (urlVersions.get(path) ?? 0) + versionEpoch;
    return v > 0 ? `${thumbUrl(path, tier)}?v=${v}` : thumbUrl(path, tier);
  };

  const thumbSrcFor = (path: string, cheap = false) => {
    // The cheap rung is always the base "j" tier — small, aspect-preserving,
    // and precached across the gallery, so fling-revealed cells show
    // something real immediately and upgrade on settle.
    if (cheap) return versionedThumbUrl(path, "j");
    // Served-original cells get a cell-sized backend resize (`?fit=`) instead of
    // the full file: the webview decodes a small image off its main thread and
    // transfers far fewer bytes. Cache-stable per (path, fit bucket), so no
    // ?v= cache-buster and no thumbnail tier.
    if (servesOriginal(path)) return mediaUrl(path, fitEdgeFor(path));
    return versionedThumbUrl(path, thumbTier());
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
    if (assignedRung.has(p)) {
      assignedRung.set(p, "full");
      setThumbMap(p, thumbSrcFor(p));
    }
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

  // Off-DOM decode-then-swap (shared helper) for cells that already show an
  // image — zoom tier changes and cheap-rung upgrades. Swaps are dropped if
  // the cell was evicted or the path list changed meanwhile, and cancelled
  // per-path on eviction / wholesale on resets. `markUrlLoaded` before the
  // swap suppresses the cell's fade-in, since the bytes are already decoded.
  const swapper = createThumbSwapper({
    generation,
    isCurrent: (path, gen) => generation() === gen && assignedRung.has(path),
    apply: (path, url) => {
      markUrlLoaded(url);
      setThumbMap(path, url);
    },
    onMiss: handleThumbError,
  });
  onCleanup(() => swapper.cancelAll());

  // Point a cell at `url`. When the cell has no image yet (first paint, a
  // freshly scrolled-in row) the URL is assigned directly — a skeleton while
  // it loads is correct. When the cell *already shows* an image, decode-then-
  // swap so the previous image stays on screen with no skeleton flash; a cold
  // new URL (404) leaves the old image up and queues generation, which the
  // fetch loop swaps in once warm.
  const assignSrc = (path: string, url: string) => {
    const current = thumbMap[path];
    if (!current || current === url) {
      setThumbMap(path, url);
      return;
    }
    swapper.swap(path, url);
  };

  // Assign protocol URLs as soon as cells become visible. Cached thumbs load
  // instantly; uncached ones 404 → onError → queued for generation.
  createEffect(on(
    [visibleCells, dynamics.decodeGate, dynamics.settled, fullStartRow, fullEndRow],
    ([cells, gated, settled, fullStart, fullEnd]) => {
    // While the WebKitGTK decode gate is up, leave new cells on their
    // placeholder so the main thread isn't buried under image decodes
    // mid-scroll. When it releases this re-runs and assigns whatever's now on
    // screen.
    if (gated && !CHEAP_RUNG_DURING_GATE) return;
    // Resolution ladder: at mid/high detail a cell gets the base "j" tier
    // (small, precached) when it's outside the inner full-res window or while
    // a fling is in progress, and upgrades to the level's real source
    // (jm/jh/fit-original) once it sits in the inner window with scrolling
    // settled — usually off-screen. At base detail "j" *is* the target, so
    // there's nothing to degrade. On the desktop webview the hard gate covers
    // flings (cheap rung behind the experiment flag). On a constrained
    // network (Save-Data / 2g) cells are held at the cheap rung outright —
    // "j" everywhere beats spending the data budget on jm/jh/fit upgrades.
    const cheapScroll = isTauri() ? gated : !settled || constrainedNetwork();
    const degradable = detailLevel() !== "base";
    for (const cell of cells as ReturnType<typeof visibleCells>) {
      const inFull = cell.row >= fullStart && cell.row < fullEnd;
      const cur = assignedRung.get(cell.path);
      if (cur === undefined) {
        const rung: Rung = degradable && (cheapScroll || !inFull) ? "cheap" : "full";
        assignedRung.set(cell.path, rung);
        assignSrc(cell.path, thumbSrcFor(cell.path, rung === "cheap"));
      } else if (cur === "cheap" && inFull && !cheapScroll) {
        assignedRung.set(cell.path, "full");
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

      const ahead = dynamics.bufferAheadRows(BUFFER_AHEAD, BUFFER_AHEAD_MAX);
      const bufTop = dynamics.direction() === 1 ? BUFFER_BEHIND : ahead;
      const bufBottom = dynamics.direction() === 1 ? ahead : BUFFER_BEHIND;
      const fullTop = dynamics.direction() === 1 ? FULL_BEHIND : FULL_AHEAD;
      const fullBottom = dynamics.direction() === 1 ? FULL_AHEAD : FULL_BEHIND;
      const newStart = Math.max(0, firstRow - bufTop);
      const newEnd = Math.min(lay.rows.length, lastRow + bufBottom);
      const newFullStart = Math.max(0, firstRow - fullTop);
      const newFullEnd = Math.min(lay.rows.length, lastRow + fullBottom);

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
      for (const p of assignedRung.keys()) {
        const idx = pathToIndex.get(p);
        if (idx !== undefined && (idx < keepStart || idx >= keepEnd)) toEvict.push(p);
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

    // A fling has a predictable destination — warm the base "j" tier around
    // the projected landing scroll position (± one viewport) so cells are
    // cached by the time the scroll arrives ("j" is both the cheap fling rung
    // and the base target, so it's the right thing to warm at every detail
    // level). Backend warm via ensureTierThumbnails; on the web client
    // additionally prime the browser's HTTP cache with the same URLs
    // (responses are cacheable for 1h). Reuses the single-flight
    // inFlightFetch slot — one batch per drain, recomputed from the live
    // projection each time, so a redirected fling self-corrects and wasted
    // warms stay bounded.
    const warmLandingZone = () => {
      const lay = layout();
      if (lay.rows.length === 0) return;
      const offset = containerRef?.offsetTop ?? 0;
      const vh = window.innerHeight;
      const landingTop = Math.max(0, dynamics.projectedLandingY() - offset);
      const firstRow = rowIndexAtOffset(lay.rowTops, Math.max(0, landingTop - vh));
      const lastRow = Math.min(
        lay.rows.length,
        rowIndexAtOffset(lay.rowTops, landingTop + 2 * vh) + 1,
      );

      const want: string[] = [];
      for (let r = firstRow; r < lastRow && want.length < BATCH_SIZE; r++) {
        const row = lay.rows[r];
        if (!row) continue;
        for (const cell of row.cells) {
          const p = props.paths[cell.index];
          if (!p || assignedRung.has(p) || inFlightSet.has(p) || failedSet.has(p) || landingWarmed.has(p)) continue;
          landingWarmed.add(p);
          want.push(p);
        }
      }
      if (want.length === 0) return;

      inFlightFetch = (async () => {
        try {
          await ensureTierThumbnails(want, "j");
          // Browser cache warm — low-priority so it can't compete with the
          // visible cells' loads. `priority` is a progressive enhancement
          // (ignored where unsupported; absent from TS 5.4's RequestInit).
          if (!isTauri()) {
            for (const p of want) {
              fetch(versionedThumbUrl(p, "j"), { priority: "low" } as RequestInit).catch(() => {});
            }
          }
        } catch (e) {
          console.error("Landing-zone warm failed:", e);
        }
        inFlightFetch = null;
      })();
    };

    const scheduleFetch = () => {
      if (inFlightFetch) return;

      evictFaraway();

      // During a fling, near-viewport generation is wasted work — those cells
      // fly past unseen. Warm the projected landing zone instead; queued
      // 404-generation drains on settle (with stale entries dropped).
      if (dynamics.velocity() > VELOCITY_FAST) {
        warmLandingZone();
        return;
      }

      const gen = generation();

      if (needsGeneration.size > 0) {
        // Drain-time prioritization (mirrors GalleryGrid): full-res window
        // first, then the rendered buffer by distance; leftovers outside the
        // rendered window are dropped and re-queue via 404 if scrolled back.
        // Row ranges map to item-index ranges through the layout (cell
        // indices are contiguous across justified rows).
        const lay = layout();
        const idxAt = (row: number) => lay.rows[row]?.cells[0]?.index ?? props.paths.length;
        const idxAfter = (rowExcl: number) => {
          const last = lay.rows[rowExcl - 1];
          return last ? last.cells[last.cells.length - 1].index + 1 : 0;
        };
        const { picked: toGenerate, stale } = pickByPriority(
          needsGeneration,
          (p) => pathToIndex.get(p),
          {
            viewStart: idxAt(fullStartRow()),
            viewEnd: idxAfter(fullEndRow()),
            renderStart: idxAt(startRow()),
            renderEnd: idxAfter(endRow()),
          },
          BATCH_SIZE,
        );
        if (stale.length > 0) {
          for (const p of stale) needsGeneration.delete(p);
          thumbGenTotal = Math.max(thumbGenDone, thumbGenTotal - stale.length);
          // Dropping may have emptied the queue — close out the progress UI.
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
          inFlightFetch = (async () => {
            try {
              forgetEvicted((await ensureTierThumbnails(toGenerate, thumbTier())).evicted);
              if (fetchAbort || generation() !== gen) return;
              batch(() => {
                for (const p of toGenerate) {
                  bumpVersion(p);
                  if (assignedRung.has(p)) {
                    assignedRung.set(p, "full");
                    setThumbMap(p, thumbSrcFor(p));
                  }
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
      if (detailLevel() !== "base" && dynamics.velocity() < 1500) {
        const lay = layout();
        // Warm ahead of the *full-res* boundary — that's where upgrades
        // request the jh tier, so the look-ahead must cover the rows about to
        // cross it (the outer cheap-tier window only needs "j").
        const from = dynamics.direction() === 1 ? fullEndRow() : Math.max(0, fullStartRow() - JH_PRECACHE_ROWS);
        const to = dynamics.direction() === 1
          ? Math.min(lay.rows.length, fullEndRow() + JH_PRECACHE_ROWS)
          : fullStartRow();
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
            try { forgetEvicted((await ensureTierThumbnails(want, tier)).evicted); }
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
      if (detailLevel() === "base" && dynamics.velocity() < 500 && bgCursor < props.paths.length) {
        const bgNeeded: string[] = [];
        while (bgNeeded.length < BATCH_SIZE && bgCursor < props.paths.length) {
          const p = props.paths[bgCursor];
          bgCursor++;
          if (p && !assignedRung.has(p) && !failedSet.has(p)) bgNeeded.push(p);
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
      dynamics.markSettled();
      scheduleFetch();
    };
    window.addEventListener("scrollend", onScrollEnd);

    const bgIntervalId = setInterval(() => { if (!inFlightFetch) scheduleFetch(); }, 500);

    createEffect(on(generation, () => { scheduleFetch(); }));

    // Drain the landing viewport whenever scrolling settles, however that was
    // detected. `scrollend` (→ onScrollEnd) already does this, but it's not
    // universal on mobile; scrollDynamics also flips `settled` true via its own
    // fling debounce, and that path had no fetch trigger — so a fast flick on a
    // browser without `scrollend` left cells blank until the 500ms poll.
    createEffect(on(dynamics.settled, (s) => { if (s) scheduleFetch(); }, { defer: true }));

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
      window.removeEventListener("scrollend", onScrollEnd);
      window.removeEventListener("wheel", onWheel);
      window.removeEventListener("lightview:thumbnails-invalidated", onInvalidate);
      window.removeEventListener("lightview:scroll-to-index", onScrollToIndex);
      if (wheelRafId) cancelAnimationFrame(wheelRafId);
      clearInterval(bgIntervalId);
      fetchAbort = true;
    });
  });

  // Prune per-path streaming state when the path list changes. Surgical, not
  // a wholesale wipe: the assign effect above has already run for this update
  // (skipping cells that still had a rung) and won't fire again until the
  // visible range moves — wiping surviving cells here would blank the grid
  // until the next scroll (e.g. after deleting one image). Cells whose path
  // survives keep their thumbMap URL and decoded <img>.
  createEffect(on(() => props.paths, (paths) => {
    pathToIndex.clear();
    for (let i = 0; i < paths.length; i++) pathToIndex.set(paths[i], i);

    const gone = new Set<string>();
    for (const p of Object.keys(thumbMap)) if (!pathToIndex.has(p)) gone.add(p);
    for (const p of assignedRung.keys()) if (!pathToIndex.has(p)) gone.add(p);
    if (gone.size > 0) {
      batch(() => {
        for (const p of gone) setThumbMap(p, undefined as any);
      });
      for (const p of gone) {
        assignedRung.delete(p);
        swapper.cancel(p);
        urlVersions.delete(p);
      }
    }
    const pruneAbsent = (s: Set<string>) => {
      for (const p of s) if (!pathToIndex.has(p)) s.delete(p);
    };
    pruneAbsent(inFlightSet);
    pruneAbsent(failedSet);
    pruneAbsent(jhPrecached);
    pruneAbsent(landingWarmed);
    let droppedQueued = 0;
    for (const p of needsGeneration) {
      if (!pathToIndex.has(p)) {
        needsGeneration.delete(p);
        droppedQueued++;
      }
    }
    if (droppedQueued > 0) {
      thumbGenTotal = Math.max(thumbGenDone, thumbGenTotal - droppedQueued);
      if (needsGeneration.size === 0 && inFlightSet.size === 0) {
        thumbGenFinished(thumbGenDone);
        thumbGenTotal = 0;
        thumbGenDone = 0;
      }
    }
    if (measuredAspects().size > 0) {
      let changed = false;
      const next = new Map<string, number>();
      for (const [p, a] of measuredAspects()) {
        if (pathToIndex.has(p)) next.set(p, a);
        else changed = true;
      }
      if (changed) setMeasuredAspects(next);
    }
    bgCursor = 0;
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
