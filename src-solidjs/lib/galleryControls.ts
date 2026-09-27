// The grid's pointer interaction: drag-select, reordering a set, click
// handling, and edge-scroll while dragging. These are state machines that do
// not depend on how cells are placed, so they live apart from the layout and
// streaming logic, which stays in the component.

import { createSignal, createEffect, onMount, onCleanup, type Accessor } from "solid-js";
import { scrollToY, scrollTop, viewportHeight } from "./scrollHost";
import { moveBlock, type Gap } from "./reorder";

// -------------------------------------------------------------------------
// Selection: Ctrl/Cmd-drag range select + click-to-open / click-to-toggle.
// -------------------------------------------------------------------------

/** The selection-related props the grid accepts, in one shape. */
export interface SelectionControlProps {
  paths: string[];
  selectedPaths: Set<string>;
  /** Explicit multi-select mode (the mobile Select button). While on, a plain
   *  click/tap toggles the cell instead of opening the viewer — the touch
   *  stand-in for holding Ctrl/Cmd. */
  selectionMode?: boolean;
  onItemClick: (index: number) => void;
  onItemSelect: (path: string) => void;
  onDragSelect?: (paths: string[]) => void;
  onBackgroundClick?: () => void;
}

export interface DragSelectControls {
  /** True while a Ctrl/Cmd-drag is sweeping a range. */
  isDragging: () => boolean;
  /** Selection to display *during* a drag (base selection + swept range). */
  effectiveSelected: () => Set<string>;
  /** Cell `onMouseDown` — begins a drag when Ctrl/Cmd + left button. */
  handleDragStart: (index: number, e: MouseEvent) => void;
  /** Cell `onMouseEnter` — extends the active drag to `index`. */
  handleDragEnter: (index: number) => void;
  /** Cell `onClick` — toggles (Ctrl), clears (has selection), or opens. */
  handleItemClick: (item: { path: string; index: number }, e: MouseEvent) => void;
  /** Container `onClick` — clears selection when the bare background is hit. */
  handleBackgroundClick: (e: MouseEvent) => void;
}

/** Selection by pointer: Ctrl/Cmd-drag selects a range (added to the existing
 *  selection), a click toggles, clears or opens, and the click that ends a drag
 *  is swallowed. */
export function createDragSelect(props: SelectionControlProps): DragSelectControls {
  const [isDragging, setIsDragging] = createSignal(false);
  const [dragStartIndex, setDragStartIndex] = createSignal(-1);
  const [dragCurrentIndex, setDragCurrentIndex] = createSignal(-1);
  // Snapshot of the selection when the drag began (for additive Ctrl+drag).
  let dragBaseSelection = new Set<string>();
  // Suppress the click that fires right after a multi-item drag completes.
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
    // Only left button, only drag-select with Ctrl/Cmd held.
    if (e.button !== 0) return;
    if (!(e.ctrlKey || e.metaKey)) return;
    e.preventDefault(); // prevent text selection during drag
    dragBaseSelection = new Set(props.selectedPaths);
    setIsDragging(true);
    setDragStartIndex(index);
    setDragCurrentIndex(index);
  };

  const handleDragEnter = (index: number) => {
    if (!isDragging()) return;
    setDragCurrentIndex(index);
  };

  // Effective selection shown during a drag = base selection + the swept range.
  const effectiveSelected = () => {
    if (!isDragging()) return props.selectedPaths;
    const merged = new Set(dragBaseSelection);
    for (const p of dragSelectedPaths()) merged.add(p);
    return merged;
  };

  onMount(() => {
    const onMouseUp = () => {
      if (!isDragging()) return;
      const dragged = dragSelectedPaths();
      const wasMultiDrag = dragStartIndex() !== dragCurrentIndex();
      setIsDragging(false);

      if (!wasMultiDrag) {
        // No real drag — let onClick handle the single-item case.
        setDragStartIndex(-1);
        setDragCurrentIndex(-1);
        return;
      }

      // Swallow the click that fires right after mouseup.
      suppressClick = true;

      const merged = new Set(dragBaseSelection);
      for (const p of dragged) merged.add(p);
      props.onDragSelect?.([...merged]);

      setDragStartIndex(-1);
      setDragCurrentIndex(-1);
    };
    window.addEventListener("mouseup", onMouseUp);
    onCleanup(() => window.removeEventListener("mouseup", onMouseUp));
  });

  const handleItemClick = (item: { path: string; index: number }, e: MouseEvent) => {
    if (suppressClick) {
      suppressClick = false;
      return;
    }
    if (e.ctrlKey || e.metaKey || props.selectionMode) {
      props.onItemSelect(item.path);
    } else if (props.selectedPaths.size > 0) {
      // Clear selection first — don't open the viewer until selection is gone.
      props.onBackgroundClick?.();
    } else {
      props.onItemClick(item.index);
    }
  };

  const handleBackgroundClick = (e: MouseEvent) => {
    // In explicit selection mode the gaps between cells are wide targets for a
    // stray thumb — dropping the whole selection there would be a nasty
    // surprise, so only the Done/Clear button leaves.
    if (props.selectionMode) return;
    // Only fire when the bare background is clicked, not a thumbnail.
    const target = e.target as HTMLElement;
    if (!target.closest(".thumb-cell") && !e.ctrlKey && !e.metaKey) {
      props.onBackgroundClick?.();
    }
  };

  return { isDragging, effectiveSelected, handleDragStart, handleDragEnter, handleItemClick, handleBackgroundClick };
}

// -------------------------------------------------------------------------
// Reordering a set: plain mouse-drag a cell to a gap.
// -------------------------------------------------------------------------

/** How far the mouse must travel with the button down before a press is a
 *  drag rather than a click — enough to absorb the jitter of a firm click, so
 *  opening the viewer is never a reorder. */
const REORDER_SLOP_PX = 6;

export interface ReorderProps {
  /** Whether the grid is a set view, where a drag reorders. */
  enabled: () => boolean;
  paths: () => readonly string[];
  selectedPaths: () => ReadonlySet<string>;
  /** The gap under a viewport point, in the grid's layout. */
  gapAt: (clientX: number, clientY: number) => Gap | null;
  /** The full new order, after a drop that changed it. */
  onReorder: (paths: string[]) => void;
  /** Called when a press becomes a drag, and once when that drag ends. */
  onLift?: () => void;
  onSettle?: () => void;
}

export interface ReorderControls {
  /** What is being dragged, or null when nothing is. */
  lifted: Accessor<ReadonlySet<string> | null>;
  /** Where the dragged block would land if released now. */
  gap: Accessor<Gap | null>;
  /** Cell `onPointerDown`. */
  handlePointerDown: (path: string, e: PointerEvent) => void;
}

/** Reordering by mouse: press a cell and move past the slop to lift it — with
 *  the rest of the selection, if it is selected — then release over a gap to
 *  put it there. Esc puts it back.
 *
 *  **Mouse only.** On touch a drag is a scroll and a long press is the context
 *  menu, and neither may become a reorder; a touch device reorders from the
 *  sort menu instead. Checked by pointer type rather than by `isMobile()`, so a
 *  laptop with a touchscreen still drags with its mouse.
 *
 *  **Ctrl/Cmd still selects.** A modified press is left to drag-select, which
 *  is why this needs no mode of its own: a plain drag did nothing before. */
export function createReorderDrag(props: ReorderProps): ReorderControls {
  const [lifted, setLifted] = createSignal<ReadonlySet<string> | null>(null);
  const [gap, setGap] = createSignal<Gap | null>(null);
  /** The press in progress. `cancelled` after Esc, so its release does
   *  nothing — not even the click that would open the viewer. */
  let press: { path: string; x: number; y: number; cancelled: boolean } | null = null;
  let pointer = { x: 0, y: 0 };

  const handlePointerDown = (path: string, e: PointerEvent) => {
    if (!props.enabled() || e.pointerType !== "mouse" || e.button !== 0) return;
    if (e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) return;
    press = { path, x: e.clientX, y: e.clientY, cancelled: false };
  };

  const settle = () => {
    setLifted(null);
    setGap(null);
    props.onSettle?.();
  };

  onMount(() => {
    const onMove = (e: PointerEvent) => {
      if (!press || press.cancelled) return;
      pointer = { x: e.clientX, y: e.clientY };
      if (!lifted()) {
        if (Math.hypot(e.clientX - press.x, e.clientY - press.y) < REORDER_SLOP_PX) return;
        const selected = props.selectedPaths();
        setLifted(selected.has(press.path) ? new Set(selected) : new Set([press.path]));
        props.onLift?.();
      }
      setGap(props.gapAt(pointer.x, pointer.y));
    };

    // Edge-scroll moves the content under a still pointer, so the gap is
    // re-read on scroll as well as on movement.
    const onScroll = () => {
      if (lifted()) setGap(props.gapAt(pointer.x, pointer.y));
    };

    const onUp = () => {
      if (!press) return;
      const { cancelled } = press;
      press = null;
      const moving = lifted();
      if (cancelled) swallowNextClick();
      if (!moving) return;
      swallowNextClick();
      const target = gap();
      if (target) {
        const order = props.paths();
        const next = moveBlock(order, moving, target.index);
        // Reorder before settling: the caller takes its own hold on refreshes
        // synchronously, so the grid's is released into it rather than into a
        // refresh that would fetch the old order.
        if (next.some((p, i) => p !== order[i])) props.onReorder(next);
      }
      settle();
    };

    // The browser took the pointer away mid-press: nothing was dropped, so
    // nothing moves — unlike a release, which lands wherever the gap was.
    const onCancel = () => {
      if (!press) return;
      press = null;
      if (lifted()) settle();
    };

    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || !lifted() || !press) return;
      // Ours alone: the same Esc must not also close a panel or the viewer.
      e.stopPropagation();
      press.cancelled = true;
      settle();
    };

    window.addEventListener("pointermove", onMove, { passive: true });
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onCancel);
    window.addEventListener("scroll", onScroll, { capture: true, passive: true });
    window.addEventListener("keydown", onKey, true);
    onCleanup(() => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onCancel);
      window.removeEventListener("scroll", onScroll, { capture: true });
      window.removeEventListener("keydown", onKey, true);
      if (lifted()) props.onSettle?.();
    });
  });

  return { lifted, gap, handlePointerDown };
}

/** Eat the click the browser fires after a drop.
 *
 *  It lands on the nearest common ancestor of where the press and the release
 *  happened — after a drag, usually the grid's background, whose handler clears
 *  the selection — so a flag checked in the cell's own click handler would not
 *  be consumed there and would eat the reader's next real click instead. A
 *  capture listener on the window sees the click first wherever it lands, and
 *  is gone by the next task if none comes, because the click follows the
 *  release within the same one. */
function swallowNextClick() {
  const eat = (e: MouseEvent) => {
    e.stopPropagation();
    e.preventDefault();
  };
  window.addEventListener("click", eat, { capture: true, once: true });
  setTimeout(() => window.removeEventListener("click", eat, { capture: true }), 0);
}

// -------------------------------------------------------------------------
// Edge-scroll while drag-selecting: with the mouse near the top/bottom of the
// viewport during a drag, auto-scroll the window so the selection can extend
// past the visible range without reaching for the wheel. Speed ramps up
// quadratically toward the very edge.
// -------------------------------------------------------------------------

const EDGE_ZONE_PX = 80;
const EDGE_MAX_SPEED = 1400; // px/sec at the very edge

/** Runs an auto-scroll loop while `isDragging()` is true. Self-cleaning. */
export function createEdgeScroll(isDragging: () => boolean) {
  onMount(() => {
    let edgeMouseY = 0;
    let edgeRafId = 0;
    let edgeLastTime = 0;

    const edgeScrollFrame = (now: number) => {
      if (!isDragging()) { edgeRafId = 0; return; }
      const dt = Math.min((now - edgeLastTime) / 1000, 0.05);
      edgeLastTime = now;
      const vh = viewportHeight();
      let speed = 0;
      if (edgeMouseY < EDGE_ZONE_PX) {
        speed = -EDGE_MAX_SPEED * Math.pow(1 - edgeMouseY / EDGE_ZONE_PX, 2);
      } else if (edgeMouseY > vh - EDGE_ZONE_PX) {
        speed = EDGE_MAX_SPEED * Math.pow((edgeMouseY - (vh - EDGE_ZONE_PX)) / EDGE_ZONE_PX, 2);
      }
      if (speed !== 0) {
        scrollToY(scrollTop() + speed * dt);
      }
      edgeRafId = requestAnimationFrame(edgeScrollFrame);
    };

    const onEdgeMouseMove = (e: MouseEvent) => { edgeMouseY = e.clientY; };
    window.addEventListener("mousemove", onEdgeMouseMove, { passive: true });

    createEffect(() => {
      if (isDragging()) {
        if (!edgeRafId) {
          edgeLastTime = performance.now();
          edgeRafId = requestAnimationFrame(edgeScrollFrame);
        }
      } else if (edgeRafId) {
        cancelAnimationFrame(edgeRafId);
        edgeRafId = 0;
      }
    });

    onCleanup(() => {
      window.removeEventListener("mousemove", onEdgeMouseMove);
      if (edgeRafId) cancelAnimationFrame(edgeRafId);
    });
  });
}
