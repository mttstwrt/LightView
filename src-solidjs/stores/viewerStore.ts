import { createSignal } from "solid-js";

const [viewerOpen, setViewerOpen] = createSignal(false);
const [viewerIndex, setViewerIndex] = createSignal(0);
const [infoPanelOpen, setInfoPanelOpen] = createSignal(false);

export {
  viewerOpen, setViewerOpen,
  viewerIndex, setViewerIndex,
  infoPanelOpen, setInfoPanelOpen,
};

export function openViewer(index: number) {
  setViewerIndex(index);
  setViewerOpen(true);
}

export function closeViewer() {
  setViewerOpen(false);
  setInfoPanelOpen(false);
}

export function nextImage(totalCount: number) {
  setViewerIndex((prev) => Math.min(prev + 1, totalCount - 1));
}

export function prevImage() {
  setViewerIndex((prev) => Math.max(prev - 1, 0));
}

export function toggleInfoPanel() {
  setInfoPanelOpen((prev) => !prev);
}
