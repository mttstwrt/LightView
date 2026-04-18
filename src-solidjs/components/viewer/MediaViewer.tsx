import { Show, For, createSignal, createEffect, on, onCleanup } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { infoPanelOpen } from "../../stores/viewerStore";
import { mediaUrl, videoSrc, thumbUrl, ensureTierThumbnails, getMediaMeta, getTags, addUserTag, removeUserTag, setRating as setRatingIpc, getAllThumbnailTiers } from "../../lib/ipc";
import type { ThumbnailTierInfo } from "../../lib/ipc";
import { ScrollBar } from "../shared/ScrollBar";
import { ViewerImageCache } from "../../lib/viewerCache";
import { setViewerCacheCountSource } from "../../lib/perfMonitor";

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

  const [videoPlaying, setVideoPlaying] = createSignal(false);
  const [videoError, setVideoError] = createSignal(false);
  let videoRef: HTMLVideoElement | undefined;

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
        setVideoPlaying(false);
        setVideoError(false);
        if (!path || isVideo()) {
          setLoaded(false);
          if (imageContainerRef) imageContainerRef.replaceChildren();
          return;
        }

        // Trigger preloading of adjacent images
        cache.preload(props.paths, idx);

        // P5: preview tier first-paint. Kick off lazy generation of the
        // 1600 px preview for this image and its neighbours in the
        // background; the <img src={thumbUrl(path, 'p')}> tag below
        // will 404 → retry once the tier is ready. This covers common
        // arrow-key navigation without an extra round-trip.
        const previewPaths = [props.paths[idx], props.paths[idx - 1], props.paths[idx + 1]]
          .filter((p): p is string => !!p);
        ensureTierThumbnails(previewPaths, "p").catch(() => {});

        // Check if already cached — instant display
        const cached = cache.get(path);
        if (cached) {
          mountImage(cached, true);
        } else {
          // Not cached yet — create a fresh img and load via protocol URL
          const img = new Image();
          img.style.maxWidth = "90vw";
          img.style.maxHeight = "90vh";
          img.style.objectFit = "contain";
          img.draggable = false;
          img.alt = filename();
          img.src = mediaUrl(path);
          mountImage(img, false);
        }
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
          <Show when={!videoError()} fallback={
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
            <video
              ref={videoRef}
              src={videoSrc(currentPath())}
              class="max-w-[90vw] max-h-[90vh] outline-none"
              controls
              preload="metadata"
              onPlay={() => setVideoPlaying(true)}
              onPause={() => setVideoPlaying(false)}
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

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

function formatDate(unixTimestamp: number): string {
  const d = new Date(unixTimestamp * 1000);
  return d.toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}


interface MetaInfo {
  media_type: string;
  file_size: number;
  date_taken: number | null;
  width: number | null;
  height: number | null;
  duration_seconds: number | null;
}

function InfoPanel(props: { path: string; filename: string }) {
  const [meta, setMeta] = createSignal<MetaInfo | null>(null);
  const [tags, setTags] = createSignal<{ namespace: string; tag: string }[]>([]);
  const [newTag, setNewTag] = createSignal("");
  const [rating, setRating] = createSignal(0);
  const [lastRated, setLastRated] = createSignal<number | null>(null);
  const [thumbTiers, setThumbTiers] = createSignal<ThumbnailTierInfo[]>([]);
  const [thumbExpanded, setThumbExpanded] = createSignal(false);

  const loadTags = async (path: string) => {
    try {
      const result = await getTags(path);
      setTags(result);
    } catch {}
  };

  createEffect(
    on(
      () => props.path,
      async (path) => {
        if (!path) return;
        setMeta(null);
        setTags([]);
        setRating(0);
        setLastRated(null);
        setThumbTiers([]);
        try {
          const result = await getMediaMeta(path);
          if (result) {
            setMeta({
              media_type: result.media_type,
              file_size: result.file_size,
              date_taken: result.date_taken,
              width: result.width,
              height: result.height,
              duration_seconds: result.duration_seconds,
            });
            setRating(result.rating ?? 0);
            setLastRated(result.last_rated);
          }
        } catch {}
        try {
          const tiers = await getAllThumbnailTiers(path);
          setThumbTiers(tiers);
        } catch {}
        loadTags(path);
      },
    ),
  );

  const handleAddTag = async (e: Event) => {
    e.preventDefault();
    const tag = newTag().trim();
    if (!tag || !props.path) return;
    try {
      await addUserTag(props.path, tag);
      setNewTag("");
      loadTags(props.path);
    } catch (err) {
      console.error("Failed to add tag:", err);
    }
  };

  const handleRemoveTag = async (tag: string) => {
    if (!props.path) return;
    try {
      await removeUserTag(props.path, tag);
      loadTags(props.path);
    } catch (err) {
      console.error("Failed to remove tag:", err);
    }
  };

  const handleSetRating = async (value: number) => {
    if (!props.path) return;
    const newRating = value === rating() ? 0 : value;
    try {
      await setRatingIpc(props.path, newRating);
      setRating(newRating);
      setLastRated(newRating > 0 ? Math.floor(Date.now() / 1000) : null);
    } catch (err) {
      console.error("Failed to set rating:", err);
    }
  };

  const userTags = () => tags().filter((t) => t.namespace === "user");
  const otherTags = () => tags().filter((t) => t.namespace !== "user");

  // Listen for rating changes from keyboard hotkeys (dispatched by App.tsx)
  const onRatingChanged = (e: Event) => {
    const { path, rating: newRating } = (e as CustomEvent).detail;
    if (path === props.path) {
      setRating(newRating);
      setLastRated(newRating > 0 ? Math.floor(Date.now() / 1000) : null);
    }
  };
  window.addEventListener("lightview:rating-changed", onRatingChanged);
  onCleanup(() => window.removeEventListener("lightview:rating-changed", onRatingChanged));

  let panelRef: HTMLDivElement | undefined;

  return (
    <div
      class="fixed right-0 top-0 h-full w-80 z-[60]"
      style={{
        background: "rgba(10, 10, 10, 0.75)",
        "backdrop-filter": "blur(12px)",
        "border-left": "1px solid rgba(255,255,255,0.05)",
      }}
      onClick={(e) => e.stopPropagation()}
    >
      <div
        ref={panelRef}
        class="h-full w-full p-4 overflow-y-auto hide-scrollbar"
      >
        <h3 class="text-sm font-medium text-neutral-300 mb-4">Info</h3>
        <div class="text-xs text-neutral-400 space-y-3">
          <InfoRow label="File" value={props.filename} breakAll />
          <InfoRow label="Path" value={props.path} breakAll />

          <Show when={meta()}>
            <div class="border-t border-neutral-800 pt-3 mt-3 space-y-3">
              <InfoRow label="Type" value={meta()!.media_type.toUpperCase()} />
              <InfoRow label="Size" value={formatBytes(meta()!.file_size)} />

              <Show when={meta()!.width && meta()!.height}>
                <InfoRow
                  label="Dimensions"
                  value={`${meta()!.width} × ${meta()!.height}`}
                />
              </Show>

              <Show when={meta()!.date_taken}>
                <InfoRow label="Date" value={formatDate(meta()!.date_taken!)} />
              </Show>

              <Show when={meta()!.duration_seconds}>
                <InfoRow
                  label="Duration"
                  value={`${meta()!.duration_seconds!.toFixed(1)}s`}
                />
              </Show>
            </div>
          </Show>

          {/* Thumbnails */}
          <Show when={thumbTiers().length > 0}>
            <div class="border-t border-neutral-800 pt-3 mt-3">
              <button
                class="flex items-center gap-1.5 text-neutral-500 font-medium cursor-pointer hover:text-neutral-400 transition-colors w-full text-left"
                onClick={() => setThumbExpanded((v) => !v)}
              >
                <svg
                  class="w-3 h-3 transition-transform"
                  style={{ transform: thumbExpanded() ? "rotate(90deg)" : "rotate(0deg)" }}
                  fill="none"
                  stroke="currentColor"
                  viewBox="0 0 24 24"
                  stroke-width="2.5"
                >
                  <path stroke-linecap="round" stroke-linejoin="round" d="M9 5l7 7-7 7" />
                </svg>
                Thumbnails
                <span class="text-neutral-600 font-normal text-[11px]">({thumbTiers().length})</span>
              </button>
              <Show when={thumbExpanded()}>
                <div class="mt-2 space-y-3">
                  <For each={thumbTiers()}>
                    {(tier) => (
                      <div class="pl-1 space-y-1.5">
                        <span class="text-neutral-400 text-[11px] font-medium uppercase tracking-wider">{tier.tier}</span>
                        <InfoRow label="Dimensions" value={`${tier.width} × ${tier.height}`} />
                        <InfoRow label="Format" value={tier.format.toUpperCase()} />
                        <InfoRow label="Size" value={formatBytes(tier.size_bytes)} />
                        <Show when={tier.resize_filter}>
                          <InfoRow label="Resize" value={tier.resize_filter!.charAt(0).toUpperCase() + tier.resize_filter!.slice(1)} />
                        </Show>
                      </div>
                    )}
                  </For>
                </div>
              </Show>
            </div>
          </Show>

          {/* Rating */}
          <div class="border-t border-neutral-800 pt-3 mt-3">
            <span class="text-neutral-500">Rating:</span>
            <div class="flex items-center gap-2 mt-1">
              <div class="flex gap-1">
                <For each={[1, 2, 3, 4, 5]}>
                  {(star) => (
                    <button
                      class="cursor-pointer text-base transition-colors"
                      style={{ color: star <= rating() ? "#f59e0b" : "#525252" }}
                      onClick={() => handleSetRating(star)}
                    >
                      &#9733;
                    </button>
                  )}
                </For>
              </div>
              <Show when={rating() > 0}>
                <span class="text-neutral-500 text-[11px]">
                  {lastRated() ? formatDate(lastRated()!) : "unknown"}
                </span>
              </Show>
            </div>
          </div>

          {/* Tags */}
          <div class="border-t border-neutral-800 pt-3 mt-3">
            <span class="text-neutral-500">Tags:</span>
            <div class="flex flex-wrap gap-1 mt-1">
              <For each={userTags()}>
                {(t) => (
                  <span class="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-neutral-300 bg-neutral-800">
                    {t.tag}
                    <button
                      class="text-neutral-500 hover:text-neutral-200 cursor-pointer"
                      onClick={() => handleRemoveTag(t.tag)}
                    >
                      &times;
                    </button>
                  </span>
                )}
              </For>
              <Show when={otherTags().length > 0}>
                <For each={otherTags()}>
                  {(t) => (
                    <span class="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-neutral-400 bg-neutral-800/50">
                      <span class="text-neutral-600">{t.namespace}:</span>
                      {t.tag}
                    </span>
                  )}
                </For>
              </Show>
            </div>
            <form onSubmit={handleAddTag} class="flex gap-1 mt-2">
              <input
                type="text"
                value={newTag()}
                onInput={(e) => setNewTag(e.currentTarget.value)}
                placeholder="Add tag..."
                class="flex-1 px-2 py-1 bg-neutral-800 border border-neutral-700 rounded text-xs text-neutral-200 placeholder-neutral-600 outline-none focus:border-neutral-500"
              />
              <button
                type="submit"
                class="px-2 py-1 bg-neutral-700 hover:bg-neutral-600 text-neutral-300 rounded text-xs cursor-pointer transition-colors"
              >
                +
              </button>
            </form>
          </div>
        </div>
      </div>
      <Show when={panelRef}>
        <ScrollBar
          container={panelRef!}
          class="absolute right-0 top-0 bottom-0 z-[10]"
        />
      </Show>
    </div>
  );
}

function InfoRow(props: { label: string; value: string; breakAll?: boolean }) {
  return (
    <div>
      <span class="text-neutral-500">{props.label}:</span>{" "}
      <span class={`text-neutral-300 ${props.breakAll ? "break-all" : ""}`}>
        {props.value}
      </span>
    </div>
  );
}

