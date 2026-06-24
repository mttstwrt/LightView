import { For, Show, createSignal, createEffect, createMemo, on, onMount, onCleanup, batch, untrack } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { safeListen as listen, type UnlistenFn } from "../../lib/runtime";
import { settings, setSettings } from "../../stores/settingsStore";
import { durationByPath } from "../../stores/galleryStore";
import { ensureTierThumbnails, thumbUrl, mediaUrl, type ThumbTier } from "../../lib/ipc";
import type { MediaMeta } from "../../stores/galleryStore";
import { thumbGenStarted, thumbGenProgress, thumbGenFinished } from "../../stores/thumbnailProgressStore";
import { recordCacheMiss } from "../../lib/perfMonitor";
import { computeJustifiedLayout, rowIndexAtOffset } from "../../lib/justifiedLayout";
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
// Target-row-height (zoom) bounds, mirroring the grid's thumb-size range.
const ROW_HEIGHT_MIN = 80;
const ROW_HEIGHT_MAX = 600;

type DetailLevel = "base" | "mid" | "high";

export function JustifiedGrid(props: JustifiedGridProps) {
  const targetRowHeight = () => settings().display.thumbnail_size;
  const gap = () => settings().display.grid_gap;

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
  // an original, and for the GIF atlas request).
  const thumbTier = (): ThumbTier => (detailLevel() === "base" ? "j" : "jh");

  const extOf = (p: string) => {
    const i = p.lastIndexOf(".");
    return i >= 0 ? p.slice(i + 1).toLowerCase() : "";
  };

  // Whether `path` should be served as its original file at the current level:
  // a native-format still image that is both small enough to stream cheaply and
  // not so high-resolution that the webview's full-res decode stalls the main
  // thread. Otherwise we fall back to the pre-resized "jh" tier.
  const servesOriginal = (path: string): boolean => {
    const lvl = detailLevel();
    if (lvl === "base") return false;
    const meta = props.itemMeta?.get(path);
    if (!meta || meta.media_type !== "image") return false;
    if (!NATIVE_IMG_EXTS.has(extOf(path))) return false;
    // Transfer-cost ceiling: never stream a very large file as the original.
    if (meta.size > (lvl === "high" ? HIGH_MAX_BYTES : MID_MAX_BYTES)) return false;
    // Decode-cost gate: the webview CPU-decodes the original at *full*
    // resolution (then downscales), so a source far larger than the cell shows
    // is a big main-thread stall — file bytes alone don't capture that (a
    // small-but-50MP JPEG is cheap to transfer, ruinous to decode). Only serve
    // the original when its longest edge is within tolerance of the displayed
    // size; otherwise the jh tier (pre-resized off the UI thread) is faster and
    // visually equivalent. Unknown dimensions fall back to the byte ceiling.
    if (meta.width != null && meta.height != null) {
      const dpr = typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1;
      const aspect = props.aspects.get(path) ?? 1;
      // Cells are row-height tall; landscape cells are wider, so the displayed
      // longest edge is rowHeight × max(1, aspect).
      const physicalLongEdge = targetRowHeight() * Math.max(1, aspect) * dpr;
      const srcLongEdge = Math.max(meta.width, meta.height);
      if (srcLongEdge > physicalLongEdge * ORIGINAL_SRC_TOLERANCE) return false;
    }
    return true;
  };

  const [containerWidth, setContainerWidth] = createSignal(0);
  const [startRow, setStartRow] = createSignal(0);
  const [endRow, setEndRow] = createSignal(0);
  const [generation, setGeneration] = createSignal(0);
  // True while scrolling fast enough that we defer assigning new <img> srcs;
  // flips back to false when scrolling settles, re-running the assignment.
  const [fastScroll, setFastScroll] = createSignal(false);
  const [thumbMap, setThumbMap] = createStore<Record<string, string>>({});

  let containerRef: HTMLDivElement | undefined;

  // -----------------------------------------------------------------------
  // Layout
  // -----------------------------------------------------------------------

  // Aspect ratios aligned to props.paths (fallback 1:1 for un-indexed items).
  const aspectArray = createMemo(() => {
    const map = props.aspects;
    return props.paths.map((p) => map.get(p) ?? 1);
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

  // Clear all streaming state and force a re-stream at the current tier. Used
  // both for explicit invalidation and when the effective tier changes (zoom).
  const resetStreaming = () => {
    setThumbMap(reconcile({}));
    assignedSet.clear();
    needsGeneration.clear();
    inFlightSet.clear();
    failedSet.clear();
    urlVersions.clear();
    jhPrecached.clear();
    versionEpoch++;
    bgCursor = 0;
    thumbGenTotal = 0;
    thumbGenDone = 0;
    setGeneration((g) => g + 1);
  };

  // Re-stream when the detail level changes so visible cells refetch at the new
  // resolution / source. Deferred so it doesn't fire on initial mount.
  createEffect(on(detailLevel, () => resetStreaming(), { defer: true }));

  const thumbSrcFor = (path: string) => {
    // Originals are served straight from the media server — no cache-buster
    // (they never regenerate) and no thumbnail tier.
    if (servesOriginal(path)) return mediaUrl(path);
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
  let unlistenRegenerated: UnlistenFn | undefined;
  onMount(() => {
    listen<{ path: string }>("thumb:regenerated", (event) => {
      const p = event.payload.path;
      bumpVersion(p);
      if (assignedSet.has(p)) setThumbMap(p, thumbSrcFor(p));
    }).then((fn) => { unlistenRegenerated = fn; });
  });
  onCleanup(() => unlistenRegenerated?.());

  // Assign protocol URLs as soon as cells become visible. Cached "j" thumbs
  // load instantly; uncached ones 404 → onError → queued for generation.
  createEffect(on([visibleCells, fastScroll], ([cells, fast]) => {
    // While flinging, leave new cells on their placeholder so the main thread
    // isn't buried under image decodes mid-scroll. When scrolling settles
    // (fast → false) this re-runs and assigns whatever's now on screen.
    if (fast) return;
    const updates: [string, string][] = [];
    for (const cell of cells as ReturnType<typeof visibleCells>) {
      if (!assignedSet.has(cell.path)) {
        assignedSet.add(cell.path);
        updates.push([cell.path, thumbSrcFor(cell.path)]);
      }
    }
    if (updates.length > 0) {
      batch(() => {
        for (const [path, url] of updates) setThumbMap(path, url);
      });
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
      if (e.ctrlKey || e.metaKey) {
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
    bgCursor = 0;
    thumbGenTotal = 0;
    thumbGenDone = 0;
    pathToIndex.clear();
    for (let i = 0; i < paths.length; i++) pathToIndex.set(paths[i], i);
    setGeneration((g) => g + 1);
    recalcRange?.();
  }));

  createEffect(on(totalHeight, (h) => { props.onContentHeight?.(h); }));

  // -----------------------------------------------------------------------
  // Click handling
  // -----------------------------------------------------------------------

  const handleItemClick = (item: { path: string; index: number }, e: MouseEvent) => {
    if (e.ctrlKey || e.metaKey) {
      props.onItemSelect(item.path);
    } else if (props.selectedPaths.size > 0) {
      props.onBackgroundClick?.();
    } else {
      props.onItemClick(item.index);
    }
  };

  const handleBackgroundClick = (e: MouseEvent) => {
    const target = e.target as HTMLElement;
    if (!target.closest(".thumb-cell") && !e.ctrlKey && !e.metaKey) {
      props.onBackgroundClick?.();
    }
  };

  return (
    <div ref={containerRef} class="w-full" onClick={handleBackgroundClick}>
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
                      selected={props.selectedPaths.has(path)}
                      onClick={(e: MouseEvent) => handleItemClick({ path, index: index() }, e)}
                      onContextMenu={(e) => props.onItemContextMenu?.(e, path, index())}
                      onError={handleThumbError}
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
