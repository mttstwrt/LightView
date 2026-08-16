// The canvas: one pannable surface with the sort spiralling out from its
// centre — item 0 in the middle, each later item one step further round.
//
// It is the third view onto the loading machine the two grids share (see
// docs/frontend/grid-loading.md) and the first that is not a reading-order
// scroller, so it is worth being explicit about which half is which. Inherited
// whole: `cellSources`, `thumbQueue`, `urlVersions`, `pathIndex`, `fetchLoop`,
// `thumbProgress`, `scrollDynamics`, `galleryControls`, `wheelScroll`,
// `thumbRegeneration`. Written here, because it *is* the view: the spiral
// geometry (lib/spiralLayout.ts), the range calculation, what "far away" means
// to eviction, what a batch drains, and what it speculates on.
//
// Two things differ from the grids in kind rather than in tuning:
//
// **The window is a rectangle of a lattice, not a run of rows.** Everything
// downstream that took an index range takes a cell range instead, and the
// drain ranks by distance from the viewport centre rather than by distance
// from a row — which is what `pickByRank` in lib/loadPriority.ts was split out
// for. In index terms this window is several disjoint runs, so the range-based
// ranker would have called most of what is on screen stale.
//
// **Growth is quadratic in the look-ahead.** A grid that renders k rows beyond
// the viewport pays k rows; a canvas that renders k cells beyond it on every
// side pays the whole border, so the buffers here are small integers where the
// grids can afford a dozen rows. The resolution ladder does the work instead:
// the cheap `j` tier covers the outer window and only cells at or next to the
// viewport are upgraded.
//
// Aspect ratios are preserved rather than cropped, which is the whole reason
// the view can share the justified `j`/`jm`/`jh` tiers and cost the gallery no
// thumbnails of its own: each cell is a square slot with the image fitted
// inside it. The letterboxing that leaves is deliberate — see the rejected
// alternatives in lib/spiralLayout.ts.

import { For, Show, createSignal, createEffect, createMemo, on, onMount, onCleanup, batch, untrack } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { isTauri, renderScale } from "../../lib/runtime";
import { createWheelScroll } from "../../lib/wheelScroll";
import { settings, setSettings } from "../../stores/settingsStore";
import { durationByPath } from "../../stores/galleryStore";
import { ensureTierThumbnails, thumbUrl, type ThumbTier } from "../../lib/ipc";
import { onThumbRegenerated } from "../../lib/thumbRegeneration";
import { createThumbProgress } from "../../lib/thumbProgress";
import { recordCacheMiss } from "../../lib/perfMonitor";
import { createDragSelect } from "../../lib/galleryControls";
import { createScrollDynamics, constrainedNetwork, CHEAP_RUNG_DURING_GATE } from "../../lib/scrollDynamics";
import {
  onScrollHost,
  scrollHost,
  scrollLeft,
  scrollTop,
  scrollToX,
  scrollToY,
  viewportHeight,
  viewportWidth,
} from "../../lib/scrollHost";
import { VIEWER_PATH_EVENT } from "../../lib/viewerTransition";
import { OUTSIDE_RANK, pickByRank } from "../../lib/loadPriority";
import { createUrlVersions } from "../../lib/urlVersions";
import { createPathIndex } from "../../lib/pathIndex";
import { createThumbQueue } from "../../lib/thumbQueue";
import { createFetchLoop } from "../../lib/fetchLoop";
import { createCellSources } from "../../lib/cellSources";
import {
  cellCentre,
  cellOfIndex,
  cellOrigin,
  cellsInRect,
  fitBox,
  indexOfCell,
  spiralGeometry,
  type CellRange,
  type SpiralCell,
  type SpiralGeometry,
} from "../../lib/spiralLayout";
import { ThumbnailCell } from "./ThumbnailCell";

interface CanvasViewProps {
  paths: string[];
  /** Aspect ratio (w/h) per path; missing entries fall back to 1:1. */
  aspects: Map<string, number>;
  onItemClick: (index: number) => void;
  onItemSelect: (path: string) => void;
  onDragSelect?: (paths: string[]) => void;
  onBackgroundClick?: () => void;
  selectedPaths: Set<string>;
  /** Tap-to-toggle multi-select mode (mobile Select button). */
  selectionMode?: boolean;
  onItemContextMenu?: (e: MouseEvent, path: string, index: number) => void;
  loading: boolean;
}

// Cells rendered beyond the viewport, and the inner window that carries the
// full-resolution source. Counted per side, and small on purpose: a border of
// k cells around a w×h viewport is (w+2k)(h+2k) − wh cells, so the third ring
// out already costs more than the screen holds. The grids' equivalents are
// twelve rows deep and can be, because a row of look-ahead is a row.
const BUFFER_AHEAD = 2;
const BUFFER_BEHIND = 1;
const BUFFER_AHEAD_MAX = 5;
const FULL_AHEAD = 1;
const FULL_BEHIND = 0;
// Cells beyond the rendered range before a thumbnail is evicted. Wider than
// the render buffer so a small pan back and forth over the same border does
// not evict and re-fetch the cells it keeps crossing.
const EVICT_CELLS = 4;
// How many paths to generate per IPC batch, matching JustifiedGrid: the
// backend decodes the whole batch before it resolves and holds the shared
// rayon pool while it does, so the batch size is also the worst-case delay
// before a pan onto cold cells can get any CPU back.
const BATCH_SIZE = 96;
const HIGH_TIER_BATCH = 12;
const batchCapFor = (tier: ThumbTier) => (tier === "j" ? BATCH_SIZE : HIGH_TIER_BATCH);
const SPECULATIVE_BATCH = 16;
// Speed (px/s) above which a pan is treated as a fling: near-viewport
// generation is skipped (those cells go past unseen) and the projected landing
// zone is warmed instead. Matches both grids.
const VELOCITY_FAST = 3000;
// Detail levels by zoom, mirroring JustifiedGrid's ladder against this view's
// simpler geometry: a fitted cell's longest edge is exactly the slot size, so
// the thresholds are the tiers' own edges (512 / 1280) rather than a row
// height times an assumed aspect. The gap between each up/down pair is
// hysteresis, and the tolerance is JustifiedGrid's: a tier stretched slightly
// is barely visible, while stepping up costs four times the memory per cell
// (measured — docs/frontend/grid-loading.md).
const TIER_UPSCALE_TOLERANCE = 1.25;
const MID_UP = 512 * TIER_UPSCALE_TOLERANCE;
const MID_DOWN = 512;
const HIGH_UP = 1280 * TIER_UPSCALE_TOLERANCE;
const HIGH_DOWN = 1280;
// Cell-size (zoom) bounds, mirroring the grids' thumbnail-size range.
const CELL_SIZE_MIN = 80;
const CELL_SIZE_MAX = 600;
// How far a mouse may travel during a background drag before the release stops
// counting as a click. Below it, a pan that barely moved still clears the
// selection the way a plain background click does.
const PAN_SLOP_PX = 4;

type DetailLevel = "base" | "mid" | "high";
type Rung = "cheap" | "full";

const sameRange = (a: CellRange, b: CellRange) =>
  a.cx0 === b.cx0 && a.cy0 === b.cy0 && a.cx1 === b.cx1 && a.cy1 === b.cy1;

const inRange = (r: CellRange, cx: number, cy: number) =>
  cx >= r.cx0 && cx <= r.cx1 && cy >= r.cy0 && cy <= r.cy1;

const grow = (r: CellRange, left: number, top: number, right: number, bottom: number): CellRange => ({
  cx0: r.cx0 - left,
  cy0: r.cy0 - top,
  cx1: r.cx1 + right,
  cy1: r.cy1 + bottom,
});

export function CanvasView(props: CanvasViewProps) {
  const gap = () => settings().display.grid_gap;

  // The zoom. `thumbnail_size` is a cell size everywhere else too, so the
  // canvas can take it as one directly — unlike the justified view, which has
  // to turn it back into a row height because on mobile the setting means a
  // column width.
  const cellSize = () => {
    const lo = settings().display.thumb_size_min ?? CELL_SIZE_MIN;
    const hi = settings().display.thumb_size_max ?? CELL_SIZE_MAX;
    return Math.max(lo, Math.min(hi, settings().display.thumbnail_size));
  };

  // Aspect ratios recovered from loaded thumbnails, for paths whose indexed
  // dimensions aren't known yet (a just-added file is inserted with NULL
  // width/height, and they are only written when its thumbnail is generated —
  // after the frontend has already fetched the sorted items). Without this
  // such a cell gets a square box and the aspect-correct thumbnail is
  // object-cover cropped into it. Same recovery as JustifiedGrid.
  const [measuredAspects, setMeasuredAspects] = createSignal<Map<string, number>>(new Map());
  const aspectOf = (path: string): number =>
    props.aspects.get(path) ?? measuredAspects().get(path) ?? 1;
  const recordMeasuredAspect = (path: string, w: number, h: number) => {
    if (w <= 0 || h <= 0) return;
    if (props.aspects.has(path)) return;
    const cur = measuredAspects();
    if (cur.has(path)) return;
    const next = new Map(cur);
    next.set(path, w / h);
    setMeasuredAspects(next);
  };

  const geometry = createMemo<SpiralGeometry>(() =>
    spiralGeometry(props.paths.length, cellSize(), gap()),
  );

  // Current detail level from the (DPR-aware, hysteretic) zoom.
  //
  // It follows the justified view's `justified_high_detail` switch rather than
  // introducing one of its own: the setting governs whether the jm/jh tiers
  // are generated at all, and those are exactly the tiers this ladder asks
  // for. A second toggle would be a second name for the same disk.
  const [detailLevel, setDetailLevel] = createSignal<DetailLevel>("base");
  createEffect(() => {
    if (settings().display.justified_high_detail === false) {
      setDetailLevel("base");
      return;
    }
    // A fitted cell's longest edge is the slot size exactly, so this is the
    // resolution the screen actually shows.
    const px = cellSize() * renderScale();
    setDetailLevel((prev): DetailLevel => {
      if (prev === "base") return px > HIGH_UP ? "high" : px > MID_UP ? "mid" : "base";
      if (prev === "mid") return px > HIGH_UP ? "high" : px < MID_DOWN ? "base" : "mid";
      return px < MID_DOWN ? "base" : px < HIGH_DOWN ? "mid" : "high";
    });
  });

  const thumbTier = (): ThumbTier =>
    detailLevel() === "base" ? "j" : detailLevel() === "mid" ? "jm" : "jh";

  // The three windows, as lattice rectangles: everything rendered, the inner
  // full-resolution window, and what the viewport actually covers. Compared
  // structurally so a pan within one cell notifies nothing — the same
  // "only update when the range changes" rule the grids' row signals follow.
  const empty: CellRange = { cx0: 0, cy0: 0, cx1: -1, cy1: -1 };
  const [renderRange, setRenderRange] = createSignal<CellRange>(empty, { equals: sameRange });
  const [fullRange, setFullRange] = createSignal<CellRange>(empty, { equals: sameRange });
  const [viewRange, setViewRange] = createSignal<CellRange>(empty, { equals: sameRange });
  const [generation, setGeneration] = createSignal(0);

  // The viewport's centre in (fractional) lattice coordinates. Read at drain
  // time to rank queued cells by distance, which is not a reactive read — a
  // plain pair updated by `recalcRange` costs nothing and notifies nobody.
  let centreCx = 0;
  let centreCy = 0;

  let containerRef: HTMLDivElement | undefined;
  let recalcRange: (() => void) | undefined;

  // Shared pointer controls: click handling and Ctrl/Cmd-drag range select,
  // identical to both grids. `createEdgeScroll` is deliberately not used —
  // it auto-scrolls toward the top and bottom of a vertical scroller, and this
  // surface has neither, so a range drag here is bounded by the viewport.
  const { isDragging, effectiveSelected, handleDragStart, handleDragEnter, handleItemClick, handleBackgroundClick } =
    createDragSelect(props);

  // -----------------------------------------------------------------------
  // Layout
  // -----------------------------------------------------------------------

  interface CanvasCell {
    path: string;
    index: number;
    cx: number;
    cy: number;
    /** The fitted box, already centred within its slot. */
    x: number;
    y: number;
    width: number;
    height: number;
  }

  const visibleCells = createMemo<CanvasCell[]>(() => {
    const g = geometry();
    const r = renderRange();
    const paths = props.paths;
    const out: CanvasCell[] = [];
    for (let cy = r.cy0; cy <= r.cy1; cy++) {
      for (let cx = r.cx0; cx <= r.cx1; cx++) {
        // Every lattice cell maps to an item, but the outermost ring is only
        // partly filled — the rest of it is past the end of the gallery.
        const index = indexOfCell(cx, cy);
        const path = paths[index];
        if (!path) continue;
        const origin = cellOrigin(g, { cx, cy });
        const box = fitBox(aspectOf(path), g.cellSize);
        out.push({
          path,
          index,
          cx,
          cy,
          x: origin.x + (g.cellSize - box.width) / 2,
          y: origin.y + (g.cellSize - box.height) / 2,
          width: box.width,
          height: box.height,
        });
      }
    }
    return out;
  });

  // Per-path geometry in a fine-grained store, so a cell whose box is
  // unchanged across a pan does not notify (reconcile diffs leaves). That is
  // what lets the render use a *path-keyed* <For>: a cell that stays on screen
  // keeps its DOM node and its <img src>, and the webview never re-decodes it.
  type CellBox = { x: number; y: number; width: number; height: number };
  const [geom, setGeom] = createStore<Record<string, CellBox>>({});
  createEffect(() => {
    const next: Record<string, CellBox> = {};
    for (const c of visibleCells()) next[c.path] = { x: c.x, y: c.y, width: c.width, height: c.height };
    setGeom(reconcile(next));
  });

  const visiblePaths = createMemo(() => visibleCells().map((c) => c.path));

  // -----------------------------------------------------------------------
  // Thumbnail streaming
  // -----------------------------------------------------------------------

  // The queue's payload is the tier that actually 404'd: a cheap-rung cell is
  // showing "j" while the detail level says the target is "jh", and
  // regenerating the target would neither fix the miss nor be cheap.
  const queue = createThumbQueue<ThumbTier>();
  const versions = createUrlVersions();
  const pathIndex = createPathIndex();
  // Paths a look-ahead warm has already requested at the current high tier.
  // The backend LRU-evicts those tiers to stay inside a disk budget and
  // reports what it dropped, so this memo is corrected rather than trusted —
  // without that an evicted cell is never re-warmed, because the look-ahead
  // still believes it is cached.
  const precached = new Set<string>();
  const forgetEvicted = (evicted: string[] | undefined) => {
    if (!evicted?.length) return;
    for (const p of evicted) precached.delete(p);
  };
  let bgCursor = 0;

  const [pinnedPath, setPinnedPath] = createSignal<string | null>(null);

  // Cells pointed at a full-resolution source that have not painted yet —
  // what the user is waiting to watch sharpen. Speculation is gated on this
  // reaching zero, because one such cell is a whole decode of the backend's
  // bounded pool.
  const awaitingFull = new Map<string, number>();
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
  /** A source pending this long is stuck (a huge file, a stalled request, an
   *  <img> whose load event never came) and must not disable the look-ahead
   *  for the rest of the session — the gate is a courtesy to the viewport,
   *  not a lock. */
  const AWAIT_BLOCK_MS = 6000;
  const blockingCount = (): number => {
    const now = performance.now();
    let n = 0;
    for (const at of awaitingFull.values()) if (now - at < AWAIT_BLOCK_MS) n++;
    return n;
  };

  const cells = createCellSources<Rung>({
    generation,
    onMiss: (path) => handleThumbError(path),
    onEvict: clearAwaiting,
  });
  onCleanup(() => cells.cancelAll());

  const progress = createThumbProgress(() => queue.idle());

  const dynamics = createScrollDynamics({
    // One "row" is one lattice step, in both axes.
    rowHeight: () => geometry().pitch,
    cellsPerRow: () => {
      const pitch = geometry().pitch;
      return pitch > 0 ? Math.max(1, viewportWidth() / pitch) : 1;
    },
    cellCostPx: () => {
      const t = thumbTier();
      const px = t === "jh" ? 2560 : t === "jm" ? 1280 : 512;
      return px * px;
    },
    onFrame: () => recalcRange?.(),
  });
  onCleanup(() => dynamics.dispose());

  const resetStreaming = (keepDisplayed = false) => {
    if (!keepDisplayed) {
      versions.clear();
      versions.bumpEpoch();
    }
    cells.clear({ keepUrls: keepDisplayed });
    awaitingFull.clear();
    setAwaitingCount(0);
    queue.reset();
    precached.clear();
    bgCursor = 0;
    progress.reset();
    setGeneration((g) => g + 1);
  };

  // Re-stream when the detail level changes, keeping the displayed images up
  // until the new tier decodes behind them. Deferred so it does not fire on
  // the initial mount.
  createEffect(on(detailLevel, () => resetStreaming(true), { defer: true }));

  const versionedThumbUrl = (path: string, tier: ThumbTier) =>
    versions.versioned(path, thumbUrl(path, tier));

  // The cheap rung is always the base "j" tier: small, aspect-preserving, and
  // precached across the gallery, so a cell revealed by a fling shows
  // something real immediately and upgrades once the pan settles.
  const thumbSrcFor = (path: string, cheap = false) =>
    versionedThumbUrl(path, cheap ? "j" : thumbTier());

  const missingTierFor = (path: string): ThumbTier =>
    cells.rungOf(path) === "cheap" ? "j" : thumbTier();

  const handleThumbError = (path: string) => {
    // Failed or not, this cell has stopped waiting — it must not hold the
    // look-ahead back forever.
    clearAwaiting(path);
    // Re-queuing an already-queued path re-aims it at whatever the cell shows
    // now (it can be upgraded between the miss and the drain) and reports
    // false, so only a genuinely fresh miss counts.
    if (!queue.queue(path, missingTierFor(path))) return;
    recordCacheMiss();
    progress.queued();
    // Wake the loop rather than waiting on its poll: a landing reveals a
    // screenful of cold cells at once and every one of them lands here.
    loop.poke();
  };

  onThumbRegenerated((p) => {
    versions.bump(p);
    if (!cells.has(p)) return;
    cells.setRung(p, "full");
    cells.point(p, thumbSrcFor(p));
  });

  // Assign URLs as cells appear. Cached thumbnails load instantly; uncached
  // ones 404 → onError → queued for generation.
  createEffect(on(
    [visibleCells, dynamics.decodeGate, dynamics.settled, fullRange, viewRange, pinnedPath,
     awaitingCount, dynamics.warping],
    ([visible, gated, settled, full, view, pinned, waiting, warping]) => {
      // While the WebKitGTK decode gate is up, leave new cells on their
      // placeholder so the main thread is not buried under image decodes
      // mid-pan. When it releases this re-runs.
      if (gated && !CHEAP_RUNG_DURING_GATE) return;
      // Same on every platform while the surface is being scrubbed past far
      // faster than anything could load: the window turns over completely each
      // frame, so this would issue a request per cell for cells nobody sees.
      if (warping) return;

      const cheapScroll = isTauri() ? gated : !settled || constrainedNetwork();
      const degradable = detailLevel() !== "base";

      // Staged upgrade, in two passes over the same cells: the cells on screen
      // (plus the viewer's pinned one) first, then the look-ahead border, and
      // only if the first pass left nothing outstanding. A full-rung source
      // costs the backend's bounded pool a decode, and the border is most of
      // the rendered window — upgrading it in one sweep puts several times
      // more expensive requests in flight than the viewport needs. Ordering
      // has to happen *within* the pass: right after a pan, `waiting` is still
      // zero from the settled previous screen.
      const apply = (cell: CanvasCell, mayTakeFull: boolean) => {
        // The viewer's current item is never degraded — it is the one cell the
        // user is looking at closely, and what the close transition lands on.
        const forced = cell.path === pinned;
        const onScreen = inRange(view, cell.cx, cell.cy);
        const inFull = forced || onScreen || inRange(full, cell.cx, cell.cy);
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

      const lookAhead: CanvasCell[] = [];
      for (const cell of visible) {
        if (inRange(view, cell.cx, cell.cy) || cell.path === pinned) apply(cell, true);
        else lookAhead.push(cell);
      }
      // `blockingCount()`, not the `waiting` snapshot — pass 1 just changed it.
      const viewportIdle = blockingCount() === 0 && waiting === 0;
      for (const cell of lookAhead) apply(cell, viewportIdle);
    },
  ));

  // -------------------------------------------------------------------
  // Fetch loop — generation, eviction, speculation
  //
  // At component scope rather than inside onMount so `loop.poke()` is
  // reachable from `handleThumbError`.
  // -------------------------------------------------------------------

  const cellOfPath = (path: string): SpiralCell | undefined => {
    const idx = pathIndex.indexOf(path);
    return idx === undefined ? undefined : cellOfIndex(idx);
  };

  const evictFaraway = () => {
    // Runs on every pass of the loop, including before the first layout.
    if (props.paths.length === 0) return;
    const keep = grow(renderRange(), EVICT_CELLS, EVICT_CELLS, EVICT_CELLS, EVICT_CELLS);
    const toEvict: string[] = [];
    for (const p of cells.held()) {
      const cell = cellOfPath(p);
      if (cell && !inRange(keep, cell.cx, cell.cy)) toEvict.push(p);
    }
    cells.evict(toEvict);
  };

  /** Rank a queued path against the current windows: on screen first, then
   *  the rendered border nearest the viewport centre, then everything else.
   *  Distance is measured from the centre in lattice steps, which is the whole
   *  reason this view cannot use the range-based ranker — its window is a
   *  rectangle of the spiral, and in index terms that is several disjoint
   *  runs, most of which the index ranker would have called stale. */
  const rankQueued = (path: string) => {
    const cell = cellOfPath(path);
    // No longer in the path list (renamed, removed, filtered away).
    if (!cell) return { rank: OUTSIDE_RANK, dist: Number.MAX_SAFE_INTEGER };
    const dist = Math.hypot(cell.cx - centreCx, cell.cy - centreCy);
    if (inRange(fullRange(), cell.cx, cell.cy)) return { rank: 0, dist };
    if (inRange(renderRange(), cell.cx, cell.cy)) return { rank: 1, dist };
    return { rank: OUTSIDE_RANK, dist };
  };

  /** Generate thumbnails for cells that are showing nothing. Returns true if a
   *  batch was issued. */
  const drainQueued = (): boolean => {
    if (queue.queuedCount() === 0) return false;

    const { picked, stale } = pickByRank(queue.queued(), rankQueued, BATCH_SIZE);
    if (stale.length > 0) {
      queue.drop(stale);
      progress.dropped(stale.length);
    }
    if (picked.length === 0) return false;

    // One tier per batch (the IPC takes a single tier), chosen from the most
    // urgent queued cell so the viewport always leads. The rest of the queue
    // keeps its own tiers and drains on a later pass.
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
            // Re-point each cell at the rung it is actually on: promoting
            // everything to "full" here would drag the whole rendered border
            // up to the high tier, which is both a wall of decodes and the
            // wrong image for a cell the ladder deliberately left cheap.
            const rung = cells.rungOf(p);
            if (rung) cells.point(p, thumbSrcFor(p, rung === "cheap"));
          }
        });
        queue.settle(toGenerate);
        // After the in-flight set is drained, so "idle" reads correctly.
        progress.completed(toGenerate.length);
      } catch (e) {
        console.error("Canvas tier generation failed:", e);
        queue.settle(toGenerate, () => true);
      }
    });
    return true;
  };

  /** Warm the base tier around where the current fling will land, so cells are
   *  cached by the time the pan arrives. Recomputed from the live projection
   *  each pass, so a redirected fling self-corrects and wasted warms stay
   *  bounded to one batch. */
  const warmLandingZone = () => {
    const g = geometry();
    if (props.paths.length === 0) return;
    const vw = viewportWidth();
    const vh = viewportHeight();
    const left = dynamics.projectedLandingX() - surfaceOffsetX();
    const top = dynamics.projectedLandingY() - surfaceOffsetY();
    const r = cellsInRect(g, left - vw / 2, top - vh / 2, left + vw * 1.5, top + vh * 1.5);

    const want: string[] = [];
    for (let cy = r.cy0; cy <= r.cy1 && want.length < SPECULATIVE_BATCH; cy++) {
      for (let cx = r.cx0; cx <= r.cx1 && want.length < SPECULATIVE_BATCH; cx++) {
        const p = props.paths[indexOfCell(cx, cy)];
        if (!p || cells.has(p) || !queue.warmable(p)) continue;
        queue.markWarmed(p);
        want.push(p);
      }
    }
    if (want.length === 0) return;

    loop.warm(async () => {
      try {
        await ensureTierThumbnails(want, "j");
        // Browser cache warm — low priority so it cannot compete with the
        // visible cells' loads. `priority` is a progressive enhancement
        // (ignored where unsupported; absent from TS 5.4's RequestInit).
        if (!isTauri()) {
          for (const p of want) {
            fetch(versionedThumbUrl(p, "j"), { priority: "low" } as RequestInit).catch(() => {});
          }
        }
      } catch (e) {
        console.error("Canvas landing-zone warm failed:", e);
      }
    });
  };

  /** Warm the high tier for the rendered border when zoomed in, so a cell is
   *  not a cold generate-on-serve round trip the moment it crosses into the
   *  full-resolution window. Returns true if it issued work. */
  const warmLookAhead = (): boolean => {
    if (detailLevel() === "base" || dynamics.velocity() >= 1500) return false;
    const r = renderRange();
    const full = fullRange();
    const tier = thumbTier();
    const cap = batchCapFor(tier);
    const want: string[] = [];
    for (let cy = r.cy0; cy <= r.cy1 && want.length < cap; cy++) {
      for (let cx = r.cx0; cx <= r.cx1 && want.length < cap; cx++) {
        // Inside the full window the drain is already responsible for it.
        if (inRange(full, cx, cy)) continue;
        const p = props.paths[indexOfCell(cx, cy)];
        if (!p || queue.hasFailed(p) || precached.has(p)) continue;
        precached.add(p);
        want.push(p);
      }
    }
    if (want.length === 0) return false;
    loop.warm(async () => {
      try { forgetEvicted((await ensureTierThumbnails(want, tier)).evicted); }
      catch (e) { console.error("Canvas look-ahead failed:", e); }
    });
    return true;
  };

  /** Background precache of the base tier when idle. The item order *is* the
   *  spiral order, so walking `props.paths` fills outward from the centre —
   *  the direction the user is going to pan in.
   *
   *  Base detail only, for JustifiedGrid's measured reason: a 512px "j" still
   *  costs a decode of the full-size original, so at high zoom it competes
   *  with the visible cells on the same bounded pool for cells nobody has
   *  asked for. */
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
      catch (e) { console.error("Canvas background precache failed:", e); }
    });
  };

  const loop = createFetchLoop({
    evict: evictFaraway,
    warping: dynamics.warping,
    flinging: () => dynamics.velocity() > VELOCITY_FAST,
    blocked: () => blockingCount() > 0,
    drain: drainQueued,
    warmLanding: warmLandingZone,
    speculate: () => {
      if (!warmLookAhead()) warmBackground();
    },
  });

  // -----------------------------------------------------------------------
  // Scroll, zoom, pan
  //
  // Surface coordinates are the scroller's content coordinates minus the
  // container's own offset within it, so these two are the only place the
  // difference is spelled out.
  // -----------------------------------------------------------------------

  const surfaceOffsetX = () => containerRef?.offsetLeft ?? 0;
  const surfaceOffsetY = () => containerRef?.offsetTop ?? 0;

  /** Scroll so surface point (`sx`, `sy`) sits at viewport point (`vx`, `vy`). */
  const placeSurfacePoint = (sx: number, sy: number, vx: number, vy: number) => {
    scrollToX(surfaceOffsetX() + sx - vx);
    scrollToY(surfaceOffsetY() + sy - vy);
  };

  const centreOnCell = (cell: SpiralCell) => {
    const c = cellCentre(geometry(), cell);
    placeSurfacePoint(c.x, c.y, viewportWidth() / 2, viewportHeight() / 2);
  };

  /** The (fractional) lattice coordinate of a surface point, measured from the
   *  centre cell. The inverse of the position `cellOrigin` computes. */
  const latticeAt = (g: SpiralGeometry, sx: number, sy: number) => ({
    u: (sx - g.gap) / g.pitch - g.rings,
    v: (sy - g.gap) / g.pitch - g.rings,
  });

  // Where the next surface change should keep the canvas still, in viewport
  // coordinates. Ctrl+wheel sets the cursor; everything else (the settings
  // slider, an added or deleted file) leaves it at the viewport centre,
  // because that is the part of the canvas the user is looking at.
  let anchorX: number | null = null;
  let anchorY: number | null = null;

  // Hold the anchored lattice point still whenever the surface's frame
  // changes, and re-derive the windows.
  //
  // Without this a zoom is a teleport. Scroll offsets mean nothing on their
  // own here: a cell's position is `(coordinate + rings) × pitch`, so both the
  // zoom (pitch) and a change in item count (rings) move every cell at once,
  // and a step from 200px to 240px cells throws the viewport a fifth of the
  // way across the spiral. Converting to a lattice coordinate and back is
  // exact for all three inputs, which is why it is written once here rather
  // than in the zoom handler.
  createEffect(on(geometry, (g, prev) => {
    const moved =
      prev !== undefined &&
      (prev.pitch !== g.pitch || prev.rings !== g.rings || prev.gap !== g.gap);
    if (moved) {
      const vx = anchorX ?? viewportWidth() / 2;
      const vy = anchorY ?? viewportHeight() / 2;
      anchorX = null;
      anchorY = null;
      // The scroll offsets still describe the *old* surface: this effect runs
      // after the DOM has taken the new size, but before anything has moved
      // the scroller.
      const { u, v } = latticeAt(prev, scrollLeft() - surfaceOffsetX() + vx, scrollTop() - surfaceOffsetY() + vy);
      placeSurfacePoint((u + g.rings) * g.pitch + g.gap, (v + g.rings) * g.pitch + g.gap, vx, vy);
    }
    // Undefined until onMount; the first geometry never needs it, because
    // onMount recalculates as soon as it has centred the view.
    recalcRange?.();
  }));

  const zoomBy = (steps: number, atX: number, atY: number) => {
    const cur = settings().display.thumbnail_size;
    const step = Math.max(8, Math.round(cur * 0.12));
    const lo = settings().display.thumb_size_min ?? CELL_SIZE_MIN;
    const hi = settings().display.thumb_size_max ?? CELL_SIZE_MAX;
    const next = Math.max(lo, Math.min(hi, cur + step * steps));
    if (next === cur) return;
    anchorX = atX;
    anchorY = atY;
    setSettings((prev) => ({ ...prev, display: { ...prev.display, thumbnail_size: next } }));
  };

  onMount(() => {
    recalcRange = () => {
      const g = geometry();
      if (props.paths.length === 0) return;
      const vw = viewportWidth();
      const vh = viewportHeight();
      const left = scrollLeft() - surfaceOffsetX();
      const top = scrollTop() - surfaceOffsetY();
      const view = cellsInRect(g, left, top, left + vw, top + vh);

      // Measured against cell *centres*, so the distance a drain ranks by is
      // symmetric about the middle of the screen.
      const centre = latticeAt(g, left + vw / 2 - g.cellSize / 2, top + vh / 2 - g.cellSize / 2);
      centreCx = centre.u;
      centreCy = centre.v;

      const ahead = dynamics.bufferAheadRows(BUFFER_AHEAD, BUFFER_AHEAD_MAX);
      const right = dynamics.directionX() === 1;
      const down = dynamics.direction() === 1;
      const render = grow(
        view,
        right ? BUFFER_BEHIND : ahead,
        down ? BUFFER_BEHIND : ahead,
        right ? ahead : BUFFER_BEHIND,
        down ? ahead : BUFFER_BEHIND,
      );
      const full = grow(
        view,
        right ? FULL_BEHIND : FULL_AHEAD,
        down ? FULL_BEHIND : FULL_AHEAD,
        right ? FULL_AHEAD : FULL_BEHIND,
        down ? FULL_AHEAD : FULL_BEHIND,
      );

      // The signals compare structurally, so a pan that stays within one cell
      // updates nothing downstream.
      batch(() => {
        setViewRange(view);
        setRenderRange(render);
        setFullRange(full);
      });
    };

    // Open in the middle: item 0 is the top of the current sort, and the
    // spiral is built around it.
    //
    // Deferred until there is both a scroll host and something to show, and
    // both really do arrive late. Solid creates a child before appending it to
    // its parent, so this `onMount` can run before `App` has registered the
    // host — centring then would measure the window and scroll nothing. And
    // the view is mounted while the gallery is still loading, where the
    // surface is one cell wide and "the middle" is where the scroller already
    // is; without waiting for the items, the canvas would open on the far
    // corner of a spiral that appeared underneath it.
    let centred = false;
    createEffect(on([scrollHost, () => props.paths.length], ([host, count]) => {
      if (centred || !host || count === 0) return;
      centred = true;
      centreOnCell({ cx: 0, cy: 0 });
      recalcRange?.();
    }));
    recalcRange();

    // Wheel pans (both axes — see lib/wheelScroll.ts); Ctrl/Cmd+wheel zooms
    // about the cursor. During a Ctrl+drag selection, fall through to a pan so
    // the drag can be extended.
    const detachWheel = createWheelScroll({
      rowHeight: () => geometry().pitch,
      onSettle: () => loop.schedule(),
      onZoom: (e) => {
        if (isDragging()) return false;
        zoomBy(e.deltaY < 0 ? 1 : -1, e.clientX, e.clientY);
        return true;
      },
    }).attach();

    const onScrollEnd = () => {
      dynamics.markSettled();
      loop.schedule();
    };
    const detachScrollEnd = onScrollHost("scrollend", onScrollEnd);

    createEffect(on(generation, () => { loop.schedule(); }));
    // `scrollend` is not universal on mobile; scrollDynamics also flips
    // `settled` true via its own fling debounce, and that path needs a fetch
    // trigger too or a flick leaves cells blank until the 500 ms poll.
    createEffect(on(dynamics.settled, (s) => { if (s) loop.schedule(); }, { defer: true }));

    const onResize = () => recalcRange?.();
    window.addEventListener("resize", onResize);

    const onInvalidate = () => resetStreaming();
    window.addEventListener("lightview:thumbnails-invalidated", onInvalidate);

    // The viewer announces which item it is showing (null on close). Hold that
    // cell at the full rung: it is what the close transition flies back onto,
    // and a cell the ladder had parked on the cheap tier is exactly the
    // "closing drops back to a blurry thumbnail" pop.
    const onViewerPath = (e: Event) => {
      const viewed = (e as CustomEvent<string | null>).detail;
      setPinnedPath(viewed ?? null);
      if (!viewed || cells.rungOf(viewed) !== "cheap") return;
      cells.setRung(viewed, "full");
      cells.assign(viewed, thumbSrcFor(viewed));
      loop.schedule();
    };
    window.addEventListener(VIEWER_PATH_EVENT, onViewerPath);

    // Keep the viewed image on screen (arrow-key nav from the viewer). Unlike
    // a scroller, "off screen" here can be in any direction, so the cell is
    // simply centred rather than nudged to the nearer edge.
    const onScrollToIndex = (e: Event) => {
      const index = (e as CustomEvent).detail as number;
      if (index < 0 || index >= props.paths.length) return;
      const cell = cellOfIndex(index);
      if (!inRange(viewRange(), cell.cx, cell.cy)) centreOnCell(cell);
    };
    window.addEventListener("lightview:scroll-to-index", onScrollToIndex);

    onCleanup(() => {
      detachScrollEnd();
      detachWheel();
      window.removeEventListener("resize", onResize);
      window.removeEventListener("lightview:thumbnails-invalidated", onInvalidate);
      window.removeEventListener("lightview:scroll-to-index", onScrollToIndex);
      window.removeEventListener(VIEWER_PATH_EVENT, onViewerPath);
    });
  });

  // Drag the background to pan, mouse only. Touch already pans the scroller
  // natively and a trackpad scrolls in both axes, but a mouse has one wheel —
  // without this, half the surface is only reachable by holding shift.
  // Pointer events on cells are left alone: a drag that starts on a thumbnail
  // is a selection gesture (Ctrl/Cmd) or a click.
  const [panning, setPanning] = createSignal(false);
  let panPointerId = -1;
  let panFromX = 0;
  let panFromY = 0;
  let panScrollX = 0;
  let panScrollY = 0;
  let panMoved = 0;
  // A pan releases with a click, and that click must not clear the selection.
  // Set on release rather than tested from `panMoved` at click time, so a
  // later click that started nowhere near a pan is unaffected.
  let suppressPanClick = false;

  const onPanStart = (e: PointerEvent) => {
    if (e.pointerType !== "mouse" || e.button !== 0) return;
    if (e.ctrlKey || e.metaKey || e.shiftKey) return;
    if ((e.target as HTMLElement).closest(".thumb-cell")) return;
    panPointerId = e.pointerId;
    panFromX = e.clientX;
    panFromY = e.clientY;
    panScrollX = scrollLeft();
    panScrollY = scrollTop();
    panMoved = 0;
    setPanning(true);
    containerRef?.setPointerCapture(e.pointerId);
  };

  const onPanMove = (e: PointerEvent) => {
    if (!panning() || e.pointerId !== panPointerId) return;
    const dx = e.clientX - panFromX;
    const dy = e.clientY - panFromY;
    panMoved = Math.max(panMoved, Math.hypot(dx, dy));
    scrollToX(panScrollX - dx);
    scrollToY(panScrollY - dy);
  };

  const onPanEnd = (e: PointerEvent) => {
    if (e.pointerId !== panPointerId) return;
    if (panMoved > PAN_SLOP_PX) suppressPanClick = true;
    setPanning(false);
    panPointerId = -1;
    try { containerRef?.releasePointerCapture(e.pointerId); } catch { /* already gone */ }
  };

  // Prune per-path streaming state when the path list changes. Surgical, not a
  // wholesale wipe: the assign effect has already run for this update and will
  // not fire again until the windows move, so wiping surviving cells would
  // blank the canvas until the next pan (e.g. after deleting one image).
  createEffect(on(() => props.paths, (paths) => {
    pathIndex.reindex(paths);
    // Unlike an eviction, these paths are not coming back — an evicted cell
    // keeps its version so a pan back reuses the same URL.
    for (const p of cells.prune(pathIndex.has)) versions.forget(p);
    pathIndex.pruneAbsent(precached);
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

  return (
    <>
      {/* Outside the surface, not inside it: `contain: strict` makes that
          element a containing block for fixed descendants, so a message
          nested in it would be centred on the spiral rather than on the
          screen — and at nought items the spiral is one cell wide. */}
      <Show when={props.loading || props.paths.length === 0}>
        <div class="fixed inset-0 flex items-center justify-center text-neutral-500 text-sm">
          {props.loading ? "Loading..." : "No media files found"}
        </div>
      </Show>

      <div
        ref={containerRef}
        style={{
          position: "relative",
          width: `${geometry().extent}px`,
          height: `${geometry().extent}px`,
          cursor: panning() ? "grabbing" : "grab",
          "user-select": isDragging() || panning() ? "none" : undefined,
          contain: "strict",
        }}
        onClick={(e) => {
          if (suppressPanClick) {
            suppressPanClick = false;
            return;
          }
          handleBackgroundClick(e);
        }}
        onPointerDown={onPanStart}
        onPointerMove={onPanMove}
        onPointerUp={onPanEnd}
        onPointerCancel={onPanEnd}
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
                      // A full-rung cell has painted — release its hold on the
                      // look-ahead.
                      if (cells.rungOf(path) === "full") clearAwaiting(path);
                    }}
                  />
                </div>
              </Show>
            );
          }}
        </For>
      </div>
    </>
  );
}
