import { Index, Show, createSignal, createEffect, on, onMount, onCleanup, batch, untrack } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { settings } from "../../stores/settingsStore";
import { getThumbnailsBatch, precacheThumbnails, thumbUrl, type ThumbnailResult } from "../../lib/ipc";
import { initGPU } from "../../lib/gpu";
import { ThumbnailCell } from "./ThumbnailCell";

interface GalleryGridProps {
  paths: string[];
  onItemClick: (index: number) => void;
  onItemSelect: (path: string) => void;
  onDragSelect?: (paths: string[]) => void;
  selectedPaths: Set<string>;
  onItemContextMenu?: (e: MouseEvent, path: string, index: number) => void;
  loading: boolean;
}

// Rows of buffer above and below the viewport to pre-render.
const BUFFER_ROWS = 3;

// How many paths to send per IPC batch call.
const BATCH_SIZE = 30;

// Velocity thresholds (px/s)
const VELOCITY_SLOW = 500;
const VELOCITY_FAST = 3000;

// Jump detection: scroll delta > 2x viewport height triggers generation bump.
const JUMP_FACTOR = 2;

// How often (ms) to schedule thumbnail fetches while scrolling.
const FETCH_INTERVAL_MS = 80;

// How many paths to send per background precache call.
const BG_BATCH_SIZE = 15;

// Maximum number of retry attempts for failed thumbnails.
const MAX_RETRIES = 3;

// Delay (ms) before retrying failed thumbnails.
const RETRY_DELAY_MS = 2000;

// Rows beyond the visible+buffer range before thumbnails are evicted from memory.
const EVICT_ROWS = 15;

// How long (ms) to wait after fast scroll ends before resuming fetches.
const SETTLE_MS = 120;

export function GalleryGrid(props: GalleryGridProps) {
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
    // Only left mouse button
    if (e.button !== 0) return;
    e.preventDefault(); // Prevent text selection during drag
    dragBaseSelection = e.ctrlKey || e.metaKey ? new Set(props.selectedPaths) : new Set<string>();
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
    } else {
      props.onItemClick(item.index);
    }
  };

  // Global mouseup to end drag
  onMount(() => {
    const onMouseUp = () => {
      if (!isDragging()) return;
      const dragged = dragSelectedPaths();
      const wasMultiDrag = dragStartIndex() !== dragCurrentIndex() || dragBaseSelection.size > 0;
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
  /** Paths pre-generated on backend via background precache IPC. */
  const precachedSet = new Set<string>();
  /** Paths where the protocol handler returned 404 — need IPC generation. */
  const generationQueue = new Set<string>();
  /** Paths currently in-flight for IPC generation (prevents duplicates). */
  const generatingSet = new Set<string>();
  /** Cache-bust version per path — incremented after IPC generation. */
  const urlVersions = new Map<string, number>();
  /** Reverse index: path → position in props.paths for O(1) eviction lookups. */
  const pathToIndex = new Map<string, number>();

  const retryCount = new Map<string, number>();
  /** Paths awaiting retry, mapped to the earliest timestamp they can be retried. */
  const pendingRetries = new Map<string, number>();
  let bgCursor = 0;                           // Background scan position
  let recalcRange: (() => void) | undefined;

  /** Called by ThumbnailCell when the protocol handler returns 404. */
  const handleThumbError = (path: string) => {
    if (!generatingSet.has(path)) {
      generationQueue.add(path);
    }
  };

  // Initialize WebGPU on mount (non-blocking, caches result).
  onMount(() => {
    initGPU();
  });

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
    const updates: [string, string][] = [];
    for (const item of items) {
      if (!assignedSet.has(item.path)) {
        assignedSet.add(item.path);
        const v = urlVersions.get(item.path);
        const url = v ? `${thumbUrl(item.path)}?v=${v}` : thumbUrl(item.path);
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
    let lastFastScrollTime = 0;

    /** Recompute the visible row range from raw scroll state and update signals if changed. */
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

        // Track fast scroll time for settle detection
        if (currentVelocity > VELOCITY_FAST) {
          lastFastScrollTime = now;
        }

        // Jump detection
        if (dy > window.innerHeight * JUMP_FACTOR) {
          setGeneration((g) => g + 1);
          assignedSet.clear();
          precachedSet.clear();
          generationQueue.clear();
          generatingSet.clear();
          bgCursor = 0;
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
        return;
      }
      const step = wheelAccumulator * (1 - WHEEL_DECAY);
      wheelAccumulator -= step;
      window.scrollBy(0, step);
      wheelRafId = requestAnimationFrame(drainWheel);
    };

    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      wheelAccumulator += e.deltaY;
      if (!wheelRafId) {
        wheelRafId = requestAnimationFrame(drainWheel);
      }
    };
    window.addEventListener("wheel", onWheel, { passive: false });

    recalcRange(); // initial

    // -----------------------------------------------------------------------
    // Thumbnail fetch loop — generation, background precache, and eviction
    // -----------------------------------------------------------------------

    let fetchAbort = false;
    let inFlightFetch: Promise<void> | null = null;

    /** Mark a path as failed, scheduling it for retry if under the limit. */
    const markFailed = (p: string) => {
      const attempts = (retryCount.get(p) ?? 0) + 1;
      retryCount.set(p, attempts);
      if (attempts < MAX_RETRIES) {
        generatingSet.delete(p);
        generationQueue.delete(p);
        pendingRetries.set(p, Date.now() + RETRY_DELAY_MS);
      }
    };

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

      // Decay velocity to zero when no scroll events arrive.
      // lastTimestamp is updated on every scroll RAF — if it's stale, user stopped.
      const now = performance.now();
      if (now - lastTimestamp > 150) {
        currentVelocity = 0;
      }

      // Settle detection: wait briefly after fast scrolling ends before fetching.
      // lastFastScrollTime is ONLY set in the scroll handler, not here.
      if (currentVelocity > VELOCITY_FAST) {
        return;
      }
      if (now - lastFastScrollTime < SETTLE_MS) {
        return;
      }

      const gen = generation();

      // Phase 1: Process generation queue (paths that 404'd on optimistic load)
      if (generationQueue.size > 0) {
        const toGenerate: string[] = [];
        for (const p of generationQueue) {
          if (toGenerate.length >= BATCH_SIZE) break;
          // Only generate paths still near the viewport
          const idx = pathToIndex.get(p);
          const sr = startRow();
          const er = endRow();
          const c = cols();
          const nearStart = Math.max(0, (sr - EVICT_ROWS)) * c;
          const nearEnd = Math.min(props.paths.length, (er + EVICT_ROWS) * c);
          if (idx !== undefined && idx >= nearStart && idx < nearEnd) {
            toGenerate.push(p);
          }
          generationQueue.delete(p);
          generatingSet.add(p);
        }

        if (toGenerate.length > 0) {
          inFlightFetch = (async () => {
            try {
              const results = await getThumbnailsBatch(toGenerate);
              if (fetchAbort || generation() !== gen) return;

              // Bump URL versions for successfully generated paths
              const resultPaths = new Set(results.map((r) => r.path));
              batch(() => {
                for (const r of results) {
                  generatingSet.delete(r.path);
                  const v = (urlVersions.get(r.path) ?? 0) + 1;
                  urlVersions.set(r.path, v);
                  setThumbMap(r.path, `${thumbUrl(r.path)}?v=${v}`);
                }
              });

              for (const p of toGenerate) {
                if (!resultPaths.has(p)) {
                  generatingSet.delete(p);
                  markFailed(p);
                }
              }
            } catch (e) {
              console.error("Thumbnail generation batch failed:", e);
              for (const p of toGenerate) {
                generatingSet.delete(p);
                markFailed(p);
              }
            }
            inFlightFetch = null;
          })();
          return;
        }
      }

      // Phase 2: Background prefetch (only when viewport is satisfied and scroll is slow)
      if (currentVelocity < VELOCITY_SLOW) {
        const bgNeeded: string[] = [];

        // 2a. Drain pending retries that are past their delay
        const now = Date.now();
        for (const [p, retryAt] of pendingRetries) {
          if (bgNeeded.length >= BG_BATCH_SIZE) break;
          if (now < retryAt) continue;
          if (retryCount.get(p)! >= MAX_RETRIES) {
            pendingRetries.delete(p);
            continue;
          }
          if (!precachedSet.has(p)) {
            precachedSet.add(p);
            bgNeeded.push(p);
          }
          pendingRetries.delete(p);
        }

        // 2b. Scan forward from bgCursor for uncached paths
        const total = props.paths.length;
        let scanned = 0;
        while (bgNeeded.length < BG_BATCH_SIZE && scanned < total) {
          if (bgCursor >= total) bgCursor = 0;
          const p = props.paths[bgCursor];
          bgCursor++;
          scanned++;
          if (p && !assignedSet.has(p) && !precachedSet.has(p)) {
            precachedSet.add(p);
            bgNeeded.push(p);
          }
        }

        if (bgNeeded.length > 0) {
          inFlightFetch = (async () => {
            try {
              const result = await precacheThumbnails(bgNeeded);
              if (fetchAbort || generation() !== gen) return;
              for (const failedPath of result.failed) {
                markFailed(failedPath);
              }
            } catch (e) {
              console.error("Background precache failed:", e);
              for (const p of bgNeeded) markFailed(p);
            }
            inFlightFetch = null;
          })();
          return;
        }
      }

      // Phase 3: Eviction — run when no other work to do
      evictFaraway();
    };

    const fetchTimerId = setInterval(scheduleFetch, FETCH_INTERVAL_MS);
    // Also fire immediately on generation change (jump/path change)
    createEffect(
      on(generation, () => {
        scheduleFetch();
      }),
    );

    // Listen for thumbnail invalidation (e.g. after rebuild or settings change)
    const onInvalidate = () => {
      setThumbMap(reconcile({}));
      assignedSet.clear();
      precachedSet.clear();
      generationQueue.clear();
      generatingSet.clear();
      urlVersions.clear();
      retryCount.clear();
      pendingRetries.clear();
      bgCursor = 0;
      setGeneration((g) => g + 1);
    };
    window.addEventListener("lightview:thumbnails-invalidated", onInvalidate);

    onCleanup(() => {
      ro.disconnect();
      window.removeEventListener("scroll", onScroll);
      window.removeEventListener("wheel", onWheel);
      window.removeEventListener("lightview:thumbnails-invalidated", onInvalidate);
      if (rafId) cancelAnimationFrame(rafId);
      if (wheelRafId) cancelAnimationFrame(wheelRafId);
      clearInterval(fetchTimerId);
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
        precachedSet.clear();
        generationQueue.clear();
        generatingSet.clear();
        urlVersions.clear();
        retryCount.clear();
        pendingRetries.clear();
        bgCursor = 0;

        // Rebuild reverse index for O(1) eviction lookups
        pathToIndex.clear();
        for (let i = 0; i < paths.length; i++) {
          pathToIndex.set(paths[i], i);
        }

        setGeneration((g) => g + 1);
        // Recompute visible range now that paths exist.
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
