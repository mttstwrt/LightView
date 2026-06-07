import { Show, createSignal, createEffect, on, onCleanup, batch } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { isWeb, hasTouch, isTauri, isMobile } from "../../lib/runtime";
import { TitleBar } from "../topbar/TitleBar";
import {
  pointerDistance,
  pointerMidpoint,
  DOUBLE_TAP_MS,
  TAP_SLOP_PX,
  AXIS_LOCK_PX,
  SWIPE_NAV_RATIO,
  SWIPE_VELOCITY,
  SWIPE_DISMISS_PX,
  DISMISS_MIN_SCALE,
  type Point,
} from "../../lib/touch";
import { infoPanelOpen, setInfoPanelOpen, infoPanelHeight } from "../../stores/viewerStore";
import { settings } from "../../stores/settingsStore";
import { mediaUrl, thumbUrl, ensureTierThumbnails, gifAtlasUrl } from "../../lib/ipc";
import { GifCanvas } from "../GifCanvas";
import { ViewerImageCache } from "../../lib/viewerCache";
import { setViewerCacheCountSource } from "../../lib/perfMonitor";
import { InfoPanel } from "./InfoPanel";

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

  const VIDEO_EXTS = ["mp4", "mov", "avi", "mkv", "webm", "m4v", "wmv", "flv"];
  const isVideoPath = (path: string) => {
    const dot = path.lastIndexOf(".");
    return dot >= 0 && VIDEO_EXTS.includes(path.slice(dot + 1).toLowerCase());
  };
  const isVideo = () => isVideoPath(currentPath());

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

  // When the info panel is open on touch, the photo shrinks + lifts to fill the
  // space above the sheet. Returns the uniform scale + vertical shift, or null
  // when no fit applies. Shared by the current image and the neighbour slides so
  // swiping between photos with the panel open keeps them the same size.
  const infoFit = (): { scale: number; ty: number } | null => {
    if (!(infoPanelOpen() && hasTouch())) return null;
    const h = window.innerHeight;
    const ph = infoPanelHeight();
    if (ph <= 0 || ph >= h) return null;
    return { scale: (h - ph) / h, ty: -ph / 2 };
  };
  const neighbourTransform = (dir: number): string => {
    const base = `translateX(${dir * 100}vw)`;
    const f = infoFit();
    return f ? `${base} translateY(${f.ty}px) scale(${f.scale})` : base;
  };

  // WebKitGTK's <video> element only handles a narrow set of containers
  // reliably (H.264/AAC in MP4, VP8/9 in WebM). MKV/AVI/WMV/FLV will fail
  // to decode regardless of how the bytes are served, so jump straight to
  // the external-player fallback for those instead of mounting <video>
  // and waiting for an error event.
  const isBrowserPlayableVideo = () =>
    ["mp4", "mov", "webm", "m4v"].includes(ext());

  // Lazy video mounting: don't attach <video src=...> until the user
  // clicks play. With `preload="metadata"` the browser would still pull
  // the moov atom + leading bytes immediately on navigation; for nearby
  // videos (e.g. arrow-keying past one) that's pure waste.
  const [videoStarted, setVideoStarted] = createSignal(false);
  const [videoError, setVideoError] = createSignal(false);

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

  /** Mount an image element into the container. */
  const mountImage = (img: HTMLImageElement, alreadyLoaded: boolean) => {
    if (!imageContainerRef) return;
    imageContainerRef.replaceChildren(img);
    if (alreadyLoaded) {
      img.style.opacity = "1";
      img.style.position = "relative";
      setLoaded(true);
      setImgNaturalWidth(img.naturalWidth);
    } else {
      img.style.opacity = "0";
      img.style.position = "absolute";
      setLoaded(false);
      img.onload = () => {
        img.style.opacity = "1";
        img.style.position = "relative";
        setLoaded(true);
        setImgNaturalWidth(img.naturalWidth);
        // Cache this freshly-loaded element for if the user navigates back
        cache.insert(currentPath(), img);
      };
    }
  };

  // When a preloaded image becomes ready, swap it in if it's the current image
  cache.onReady((path) => {
    if (path !== currentPath() || isVideo()) return;
    const img = cache.get(path);
    if (img) mountImage(img, true);
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

  const handleBackdropClick = (e: MouseEvent) => {
    // A touch tap handles itself (toggles chrome); swallow the compatibility
    // click it synthesizes so we don't also close the viewer.
    if (suppressBackdropClick) { suppressBackdropClick = false; return; }
    if (didDrag) { didDrag = false; return; }
    if (e.target === e.currentTarget) {
      props.onClose();
    }
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
      // Plain scroll when zoomed = pan the image
      setPanX(panX() - e.deltaX);
      setPanY(panY() - e.deltaY);
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

  const handleDblClick = (e: MouseEvent) => {
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
  // Double-tap detection.
  let lastTapT = 0;
  let lastTapX = 0;
  let lastTapY = 0;
  let singleTapTimer: number | null = null;
  // Suppresses the compatibility click a tap synthesizes (so a backdrop tap
  // toggles chrome instead of also closing the viewer).
  let suppressBackdropClick = false;

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
    if (e.pointerType !== "touch" || isVideo()) return;
    const t = e.target as HTMLElement;
    if (t.closest("button, video, input, a")) return;
    // If a swipe-navigation is still mid-flight, land it now so the new gesture
    // operates on the settled index rather than racing the pending commit.
    runNavCommit();
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });

    if (pointers.size === 2) {
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
      lastT = prevT = e.timeStamp;
      gestureMoved = false;
      panStartTX = panX();
      panStartTY = panY();
      gestureMode = zoom() > 1 ? "pan" : "pending";
      cancelSnap();
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
      props.onClose();
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
    const now = e.timeStamp;
    const near = Math.abs(e.clientX - lastTapX) < 40 && Math.abs(e.clientY - lastTapY) < 40;
    if (now - lastTapT < DOUBLE_TAP_MS && near) {
      if (singleTapTimer !== null) {
        clearTimeout(singleTapTimer);
        singleTapTimer = null;
      }
      lastTapT = 0;
      touchDoubleTap(e.clientX, e.clientY);
      return;
    }
    lastTapT = now;
    lastTapX = e.clientX;
    lastTapY = e.clientY;
    // Defer the single-tap action so a following tap can upgrade to double.
    if (singleTapTimer !== null) clearTimeout(singleTapTimer);
    singleTapTimer = window.setTimeout(() => {
      singleTapTimer = null;
      setChromeVisible((v) => !v);
    }, DOUBLE_TAP_MS);
  };

  const onPointerUp = (e: PointerEvent) => {
    if (e.pointerType !== "touch" || !pointers.has(e.pointerId)) return;
    pointers.delete(e.pointerId);

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
    if (singleTapTimer !== null) clearTimeout(singleTapTimer);
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
        if (swipeEndTimer !== null) { clearTimeout(swipeEndTimer); swipeEndTimer = null; }
        setSwiping(false);

        const path = props.paths[idx];
        setVideoStarted(false);
        setVideoError(false);
        if (!path || isVideo()) {
          setLoaded(false);
          if (imageContainerRef) imageContainerRef.replaceChildren();
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
            mountImage(cached, true);
          } else {
            const img = new Image();
            img.style.maxWidth = "100vw";
            img.style.maxHeight = "100vh";
            img.style.objectFit = "contain";
            img.draggable = false;
            img.alt = filename();
            img.src = mediaUrl(path);
            mountImage(img, false);
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
  // vertical-gesture offsets (dismiss follow + shrink) and, when the info panel
  // is open on touch, a uniform shrink + lift so the photo fills the space
  // above the sheet (Apple-Photos style) instead of sitting centred behind it.
  createEffect(() => {
    if (!imageContainerRef) return;
    let z = zoom() * dragScale();
    const px = panX();
    let py = panY() + dragY();
    const f = infoFit();
    if (f && zoom() === 1) {
      z *= f.scale;
      py += f.ty;
    }
    if (z === 1 && px === 0 && py === 0) {
      imageContainerRef.style.transform = "";
    } else {
      imageContainerRef.style.transform = `translate(${px}px, ${py}px) scale(${z})`;
    }
  });

  // Ease the photo into / out of its info-panel position whenever the panel
  // toggles (so opening or closing the sheet animates the shrink, not just the
  // gesture-driven path). Deferred so it doesn't fire on initial mount.
  createEffect(
    on(infoPanelOpen, () => {
      if (hasTouch()) animateContainer();
    }, { defer: true }),
  );

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

      {/* Media display — video */}
      <Show when={isVideo()}>
        <div class="absolute inset-0 flex items-center justify-center">
          <Show when={isBrowserPlayableVideo() && !videoError()} fallback={
            <div class="text-neutral-400 text-sm text-center" onClick={(e) => e.stopPropagation()}>
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
              <div
                class="relative max-w-[100vw] max-h-[100vh] flex items-center justify-center"
                onClick={(e) => e.stopPropagation()}
              >
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
                  onClick={() => setVideoStarted(true)}
                  aria-label="Play video"
                >
                  <span class="text-white text-3xl ml-1">&#9654;</span>
                </button>
              </div>
            }>
              <video
                src={mediaUrl(currentPath())}
                class="max-w-[100vw] max-h-[100vh] outline-none"
                controls
                autoplay
                preload="auto"
                onLoadedData={() => setLoaded(true)}
                onEnded={(e) => {
                  // Auto-replay. We deliberately avoid the native `loop`
                  // attribute: on WebKitGTK/GStreamer with our streamed HTTP
                  // source, looping does a non-flushing segment seek that
                  // drifts the clock (playback speeds up after the first
                  // pass) and re-issues a Range request that can stall the
                  // pipeline (freeze / jump to a random spot). A manual
                  // flushing seek to 0 followed by play() restarts cleanly.
                  if (!settings().display.video_autoplay_loop) return;
                  const v = e.currentTarget;
                  try { v.currentTime = 0; } catch { /* not seekable yet */ }
                  void v.play().catch(() => {});
                }}
                onError={(e) => {
                  const v = e.currentTarget as HTMLVideoElement;
                  const codeNames: Record<number, string> = {
                    1: "ABORTED",
                    2: "NETWORK",
                    3: "DECODE",
                    4: "SRC_NOT_SUPPORTED",
                  };
                  console.error(
                    "Video load failed:",
                    "code=" + (v.error?.code ?? "null"),
                    codeNames[v.error?.code ?? -1] ?? "",
                    "message=" + JSON.stringify(v.error?.message ?? ""),
                    "src=" + v.src,
                  );
                  setVideoError(true);
                }}
                onClick={(e) => e.stopPropagation()}
              />
            </Show>
          </Show>
        </div>
      </Show>

      {/* Media display — image filmstrip. A horizontally-sliding track holds
          the previous / current / next photos so a swipe glides between them
          (neighbours are only mounted during a swipe). Only the current slide
          carries the imperative cache-swapped <img> plus zoom/pan. The track is
          click-through except the current photo, so a backdrop tap/click in the
          surrounding margin still closes the viewer. */}
      <Show when={!isVideo()}>
        <div ref={trackRef} class="absolute inset-0 pointer-events-none">
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
            {/* Preview tier underlay — shown while the full-resolution image
                is still decoding. Fails silently (404) until the tier is
                ready, then a natural re-render swaps it in. Sized to contain
                in the full viewport so it aligns with the full image. */}
            <Show when={!loaded()}>
              <img
                src={thumbUrl(currentPath(), "p")}
                class="absolute inset-0 w-full h-full object-contain pointer-events-none"
                style={{ filter: "blur(2px)" }}
                draggable={false}
                onError={(e) => {
                  (e.currentTarget as HTMLImageElement).style.visibility = "hidden";
                }}
              />
            </Show>

            {/* Image container — preloaded Image elements are swapped in
                directly. pointer-events-auto so a click/tap on the photo
                doesn't fall through to the backdrop's click-to-close. */}
            <div ref={imageContainerRef} class="flex items-center justify-center pointer-events-auto" />

            {/* Canvas GIF playback (desktop) — replaces the imperative <img>
                for GIFs. Zoom/pan don't apply here (the transform targets the
                image container); acceptable for animated GIFs. */}
            <Show when={useGifCanvas()}>
              <GifCanvas
                url={gifAtlasUrl(currentPath(), "p")}
                class="max-w-[100vw] max-h-[100vh] pointer-events-auto"
                style={{ "object-fit": "contain" }}
              />
            </Show>

            <Show when={!loaded()}>
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
      </Show>

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

      {/* Bottom info bar — fades with the chrome on a touch tap. */}
      <div
        class="absolute bottom-4 left-1/2 -translate-x-1/2 text-white/40 text-xs font-mono transition-opacity duration-200"
        style={{
          opacity: chromeVisible() ? undefined : "0",
          "pointer-events": chromeVisible() ? undefined : "none",
        }}
      >
        {filename()} — {props.currentIndex + 1} / {props.paths.length}{pixelRatioLabel()}
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
        onClick={props.onClose}
      >
        &times;
      </button>

      {/* Info panel */}
      <Show when={infoPanelOpen()}>
        <InfoPanel path={currentPath()} filename={filename()} />
      </Show>
    </div>
  );
}
