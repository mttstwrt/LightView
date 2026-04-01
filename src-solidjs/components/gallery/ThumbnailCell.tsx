import { Show, createSignal, createEffect, on } from "solid-js";
import { settings } from "../../stores/settingsStore";

interface ThumbnailCellProps {
  path: string;
  /** Protocol URL for the thumbnail image. */
  thumbSrc: string | null;
  selected: boolean;
  onClick: (e: MouseEvent) => void;
  onMouseDown?: (e: MouseEvent) => void;
  onMouseEnter?: (e: MouseEvent) => void;
  onContextMenu?: (e: MouseEvent) => void;
  /** Called when the protocol handler returns 404 (thumbnail not cached). */
  onError?: (path: string) => void;
}

// Module-level set of URLs that have already been loaded at least once.
// Survives cell recycling so a revisited thumbnail won't re-fade.
const loadedUrls = new Set<string>();

export function ThumbnailCell(props: ThumbnailCellProps) {
  const [errored, setErrored] = createSignal(false);

  const filename = () => {
    const parts = props.path.split("/");
    return parts[parts.length - 1] || props.path;
  };

  const ext = () => {
    const name = filename();
    const dot = name.lastIndexOf(".");
    return dot >= 0 ? name.slice(dot + 1).toLowerCase() : "";
  };

  const isVideo = () => {
    const e = ext();
    return ["mp4", "mov", "avi", "mkv", "webm", "m4v", "wmv", "flv"].includes(e);
  };

  const isGif = () => ext() === "gif";

  // Whether this cell's current image has finished loading (for fade-in).
  // Starts true if the URL was already seen, so recycled cells don't re-fade.
  const [loaded, setLoaded] = createSignal(false);

  // Clear error/loaded state when a new URL is provided (e.g. after generation + cache-bust).
  createEffect(on(() => props.thumbSrc, (url) => {
    setErrored(false);
    setLoaded(url ? loadedUrls.has(url) : false);
  }));

  // Whether we have a valid URL to attempt loading.
  const hasUrl = () => !!props.thumbSrc && !errored();

  const fadeEnabled = () => settings().display.scroll_blur;

  return (
    <div
      class="thumb-cell relative aspect-square overflow-hidden cursor-pointer"
      style={{
        background: "#1a1a1a",
        contain: "layout style paint",
        outline: props.selected ? "2px solid #3b82f6" : "none",
        "outline-offset": "-2px",
      }}
      onClick={(e) => props.onClick(e)}
      onMouseDown={(e) => props.onMouseDown?.(e)}
      onMouseEnter={(e) => props.onMouseEnter?.(e)}
      onContextMenu={(e) => {
        if (props.onContextMenu) {
          e.preventDefault();
          props.onContextMenu(e);
        }
      }}
    >
      {/*
        Skeleton: visible only when there is no URL to load (not yet assigned,
        or errored). Hidden via display:none when an img is present so it
        doesn't pulse underneath a loaded thumbnail.

        We intentionally do NOT track an onLoad "loaded" signal. With <Index>,
        every cell is recycled on each row shift. Resetting a loaded flag would
        flash the skeleton across the entire grid. Instead we rely on
        decoding="async" — the browser keeps the previous frame while decoding
        the new image, giving a seamless swap with no skeleton flash.
      */}
      <Show when={!hasUrl()}>
        <div class="absolute inset-0 skeleton-pulse" />
      </Show>

      <Show when={hasUrl()}>
        <img
          src={props.thumbSrc!}
          alt={filename()}
          class="w-full h-full object-cover"
          style={{
            opacity: fadeEnabled() && !loaded() ? "0" : "1",
            transition: fadeEnabled() ? "opacity 0.15s ease-in" : "none",
          }}
          decoding="async"
          draggable={false}
          onLoad={() => {
            if (props.thumbSrc) loadedUrls.add(props.thumbSrc);
            setLoaded(true);
          }}
          onError={() => {
            setErrored(true);
            props.onError?.(props.path);
          }}
        />
      </Show>

      {/* Selection check indicator */}
      <Show when={props.selected}>
        <div
          class="absolute top-1.5 left-1.5 w-5 h-5 rounded-full flex items-center justify-center text-white text-xs"
          style={{ background: "#3b82f6" }}
        >
          &#10003;
        </div>
      </Show>

      <Show when={isVideo()}>
        <span class="absolute bottom-1.5 right-1.5 px-1.5 py-0.5 text-xs font-mono text-white/80 bg-black/50 rounded">
          VID
        </span>
      </Show>

      <Show when={isGif()}>
        <span class="absolute bottom-1.5 right-1.5 px-1.5 py-0.5 text-xs font-mono text-white/80 bg-black/50 rounded">
          GIF
        </span>
      </Show>
    </div>
  );
}
