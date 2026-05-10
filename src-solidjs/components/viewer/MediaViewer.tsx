import { Show, createSignal, createEffect, on, onCleanup } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { infoPanelOpen } from "../../stores/viewerStore";
import { settings } from "../../stores/settingsStore";
import { mediaUrl, thumbUrl, ensureTierThumbnails } from "../../lib/ipc";
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

  const isVideo = () =>
    ["mp4", "mov", "avi", "mkv", "webm", "m4v", "wmv", "flv"].includes(ext());

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

  // Load full media when index changes, using cache + preloading.
  createEffect(
    on(
      () => props.currentIndex,
      (idx) => {
        // Reset zoom/pan on image change
        setZoom(1);
        setPanX(0);
        setPanY(0);
        setImgNaturalWidth(0);

        const path = props.paths[idx];
        setVideoStarted(false);
        setVideoError(false);
        if (!path || isVideo()) {
          setLoaded(false);
          if (imageContainerRef) imageContainerRef.replaceChildren();
          return;
        }

        // Mount the current image first, before queueing background work.
        const cached = cache.get(path);
        if (cached) {
          mountImage(cached, true);
        } else {
          const img = new Image();
          img.style.maxWidth = "90vw";
          img.style.maxHeight = "90vh";
          img.style.objectFit = "contain";
          img.draggable = false;
          img.alt = filename();
          img.src = mediaUrl(path);
          mountImage(img, false);
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

  const handleBackdropClick = (e: MouseEvent) => {
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

  // Apply CSS transform reactively
  createEffect(() => {
    if (!imageContainerRef) return;
    const z = zoom();
    const px = panX();
    const py = panY();
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
        el.addEventListener("wheel", handleWheel, { passive: false });
        onCleanup(() => el.removeEventListener("wheel", handleWheel));
      }}
      class="fixed inset-0 z-50 flex items-center justify-center"
      style={{ background: "rgba(0, 0, 0, 0.95)", cursor: zoom() > 1 ? "grab" : "default" }}
      onClick={handleBackdropClick}
      onMouseDown={handleMouseDown}
      onMouseMove={handleMouseMove}
      onMouseUp={handleMouseUp}
      onDblClick={handleDblClick}
      onContextMenu={(e) => {
        if (props.onContextMenu) {
          e.preventDefault();
          props.onContextMenu(e, currentPath(), props.currentIndex);
        }
      }}
    >
      {/* Navigation — left */}
      <button
        class="absolute left-0 top-0 h-full w-16 flex items-center justify-center opacity-0 hover:opacity-100 transition-opacity cursor-pointer z-10"
        onClick={(e) => { e.stopPropagation(); props.onPrev(); }}
      >
        <span class="text-white/60 text-2xl">&lsaquo;</span>
      </button>

      {/* Media display */}
      <div class="max-w-[90vw] max-h-[90vh] flex items-center justify-center">
        <Show when={isVideo()}>
          <Show when={isBrowserPlayableVideo() && !videoError()} fallback={
            <div class="text-neutral-400 text-sm text-center" onClick={(e) => e.stopPropagation()}>
              <button
                class="w-20 h-20 rounded-full bg-white/10 hover:bg-white/20 flex items-center justify-center cursor-pointer transition-colors mb-4"
                onClick={() => openVideoExternal()}
              >
                <span class="text-white/80 text-3xl ml-1">&#9654;</span>
              </button>
              <div>Open in external player</div>
              <div class="text-neutral-600 mt-1">{filename()}</div>
            </div>
          }>
            <Show when={videoStarted()} fallback={
              <div
                class="relative max-w-[90vw] max-h-[90vh] flex items-center justify-center"
                onClick={(e) => e.stopPropagation()}
              >
                <img
                  src={thumbUrl(currentPath(), "p")}
                  class="max-w-[90vw] max-h-[90vh] object-contain"
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
                class="max-w-[90vw] max-h-[90vh] outline-none"
                controls
                autoplay
                loop={settings().display.video_autoplay_loop}
                preload="auto"
                onLoadedData={() => setLoaded(true)}
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
        </Show>

        {/* Preview tier underlay — shown while the full-resolution image
            is still decoding. Uses the lazy 1600 px preview cached by
            ensureTierThumbnails above. Fails silently (404) until the
            tier is ready, at which point a natural re-render swaps it in. */}
        <Show when={!isVideo() && !loaded()}>
          <img
            src={thumbUrl(currentPath(), "p")}
            class="absolute max-w-[90vw] max-h-[90vh] object-contain pointer-events-none"
            style={{ filter: "blur(2px)" }}
            draggable={false}
            onError={(e) => {
              // Hide the broken-image icon if the preview isn't ready yet.
              (e.currentTarget as HTMLImageElement).style.visibility = "hidden";
            }}
          />
        </Show>

        {/* Image container — preloaded Image elements are swapped in directly */}
        <Show when={!isVideo()}>
          <div ref={imageContainerRef} class="max-w-[90vw] max-h-[90vh] flex items-center justify-center" />
        </Show>

        <Show when={!isVideo() && !loaded()}>
          <svg
            width="48"
            height="48"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.5"
            stroke-linecap="round"
            stroke-linejoin="round"
            class="text-white/20"
          >
            <rect x="3" y="3" width="18" height="18" rx="2" ry="2" />
            <circle cx="8.5" cy="8.5" r="1.5" />
            <polyline points="21 15 16 10 5 21" />
          </svg>
        </Show>
      </div>

      {/* Navigation — right */}
      <button
        class="absolute right-0 top-0 h-full w-16 flex items-center justify-center opacity-0 hover:opacity-100 transition-opacity cursor-pointer z-10"
        onClick={(e) => { e.stopPropagation(); props.onNext(); }}
      >
        <span class="text-white/60 text-2xl">&rsaquo;</span>
      </button>

      {/* Bottom info bar */}
      <div class="absolute bottom-4 left-1/2 -translate-x-1/2 text-white/40 text-xs font-mono">
        {filename()} — {props.currentIndex + 1} / {props.paths.length}{pixelRatioLabel()}
      </div>

      {/* Close button */}
      <button
        class="absolute top-4 right-4 text-white/40 hover:text-white/80 text-xl cursor-pointer transition-colors"
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
