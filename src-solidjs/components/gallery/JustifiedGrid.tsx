import { Index, Show, createSignal, createEffect, createMemo, on, onMount, onCleanup, batch, untrack } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { safeListen as listen, type UnlistenFn } from "../../lib/runtime";
import { settings, setSettings } from "../../stores/settingsStore";
import { durationByPath } from "../../stores/galleryStore";
import { ensureTierThumbnails, thumbUrl } from "../../lib/ipc";
import { thumbGenStarted, thumbGenProgress, thumbGenFinished } from "../../stores/thumbnailProgressStore";
import { recordCacheMiss } from "../../lib/perfMonitor";
import { computeJustifiedLayout, rowIndexAtOffset } from "../../lib/justifiedLayout";
import { ThumbnailCell } from "./ThumbnailCell";

interface JustifiedGridProps {
  paths: string[];
  /** Aspect ratio (w/h) per path; missing entries fall back to 1:1. */
  aspects: Map<string, number>;
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
// How many paths to generate per IPC batch.
const BATCH_SIZE = 96;
// The justified tier is always served at the "j" segment.
const TIER = "j" as const;
// Target-row-height (zoom) bounds, mirroring the grid's thumb-size range.
const ROW_HEIGHT_MIN = 80;
const ROW_HEIGHT_MAX = 600;

export function JustifiedGrid(props: JustifiedGridProps) {
  const targetRowHeight = () => settings().display.thumbnail_size;
  const gap = () => settings().display.grid_gap;

  const [containerWidth, setContainerWidth] = createSignal(0);
  const [startRow, setStartRow] = createSignal(0);
  const [endRow, setEndRow] = createSignal(0);
  const [generation, setGeneration] = createSignal(0);
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

  // Cells within the rendered row range, with absolute positions.
  const visibleCells = () => {
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
  };

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
  let bgCursor = 0;
  let recalcRange: (() => void) | undefined;

  let thumbGenTotal = 0;
  let thumbGenDone = 0;

  const thumbSrcFor = (path: string) => {
    const v = (urlVersions.get(path) ?? 0) + versionEpoch;
    return v > 0 ? `${thumbUrl(path, TIER)}?v=${v}` : thumbUrl(path, TIER);
  };
  const bumpVersion = (path: string) => {
    urlVersions.set(path, (urlVersions.get(path) ?? 0) + 1);
  };

  const handleThumbError = (path: string) => {
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
  createEffect(on(visibleCells, (cells) => {
    const updates: [string, string][] = [];
    for (const cell of cells) {
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
        const next = Math.max(ROW_HEIGHT_MIN, Math.min(ROW_HEIGHT_MAX, cur + (e.deltaY < 0 ? step : -step)));
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
              await ensureTierThumbnails(toGenerate, TIER);
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

      // Background precache of upcoming items when idle.
      if (currentVelocity < 500 && bgCursor < props.paths.length) {
        const bgNeeded: string[] = [];
        while (bgNeeded.length < BATCH_SIZE && bgCursor < props.paths.length) {
          const p = props.paths[bgCursor];
          bgCursor++;
          if (p && !assignedSet.has(p) && !failedSet.has(p)) bgNeeded.push(p);
        }
        if (bgNeeded.length > 0) {
          inFlightFetch = (async () => {
            try { await ensureTierThumbnails(bgNeeded, TIER); }
            catch (e) { console.error("Justified background precache failed:", e); }
            inFlightFetch = null;
          })();
        }
      }
    };

    const onScrollEnd = () => scheduleFetch();
    window.addEventListener("scrollend", onScrollEnd);

    const bgIntervalId = setInterval(() => { if (!inFlightFetch) scheduleFetch(); }, 500);

    createEffect(on(generation, () => { scheduleFetch(); }));

    // Re-run the visible range whenever the layout changes (width / zoom).
    createEffect(on(layout, () => { recalcRange?.(); }));

    const onInvalidate = () => {
      setThumbMap(reconcile({}));
      assignedSet.clear();
      needsGeneration.clear();
      inFlightSet.clear();
      failedSet.clear();
      urlVersions.clear();
      versionEpoch++;
      bgCursor = 0;
      thumbGenTotal = 0;
      thumbGenDone = 0;
      setGeneration((g) => g + 1);
    };
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
          <Index each={visibleCells()}>
            {(cell) => (
              <div
                style={{
                  position: "absolute",
                  top: `${cell().y}px`,
                  left: `${cell().x}px`,
                  width: `${cell().width}px`,
                  height: `${cell().height}px`,
                }}
              >
                <ThumbnailCell
                  path={cell().path}
                  thumbSrc={thumbMap[cell().path] ?? null}
                  tier={TIER}
                  freeSize={true}
                  durationSec={durationByPath().get(cell().path) ?? null}
                  selected={props.selectedPaths.has(cell().path)}
                  onClick={(e: MouseEvent) => handleItemClick(cell(), e)}
                  onContextMenu={(e) => props.onItemContextMenu?.(e, cell().path, cell().index)}
                  onError={handleThumbError}
                />
              </div>
            )}
          </Index>
        </div>
      </Show>
    </div>
  );
}
