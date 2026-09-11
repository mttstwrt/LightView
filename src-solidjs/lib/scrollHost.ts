// Which element the gallery scrolls in, and the handful of reads and writes
// everything else needs from it.
//
// The gallery used to scroll the document. It scrolls an element now for one
// reason: iOS draws its own scroll indicator, that indicator is interactive
// (press and hold and you can scrub the whole gallery), and it cannot be styled
// away on the document scroller — `::-webkit-scrollbar` only reaches element
// scrollers, which is why `.hide-scrollbar` works on the app's panels but never
// on the page. Two bars, one of them ours and one we could not hide, was the
// visible problem; the scrub crossing thousands of rows in a second was the
// expensive one. Owning the scroller settles both, and leaves LightView's bar —
// the one with the date markers — as the only one on screen.
//
// Everything routes through here rather than reaching for `window.scrollY`,
// because the host does not exist until the gallery mounts and does not exist
// at all on the welcome screen. With no host registered these fall back to the
// window, so a caller that runs early, or a view that never registers one, sees
// exactly the old behaviour.
//
// The host must be `position: relative` — `recalcRange` in both grids measures
// its content with `offsetTop`, which is relative to the nearest positioned
// ancestor, and that has to be the same origin `scrollTop` counts from.

import { createSignal } from "solid-js";

const [hostEl, setHostEl] = createSignal<HTMLElement | null>(null);

/** Live `onScrollHost` subscriptions, rebound whenever the host changes. */
const subscriptions = new Set<{ rebind: (target: HTMLElement | Window) => void }>();

/**
 * Register (or clear, with null) the element the gallery scrolls in. Reactive:
 * `ScrollBar` reads it to decide which scroller to track.
 *
 * Existing subscriptions are moved across explicitly rather than left to
 * re-resolve on their own. Solid creates a child before appending it to its
 * parent, so a grid's `onMount` can easily run before the host's `ref` has been
 * assigned — a listener bound at that moment would sit on `window` for the rest
 * of the session, watching a scroller that never moves again.
 */
export function setScrollHost(el: HTMLElement | null): void {
  setHostEl(el);
  const target = el ?? window;
  for (const sub of subscriptions) sub.rebind(target);
}

/** The registered scroll host, or null when the window is the scroller. */
export const scrollHost = hostEl;

/** Current scroll offset. */
export function scrollTop(): number {
  const el = hostEl();
  return el ? el.scrollTop : window.scrollY;
}

/** Height of the visible area. */
export function viewportHeight(): number {
  const el = hostEl();
  return el ? el.clientHeight : window.innerHeight;
}

/** Full scrollable height of the content. */
export function scrollHeight(): number {
  const el = hostEl();
  return el ? el.scrollHeight : document.documentElement.scrollHeight;
}

/** Largest valid scroll offset. */
export function maxScroll(): number {
  return Math.max(0, scrollHeight() - viewportHeight());
}

/** Jump to `y`, clamped to the scrollable range. */
export function scrollToY(y: number): void {
  const clamped = Math.max(0, Math.min(y, maxScroll()));
  const el = hostEl();
  if (el) el.scrollTop = clamped;
  else window.scrollTo(0, clamped);
}

/**
 * Subscribe to the host's scroll events, following the host if it changes.
 * Returns an unsubscribe. `scroll` does not bubble, so this has to be bound to
 * whichever object is actually scrolling — the reason a plain
 * `window.addEventListener("scroll")` silently stops working once the gallery
 * owns its scroller.
 */
export function onScrollHost(
  type: "scroll" | "scrollend",
  handler: () => void,
  options?: AddEventListenerOptions,
): () => void {
  let bound: HTMLElement | Window | null = null;
  const rebind = (target: HTMLElement | Window) => {
    if (bound === target) return;
    bound?.removeEventListener(type, handler);
    bound = target;
    bound.addEventListener(type, handler, options);
  };
  const sub = { rebind };
  subscriptions.add(sub);
  rebind(hostEl() ?? window);
  return () => {
    subscriptions.delete(sub);
    bound?.removeEventListener(type, handler);
    bound = null;
  };
}
