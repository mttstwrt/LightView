import { createSignal } from "solid-js";
import { recordView } from "../lib/ipc";
import { displayPaths } from "./galleryStore";

const [viewerOpen, setViewerOpen] = createSignal(false);
const [viewerIndex, setViewerIndex] = createSignal(0);
const [infoPanelOpen, setInfoPanelOpen] = createSignal(false);

export {
  viewerOpen, setViewerOpen,
  viewerIndex, setViewerIndex,
  infoPanelOpen, setInfoPanelOpen,
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
