import { createSignal } from "solid-js";
import { recordView } from "../lib/ipc";
import { displayPaths, setSortedItems } from "./galleryStore";

const [viewerOpen, setViewerOpen] = createSignal(false);
const [viewerIndex, setViewerIndex] = createSignal(0);
const [infoPanelOpen, setInfoPanelOpen] = createSignal(false);

export {
  viewerOpen, setViewerOpen,
  viewerIndex, setViewerIndex,
  infoPanelOpen, setInfoPanelOpen,
};

// ---------------------------------------------------------------------------
// "Recently Viewed" tracking
// ---------------------------------------------------------------------------

/** How long an item must stay on screen before it counts as viewed. Swiping
 *  or arrow-keying past a photo shouldn't stamp it — on a phone a single flick
 *  through a filmstrip crosses a dozen items, and each stamp is an HTTP round
 *  trip on the web client. Only what the user actually stopped on lands in the
 *  "Recently Viewed" sort. */
const VIEW_DWELL_MS = 700;

let dwellTimer: number | null = null;

function cancelDwell() {
  if (dwellTimer !== null) {
    clearTimeout(dwellTimer);
    dwellTimer = null;
  }
}

/** Record that the item at the given index was viewed, once it's been held
 *  for `VIEW_DWELL_MS`. Also stamps `last_viewed` on the in-memory sorted
 *  items so a re-sort within the session sees the same order the backend
 *  would return. */
function trackView(index: number) {
  cancelDwell();
  const paths = displayPaths();
  if (index < 0 || index >= paths.length) return;
  const path = paths[index];
  dwellTimer = window.setTimeout(() => {
    dwellTimer = null;
    recordView(path).catch(() => {});
    const now = Math.floor(Date.now() / 1000);
    setSortedItems((items) =>
      items.map((it) => (it.path === path ? { ...it, last_viewed: now } : it)),
    );
  }, VIEW_DWELL_MS);
}

export function openViewer(index: number) {
  setViewerIndex(index);
  setViewerOpen(true);
  trackView(index);
}

export function closeViewer() {
  setViewerOpen(false);
  setInfoPanelOpen(false);
  cancelDwell();
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
