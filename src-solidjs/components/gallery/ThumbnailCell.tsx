import { createSignal, createEffect, on, onMount, onCleanup, Show } from "solid-js";
import { settings } from "../../stores/settingsStore";
import { mediaUrl, gifAtlasUrl, type ThumbTier } from "../../lib/ipc";
import { VIDEO_EXTS, PLAYABLE_VIDEO_EXTS } from "../../lib/mediaExts";
import { saveVideoPosition, restoreVideoPosition } from "../../lib/mediaPlayback";
import { isTauri } from "../../lib/runtime";
import { recordImageLoad, isActive as perfActive } from "../../lib/perfMonitor";
import { GifCanvas } from "../GifCanvas";

interface ThumbnailCellProps {
  path: string;
  /** Protocol URL for the thumbnail image. */
  thumbSrc: string | null;
  /** LOD tier for this cell — used to request a matching GIF frame atlas. */
  tier: ThumbTier;
  /** Video duration in seconds, if known — gates short-video grid autoplay. */
  durationSec?: number | null;
  selected: boolean;
  onClick: (e: MouseEvent) => void;
  onMouseDown?: (e: MouseEvent) => void;
  onMouseEnter?: (e: MouseEvent) => void;
  onContextMenu?: (e: MouseEvent) => void;
  /** Called when the protocol handler returns 404 (thumbnail not cached). */
  onError?: (path: string) => void;
  /** Called when the thumbnail <img> finishes loading, with its natural pixel
   *  dimensions. The justified view uses this to recover a cell's aspect ratio
   *  when the indexed dimensions aren't known yet (e.g. a just-added file). */
  onImageLoad?: (naturalWidth: number, naturalHeight: number) => void;
  /**
   * Free-size mode: fill the parent (which sizes the cell explicitly) instead of
   * forcing a square aspect ratio. Used by the justified gallery view.
   */
  freeSize?: boolean;
}

// Module-level set of URLs that have already been loaded at least once.
// Survives cell recycling so a revisited thumbnail won't re-fade.
// Capped so a long session browsing huge galleries doesn't accumulate
// URLs forever; on overflow the set resets and some cells fade once more.
const LOADED_URLS_CAP = 4096;
const loadedUrls = new Set<string>();

export function markUrlLoaded(url: string): void {
  if (loadedUrls.size >= LOADED_URLS_CAP) loadedUrls.clear();
  loadedUrls.add(url);
}

// --- Grid GIF autoplay budget -------------------------------------------
// Autoplaying a GIF swaps the cell's <img> to the full-resolution original,
// which WebKitGTK decodes at native size × every frame and caches in full to
// loop. A handful of large GIFs is gigabytes. Two guards keep that bounded:
//   1. Only cells actually intersecting the viewport are allowed to animate
//      (the virtual-scroll buffer renders extra off-screen rows we must not
//      let play), and off-screen cells fall back to their static thumbnail so
//      WebKit can release the decoded frames.
//   2. A soft cap on how many may animate at once, so a wall of small GIF
//      cells on a large display can't all decode simultaneously.
// This bounds the *count* of decoding GIFs; per-GIF cost still scales with the
// original's resolution — that needs downscaled animated previews to fix.
const MAX_AUTOPLAY_GIFS = 16;
const autoplayingGifs = new Set<symbol>();

/** Reserve an autoplay slot for `token`. Returns whether one is held. */
function acquireGifSlot(token: symbol): boolean {
  if (autoplayingGifs.has(token)) return true;
  if (autoplayingGifs.size >= MAX_AUTOPLAY_GIFS) return false;
  autoplayingGifs.add(token);
  return true;
}

/** Release the autoplay slot held by `token`, if any. */
function releaseGifSlot(token: symbol): void {
  autoplayingGifs.delete(token);
}

// Grid video autoplay shares the same idea as GIFs but with a tighter cap: a
// decoding <video> is heavier than an animated GIF, so fewer may run at once.
const MAX_AUTOPLAY_VIDEOS = 8;
const autoplayingVideos = new Set<symbol>();

function acquireVideoSlot(token: symbol): boolean {
  if (autoplayingVideos.has(token)) return true;
  if (autoplayingVideos.size >= MAX_AUTOPLAY_VIDEOS) return false;
  autoplayingVideos.add(token);
  return true;
}

function releaseVideoSlot(token: symbol): void {
  autoplayingVideos.delete(token);
}

// Shared IntersectionObserver routing visibility to per-cell callbacks. One
// observer for the whole grid is far cheaper than one per cell.
type InViewCb = (inView: boolean) => void;
let sharedObserver: IntersectionObserver | null = null;
const inViewCallbacks = new WeakMap<Element, InViewCb>();

function observeInView(el: Element, cb: InViewCb): () => void {
  if (!sharedObserver) {
    sharedObserver = new IntersectionObserver(
      (entries) => {
        for (const e of entries) {
          inViewCallbacks.get(e.target)?.(e.isIntersecting);
        }
      },
      { root: null, rootMargin: "0px", threshold: 0 },
    );
  }
  inViewCallbacks.set(el, cb);
  sharedObserver.observe(el);
  return () => {
    sharedObserver?.unobserve(el);
    inViewCallbacks.delete(el);
  };
}

export function ThumbnailCell(props: ThumbnailCellProps) {
  const [errored, setErrored] = createSignal(false);
  const [hovered, setHovered] = createSignal(false);

  const filename = () => {
    const parts = props.path.split("/");
    return parts[parts.length - 1] || props.path;
  };

  const ext = () => {
    const name = filename();
    const dot = name.lastIndexOf(".");
    return dot >= 0 ? name.slice(dot + 1).toLowerCase() : "";
  };

  const isVideo = () => VIDEO_EXTS.has(ext());

  const isGif = () => ext() === "gif";

  // Browser-decodable video subset — only these are worth a hover preview.
  const isPlayableVideo = () => PLAYABLE_VIDEO_EXTS.has(ext());

  // Whether this cell is actually intersecting the viewport. Drives GIF
  // autoplay so the virtual-scroll buffer's off-screen rows don't decode.
  const [inView, setInView] = createSignal(false);
  let cellRef: HTMLDivElement | undefined;
  onMount(() => {
    if (!cellRef) return;
    onCleanup(observeInView(cellRef, setInView));
  });

  // Whether this cell currently holds an autoplay slot. Gated on being a GIF,
  // having a thumbnail, the setting being on, and being on-screen; the cap is
  // applied when claiming a slot. Re-evaluated whenever those inputs change
  // (notably scrolling toggles inView, which frees/claims slots).
  const playToken = Symbol("gif-autoplay");
  const [canAutoplay, setCanAutoplay] = createSignal(false);
  createEffect(() => {
    const want =
      isGif() &&
      !!props.thumbSrc &&
      settings().display.gif_autoplay_grid &&
      inView();
    if (!want) {
      releaseGifSlot(playToken);
      setCanAutoplay(false);
      return;
    }
    setCanAutoplay(acquireGifSlot(playToken));
  });
  onCleanup(() => releaseGifSlot(playToken));

  // A GIF should animate when hovered (always allowed — one visible cell) or
  // when it holds an autoplay slot.
  const gifActive = () => isGif() && !!props.thumbSrc && (hovered() || canAutoplay());

  // A playable video counts as "short" (GIF-like) when its known duration is at
  // or under the configured threshold. Unknown duration (not yet probed) never
  // qualifies, so it falls back to the static thumbnail + hover preview.
  const isShortVideo = () => {
    if (!isPlayableVideo()) return false;
    const d = props.durationSec;
    return d != null && d <= settings().display.video_autoplay_max_seconds;
  };

  // Mirror the GIF autoplay slot logic for short videos, gated on its own
  // setting, a known-short duration, and on-screen visibility. Re-evaluated as
  // scrolling toggles inView (claiming/freeing slots), capped separately.
  const videoToken = Symbol("video-autoplay");
  const [canVideoAutoplay, setCanVideoAutoplay] = createSignal(false);
  createEffect(() => {
    const want =
      isShortVideo() &&
      !!props.thumbSrc &&
      settings().display.video_autoplay_grid &&
      inView();
    if (!want) {
      releaseVideoSlot(videoToken);
      setCanVideoAutoplay(false);
      return;
    }
    setCanVideoAutoplay(acquireVideoSlot(videoToken));
  });
  onCleanup(() => releaseVideoSlot(videoToken));

  // On the desktop webview (WebKitGTK) we render GIFs on a <canvas> from a
  // backend frame atlas — its `<img>` GIF animation is broken (too fast + leaks).
  // A real browser (web client) animates `<img>` GIFs fine, so swap there.
  const useCanvasGif = () => isTauri();
  const showGifCanvas = () => useCanvasGif() && gifActive();

  // The base <img> shows the static thumbnail (and acts as the poster under the
  // canvas). On the web client it swaps to the full animated GIF when active.
  const effectiveSrc = () => {
    if (!useCanvasGif() && gifActive()) {
      return mediaUrl(props.path);
    }
    return props.thumbSrc;
  };

  // Show a muted looping video preview while hovering (when enabled), or
  // continuously for short videos when grid autoplay is on / they hold a slot.
  const showVideoPreview = () =>
    (isPlayableVideo() && hovered() && settings().display.video_hover_preview) ||
    (isShortVideo() && canVideoAutoplay());

  // Whether this cell's current image has finished loading (for fade-in).
  // Starts true if the URL was already seen, so recycled cells don't re-fade.
  const [loaded, setLoaded] = createSignal(false);

  // Wall-clock start for the current (fresh) image load, for perf timing.
  // 0 means "don't time this load" (no URL, or already-seen URL).
  let loadStart = 0;

  // Clear error/loaded state when a new URL is provided (e.g. after generation + cache-bust).
  createEffect(on(() => props.thumbSrc, (url) => {
    setErrored(false);
    const seen = url ? loadedUrls.has(url) : false;
    setLoaded(seen);
    // Only timestamp when the perf monitor is active, so this is zero-cost
    // (no performance.now() per load) when the debug overlay is closed.
    loadStart = url && !seen && perfActive() ? performance.now() : 0;
  }));

  // Whether we have a valid URL to attempt loading.
  const hasUrl = () => !!props.thumbSrc && !errored();

  const fadeEnabled = () => settings().display.scroll_blur;

  return (
    <div
      ref={cellRef}
      class={`thumb-cell relative overflow-hidden cursor-pointer ${props.freeSize ? "w-full h-full" : "aspect-square"}`}
      style={{
        background: "#1a1a1a",
        contain: "layout style paint",
        // Let the engine skip rendering/decoding cells that are in the
        // virtual-scroll buffer but off-screen until they scroll into view.
        // Cell size is set externally (grid track / explicit), so the
        // intrinsic-size fallback is only a pre-first-paint placeholder.
        "content-visibility": "auto",
        "contain-intrinsic-size": "auto 250px",
        outline: props.selected ? "2px solid #3b82f6" : "none",
        "outline-offset": "-2px",
      }}
      onClick={(e) => props.onClick(e)}
      onMouseDown={(e) => props.onMouseDown?.(e)}
      onMouseEnter={(e) => props.onMouseEnter?.(e)}
      // Hover preview (e.g. GIF → animated) is a mouse-only affordance. Touch
      // synthesizes a mouseenter that never gets a matching leave, which would
      // leave a cell stuck "hovered" mid-pinch — so drive it from pointer
      // events gated on a real mouse instead.
      onPointerEnter={(e) => { if (e.pointerType === "mouse") setHovered(true); }}
      onPointerLeave={() => setHovered(false)}
      onPointerCancel={() => setHovered(false)}
      onContextMenu={(e) => {
        if (props.onContextMenu) {
          e.preventDefault();
          props.onContextMenu(e);
        }
      }}
    >
      {/* Skeleton: hidden when img has a URL. Always present to avoid
          DOM creation/destruction during <Index> recycling. */}
      <div
        class="absolute inset-0 skeleton-pulse"
        style={{ display: hasUrl() ? "none" : undefined }}
      />

      {/* Img: always present, hidden when no URL. src cleared to cancel
          any in-flight decode when recycled without a URL. */}
      <img
        src={hasUrl() ? effectiveSrc()! : undefined}
        alt={filename()}
        class="w-full h-full object-cover"
        style={{
          display: hasUrl() ? undefined : "none",
          opacity: fadeEnabled() && !loaded() ? "0" : "1",
          transition: fadeEnabled() ? "opacity 0.15s ease-in" : "none",
        }}
        decoding="async"
        draggable={false}
        onLoad={(e) => {
          if (props.thumbSrc) markUrlLoaded(props.thumbSrc);
          if (loadStart > 0) {
            recordImageLoad(performance.now() - loadStart);
            loadStart = 0;
          }
          const img = e.currentTarget;
          if (img.naturalWidth > 0 && img.naturalHeight > 0) {
            props.onImageLoad?.(img.naturalWidth, img.naturalHeight);
          }
          setLoaded(true);
        }}
        onError={() => {
          // Don't treat media URL errors as thumbnail errors
          if (isGif() && hovered()) return;
          setErrored(true);
          props.onError?.(props.path);
        }}
      />

      {/* Canvas GIF playback (desktop), layered over the static thumbnail.
          Keyed on the URL so recycling to a different GIF restarts cleanly. */}
      <Show when={showGifCanvas()}>
        <GifCanvas
          // Cap the animated-frame atlas at the "j" tier — a high-res justified
          // atlas ("jm" 1280px / "jh" 2560px) would decode every frame at that
          // size and blow up memory.
          url={gifAtlasUrl(props.path, props.tier === "jm" || props.tier === "jh" ? "j" : props.tier)}
          class="absolute inset-0 w-full h-full object-cover"
        />
      </Show>

      {/* Muted video hover preview, layered over the static thumbnail. */}
      <Show when={showVideoPreview()}>
        <video
          src={mediaUrl(props.path)}
          class="absolute inset-0 w-full h-full object-cover"
          muted
          autoplay
          preload="auto"
          // Resume where this clip last left off, so scrolling it out of view
          // and back doesn't restart it from the beginning.
          ref={(el) => restoreVideoPosition(el, props.path)}
          onTimeUpdate={(e) => saveVideoPosition(e.currentTarget, props.path)}
          // Manual flushing-seek restart instead of native `loop` — see
          // MediaViewer: native looping drifts the clock and stalls the
          // streamed HTTP source on WebKitGTK.
          onEnded={(e) => {
            const v = e.currentTarget;
            try { v.currentTime = 0; } catch { /* not seekable yet */ }
            void v.play().catch(() => {});
          }}
        />
      </Show>

      {/* Selection check indicator — always present, toggled via display */}
      <div
        class="absolute top-1.5 left-1.5 w-5 h-5 rounded-full flex items-center justify-center text-white text-xs"
        style={{ background: "#3b82f6", display: props.selected ? undefined : "none" }}
      >
        &#10003;
      </div>

      {/* Media badge — always present, toggled via display */}
      <span
        class="absolute bottom-1.5 right-1.5 px-1.5 py-0.5 text-xs font-mono text-white/80 bg-black/50 rounded"
        style={{ display: isVideo() || isGif() ? undefined : "none" }}
      >
        {isVideo() ? "VID" : "GIF"}
      </span>
    </div>
  );
}
