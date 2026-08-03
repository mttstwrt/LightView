// Helpers for the grid ↔ viewer shared-element transition: the tapped
// thumbnail's rect expands into the viewer's fitted media rect on open, and
// the media flies back down into its cell on close. Grid cells advertise
// themselves with `data-vt-path` (ThumbnailCell); the viewer queries the DOM
// at transition time — no store plumbing, and a missing cell (virtualized
// away, map view, filtered out) simply skips the animation.

export interface Rect {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** `CustomEvent<string | null>` the viewer dispatches with the path it is
 *  currently showing (null when it closes). The justified grid uses it to hold
 *  that cell at its full-resolution rung, so the close transition flies back
 *  down onto the same image quality it flew out of. Same DOM-query spirit as
 *  `findCellImage` — no store plumbing between the two components. */
export const VIEWER_PATH_EVENT = "lightview:viewer-path";

/** Asks the open viewer to close *through* its fly-back-to-cell transition
 *  rather than vanishing. Dispatched by close paths that live outside the
 *  component — the app-level Escape handler — which otherwise have no way to
 *  reach `requestClose`. Same DOM-event plumbing as `VIEWER_PATH_EVENT`. */
export const VIEWER_CLOSE_REQUEST_EVENT = "lightview:viewer-close-request";

/** The grid cell's thumbnail <img> for a path, or null when the cell isn't
 *  currently rendered. Cells contain several stacked same-rect <img>s
 *  (underlay, main, hover overlays) — prefer the last one with decoded
 *  bitmap data so the clone starts from real pixels. */
export function findCellImage(path: string): HTMLImageElement | null {
  const cell = document.querySelector(`[data-vt-path="${CSS.escape(path)}"]`);
  if (!cell) return null;
  const imgs = cell.querySelectorAll("img");
  let best: HTMLImageElement | null = null;
  for (const img of imgs) {
    if (img.naturalWidth > 0) best = img;
  }
  return best ?? (imgs.length ? imgs[imgs.length - 1] : null);
}

export function prefersReducedMotion(): boolean {
  return (
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches
  );
}

/** The rect a media item of the given aspect ratio occupies when
 *  contain-fitted and centred in the viewport (the viewer's resting frame). */
export function viewportContainRect(aspect: number): Rect {
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  const va = vw / vh;
  let w: number;
  let h: number;
  if (aspect > va) {
    w = vw;
    h = vw / aspect;
  } else {
    h = vh;
    w = vh * aspect;
  }
  return { left: (vw - w) / 2, top: (vh - h) / 2, width: w, height: h };
}

/** A Web-Animations keyframe pinning an element to a rect. The clone animates
 *  left/top/width/height (not transform): non-uniform scale would distort the
 *  image, whereas a resizing box over `object-fit: cover` smoothly un-crops
 *  a square grid thumb into the full aspect ratio. */
export function rectKeyframe(r: Rect): Keyframe {
  return {
    left: `${r.left}px`,
    top: `${r.top}px`,
    width: `${r.width}px`,
    height: `${r.height}px`,
  };
}
