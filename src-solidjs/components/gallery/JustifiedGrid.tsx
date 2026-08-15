// The justified grid: aspect-preserving rows, zoomable.
//
// Shares the loading machine with GalleryGrid (see
// docs/frontend/grid-loading.md); what differs is policy.
//
// Tier selection follows a hysteretic zoom level rather than a pixel size, so
// small layout changes do not thrash between tiers. At mid and high detail it
// can bypass the thumbnail tiers entirely: a native-format still that is
// neither too large in bytes nor far bigger than the cell is served as a
// backend resize of the original (`GET /media?fit=`), which is sharper and
// costs no cache storage. The requested edge is quantized to 256px buckets so
// the URL stays cache-stable.
//
// Served-original cells are deliberately never warmed ahead of the viewport.
// Each is an on-demand source decode inside the request, landing on the same
// bounded pool as the visible cells; measured, warming them made things worse,
// because a look-ahead only pays off if it wins the race and it cannot when
// each decode takes seconds.

import { For, Show, createSignal, createEffect, createMemo, on, onMount, onCleanup, batch, untrack } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { isMobile, isTauri, renderScale } from "../../lib/runtime";
import { createWheelScroll } from "../../lib/wheelScroll";
import { settings, setSettings } from "../../stores/settingsStore";
import { durationByPath } from "../../stores/galleryStore";
import { ensureTierThumbnails, thumbUrl, mediaUrl, type ThumbTier } from "../../lib/ipc";
import { onThumbRegenerated } from "../../lib/thumbRegeneration";
import type { MediaMeta } from "../../stores/galleryStore";
import { createThumbProgress } from "../../lib/thumbProgress";
import { recordCacheMiss } from "../../lib/perfMonitor";
import { computeJustifiedLayout, rowIndexAtOffset, portraitRowBoost } from "../../lib/justifiedLayout";
import { createDragSelect, createEdgeScroll } from "../../lib/galleryControls";
import { createScrollDynamics, constrainedNetwork, CHEAP_RUNG_DURING_GATE } from "../../lib/scrollDynamics";
import { onScrollHost, scrollToY, scrollTop, viewportHeight } from "../../lib/scrollHost";
import { VIEWER_PATH_EVENT } from "../../lib/viewerTransition";
import { pickByPriority } from "../../lib/loadPriority";
import { createUrlVersions } from "../../lib/urlVersions";
import { createPathIndex } from "../../lib/pathIndex";
import { createThumbQueue } from "../../lib/thumbQueue";
import { createFetchLoop } from "../../lib/fetchLoop";
import { createCellSources } from "../../lib/cellSources";
import { ThumbnailCell } from "./ThumbnailCell";

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
// How many paths to generate per IPC batch. `ensure_tier_thumbnails` decodes
// the whole batch before it resolves *and* holds the shared rayon thumb pool
// while it does, so a batch is also the granularity at which the frontend can
// re-prioritize and at which on-demand serves get the CPU back. 96 is fine for
// the 512px "j" tier; at 1280/2560px it's tens of seconds of pool time for work
// that is mostly speculative — hence the much smaller high-tier cap.
const BATCH_SIZE = 96;
const HIGH_TIER_BATCH = 12;
const batchCapFor = (tier: ThumbTier) => (tier === "j" ? BATCH_SIZE : HIGH_TIER_BATCH);
// Speculative warms (landing zone, background precache) use a much smaller
// batch than the on-screen drain. Nothing can preempt a batch once it's issued
// — the backend takes the whole list, holds the pool until the last image is
// done, and only then resolves — so the batch size *is* the worst-case delay
// before a scroll that lands on cold cells can get any CPU. A 96-image "j"
// batch is several seconds of that; 16 keeps the loop responsive at a small
// cost in per-call overhead.
const SPECULATIVE_BATCH = 16;
// Scroll velocity (px/s) above which the fetch loop treats the scroll as a
// fling: near-viewport generation is skipped (cells fly past unseen) and the
// projected landing zone is warmed instead. Matches GalleryGrid.
const VELOCITY_FAST = 3000;
// Detail levels by zoom. "base" serves the 512px "j" tier. When zoomed in, the
// view serves the *original file* for cheap native-format images (sharp, no
// extra storage) and a 2560px "jh" thumbnail for large/non-native ones. The
// byte cutoff for "serve original" grows with zoom: at "mid" only smaller files
// stream as originals; at "high" most do. Thresholds compare the rendered row
// height in *physical* pixels (row height × `renderScale()`), so hi-DPI screens
// step up earlier — exactly where the 512px tier starts to look soft. The gap
// between each up/down pair is hysteresis to avoid thrashing at a boundary.
//
// The scale is capped rather than the raw DPR, and this is where that matters
// most: taken literally, a phone's 3× put every ordinary cell past HIGH_UP, so
// a 194px-wide thumbnail was backed by a 2560px image — around forty times the
// pixels the screen can show, on the device least able to hold them.
//
// The values carry `GalleryGrid`'s TIER_UPSCALE_TOLERANCE, and for the same
// reason: a tier that has to stretch slightly is barely visible, while stepping
// up costs four times the memory per cell (measured — see
// docs/frontend/grid-loading.md). Without it the two grids disagreed about how
// much softness is acceptable, and this one stepped up the moment a tier was
// stretched at all: a phone at two columns needs 576 physical px on the long
// edge and so left the 512px "j" tier for the 1280px "jm" one to cover a 12%
// gap. The bases are the row heights at which each tier's long edge is exactly
// covered, assuming the ~1.5 landscape aspect these rows average.
const TIER_UPSCALE_TOLERANCE = 1.25;
const MID_UP = 360 * TIER_UPSCALE_TOLERANCE;
const MID_DOWN = 320 * TIER_UPSCALE_TOLERANCE;
const HIGH_UP = 560 * TIER_UPSCALE_TOLERANCE;
const HIGH_DOWN = 500 * TIER_UPSCALE_TOLERANCE;
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
    const px = targetRowHeight() * renderScale();
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
    const h = targetRowHeight() * portraitRowBoost(aspect);
    return h * Math.max(1, aspect) * renderScale();
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
  // Rows actually intersecting the viewport — the subset of the full-res window
  // the user can see right now. The full window is deliberately wider (it
  // pre-loads the rows about to arrive), but "wider" only helps if the visible
  // rows are served first; see the staged upgrade in the assign effect.
  const [viewStartRow, setViewStartRow] = createSignal(0);
  const [viewEndRow, setViewEndRow] = createSignal(0);
  const [generation, setGeneration] = createSignal(0);

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

  // A cell's rung: "cheap" is the base "j" tier assigned during flings at
  // mid/high detail; "full" is whatever thumbSrcFor picks for the current
  // detail level (j / jm / jh / fit-original). Cheap cells upgrade once
  // scrolling settles. The registry itself is created below, once
  // `clearAwaiting` exists for it to call on eviction.
  type Rung = "cheap" | "full";
  // 404 → generation bookkeeping (queued / in flight / failed / warmed). The
  // queue's payload is the tier whose URL actually 404'd, so the drain
  // regenerates *that* tier. Keying it on the path alone and generating
  // `thumbTier()` for everything meant a cheap-rung cell missing its 512px "j"
  // tier triggered a 2560px "jh" decode that didn't even fix the miss — the
  // single most expensive way to not solve the problem, repeated for every
  // cell in the (12–20 row) render buffer whenever the view was zoomed in.
  const queue = createThumbQueue<ThumbTier>();
  // `?v=` cache-busting for the tier URLs. Served-original cells bypass it:
  // `mediaUrl(path, fit)` is already cache-stable per (path, fit bucket).
  const versions = createUrlVersions();
  const pathIndex = createPathIndex();
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
  // NB: served-original (`?fit=`) cells are deliberately left un-warmed. They
  // aren't tier-backed — each is an on-demand source decode inside the request
  // (`serve_fit_image`) — so the obvious move is to load the same URL off-DOM
  // ahead of the viewport and let the webview cache it. Measured, that made
  // things worse: the warm decodes land on the same bounded pool as the cells
  // already on screen, and a look-ahead only pays off if it wins the race,
  // which it can't when each decode is seconds long. What actually helps at
  // this zoom is issuing *less* concurrent work, not more (see the staged
  // upgrade in the assign effect).
  // The path the viewer is currently showing (kept in sync via
  // `lightview:scroll-to-index`). Held at the full rung regardless of where the
  // inner window lands, so closing the viewer reveals the cell at the same
  // resolution it was opened from rather than dropping to the cheap tier.
  const [pinnedPath, setPinnedPath] = createSignal<string | null>(null);

  // Cells that have been pointed at a full-resolution source but haven't
  // painted it yet — precisely what the user is waiting to watch sharpen.
  //
  // This is the budget that actually matters when zoomed in. A full-rung source
  // (a `jh` tier, or a `?fit=` resize decoded from the original on demand)
  // costs the backend's bounded thumb pool a decode per request, and the
  // full-res window spans several rows beyond the viewport — so a screen of 5
  // images was issuing that many expensive requests for cells nobody can see,
  // all at once and at equal priority. The five on screen then finished last.
  // Everything speculative is gated on this reaching zero.
  const awaitingFull = new Map<string, number>(); // path → assigned-at (ms)
  const [awaitingCount, setAwaitingCount] = createSignal(0);
  const markAwaiting = (path: string) => {
    if (awaitingFull.has(path)) return;
    awaitingFull.set(path, performance.now());
    setAwaitingCount(awaitingFull.size);
  };
  const clearAwaiting = (path: string) => {
    if (!awaitingFull.delete(path)) return;
    setAwaitingCount(awaitingFull.size);
  };
  /** Cells still worth deferring speculation for. A source that has been
   *  pending this long is stuck (a huge file, a stalled request, an <img> whose
   *  load event never came) and must not disable the look-ahead for the rest of
   *  the session — the gate is a courtesy to the viewport, not a lock. */
  const AWAIT_BLOCK_MS = 6000;
  const blockingCount = (): number => {
    const now = performance.now();
    let n = 0;
    for (const at of awaitingFull.values()) if (now - at < AWAIT_BLOCK_MS) n++;
    return n;
  };
  // What each cell is showing and at which rung, plus the off-DOM swapper
  // that upgrades one without a skeleton flash. Evicting a cell also releases
  // its hold on the look-ahead: a cell that has gone away is no longer
  // something the viewport is waiting on, and leaving it in `awaitingFull`
  // would gate speculation on an image nobody will ever see.
  const cells = createCellSources<Rung>({
    generation,
    onMiss: (path) => handleThumbError(path),
    onEvict: clearAwaiting,
  });
  onCleanup(() => cells.cancelAll());

  let bgCursor = 0;
  let recalcRange: (() => void) | undefined;

  // Thumbnail generation progress ("Generating N / M"). Idle means nothing is
  // queued for generation and nothing is in flight.
  const progress = createThumbProgress(() => queue.idle());

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
      versions.clear();
      versions.bumpEpoch();
    }
    cells.clear({ keepUrls: keepDisplayed });
    awaitingFull.clear();
    setAwaitingCount(0);
    queue.reset();
    jhPrecached.clear();
    bgCursor = 0;
    progress.reset();
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

  const versionedThumbUrl = (path: string, tier: ThumbTier) =>
    versions.versioned(path, thumbUrl(path, tier));

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

  /** The tier a cell's *currently assigned* URL comes from — i.e. the one that
   *  404'd. Cheap-rung cells always show the base "j" tier, whatever the
   *  detail level says the full source should be. */
  const missingTierFor = (path: string): ThumbTier =>
    cells.rungOf(path) === "cheap" ? "j" : thumbTier();

  const handleThumbError = (path: string) => {
    // Cells served their original file aren't thumbnail-backed — a load error
    // there isn't a cache miss, so don't queue thumbnail generation. Only the
    // *full* rung serves the original though: the same cell on the cheap rung
    // is showing the "j" tier and a miss on that does need generating.
    // Failed or not, this cell has stopped waiting — it must not hold the
    // look-ahead back forever.
    clearAwaiting(path);
    if (cells.rungOf(path) !== "cheap" && servesOriginal(path)) return;
    // Re-queuing an already-queued path re-aims it at whatever the cell shows
    // now — a cell can be upgraded between the miss and the drain — and
    // reports false, so only a genuinely fresh miss counts.
    if (!queue.queue(path, missingTierFor(path))) return;
    recordCacheMiss();
    progress.queued();
    // Wake the loop rather than waiting on its poll. A landing reveals a
    // screenful of cold cells at once, and every one of them lands here.
    loop.poke();
  };

  // Regeneration / one-shot invalidation listener.
  onThumbRegenerated((p) => {
    versions.bump(p);
    if (!cells.has(p)) return;
    cells.setRung(p, "full");
    cells.point(p, thumbSrcFor(p));
  });

  // Assign protocol URLs as soon as cells become visible. Cached thumbs load
  // instantly; uncached ones 404 → onError → queued for generation.
  createEffect(on(
    [visibleCells, dynamics.decodeGate, dynamics.settled, fullStartRow, fullEndRow, pinnedPath,
     viewStartRow, viewEndRow, awaitingCount, dynamics.warping],
    ([visible, gated, settled, fullStart, fullEnd, pinned, viewStart, viewEnd, waiting, warping]) => {
    // While the WebKitGTK decode gate is up, leave new cells on their
    // placeholder so the main thread isn't buried under image decodes
    // mid-scroll. When it releases this re-runs and assigns whatever's now on
    // screen.
    if (gated && !CHEAP_RUNG_DURING_GATE) return;
    // Same, on every platform, while the view is being scrubbed past far
    // faster than anything could load — the window turns over completely each
    // frame, so this would issue a request per cell for thousands of cells
    // nobody sees. Assignment resumes on the frame the scrub slows down.
    if (warping) return;
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

    // Staged upgrade, in two passes over the same cells.
    //
    // Pass 1 takes the rows on screen (plus the viewer's pinned cell); pass 2
    // takes the look-ahead rows inside the full-res window, but only if pass 1
    // left nothing outstanding. A full-rung source costs the backend's bounded
    // pool a decode per request, and the full window is several rows deep —
    // upgrading it all in one sweep put ~7x more expensive requests in flight
    // than the viewport needed. Ordering has to happen *within* the pass, not
    // just across re-runs: on the sweep right after a scroll, `waiting` is
    // still zero from the settled previous screen, so a single loop would
    // issue the whole window at once before any of it registered.
    //
    // Measured neutral on the web client, where the browser decodes off the
    // main thread and the backend's pool already serves in arrival order (the
    // viewport is requested first either way). The reason to keep it is the
    // desktop webview: WebKitGTK decodes on the main thread — the premise of
    // the decode gate and of every isTauri() branch in this file — so cutting
    // concurrent full-res decodes from ~17 to ~5 is main-thread work not done
    // while the user is waiting on the visible rows. That platform is exactly
    // what this harness cannot measure, so treat the win as reasoned, not
    // demonstrated.
    const onScreenOf = (row: number) => row >= viewStart && row < viewEnd;
    const apply = (cell: ReturnType<typeof visibleCells>[number], mayTakeFull: boolean) => {
      // The viewer's current item is always treated as full-res and never
      // degraded — it's the one cell the user is guaranteed to be looking at
      // closely, and it's what the close transition lands back onto.
      const forced = cell.path === pinned;
      const inFull = forced || onScreenOf(cell.row) || (cell.row >= fullStart && cell.row < fullEnd);
      const takeFull = forced || mayTakeFull;
      const cur = cells.rungOf(cell.path);
      if (cur === undefined) {
        const rung: Rung =
          degradable && !forced && (cheapScroll || !inFull || !takeFull) ? "cheap" : "full";
        cells.setRung(cell.path, rung);
        if (rung === "full") markAwaiting(cell.path);
        cells.assign(cell.path, thumbSrcFor(cell.path, rung === "cheap"));
      } else if (cur === "cheap" && inFull && takeFull && (forced || !cheapScroll)) {
        cells.setRung(cell.path, "full");
        markAwaiting(cell.path);
        cells.assign(cell.path, thumbSrcFor(cell.path));
      }
    };

    const all = visible as ReturnType<typeof visibleCells>;
    const lookAhead: typeof all = [];
    for (const cell of all) {
      if (onScreenOf(cell.row) || cell.path === pinned) apply(cell, true);
      else lookAhead.push(cell);
    }
    // `awaitingFull.size`, not the `waiting` snapshot — pass 1 just changed it.
    const viewportIdle = blockingCount() === 0 && waiting === 0;
    for (const cell of lookAhead) apply(cell, viewportIdle);
  }));

  // -------------------------------------------------------------------
  // Fetch loop — lazy "j" tier generation + eviction
  //
  // At component scope rather than inside onMount so `loop.poke()` is
  // reachable from `handleThumbError`: a 404 used to have no way to reach the
  // schedule, because the schedule was a closure inside onMount, so a
  // screenful of misses waited on the 500 ms poll before anything was asked
  // for. At mid/high detail that is the whole visible row.
  // -------------------------------------------------------------------

  const evictFaraway = () => {
    const lay = layout();
    if (lay.rows.length === 0) return;
    const keepStartRow = Math.max(0, startRow() - EVICT_ROWS);
    const keepEndRow = Math.min(lay.rows.length, endRow() + EVICT_ROWS);
    const keepStart = lay.rows[keepStartRow]?.cells[0]?.index ?? 0;
    const lastKeepRow = lay.rows[keepEndRow - 1];
    const keepEnd = lastKeepRow ? lastKeepRow.cells[lastKeepRow.cells.length - 1].index + 1 : props.paths.length;

    const toEvict: string[] = [];
    for (const p of cells.held()) {
      const idx = pathIndex.indexOf(p);
      if (idx !== undefined && (idx < keepStart || idx >= keepEnd)) toEvict.push(p);
    }
    cells.evict(toEvict);
  };

  // A fling has a predictable destination — warm the base "j" tier around
  // the projected landing scroll position (± one viewport) so cells are
  // cached by the time the scroll arrives ("j" is both the cheap fling rung
  // and the base target, so it's the right thing to warm at every detail
  // level). Backend warm via ensureTierThumbnails; on the web client
  // additionally prime the browser's HTTP cache with the same URLs
  // (responses are cacheable for 1h). One batch per pass, recomputed from the
  // live projection each time, so a redirected fling self-corrects and wasted
  // warms stay bounded.
  const warmLandingZone = () => {
    const lay = layout();
    if (lay.rows.length === 0) return;
    const offset = containerRef?.offsetTop ?? 0;
    const vh = viewportHeight();
    const landingTop = Math.max(0, dynamics.projectedLandingY() - offset);
    const firstRow = rowIndexAtOffset(lay.rowTops, Math.max(0, landingTop - vh));
    const lastRow = Math.min(
      lay.rows.length,
      rowIndexAtOffset(lay.rowTops, landingTop + 2 * vh) + 1,
    );

    const want: string[] = [];
    for (let r = firstRow; r < lastRow && want.length < SPECULATIVE_BATCH; r++) {
      const row = lay.rows[r];
      if (!row) continue;
      for (const cell of row.cells) {
        const p = props.paths[cell.index];
        if (!p || cells.has(p) || !queue.warmable(p)) continue;
        queue.markWarmed(p);
        want.push(p);
      }
    }
    if (want.length === 0) return;

    loop.warm(async () => {
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
    });
  };

  // Item-index range covered by a row range. Cell indices are contiguous
  // across justified rows, so the render/full row windows map straight onto
  // the index zones `pickByPriority` ranks against.
  const idxAt = (row: number) => layout().rows[row]?.cells[0]?.index ?? props.paths.length;
  const idxAfter = (rowExcl: number) => {
    const last = layout().rows[rowExcl - 1];
    return last ? last.cells[last.cells.length - 1].index + 1 : 0;
  };

  /** Generate thumbnails for on-screen cells that are showing nothing.
   *  Returns true if a batch was issued. */
  const drainQueued = (): boolean => {
    if (queue.queuedCount() === 0) return false;

    // Drain-time prioritization (mirrors GalleryGrid): full-res window
    // first, then the rendered buffer by distance; leftovers outside the
    // rendered window are dropped and re-queue via 404 if scrolled back.
    const { picked, stale } = pickByPriority(
      queue.queued(),
      (p) => pathIndex.indexOf(p),
      {
        viewStart: idxAt(fullStartRow()),
        viewEnd: idxAfter(fullEndRow()),
        renderStart: idxAt(startRow()),
        renderEnd: idxAfter(endRow()),
      },
      BATCH_SIZE,
    );
    if (stale.length > 0) {
      queue.drop(stale);
      progress.dropped(stale.length);
    }
    if (picked.length === 0) return false;

    // One tier per batch (the IPC takes a single tier), chosen from the
    // most urgent queued cell so the viewport always leads. The rest of the
    // queue keeps its own tiers and drains on a later pass.
    const tier = queue.payloadFor(picked[0])!;
    const cap = batchCapFor(tier);
    const toGenerate: string[] = [];
    for (const p of picked) {
      if (queue.payloadFor(p) !== tier) continue;
      toGenerate.push(p);
      if (toGenerate.length >= cap) break;
    }
    if (toGenerate.length === 0) return false;

    queue.take(toGenerate);
    const gen = generation();
    loop.fetch(async () => {
      try {
        forgetEvicted((await ensureTierThumbnails(toGenerate, tier)).evicted);
        if (loop.aborted() || generation() !== gen) return;
        batch(() => {
          for (const p of toGenerate) {
            versions.bump(p);
            // Re-point each cell at the rung it is actually on. Promoting
            // everything to "full" here (the old behavior) dragged the whole
            // render buffer — not just the inner window — up to the 2560px
            // tier, which is both a wall of decodes and the wrong image for
            // a cell the ladder had deliberately put on the cheap rung.
            const rung = cells.rungOf(p);
            if (rung) cells.point(p, thumbSrcFor(p, rung === "cheap"));
          }
        });
        queue.settle(toGenerate);
        // After the in-flight set is drained, so "idle" reads correctly.
        progress.completed(toGenerate.length);
      } catch (e) {
        console.error("Justified tier generation failed:", e);
        queue.settle(toGenerate, () => true);
      }
    });
    return true;
  };

  // Look-ahead precache in the scroll direction when zoomed in. Without it,
  // every newly-revealed cell at mid/high detail is a cold generate-on-serve
  // round-trip (the "loads too slowly" symptom). Warming a bounded window
  // ahead of the viewport means cells are usually ready by the time they
  // scroll in. Disk stays bounded because the backend LRU-evicts these tiers.
  // Returns true if it issued work.
  const warmLookAhead = (): boolean => {
    if (detailLevel() === "base" || dynamics.velocity() >= 1500) return false;
    const lay = layout();
    // Warm ahead of the *full-res* boundary — that's where upgrades request
    // the high tier, so the look-ahead must cover the rows about to cross it
    // (the outer cheap-tier window only needs "j").
    const from = dynamics.direction() === 1 ? fullEndRow() : Math.max(0, fullStartRow() - JH_PRECACHE_ROWS);
    const to = dynamics.direction() === 1
      ? Math.min(lay.rows.length, fullEndRow() + JH_PRECACHE_ROWS)
      : fullStartRow();
    const tier = thumbTier();
    const cap = batchCapFor(tier);
    const want: string[] = [];
    for (let r = from; r < to; r++) {
      const row = lay.rows[r];
      if (!row) continue;
      for (const cell of row.cells) {
        const p = props.paths[cell.index];
        if (!p || queue.hasFailed(p)) continue;
        // Originals aren't tier-backed — only tier-served cells need warming.
        if (servesOriginal(p)) continue;
        if (jhPrecached.has(p) || want.length >= cap) continue;
        jhPrecached.add(p);
        want.push(p);
      }
    }
    if (want.length === 0) return false;
    loop.warm(async () => {
      try { forgetEvicted((await ensureTierThumbnails(want, tier)).evicted); }
      catch (e) { console.error("Justified look-ahead failed:", e); }
    });
    return true;
  };

  // Background precache of upcoming items when idle. Only the base "j" tier
  // is precached across the whole gallery — the high tiers are warmed via
  // the look-ahead above (visible + a window ahead) so their disk cost stays
  // bounded to what's actually viewed zoomed in.
  //
  // Deliberately base-detail only. A zoomed-in screen does reveal a couple
  // of dozen cold cheap-rung cells per scroll, and walking the gallery ahead
  // of time to warm them looks like the obvious fix — but a 512px "j" still
  // costs a decode of the full-size original, so it is nearly as expensive
  // as the full-res sources the visible cells need, on the same bounded
  // pool. Measured over 4 cold runs of a continuous 3-viewport scroll at
  // high zoom, enabling it here more than doubled time-to-sharp for the
  // cells on screen (6.8s → 15.2s mean). Speculation is only free when
  // there is spare capacity, and at high zoom there isn't any.
  const warmBackground = () => {
    if (detailLevel() !== "base" || dynamics.velocity() >= 500) return;
    if (bgCursor >= props.paths.length) return;
    const bgNeeded: string[] = [];
    while (bgNeeded.length < SPECULATIVE_BATCH && bgCursor < props.paths.length) {
      const p = props.paths[bgCursor];
      bgCursor++;
      if (p && !cells.has(p) && !queue.hasFailed(p)) bgNeeded.push(p);
    }
    if (bgNeeded.length === 0) return;
    loop.warm(async () => {
      try { await ensureTierThumbnails(bgNeeded, "j"); }
      catch (e) { console.error("Justified background precache failed:", e); }
    });
  };

  const loop = createFetchLoop({
    evict: evictFaraway,
    warping: dynamics.warping,
    flinging: () => dynamics.velocity() > VELOCITY_FAST,
    // Speculation also waits on cells already pointed at a full-resolution
    // source that have not painted yet — one of those is a whole decode of
    // the backend's bounded pool, for something the user is watching.
    blocked: () => blockingCount() > 0,
    drain: drainQueued,
    warmLanding: warmLandingZone,
    speculate: () => {
      if (!warmLookAhead()) warmBackground();
    },
  });

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
      const sy = scrollTop();
      const vh = viewportHeight();
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
        newFullEnd !== untrack(fullEndRow) ||
        firstRow !== untrack(viewStartRow) ||
        lastRow !== untrack(viewEndRow)
      ) {
        batch(() => {
          setStartRow(newStart);
          setEndRow(newEnd);
          setFullStartRow(newFullStart);
          setFullEndRow(newFullEnd);
          setViewStartRow(firstRow);
          setViewEndRow(lastRow);
        });
      }
    };

    // Ctrl+wheel zooms (changes the target row height) instead of scrolling.
    // During a Ctrl+drag selection, fall through to scroll (mirrors GalleryGrid).
    const detachWheel = createWheelScroll({
      rowHeight: () => targetRowHeight() + gap(),
      onSettle: () => loop.schedule(),
      onZoom: (e) => {
        if (isDragging()) return false;
        const cur = settings().display.thumbnail_size;
        const step = Math.max(8, Math.round(cur * 0.12));
        const lo = settings().display.thumb_size_min ?? ROW_HEIGHT_MIN;
        const hi = settings().display.thumb_size_max ?? ROW_HEIGHT_MAX;
        const next = Math.max(lo, Math.min(hi, cur + (e.deltaY < 0 ? step : -step)));
        if (next !== cur) {
          setSettings((prev) => ({ ...prev, display: { ...prev.display, thumbnail_size: next } }));
        }
        return true;
      },
    }).attach();

    recalcRange();

    const onScrollEnd = () => {
      dynamics.markSettled();
      loop.schedule();
    };
    const detachScrollEnd = onScrollHost("scrollend", onScrollEnd);

    createEffect(on(generation, () => { loop.schedule(); }));

    // Drain the landing viewport whenever scrolling settles, however that was
    // detected. `scrollend` (→ onScrollEnd) already does this, but it's not
    // universal on mobile; scrollDynamics also flips `settled` true via its own
    // fling debounce, and that path had no fetch trigger — so a fast flick on a
    // browser without `scrollend` left cells blank until the 500ms poll.
    createEffect(on(dynamics.settled, (s) => { if (s) loop.schedule(); }, { defer: true }));

    // Re-run the visible range whenever the layout changes (width / zoom).
    createEffect(on(layout, () => { recalcRange?.(); }));

    const onInvalidate = () => resetStreaming();
    window.addEventListener("lightview:thumbnails-invalidated", onInvalidate);

    // The viewer announces which item it's showing (VIEWER_PATH_EVENT, null on
    // close). Hold that cell at the full rung: it's the one the user is looking
    // at closely, and it's what the close transition flies back down onto — a
    // cell the ladder had parked on the 512px cheap rung is exactly the
    // "closing drops back to a blurry thumbnail" pop. Clearing the pin on close
    // doesn't downgrade anything; the ladder only ever upgrades cheap → full.
    const onViewerPath = (e: Event) => {
      const viewed = (e as CustomEvent<string | null>).detail;
      setPinnedPath(viewed ?? null);
      if (!viewed || cells.rungOf(viewed) !== "cheap") return;
      cells.setRung(viewed, "full");
      cells.assign(viewed, thumbSrcFor(viewed));
      loop.schedule();
    };
    window.addEventListener(VIEWER_PATH_EVENT, onViewerPath);

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
      const viewTop = scrollTop();
      const viewBottom = viewTop + viewportHeight();
      if (imageTop < viewTop || imageBottom > viewBottom) {
        scrollToY(imageTop - (viewportHeight() - row.height) / 2);
      }
    };
    window.addEventListener("lightview:scroll-to-index", onScrollToIndex);

    onCleanup(() => {
      ro.disconnect();
      detachScrollEnd();
      detachWheel();
      window.removeEventListener("lightview:thumbnails-invalidated", onInvalidate);
      window.removeEventListener("lightview:scroll-to-index", onScrollToIndex);
      window.removeEventListener(VIEWER_PATH_EVENT, onViewerPath);
    });
  });

  // Prune per-path streaming state when the path list changes. Surgical, not
  // a wholesale wipe: the assign effect above has already run for this update
  // (skipping cells that still had a rung) and won't fire again until the
  // visible range moves — wiping surviving cells here would blank the grid
  // until the next scroll (e.g. after deleting one image). Cells whose path
  // survives keeps its URL and decoded <img>.
  createEffect(on(() => props.paths, (paths) => {
    pathIndex.reindex(paths);

    // Unlike an eviction, these paths are not coming back — an evicted cell
    // keeps its version so a scroll back reuses the same URL.
    for (const p of cells.prune(pathIndex.has)) versions.forget(p);
    pathIndex.pruneAbsent(jhPrecached);
    const pinned = untrack(pinnedPath);
    if (pinned && !pathIndex.has(pinned)) setPinnedPath(null);
    progress.dropped(queue.prune(pathIndex.has));
    if (measuredAspects().size > 0) {
      let changed = false;
      const next = new Map<string, number>();
      for (const [p, a] of measuredAspects()) {
        if (pathIndex.has(p)) next.set(p, a);
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
              const index = () => pathIndex.indexOf(path) ?? -1;
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
                      thumbSrc={cells.srcOf(path)}
                      tier={thumbTier()}
                      freeSize={true}
                      durationSec={durationByPath().get(path) ?? null}
                      selected={effectiveSelected().has(path)}
                      onClick={(e: MouseEvent) => handleItemClick({ path, index: index() }, e)}
                      onMouseDown={(e: MouseEvent) => handleDragStart(index(), e)}
                      onMouseEnter={() => handleDragEnter(index())}
                      onContextMenu={(e) => props.onItemContextMenu?.(e, path, index())}
                      onError={handleThumbError}
                      onImageLoad={(w, h) => {
                        recordMeasuredAspect(path, w, h);
                        // A full-rung cell has painted — release its hold on
                        // the look-ahead.
                        if (cells.rungOf(path) === "full") clearAwaiting(path);
                      }}
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
