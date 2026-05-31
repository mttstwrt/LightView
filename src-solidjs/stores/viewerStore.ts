import { createSignal } from "solid-js";
import { recordView } from "../lib/ipc";
import { displayPaths } from "./galleryStore";

const [viewerOpen, setViewerOpen] = createSignal(false);
const [viewerIndex, setViewerIndex] = createSignal(0);
const [infoPanelOpen, setInfoPanelOpen] = createSignal(false);
// Measured height (px) of the touch info sheet, reported by InfoPanel via a
// ResizeObserver. The viewer reads this to shrink the photo into the space
// above the sheet (Apple-Photos–style). 0 when no sheet is mounted.
const [infoPanelHeight, setInfoPanelHeight] = createSignal(0);

export {
  viewerOpen, setViewerOpen,
  viewerIndex, setViewerIndex,
  infoPanelOpen, setInfoPanelOpen,
  infoPanelHeight, setInfoPanelHeight,
};

/** Record that the item at the given index was viewed. */
function trackView(index: number) {
  const paths = displayPaths();
  if (index >= 0 && index < paths.length) {
    recordView(paths[index]).catch(() => {});
  }
}

export function openViewer(index: number) {
  setViewerIndex(index);
  setViewerOpen(true);
  trackView(index);
}

export function closeViewer() {
  setViewerOpen(false);
  setInfoPanelOpen(false);
}

export function nextImage(totalCount: number) {
  setViewerIndex((prev) => {
    const next = Math.min(prev + 1, totalCount - 1);
    if (next !== prev) trackView(next);
    return next;
  });
}

export function prevImage() {
  setViewerIndex((prev) => {
    const next = Math.max(prev - 1, 0);
    if (next !== prev) trackView(next);
    return next;
  });
}

export function toggleInfoPanel() {
  setInfoPanelOpen((prev) => !prev);
}
