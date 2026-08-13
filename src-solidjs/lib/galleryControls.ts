// Shared gallery input controls. GalleryGrid (uniform grid) and JustifiedGrid
// (aspect-preserving rows) render differently but should behave identically
// under the pointer, so the input handling lives here once instead of being
// copy-pasted into each. The bespoke *layout/streaming* logic stays in the
// components; this module only owns the interaction state machines that don't
// depend on how cells are placed.

import { createSignal, createEffect, onMount, onCleanup } from "solid-js";
import { scrollToY, scrollTop, viewportHeight } from "./scrollHost";

// -------------------------------------------------------------------------
// Selection: Ctrl/Cmd-drag range select + click-to-open / click-to-toggle.
// -------------------------------------------------------------------------

/** The selection-related props both grids already accept, in one shape. */
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
