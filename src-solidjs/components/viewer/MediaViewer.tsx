import { Show, For, createSignal, createEffect, on, onMount, onCleanup, batch } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { isWeb, hasTouch, isTauri, isMobile } from "../../lib/runtime";
import { wheelPxPerUnit } from "../../lib/wheel";
import {
  findCellImage,
  prefersReducedMotion,
  viewportContainRect,
  rectKeyframe,
  VIEWER_PATH_EVENT,
  VIEWER_CLOSE_REQUEST_EVENT,
  type Rect,
} from "../../lib/viewerTransition";
import { aspectByPath, mediaMetaByPath, sortedItems, rateItem } from "../../stores/galleryStore";
import { applyQueryAndRefresh } from "../../stores/filterStore";
import { TitleBar } from "../topbar/TitleBar";
import {
  pointerDistance,
  pointerMidpoint,
  DOUBLE_TAP_MS,
  DOUBLE_TAP_SLOP_PX,
  SYNTHETIC_MOUSE_MS,
  TAP_SLOP_PX,
  AXIS_LOCK_PX,
  SWIPE_NAV_RATIO,
  SWIPE_VELOCITY,
  SWIPE_DISMISS_PX,
  DISMISS_MIN_SCALE,
  LONG_PRESS_MS,
  suppressNextClick,
  type Point,
} from "../../lib/touch";
import { hapticTick } from "../../lib/haptics";
import { infoPanelOpen, setInfoPanelOpen } from "../../stores/viewerStore";
import { settings } from "../../stores/settingsStore";
import { mediaUrl, thumbUrl, ensureTierThumbnails, gifAtlasUrl } from "../../lib/ipc";
import { thumbhashDataUrl } from "../../lib/thumbhashPlaceholder";
import { isVideoPath, PLAYABLE_VIDEO_EXTS } from "../../lib/mediaExts";
import { GifCanvas } from "../GifCanvas";
import { ViewerImageCache } from "../../lib/viewerCache";
import { setViewerCacheCountSource } from "../../lib/perfMonitor";
import { InfoPanel } from "./InfoPanel";
import { VideoPlayer } from "./VideoPlayer";

interface MediaViewerProps {
  paths: string[];
  currentIndex: number;
  onClose: () => void;
  onNext: () => void;
  onPrev: () => void;
  onContextMenu?: (e: MouseEvent, path: string, index: number) => void;
}

export function MediaViewer(props: MediaViewerProps) {
  const [loaded, setLoaded] = createSignal(false);

  const cache = new ViewerImageCache();
  setViewerCacheCountSource(() => cache.size);
  onCleanup(() => {
    setViewerCacheCountSource(null);
    cache.destroy();
    // Release the grid's full-resolution pin on the item we were showing.
    window.dispatchEvent(new CustomEvent<string | null>(VIEWER_PATH_EVENT, { detail: null }));
  });

  // Container ref for direct DOM image swapping
  let imageContainerRef: HTMLDivElement | undefined;
  // The horizontally-translating filmstrip track holding prev/current/next
  // slots. Only `dragX` (swipe-to-navigate) transforms this element.
  let trackRef: HTMLDivElement | undefined;

  // Zoom and pan state
  const [zoom, setZoom] = createSignal(1);
  const [panX, setPanX] = createSignal(0);
  const [panY, setPanY] = createSignal(0);
  const [imgNaturalWidth, setImgNaturalWidth] = createSignal(0);
  let isDragging = false;
  let didDrag = false;
  let dragStartX = 0;
  let dragStartY = 0;
  let panStartX = 0;
  let panStartY = 0;

  // Transient touch-gesture transforms, composed on top of zoom/pan by the
  // transform effect below. `dragScale` multiplies zoom (swipe-down shrink);
  // `dragX`/`dragY` are added to pan (swipe-to-navigate follow + dismiss
  // travel). `backdropAlpha` fades the backdrop during a dismiss drag;
  // `chromeVisible` toggles the overlay UI on tap.
  const [dragX, setDragX] = createSignal(0);
  const [dragY, setDragY] = createSignal(0);
  const [dragScale, setDragScale] = createSignal(1);
  const [backdropAlpha, setBackdropAlpha] = createSignal(1);
  const [chromeVisible, setChromeVisible] = createSignal(true);

  // Frameless desktop window: the custom titlebar lives outside this overlay,
  // so reveal a copy on the top edge here so window controls stay reachable
  // while viewing an image. Mirrors the hover-reveal used in the grid.
  const frameless = () => isTauri() && !isMobile();
  const [titlebarVisible, setTitlebarVisible] = createSignal(false);
  let titlebarHideTimer: number | undefined;
  const revealTitlebar = () => {
    if (titlebarHideTimer) { clearTimeout(titlebarHideTimer); titlebarHideTimer = undefined; }
    setTitlebarVisible(true);
  };
  const hideTitlebar = () => {
    if (titlebarHideTimer) clearTimeout(titlebarHideTimer);
    titlebarHideTimer = window.setTimeout(() => setTitlebarVisible(false), 100);
  };
  onCleanup(() => { if (titlebarHideTimer) clearTimeout(titlebarHideTimer); });
  // True while a horizontal swipe (or its commit animation) is in flight. Gates
  // the prev/next filmstrip slides so the (pressure-managed) full-res neighbour
  // images are only mounted in the DOM during navigation, not while idle.
  const [swiping, setSwiping] = createSignal(false);

  const currentPath = () => props.paths[props.currentIndex] || "";

  const filename = () => {
    const parts = currentPath().split("/");
    return parts[parts.length - 1] || "";
  };

  const ext = () => {
    const name = filename();
    const dot = name.lastIndexOf(".");
    return dot >= 0 ? name.slice(dot + 1).toLowerCase() : "";
  };

  const isVideo = () => isVideoPath(currentPath());

  // Low-profile rating stars next to the filename. Seeded from `sortedItems`
  // (already loaded, so no IPC round-trip on every navigation) and kept in
  // sync with the info panel / context menu / keyboard rating shortcuts via
  // the same `lightview:rating-changed` event they all dispatch.
  const [rating, setRatingSignal] = createSignal(0);
  createEffect(
    on(currentPath, (path) => {
      setRatingSignal(path ? (sortedItems().find((it) => it.path === path)?.rating ?? 0) : 0);
    }),
  );
  const onRatingChanged = (e: Event) => {
    const { path, rating: newRating } = (e as CustomEvent).detail;
    if (path === currentPath()) setRatingSignal(newRating);
  };
  window.addEventListener("lightview:rating-changed", onRatingChanged);
  onCleanup(() => window.removeEventListener("lightview:rating-changed", onRatingChanged));

  // Mirrors InfoPanel's handleSetRating: tapping the star matching the
  // current rating clears it, otherwise sets the rating to that star.
  const handleSetRating = (value: number) => {
    const path = currentPath();
    if (!path) return;
    rateItem(path, value === rating() ? 0 : value).catch((err) => {
      console.error("Failed to set rating:", err);
    });
  };

  // Tapping a tag chip in the info panel filters the gallery to that tag and
  // drops back to the grid. Close instantly (no fly-back) — the cell likely
  // won't exist under the new filter; the refresh completes in the background.
  const handleTagFilter = (namespace: string, tag: string) => {
    applyQueryAndRefresh(`${namespace}::${tag}`).catch(() => {});
    setInfoPanelOpen(false);
    props.onClose();
  };

  // Desktop webview renders GIFs on a <canvas> from a backend frame atlas —
  // WebKitGTK's <img> GIF animation is broken (too fast + leaks). A real
  // browser (web client) animates GIFs fine, so it uses the normal <img> path.
  const isGif = () => ext() === "gif";
  const useGifCanvas = () => isTauri() && isGif();

  // Adjacent paths for the filmstrip neighbour slots (undefined past the ends).
  const prevPath = () => props.paths[props.currentIndex - 1];
  const nextPath = () => props.paths[props.currentIndex + 1];
  // What to render in a neighbour slide: the full image, or a poster thumb for
  // videos (which can't be swiped into anyway, but want a preview while sliding).
  const neighbourSrc = (path: string) =>
    isVideoPath(path) ? thumbUrl(path, "p") : mediaUrl(path);

  // Neighbour slides sit one viewport-width to either side of the current one.
  const neighbourTransform = (dir: number): string => `translateX(${dir * 100}vw)`;

  // WebKitGTK's <video> element only handles a narrow set of containers
  // reliably (H.264/AAC in MP4, VP8/9 in WebM). MKV/AVI/WMV/FLV will fail
  // to decode regardless of how the bytes are served, so jump straight to
  // the external-player fallback for those instead of mounting <video>
  // and waiting for an error event.
  const isBrowserPlayableVideo = () => PLAYABLE_VIDEO_EXTS.has(ext());

  // Lazy video mounting: don't attach <video src=...> until playback should
  // start. With `preload="metadata"` the browser would still pull the moov
  // atom + leading bytes immediately on navigation; for nearby videos (e.g.
  // arrow-keying past one) that's pure waste. Playback starts from a manual
  // play tap, or — with the autoplay setting on — automatically once the user
  // has *settled* on the video (a short delay keeps skimming past cheap).
  // Autoplay always starts muted (VideoPlayer shows a tap-to-unmute pill);
  // manual play starts with sound.
  const [videoStarted, setVideoStarted] = createSignal(false);
  const [videoError, setVideoError] = createSignal(false);
  const [videoAutoMuted, setVideoAutoMuted] = createSignal(false);
  let autoplayTimer: number | null = null;
  const clearAutoplayTimer = () => {
    if (autoplayTimer !== null) { clearTimeout(autoplayTimer); autoplayTimer = null; }
  };
  onCleanup(clearAutoplayTimer);

  const openVideoExternal = () => {
    const path = currentPath();
    if (!path) return;
    // On the desktop, hand the file to the host's default player. The web
    // client has no host to launch, so open the streamed media URL in a new
    // tab and let the browser download or play it.
    if (isWeb()) {
      window.open(mediaUrl(path), "_blank", "noopener");
      return;
    }
    invoke("open_with", { command: "xdg-open", args: [path] }).catch((err) =>
      console.error("Failed to open video:", err)
    );
  };

  // --- Loading placeholder ------------------------------------------------
  // The viewer always refetches: the grid holds a `?fit=`-resized original or
  // a jm/jh tier, never the unresized file this requests, so there is no URL
  // for the two to share. What the grid *does* have is those bytes already
  // fetched and decoded — so the cell's own image is the best possible
  // stand-in, and painting it here is free. That beats the "p" tier underlay
  // this used to show alone, which is generated lazily *after* the viewer
  // opens (so it 404s on the first look at any image) and is a square centre
  // crop besides, letterboxed into a full-viewport box at whatever shape the
  // photo isn't.
  const [gridSrc, setGridSrc] = createSignal<string | null>(null);
  // Aspect measured off the grid cell's own thumbnail. The j/jm/jh tiers and
  // the `?fit=` resize all preserve aspect, so a loaded cell gives the exact
  // source ratio.
  const [gridAspect, setGridAspect] = createSignal<number | null>(null);
  const captureGridSrc = (path: string) => {
    const cellImg = path ? findCellImage(path) : null;
    const loaded = !!cellImg && cellImg.naturalWidth > 0 && cellImg.naturalHeight > 0;
    setGridSrc(loaded ? cellImg!.currentSrc || cellImg!.src : null);
    setGridAspect(loaded ? cellImg!.naturalWidth / cellImg!.naturalHeight : null);
  };

  /** Indexed source pixel size, when the backend has it. Caps the fitted rect
   *  so an item smaller than the window isn't laid out as if it filled it. */
  const naturalSize = (path: string) => {
    const m = mediaMetaByPath().get(path);
    return m && m.width && m.height ? { width: m.width, height: m.height } : null;
  };

  /** The rect the full image will occupy once it loads. The placeholder is
   *  drawn into exactly this box so it has the photo's dimensions from the
   *  first frame and nothing shifts on reveal.
   *
   *  Indexed dimensions first, then the grid cell's measured aspect. That
   *  fallback is not a nicety: a file whose dimensions haven't been indexed
   *  yet has no `aspectByPath` entry at all, and defaulting to 1:1 would put
   *  a square box around a landscape photo — the exact mis-shaped placeholder
   *  this is meant to fix. The justified grid keeps its own `measuredAspects`
   *  map for the same reason. */
  const placeholderRect = (): Rect | null => {
    const a = aspectByPath().get(currentPath()) ?? gridAspect();
    return a && a > 0 ? viewportContainRect(a, naturalSize(currentPath())) : null;
  };
  const placeholderBox = () => {
    const r = placeholderRect();
    return r
      ? { left: `${r.left}px`, top: `${r.top}px`, width: `${r.width}px`, height: `${r.height}px` }
      : { inset: "0" };
  };
  /** Best real pixels available before the full image lands. */
  const placeholderSrc = (): string | null =>
    gridSrc() ?? (currentPath() ? thumbUrl(currentPath(), "p") : null);

  /** Mount an image element into the container. `path` is captured at call
   *  time — the async reveal below can outlive a navigation. */
  const mountImage = (img: HTMLImageElement, alreadyLoaded: boolean, path: string) => {
    if (!imageContainerRef) return;
    imageContainerRef.replaceChildren(img);
    const reveal = () => {
      // A navigation (or another mount) may have landed during the decode.
      if (imageContainerRef?.firstChild !== img) return;
      img.style.opacity = "1";
      img.style.position = "relative";
      setLoaded(true);
      setImgNaturalWidth(img.naturalWidth);
      cache.insert(path, img);
    };
    if (alreadyLoaded) {
      reveal();
      return;
    }
    img.style.opacity = "0";
    img.style.position = "absolute";
    setLoaded(false);
    img.onload = () => {
      // onload means fetched, not decoded. Revealing here tears the
      // placeholder down a frame or more before the real pixels can paint,
      // which is the flash of empty backdrop between the open transition and
      // the photo. Wait for the decode (skipped on WebKitGTK, which decodes
      // on the main thread at paint time — there's no gap to cover).
      if (isTauri() || !img.decode) {
        reveal();
        return;
      }
      img.decode().then(reveal, reveal);
    };
  };

  // When a preloaded image becomes ready, swap it in if it's the current image
  cache.onReady((path) => {
    if (path !== currentPath() || isVideo()) return;
    const img = cache.get(path);
    if (img) mountImage(img, true, path);
  });

  // Schedule non-critical work (neighbour preload, preview-tier generation)
  // for the next idle period so the current image's decode/paint owns the
  // main thread. Cancelled on each navigation so rapid arrow-keying doesn't
  // pile up stale work.
  let idleHandle: number | null = null;
  const scheduleIdle = (fn: () => void) => {
    if (idleHandle !== null) {
      const w = window as unknown as {
        cancelIdleCallback?: (h: number) => void;
      };
      if (w.cancelIdleCallback) w.cancelIdleCallback(idleHandle);
      else clearTimeout(idleHandle);
    }
    const w = window as unknown as {
      requestIdleCallback?: (cb: () => void, opts?: { timeout: number }) => number;
    };
    idleHandle = w.requestIdleCallback
      ? w.requestIdleCallback(() => { idleHandle = null; fn(); }, { timeout: 200 })
      : (setTimeout(() => { idleHandle = null; fn(); }, 0) as unknown as number);
  };
  onCleanup(() => {
    if (idleHandle === null) return;
    const w = window as unknown as { cancelIdleCallback?: (h: number) => void };
    if (w.cancelIdleCallback) w.cancelIdleCallback(idleHandle);
    else clearTimeout(idleHandle);
  });

  // --- Grid ↔ viewer shared-element transition ----------------------------
  // On open, the tapped thumbnail's rect expands into the fitted media rect;
  // on close, the media flies back down into its cell. A fixed-position clone
  // (object-fit: cover, so square grid crops un-crop smoothly) does the
  // flying; the real content is revealed / hidden underneath it. Any missing
  // ingredient (reduced motion, cell not rendered, zoomed in) falls back to a
  // plain fade.
  let rootRef: HTMLDivElement | undefined;
  let cloneRef: HTMLDivElement | undefined;
  const [cloneState, setCloneState] = createSignal<{ src: string; from: Rect } | null>(null);
  const [closing, setClosing] = createSignal(false);
  let closingNow = false;

  const mediaAspect = (): number | null => {
    const a = aspectByPath().get(currentPath());
    if (a) return a;
    const img = imageContainerRef?.querySelector("img") as HTMLImageElement | null;
    if (img && img.naturalWidth > 0) return img.naturalWidth / img.naturalHeight;
    return null;
  };

  onMount(() => {
    if (prefersReducedMotion()) return;
    const cellImg = findCellImage(currentPath());
    if (!cellImg) return;
    const from = cellImg.getBoundingClientRect();
    if (from.width < 4 || from.height < 4) return;
    const src = cellImg.currentSrc || cellImg.src;
    if (!src) return;
    const aspect =
      aspectByPath().get(currentPath()) ??
      (cellImg.naturalWidth > 0 ? cellImg.naturalWidth / cellImg.naturalHeight : 1);
    // Backdrop fades in from transparent underneath the flying clone.
    setBackdropAlpha(0);
    setCloneState({ src, from });
    // Everything below has to wait a frame. `onMount` runs inside Solid's
    // effect phase, where signal writes are *queued* rather than applied — the
    // `<Show>` holding the clone (and with it `cloneRef`) is only committed
    // once the current update batch flushes, after this callback returns. So
    // reading `cloneRef` on the next line always found `undefined`, took the
    // no-clone early-out, and silently skipped the open transition entirely;
    // close worked only because it starts from a rAF callback, outside the
    // batch. The frame also gives the clone's <img> (already in cache — it is
    // the cell's own source) a chance to paint at the cell's rect before it
    // starts moving.
    requestAnimationFrame(() => {
      setBackdropAlpha(1);
      if (!cloneRef) { setCloneState(null); return; }
      const anim = cloneRef.animate(
        [
          rectKeyframe(from),
          rectKeyframe(viewportContainRect(aspect, naturalSize(currentPath()))),
        ],
        { duration: 260, easing: "cubic-bezier(0.2, 0, 0.2, 1)", fill: "forwards" },
      );
      anim.onfinish = () => setCloneState(null);
      anim.oncancel = () => setCloneState(null);
    });
  });

  /** Close with the fly-back-to-cell transition (or a fade fallback). Every
   *  close path routes through here — in-viewer ones call it directly, and the
   *  app-level Escape handler reaches it by dispatching
   *  `VIEWER_CLOSE_REQUEST_EVENT`. */
  const requestClose = () => {
    if (closingNow) return;
    if (prefersReducedMotion() || zoom() > 1) { props.onClose(); return; }
    closingNow = true;
    // Ask the grid to bring the (possibly navigated-to) item's cell into
    // view, then give it two frames to scroll + render the virtual slice.
    window.dispatchEvent(
      new CustomEvent("lightview:scroll-to-index", { detail: props.currentIndex }),
    );
    requestAnimationFrame(() => requestAnimationFrame(() => {
      const path = currentPath();
      const cellImg = findCellImage(path);
      if (!cellImg) {
        // Fade fallback: dim the backdrop, hide the content, close.
        setClosing(true);
        setChromeVisible(false);
        setBackdropAlpha(0);
        window.setTimeout(() => props.onClose(), 200);
        return;
      }
      // Fly from wherever the media currently is on screen (including a
      // mid-dismiss dragged/shrunken position) back into the cell.
      const mediaEl =
        (imageContainerRef?.querySelector("img") as HTMLElement | null) ??
        (rootRef?.querySelector("video, canvas") as HTMLElement | null);
      let from: Rect | null = null;
      if (mediaEl) from = mediaEl.getBoundingClientRect();
      else {
        const a = mediaAspect();
        if (a) from = viewportContainRect(a, naturalSize(path));
      }
      if (!from || from.width < 4) {
        setClosing(true);
        setChromeVisible(false);
        setBackdropAlpha(0);
        window.setTimeout(() => props.onClose(), 200);
        return;
      }
      const src =
        mediaEl instanceof HTMLImageElement && mediaEl.naturalWidth > 0
          ? mediaEl.currentSrc || mediaEl.src
          : thumbUrl(path, "p");
      // One batch so the track hides in the same frame the clone appears.
      batch(() => {
        setClosing(true);
        setChromeVisible(false);
        setBackdropAlpha(0);
        setCloneState({ src, from });
      });
      if (!cloneRef) { props.onClose(); return; }
      const anim = cloneRef.animate(
        [rectKeyframe(from), rectKeyframe(cellImg.getBoundingClientRect())],
        { duration: 230, easing: "cubic-bezier(0.4, 0, 0.2, 1)", fill: "forwards" },
      );
      anim.onfinish = () => props.onClose();
      anim.oncancel = () => props.onClose();
    }));
  };

  window.addEventListener(VIEWER_CLOSE_REQUEST_EVENT, requestClose);
  onCleanup(() => window.removeEventListener(VIEWER_CLOSE_REQUEST_EVENT, requestClose));

  const handleBackdropClick = (e: MouseEvent) => {
    // A touch tap handles itself (toggles chrome); swallow the compatibility
    // click it synthesizes so we don't also close the viewer.
    if (suppressBackdropClick) { suppressBackdropClick = false; return; }
    if (didDrag) { didDrag = false; return; }
    if (e.target === e.currentTarget) {
      requestClose();
    }
  };

  // A plain click on the media itself toggles the chrome overlay (rating stars
  // + filename + close button) instead of closing — so a click clears the
  // corner text that crowds (and on mobile can clip off the edge of) the image.
  // Deferred behind a short timer so a desktop double-click (zoom) cancels the
  // toggle rather than flashing the chrome off and back on. Touch taps already
  // toggle chrome through the gesture machine and set `suppressBackdropClick`,
  // so their synthesized click is swallowed here without a second toggle.
  let mediaClickTimer: number | null = null;
  const cancelMediaClick = () => {
    if (mediaClickTimer !== null) { clearTimeout(mediaClickTimer); mediaClickTimer = null; }
  };
  const handleMediaClick = (e: MouseEvent) => {
    e.stopPropagation();
    if (suppressBackdropClick) { suppressBackdropClick = false; return; }
    if (didDrag) { didDrag = false; return; }
    cancelMediaClick();
    mediaClickTimer = window.setTimeout(() => {
      mediaClickTimer = null;
      setChromeVisible((v) => !v);
    }, 250);
  };

  // --- Zoom / Pan ---

  const handleWheel = (e: WheelEvent) => {
    // Always block scroll from reaching the gallery grid underneath
    e.preventDefault();
    e.stopPropagation();

    if (e.ctrlKey) {
      // Ctrl+scroll = zoom toward cursor
      const factor = e.deltaY > 0 ? 1 / 1.15 : 1.15;
      const newZoom = Math.max(0.1, Math.min(50, zoom() * factor));

      const mx = e.clientX - window.innerWidth / 2;
      const my = e.clientY - window.innerHeight / 2;
      const ratio = newZoom / zoom();
      setPanX(mx - (mx - panX()) * ratio);
      setPanY(my - (my - panY()) * ratio);
      setZoom(newZoom);
    } else if (zoom() > 1) {
      // Plain scroll when zoomed = pan the image. Normalize first: Firefox
      // reports line mode, so raw deltas would pan ~3px per notch.
      const px = wheelPxPerUnit(e);
      setPanX(panX() - e.deltaX * px);
      setPanY(panY() - e.deltaY * px);
    }
  };

  const handleMouseDown = (e: MouseEvent) => {
    if (zoom() <= 1 || e.button !== 0) return;
    isDragging = true;
    didDrag = false;
    dragStartX = e.clientX;
    dragStartY = e.clientY;
    panStartX = panX();
    panStartY = panY();
    e.preventDefault();
  };

  const handleMouseMove = (e: MouseEvent) => {
    // Reveal the window titlebar when the cursor nears the top edge.
    if (frameless()) {
      if (e.clientY < 40) revealTitlebar();
      else if (titlebarVisible()) hideTitlebar();
    }
    if (!isDragging) return;
    const dx = e.clientX - dragStartX;
    const dy = e.clientY - dragStartY;
    if (Math.abs(dx) > 3 || Math.abs(dy) > 3) didDrag = true;
    setPanX(panStartX + dx);
    setPanY(panStartY + dy);
  };

  const handleMouseUp = () => {
    isDragging = false;
  };

  // Timestamp of the last touch pointer lift, used to recognise the mouse
  // events the platform synthesizes from a touch gesture. Assigned by the
  // touch handlers further down.
  let lastTouchEndT = -Infinity;

  const handleDblClick = (e: MouseEvent) => {
    cancelMediaClick(); // the two clicks that precede this shouldn't toggle chrome
    // A touch double-tap is already handled by the gesture machine
    // (`touchDoubleTap`), but the platform *also* synthesizes a `dblclick` for
    // it. Letting that through ran this handler against the zoom the tap had
    // just applied and toggled it straight back — so a quick double-tap
    // appeared to do nothing at all, while a slower one (past DOUBLE_TAP_MS,
    // inside the platform's own threshold) worked because only this path
    // fired. Ignore the synthetic one; real mice are unaffected.
    if (e.timeStamp - lastTouchEndT < SYNTHETIC_MOUSE_MS) return;
    if (isVideo()) return;
    if (zoom() !== 1) {
      // Reset to fit
      setZoom(1);
      setPanX(0);
      setPanY(0);
    } else {
      // Zoom to 1:1 (one image pixel = one physical screen pixel)
      const img = imageContainerRef?.querySelector("img") as HTMLImageElement | null;
      if (!img || !img.clientWidth || !img.naturalWidth) return;
      const dpr = window.devicePixelRatio || 1;
      const targetZoom = img.naturalWidth / (img.clientWidth * dpr);
      const mx = e.clientX - window.innerWidth / 2;
      const my = e.clientY - window.innerHeight / 2;
      const ratio = targetZoom; // zoom() is 1
      setPanX(mx - mx * ratio);
      setPanY(my - my * ratio);
      setZoom(targetZoom);
    }
  };

  // --- Touch gestures (Apple Photos–style) -------------------------------
  // Native Pointer Events, gated on `pointerType === "touch"` so mouse / pen
  // fall through to the handlers above untouched. The fullscreen backdrop is
  // the listening element; buttons / video / inputs are excluded so their own
  // taps still work.

  const pointers = new Map<number, Point>();
  type GestureMode =
    | "none"
    | "pending" // single finger, axis not yet locked
    | "horizontal" // swipe to navigate
    | "vertical" // swipe down to dismiss
    | "info" // swipe up to open the info panel
    | "pan" // single finger drag while zoomed in
    | "pinch";
  let gestureMode: GestureMode = "none";
  let gestureMoved = false;
  let startX = 0;
  let startY = 0;
  let panStartTX = 0;
  let panStartTY = 0;
  let lastX = 0;
  let lastY = 0;
  let lastT = 0;
  let prevX = 0;
  let prevY = 0;
  let prevT = 0;
  // Pinch anchor: the image-local point (centred coords) under the initial
  // two-finger midpoint, kept fixed under the moving midpoint as fingers move.
  let pinchStartDist = 0;
  let pinchStartZoom = 1;
  let pinchAnchorX = 0;
  let pinchAnchorY = 0;
  // Double-tap detection. `lastTapT` is the previous tap's *lift*; `tapDownT`
  // the current tap's *touchdown* — the gap between those two is what
  // DOUBLE_TAP_MS bounds, so how long the second tap is held doesn't count
  // against it.
  let lastTapT = -Infinity;
  let tapDownT = 0;
  let lastTapX = 0;
  let lastTapY = 0;
  // Suppresses the compatibility click a tap synthesizes (so a backdrop tap
  // toggles chrome instead of also closing the viewer).
  let suppressBackdropClick = false;
  // Long-press → context menu (iOS never fires `contextmenu` for touch, so a
  // timer stands in). Cancelled by movement past the tap slop, a second
  // finger, or lift; on fire the gesture is consumed (no tap follows).
  let longPressTimer: number | null = null;
  let longPressFired = false;
  const cancelLongPress = () => {
    if (longPressTimer !== null) {
      clearTimeout(longPressTimer);
      longPressTimer = null;
    }
  };

  const ptList = (): Point[] => [...pointers.values()];

  // Snap-back animation: briefly enable a CSS transition on the image
  // container (zoom/pan/vertical) and/or the filmstrip track (horizontal swipe)
  // so released gestures ease home instead of jumping.
  let snapTimer: number | null = null;
  const clearTransitions = () => {
    if (imageContainerRef) imageContainerRef.style.transition = "none";
    if (trackRef) trackRef.style.transition = "none";
  };
  const armSnapClear = () => {
    if (snapTimer !== null) clearTimeout(snapTimer);
    snapTimer = window.setTimeout(() => {
      clearTransitions();
      snapTimer = null;
    }, 300);
  };
  const cancelSnap = () => {
    clearTransitions();
    if (snapTimer !== null) {
      clearTimeout(snapTimer);
      snapTimer = null;
    }
  };
  const animateContainer = () => {
    if (imageContainerRef) imageContainerRef.style.transition = "transform 0.22s ease-out";
    armSnapClear();
  };
  const animateTrack = () => {
    if (trackRef) trackRef.style.transition = "transform 0.28s ease-out";
    armSnapClear();
  };

  // A pending swipe-navigation commit: the track is mid-slide toward a
  // neighbour; once it lands we flip the index and snap the track back to zero
  // in the same synchronous batch so the (identical) image doesn't flicker.
  let navCommit: (() => void) | null = null;
  let navTimer: number | null = null;
  const runNavCommit = () => {
    if (navTimer !== null) { clearTimeout(navTimer); navTimer = null; }
    const fn = navCommit;
    navCommit = null;
    if (!fn) return;
    if (trackRef) trackRef.style.transition = "none";
    // One batch: snap the track home, flip the index, and drop the strip — the
    // neighbour we slid to becomes the current slide with no intermediate frame.
    batch(() => { setDragX(0); setSwiping(false); fn(); });
  };
  const scheduleNavCommit = (fn: () => void) => {
    navCommit = fn;
    if (navTimer !== null) clearTimeout(navTimer);
    navTimer = window.setTimeout(runNavCommit, 280);
  };

  // Keep the neighbour slides mounted until the track has settled, then drop
  // them so idle memory isn't held by adjacent full-res images.
  let swipeEndTimer: number | null = null;
  const beginSwipe = () => {
    if (swipeEndTimer !== null) { clearTimeout(swipeEndTimer); swipeEndTimer = null; }
    setSwiping(true);
  };
  const endSwipeSoon = () => {
    if (swipeEndTimer !== null) clearTimeout(swipeEndTimer);
    swipeEndTimer = window.setTimeout(() => { setSwiping(false); swipeEndTimer = null; }, 320);
  };

  const snapDragReset = () => {
    animateContainer();
    animateTrack();
    setDragX(0);
    setDragY(0);
    setDragScale(1);
    setBackdropAlpha(1);
  };
  const snapZoomReset = () => {
    animateContainer();
    setZoom(1);
    setPanX(0);
    setPanY(0);
  };

  // Double-tap zoom for touch. Caps the zoom so a huge photo doesn't jump to a
  // wildly cropped 1:1 (the mouse double-click path keeps its exact 1:1).
  const touchDoubleTap = (x: number, y: number) => {
    if (isVideo()) return;
    if (zoom() !== 1) {
      snapZoomReset();
      return;
    }
    const img = imageContainerRef?.querySelector("img") as HTMLImageElement | null;
    if (!img || !img.clientWidth || !img.naturalWidth) return;
    const dpr = window.devicePixelRatio || 1;
    let target = img.naturalWidth / (img.clientWidth * dpr);
    target = Math.min(target, 3);
    if (target <= 1.01) target = 2;
    const mx = x - window.innerWidth / 2;
    const my = y - window.innerHeight / 2;
    animateContainer();
    setPanX(mx - mx * target);
    setPanY(my - my * target);
    setZoom(target);
  };

  const onPointerDown = (e: PointerEvent) => {
    if (e.pointerType !== "touch") return;
    if (closingNow) return; // close transition in flight — ignore new gestures
    const t = e.target as HTMLElement;
    // Touches on real controls (buttons / inputs / links / anything marked
    // data-gesture-exempt, like the video scrubber) are left to them. The
    // video surface itself is NOT excluded — with custom controls it behaves
    // like a photo: tap toggles chrome, swipes navigate / dismiss / open info.
    if (t.closest("button, input, a, [data-gesture-exempt]")) return;
    // If a swipe-navigation is still mid-flight, land it now so the new gesture
    // operates on the settled index rather than racing the pending commit.
    runNavCommit();
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });

    if (pointers.size === 2) {
      cancelLongPress(); // a second finger means pinch, not press-and-hold
      // Videos don't zoom, so ignore two-finger pinches on them (otherwise the
      // pinch would apply a stuck zoom transform to the empty image container).
      if (isVideo()) { gestureMode = "none"; e.preventDefault(); return; }
      const [a, b] = ptList();
      pinchStartDist = pointerDistance(a, b) || 1;
      pinchStartZoom = zoom();
      const m = pointerMidpoint(a, b);
      const mx = m.x - window.innerWidth / 2;
      const my = m.y - window.innerHeight / 2;
      pinchAnchorX = (mx - panX()) / zoom();
      pinchAnchorY = (my - panY()) / zoom();
      gestureMode = "pinch";
      cancelSnap();
      e.preventDefault();
      return;
    }

    if (pointers.size === 1) {
      startX = lastX = prevX = e.clientX;
      startY = lastY = prevY = e.clientY;
      lastT = prevT = tapDownT = e.timeStamp;
      gestureMoved = false;
      panStartTX = panX();
      panStartTY = panY();
      gestureMode = zoom() > 1 ? "pan" : "pending";
      cancelSnap();

      // Arm the long-press. On fire: haptic tick, open the context menu at
      // the press point, and consume the gesture so no tap follows the lift.
      longPressFired = false;
      cancelLongPress();
      if (props.onContextMenu) {
        longPressTimer = window.setTimeout(() => {
          longPressTimer = null;
          longPressFired = true;
          gestureMode = "none";
          hapticTick();
          props.onContextMenu!(e, currentPath(), props.currentIndex);
        }, LONG_PRESS_MS);
      }
    }
  };

  const onPointerMove = (e: PointerEvent) => {
    if (e.pointerType !== "touch" || !pointers.has(e.pointerId)) return;
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });

    if (gestureMode === "pinch" && pointers.size >= 2) {
      e.preventDefault();
      const [a, b] = ptList();
      const dist = pointerDistance(a, b) || 1;
      const newZoom = Math.max(0.5, Math.min(50, pinchStartZoom * (dist / pinchStartDist)));
      const m = pointerMidpoint(a, b);
      const mx = m.x - window.innerWidth / 2;
      const my = m.y - window.innerHeight / 2;
      setPanX(mx - pinchAnchorX * newZoom);
      setPanY(my - pinchAnchorY * newZoom);
      setZoom(newZoom);
      return;
    }

    prevX = lastX;
    prevY = lastY;
    prevT = lastT;
    lastX = e.clientX;
    lastY = e.clientY;
    lastT = e.timeStamp;
    const dx = e.clientX - startX;
    const dy = e.clientY - startY;

    // Wandering past the tap slop is a drag, not a press-and-hold.
    if (longPressTimer !== null && Math.hypot(dx, dy) > TAP_SLOP_PX) cancelLongPress();

    if (gestureMode === "pan") {
      e.preventDefault();
      if (Math.abs(dx) > TAP_SLOP_PX || Math.abs(dy) > TAP_SLOP_PX) gestureMoved = true;
      setPanX(panStartTX + dx);
      setPanY(panStartTY + dy);
      return;
    }

    if (gestureMode === "pending") {
      if (Math.hypot(dx, dy) < AXIS_LOCK_PX) return;
      if (Math.abs(dx) > Math.abs(dy)) { gestureMode = "horizontal"; beginSwipe(); }
      else if (dy > 0) gestureMode = "vertical";
      else gestureMode = "info"; // upward swipe reveals the info panel
      gestureMoved = true;
    }

    if (gestureMode === "horizontal") {
      e.preventDefault();
      const atFirst = props.currentIndex <= 0;
      const atLast = props.currentIndex >= props.paths.length - 1;
      let d = dx;
      // Rubber-band past the ends of the gallery.
      if ((d > 0 && atFirst) || (d < 0 && atLast)) d *= 0.35;
      setDragX(d);
    } else if (gestureMode === "vertical") {
      e.preventDefault();
      const d = Math.max(0, dy);
      setDragY(d);
      // With the info panel open a downward swipe just closes the panel, so
      // keep the photo full-bright; otherwise it shrinks + fades to dismiss.
      if (!infoPanelOpen()) {
        const p = Math.min(1, d / (SWIPE_DISMISS_PX * 1.6));
        setDragScale(1 - p * (1 - DISMISS_MIN_SCALE));
        setBackdropAlpha(1 - p * 0.7);
      }
    } else if (gestureMode === "info") {
      e.preventDefault();
      // Damped upward lift of the photo, hinting at the sheet rising below.
      // No-op once the panel is already open (it's at its resting gap position).
      if (!infoPanelOpen()) setDragY(Math.min(0, dy) * 0.5);
    }
  };

  const finishHorizontal = () => {
    const dx = lastX - startX;
    const vx = (lastX - prevX) / Math.max(1, lastT - prevT);
    const W = window.innerWidth;
    const goPrev =
      (dx > W * SWIPE_NAV_RATIO || vx > SWIPE_VELOCITY) && props.currentIndex > 0;
    const goNext =
      (dx < -W * SWIPE_NAV_RATIO || vx < -SWIPE_VELOCITY) &&
      props.currentIndex < props.paths.length - 1;
    if (goPrev || goNext) {
      // Slide the whole filmstrip a full screen so the neighbour glides to
      // centre, then commit the index + reset the track in one batch.
      animateTrack();
      setDragX(goNext ? -W : W);
      scheduleNavCommit(goNext ? props.onNext : props.onPrev);
    } else {
      snapDragReset();
      endSwipeSoon();
    }
  };

  const finishVertical = () => {
    const dy = lastY - startY;
    const vy = (lastY - prevY) / Math.max(1, lastT - prevT);
    if (infoPanelOpen()) {
      // First stage: a downward swipe closes the panel and re-centres the
      // photo full-screen (a second swipe-down then dismisses the viewer).
      if (dy > SWIPE_DISMISS_PX * 0.5 || vy > SWIPE_VELOCITY) {
        setInfoPanelOpen(false);
      }
      snapDragReset();
    } else if (dy > SWIPE_DISMISS_PX || vy > SWIPE_VELOCITY) {
      requestClose();
    } else {
      snapDragReset();
    }
  };

  const finishInfo = () => {
    if (infoPanelOpen()) {
      snapDragReset();
      return;
    }
    const dy = lastY - startY; // negative for an upward swipe
    const vy = (lastY - prevY) / Math.max(1, lastT - prevT);
    if (-dy > SWIPE_DISMISS_PX * 0.6 || -vy > SWIPE_VELOCITY) {
      setInfoPanelOpen(true);
    }
    snapDragReset(); // photo always eases back to its resting position
  };

  const handleTap = (e: PointerEvent) => {
    // A real backdrop tap shouldn't also fire the click-to-close handler.
    suppressBackdropClick = true;
    // While the info panel is open, a tap on the photo dismisses it.
    if (infoPanelOpen()) {
      setInfoPanelOpen(false);
      return;
    }
    const near =
      Math.abs(e.clientX - lastTapX) < DOUBLE_TAP_SLOP_PX &&
      Math.abs(e.clientY - lastTapY) < DOUBLE_TAP_SLOP_PX;
    if (tapDownT - lastTapT < DOUBLE_TAP_MS && near) {
      lastTapT = -Infinity; // a third tap starts a fresh pair, not another zoom
      touchDoubleTap(e.clientX, e.clientY);
      return;
    }
    lastTapT = e.timeStamp;
    lastTapX = e.clientX;
    lastTapY = e.clientY;
    // Toggle immediately rather than waiting out the double-tap window. The
    // mouse path has to defer (a double-click's two clicks would each toggle,
    // flashing the chrome off and back on), but a touch double-tap is caught
    // above and returns before toggling a second time — so the only thing
    // waiting bought was a tap that felt a beat behind the finger. A
    // double-tap-to-zoom now carries the first tap's toggle with it, which is
    // what the platform photo viewers do anyway.
    setChromeVisible((v) => !v);
  };

  const onPointerUp = (e: PointerEvent) => {
    if (e.pointerType !== "touch" || !pointers.has(e.pointerId)) return;
    pointers.delete(e.pointerId);
    lastTouchEndT = e.timeStamp; // marks the mouse events that follow as synthetic
    cancelLongPress();
    if (longPressFired) {
      // The hold already opened the context menu; swallow the compatibility
      // click the lift synthesizes (it would land on the menu or backdrop).
      longPressFired = false;
      suppressNextClick();
    }

    if (gestureMode === "pinch") {
      if (zoom() <= 1) snapZoomReset();
      // A lingering finger after a pinch is ignored to avoid a jump.
      gestureMode = "none";
      return;
    }
    if (pointers.size > 0) return;

    const m = gestureMode;
    gestureMode = "none";
    if (m === "horizontal") finishHorizontal();
    else if (m === "vertical") finishVertical();
    else if (m === "info") finishInfo();
    else if ((m === "pending" || m === "pan") && !gestureMoved) handleTap(e);
  };

  const onPointerCancel = (e: PointerEvent) => {
    if (e.pointerType !== "touch" || !pointers.has(e.pointerId)) return;
    pointers.delete(e.pointerId);
    lastTouchEndT = e.timeStamp;
    cancelLongPress();
    longPressFired = false;
    if (gestureMode === "horizontal" || gestureMode === "vertical" || gestureMode === "info") {
      snapDragReset();
      if (gestureMode === "horizontal") endSwipeSoon();
    } else if (gestureMode === "pinch" && zoom() <= 1) snapZoomReset();
    if (pointers.size === 0) {
      gestureMode = "none";
      gestureMoved = false;
    }
  };

  onCleanup(() => {
    cancelLongPress();
    cancelMediaClick();
    if (snapTimer !== null) clearTimeout(snapTimer);
    if (navTimer !== null) clearTimeout(navTimer);
    if (swipeEndTimer !== null) clearTimeout(swipeEndTimer);
  });

  // Load full media when index changes, using cache + preloading. Declared
  // after the touch-gesture state because the on-mount reset below references
  // it (cancelSnap, pointers, gestureMode).
  createEffect(
    on(
      () => props.currentIndex,
      (idx) => {
        // Reset zoom/pan on image change
        setZoom(1);
        setPanX(0);
        setPanY(0);
        setImgNaturalWidth(0);
        // Reset any in-flight touch gesture so the new image starts centered.
        cancelSnap();
        setDragX(0);
        setDragY(0);
        setDragScale(1);
        setBackdropAlpha(1);
        pointers.clear();
        gestureMode = "none";
        gestureMoved = false;
        lastTapT = -Infinity; // a tap on the new photo can't pair with the old one
        cancelLongPress();
        cancelMediaClick();
        longPressFired = false;
        if (swipeEndTimer !== null) { clearTimeout(swipeEndTimer); swipeEndTimer = null; }
        setSwiping(false);

        const path = props.paths[idx];
        // Read the grid cell's image before anything else moves — it's the
        // placeholder for the load that's about to start.
        captureGridSrc(path);
        // Tell the grid which cell is being viewed so it holds that one at
        // full resolution for the close transition to land on.
        window.dispatchEvent(
          new CustomEvent<string | null>(VIEWER_PATH_EVENT, { detail: path ?? null }),
        );
        clearAutoplayTimer();
        setVideoStarted(false);
        setVideoError(false);
        setVideoAutoMuted(false);
        // A playing video may have idle-hidden the chrome; navigation must
        // bring it back (desktop has no tap-to-toggle to recover it).
        setChromeVisible(true);
        if (!path || isVideo()) {
          setLoaded(false);
          if (imageContainerRef) imageContainerRef.replaceChildren();
          // Settle-then-autoplay: wait a beat so arrow-keying / swiping past
          // videos never mounts <video> or pulls bytes.
          if (
            path &&
            isBrowserPlayableVideo() &&
            settings().display.video_autoplay_viewer
          ) {
            autoplayTimer = window.setTimeout(() => {
              autoplayTimer = null;
              setVideoAutoMuted(true);
              setVideoStarted(true);
            }, 300);
          }
          return;
        }

        // GIFs (desktop) are drawn declaratively on a <canvas>; skip the
        // imperative <img> swap and mark loaded so the spinner/underlay clear.
        if (useGifCanvas()) {
          if (imageContainerRef) imageContainerRef.replaceChildren();
          setLoaded(true);
        } else {
          // Mount the current image first, before queueing background work.
          const cached = cache.get(path);
          if (cached) {
            mountImage(cached, true, path);
          } else {
            const img = new Image();
            img.style.maxWidth = "100vw";
            img.style.maxHeight = "100vh";
            img.style.objectFit = "contain";
            img.draggable = false;
            img.alt = filename();
            img.src = mediaUrl(path);
            mountImage(img, false, path);
          }
        }

        // Defer neighbour preloading + preview-tier generation to idle so
        // they don't contend with the current image's decode.
        scheduleIdle(() => {
          cache.preload(props.paths, idx);
          // P5: preview tier first-paint. Lazy 1600 px generation for this
          // image and its neighbours; the underlay <img src={thumbUrl(path, 'p')}>
          // 404s until the tier is ready, then a natural re-render swaps it in.
          const previewPaths = [props.paths[idx], props.paths[idx - 1], props.paths[idx + 1]]
            .filter((p): p is string => !!p);
          ensureTierThumbnails(previewPaths, "p").catch(() => {});
        });
      },
    ),
  );

  // Filmstrip track transform — horizontal swipe-to-navigate only. The track
  // holds the prev/current/next slots; sliding it moves all three together.
  createEffect(() => {
    if (!trackRef) return;
    const dx = dragX();
    trackRef.style.transform = dx === 0 ? "" : `translateX(${dx}px)`;
  });

  // Current image transform. Composes persistent zoom/pan with the transient
  // vertical-gesture offsets (dismiss follow + shrink). The info sheet simply
  // overlays the photo — no re-fit, the photo stays where it is.
  createEffect(() => {
    if (!imageContainerRef) return;
    const z = zoom() * dragScale();
    const px = panX();
    const py = panY() + dragY();
    if (z === 1 && px === 0 && py === 0) {
      imageContainerRef.style.transform = "";
    } else {
      imageContainerRef.style.transform = `translate(${px}px, ${py}px) scale(${z})`;
    }
  });

  // Pixel ratio: effective physical display width / natural width
  // Accounts for devicePixelRatio so 1:1 = one image pixel per physical screen pixel
  const pixelRatioPercent = (): number | null => {
    const nw = imgNaturalWidth();
    if (!nw) return null;
    const img = imageContainerRef?.querySelector("img") as HTMLImageElement | null;
    if (!img || !img.clientWidth) return null;
    const dpr = window.devicePixelRatio || 1;
    return (img.clientWidth * zoom() * dpr / nw) * 100;
  };

  const pixelRatioLabel = (): string => {
    const pct = pixelRatioPercent();
    if (pct === null) return "";
    const rounded = Math.round(pct);
    if (rounded === 100) return " — 1:1";
    return ` — ${rounded}%`;
  };

  return (
    <div
      ref={(el) => {
        rootRef = el;
        el.addEventListener("wheel", handleWheel, { passive: false });
        onCleanup(() => el.removeEventListener("wheel", handleWheel));
      }}
      class="fixed inset-0 z-50 flex items-center justify-center"
      style={{
        background: `rgba(0, 0, 0, ${0.95 * backdropAlpha()})`,
        transition: "background 0.22s ease-out",
        cursor: zoom() > 1 ? "grab" : "default",
        "touch-action": hasTouch() ? "none" : undefined,
      }}
      onClick={handleBackdropClick}
      onMouseDown={handleMouseDown}
      onMouseMove={handleMouseMove}
      onMouseUp={handleMouseUp}
      onDblClick={handleDblClick}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerCancel}
      onContextMenu={(e) => {
        // A touch-generated contextmenu (Android fires one natively on
        // long-press) is owned by the gesture machine's timer — suppress it
        // so the menu doesn't open twice. `pointers` only ever holds touch
        // pointers, so a mouse right-click passes through untouched.
        if (pointers.size > 0 || longPressFired) {
          e.preventDefault();
          return;
        }
        if (props.onContextMenu) {
          e.preventDefault();
          props.onContextMenu(e, currentPath(), props.currentIndex);
        }
      }}
    >
      {/* Navigation — left. Hidden on touch (swipe navigates instead). */}
      <Show when={!hasTouch()}>
        <button
          class="absolute left-0 top-0 h-full w-16 flex items-center justify-center opacity-0 hover:opacity-100 transition-opacity cursor-pointer z-10"
          onClick={(e) => { e.stopPropagation(); props.onPrev(); }}
        >
          <span class="text-white/60 text-2xl">&lsaquo;</span>
        </button>
      </Show>

      {/* Media display — filmstrip. A horizontally-sliding track holds the
          previous / current / next items so a swipe glides between them
          (neighbours are only mounted during a swipe; video neighbours show
          their poster thumb). The current slide holds either the imperative
          cache-swapped <img> plus zoom/pan, or the video player. The track is
          click-through except the media itself, so a backdrop tap/click in
          the surrounding margin still closes the viewer. */}
      <div
        ref={trackRef}
        class="absolute inset-0 pointer-events-none"
        // Hidden the instant the close clone takes over (the clone starts at
        // the media's exact on-screen rect, so there's no visible seam). The
        // clone-less fade fallback eases out instead.
        style={{
          opacity: closing() ? "0" : undefined,
          transition: closing() && !cloneState() ? "opacity 0.18s ease-out" : undefined,
        }}
      >
        {/* Previous neighbour slide */}
        <Show when={swiping() && prevPath()}>
          <div class="absolute inset-0 flex items-center justify-center" style={{ transform: neighbourTransform(-1) }}>
            <img
              src={neighbourSrc(prevPath()!)}
              class="max-w-[100vw] max-h-[100vh] object-contain"
              draggable={false}
              onError={(e) => { (e.currentTarget as HTMLImageElement).style.visibility = "hidden"; }}
            />
          </div>
        </Show>

        {/* Current slide */}
        <div class="absolute inset-0 flex items-center justify-center">
          <Show
            when={isVideo()}
            fallback={
              <>
                {/* Loading placeholder — occupies the exact rect the full
                    image will fill (from its indexed aspect), so it reads as
                    the photo arriving rather than an unrelated shape being
                    replaced. Layered floor-first: the ~1KB ThumbHash, then
                    the best real pixels we have (the grid cell's own image,
                    else the "p" tier, which fails silently until generated).
                    Both are object-cover *inside the correct-aspect box*, so
                    the square-cropped "p" tier crops back to the right shape
                    instead of letterboxing as a square. */}
                <Show when={!loaded()}>
                  <div class="absolute overflow-hidden pointer-events-none" style={placeholderBox()}>
                    <Show when={thumbhashDataUrl(currentPath())}>
                      <img
                        src={thumbhashDataUrl(currentPath())!}
                        alt=""
                        aria-hidden="true"
                        class="absolute inset-0 w-full h-full"
                        style={{
                          "object-fit": placeholderRect() ? "cover" : "contain",
                          // Slightly overscanned: the blur would otherwise
                          // fade out at the clipped edges of the box and let
                          // the backdrop show through as a halo.
                          filter: "blur(8px)",
                          transform: "scale(1.06)",
                        }}
                        draggable={false}
                      />
                    </Show>
                    <Show when={placeholderSrc()}>
                      <img
                        src={placeholderSrc()!}
                        alt=""
                        aria-hidden="true"
                        class="absolute inset-0 w-full h-full"
                        style={{
                          "object-fit": placeholderRect() ? "cover" : "contain",
                          // The grid's image is already a high-resolution
                          // render of this photo — blurring it would make the
                          // handoff worse, not softer. The "p" tier fallback
                          // keeps the blur that hides its lower detail.
                          filter: gridSrc() ? undefined : "blur(2px)",
                        }}
                        draggable={false}
                        // This one <img> node is reused across navigations, so
                        // a hide from one item's missing "p" tier has to be
                        // undone when the next item's source does load —
                        // otherwise one 404 blanks the placeholder for the
                        // rest of the session.
                        onLoad={(e) => {
                          (e.currentTarget as HTMLImageElement).style.visibility = "";
                        }}
                        onError={(e) => {
                          (e.currentTarget as HTMLImageElement).style.visibility = "hidden";
                        }}
                      />
                    </Show>
                  </div>
                </Show>

                {/* Image container — preloaded Image elements are swapped in
                    directly. pointer-events-auto so a click/tap on the photo
                    doesn't fall through to the backdrop's click-to-close. */}
                <div ref={imageContainerRef} class="flex items-center justify-center pointer-events-auto" onClick={handleMediaClick} />

                {/* Canvas GIF playback (desktop) — replaces the imperative
                    <img> for GIFs. Zoom/pan don't apply here (the transform
                    targets the image container); acceptable for GIFs. */}
                <Show when={useGifCanvas()}>
                  <GifCanvas
                    url={gifAtlasUrl(currentPath(), "p")}
                    class="max-w-[100vw] max-h-[100vh] pointer-events-auto"
                    style={{ "object-fit": "contain" }}
                    onClick={handleMediaClick}
                  />
                </Show>

                {/* Spinner — suppressed while the open-transition clone is
                    flying so it doesn't flash behind it, and whenever a
                    placeholder is already showing the photo (a spinner over a
                    visible image just reads as breakage). */}
                <Show when={!loaded() && !cloneState() && !gridSrc() && !thumbhashDataUrl(currentPath())}>
                  <svg
                    width="48"
                    height="48"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="1.5"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    class="text-white/20 pointer-events-none"
                  >
                    <rect x="3" y="3" width="18" height="18" rx="2" ry="2" />
                    <circle cx="8.5" cy="8.5" r="1.5" />
                    <polyline points="21 15 16 10 5 21" />
                  </svg>
                </Show>
              </>
            }
          >
            <Show when={isBrowserPlayableVideo() && !videoError()} fallback={
              <div class="text-neutral-400 text-sm text-center pointer-events-auto" onClick={(e) => e.stopPropagation()}>
                <button
                  class="w-20 h-20 rounded-full bg-white/10 hover:bg-white/20 flex items-center justify-center cursor-pointer transition-colors mb-4"
                  onClick={() => openVideoExternal()}
                >
                  <span class="text-white/80 text-3xl ml-1">&#9654;</span>
                </button>
                <div>{isWeb() ? "Open / download video" : "Open in external player"}</div>
                <div class="text-neutral-600 mt-1">{filename()}</div>
              </div>
            }>
              <Show when={videoStarted()} fallback={
                <div class="relative max-w-[100vw] max-h-[100vh] flex items-center justify-center pointer-events-auto">
                  <img
                    src={thumbUrl(currentPath(), "p")}
                    class="max-w-[100vw] max-h-[100vh] object-contain"
                    draggable={false}
                    onError={(e) => {
                      (e.currentTarget as HTMLImageElement).style.visibility = "hidden";
                    }}
                  />
                  <button
                    class="absolute w-20 h-20 rounded-full bg-black/40 hover:bg-black/60 flex items-center justify-center cursor-pointer transition-colors"
                    onClick={() => { clearAutoplayTimer(); setVideoAutoMuted(false); setVideoStarted(true); }}
                    aria-label="Play video"
                  >
                    <span class="text-white text-3xl ml-1">&#9654;</span>
                  </button>
                </div>
              }>
                <VideoPlayer
                  src={mediaUrl(currentPath())}
                  startMuted={videoAutoMuted()}
                  chromeVisible={chromeVisible()}
                  onLoaded={() => setLoaded(true)}
                  onError={() => setVideoError(true)}
                  onChromeShow={() => setChromeVisible(true)}
                  onChromeHide={() => setChromeVisible(false)}
                />
              </Show>
            </Show>
          </Show>
        </div>

        {/* Next neighbour slide */}
        <Show when={swiping() && nextPath()}>
          <div class="absolute inset-0 flex items-center justify-center" style={{ transform: neighbourTransform(1) }}>
            <img
              src={neighbourSrc(nextPath()!)}
              class="max-w-[100vw] max-h-[100vh] object-contain"
              draggable={false}
              onError={(e) => { (e.currentTarget as HTMLImageElement).style.visibility = "hidden"; }}
            />
          </div>
        </Show>
      </div>

      {/* Navigation — right. Hidden on touch (swipe navigates instead). */}
      <Show when={!hasTouch()}>
        <button
          class="absolute right-0 top-0 h-full w-16 flex items-center justify-center opacity-0 hover:opacity-100 transition-opacity cursor-pointer z-10"
          onClick={(e) => { e.stopPropagation(); props.onNext(); }}
        >
          <span class="text-white/60 text-2xl">&rsaquo;</span>
        </button>
      </Show>

      {/* Window titlebar for the frameless desktop window — reveals on the top
          edge so min/maximize/close + dragging stay reachable while viewing. */}
      <Show when={frameless()}>
        <TitleBar
          visible={titlebarVisible()}
          onMouseEnter={revealTitlebar}
          onMouseLeave={hideTitlebar}
        />
      </Show>

      {/* Bottom info bar — fades with the chrome on a touch tap. Lifted clear
          of the video control bar (including its safe-area padding) when the
          player is active. The row itself is never interactive (pointer-events
          none, so it doesn't sit over the seek bar and steal its taps); the
          rating stars opt back in so they stay tappable — on touch they're
          drawn larger and padded out to a ~36px hit target (the negative
          y-margin keeps the row's height, and so the bar's position, put).
          Bounded to the viewport (max-w) so a long filename wraps instead of
          clipping off the edge; the stars are shrink-0 (always visible) and
          the text clamps to two lines. */}
      <div
        class="absolute left-1/2 -translate-x-1/2 flex items-center gap-1.5 max-w-[90vw] text-white/40 text-xs font-mono transition-opacity duration-200 pointer-events-none"
        style={{
          bottom:
            isVideo() && videoStarted()
              ? "calc(env(safe-area-inset-bottom, 0px) + 76px)"
              : "1rem",
          opacity: chromeVisible() ? undefined : "0",
        }}
      >
        <div class="flex items-center shrink-0" classList={{ "gap-0.5": !hasTouch() }} style={{ "pointer-events": chromeVisible() ? "auto" : "none" }}>
          <For each={[1, 2, 3, 4, 5]}>
            {(star) => (
              <button
                class="leading-none hover:text-white/70 transition-colors cursor-pointer"
                classList={{ "text-xl px-2 py-2 -my-2": hasTouch() }}
                style={{ color: star <= rating() ? "#f59e0b" : undefined }}
                onClick={() => handleSetRating(star)}
                aria-label={`Rate ${star} star${star > 1 ? "s" : ""}`}
              >
                {star <= rating() ? "★" : "☆"}
              </button>
            )}
          </For>
        </div>
        <span class="min-w-0 line-clamp-2 break-words">{filename()} — {props.currentIndex + 1} / {props.paths.length}{pixelRatioLabel()}</span>
      </div>

      {/* Close button — fades with the chrome on a touch tap. On touch it's
          larger for an easier hit target. */}
      <button
        class="absolute right-4 text-white/40 hover:text-white/80 cursor-pointer transition-opacity duration-200"
        classList={{
          "text-xl": !hasTouch(),
          "text-3xl p-1": hasTouch(),
          // Clear the frameless titlebar so it isn't covered (and so the image
          // close isn't confused with the window close).
          "top-12": frameless(),
          "top-4": !frameless(),
        }}
        style={{
          opacity: chromeVisible() ? undefined : "0",
          "pointer-events": chromeVisible() ? undefined : "none",
        }}
        onClick={requestClose}
      >
        &times;
      </button>

      {/* Info panel */}
      <Show when={infoPanelOpen()}>
        <InfoPanel path={currentPath()} filename={filename()} onTagFilter={handleTagFilter} />
      </Show>

      {/* Shared-element transition clone — flies between the grid cell rect
          and the fitted media rect (see onMount / requestClose). */}
      <Show when={cloneState()}>
        <div
          ref={cloneRef}
          class="fixed z-[70] overflow-hidden pointer-events-none"
          style={{
            left: `${cloneState()!.from.left}px`,
            top: `${cloneState()!.from.top}px`,
            width: `${cloneState()!.from.width}px`,
            height: `${cloneState()!.from.height}px`,
          }}
        >
          <img
            src={cloneState()!.src}
            class="w-full h-full object-cover"
            draggable={false}
          />
        </div>
      </Show>
    </div>
  );
}
