import { Show, For, createSignal, createEffect, on } from "solid-js";
import { convertFileSrc } from "@tauri-apps/api/core";
import { infoPanelOpen } from "../../stores/viewerStore";
import { getFullMedia, getMediaMeta, getTags, addUserTag, removeUserTag, setRating as setRatingIpc, getCachedThumbnailInfo } from "../../lib/ipc";
import type { CachedThumbnailInfo } from "../../lib/ipc";

interface MediaViewerProps {
  paths: string[];
  currentIndex: number;
  onClose: () => void;
  onNext: () => void;
  onPrev: () => void;
}

export function MediaViewer(props: MediaViewerProps) {
  const [mediaSrc, setMediaSrc] = createSignal<string>("");
  const [loaded, setLoaded] = createSignal(false);

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

  // Load full media when index changes.
  // Videos use Tauri's asset protocol for streaming (data URIs crash WebKitGTK
  // on large files). Images use the existing data URI path.
  createEffect(
    on(
      () => props.currentIndex,
      async (idx) => {
        const path = props.paths[idx];
        if (!path) return;

        setLoaded(false);

        const e = path.split(".").pop()?.toLowerCase() ?? "";
        const videoExts = ["mp4", "mov", "avi", "mkv", "webm", "m4v", "wmv", "flv"];

        if (videoExts.includes(e)) {
          // Stream video directly from disk via Tauri's asset protocol.
          setMediaSrc(convertFileSrc(path));
        } else {
          try {
            const dataUri = await getFullMedia(path);
            setMediaSrc(dataUri);
          } catch (err) {
            console.error("Failed to load media:", err);
          }
        }
      },
    ),
  );

  const handleBackdropClick = (e: MouseEvent) => {
    if (e.target === e.currentTarget) {
      props.onClose();
    }
  };

  return (
    <div
      class="fixed inset-0 z-50 flex items-center justify-center"
      style={{ background: "rgba(0, 0, 0, 0.95)" }}
      onClick={handleBackdropClick}
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
        <Show when={isVideo() && mediaSrc()}>
          <video
            src={mediaSrc()}
            controls
            autoplay
            class="max-w-[90vw] max-h-[90vh] object-contain"
            style={{ outline: "none" }}
            onError={(e) => console.error("Video playback error:", (e.target as HTMLVideoElement).error)}
            onLoadedData={() => setLoaded(true)}
          />
        </Show>

        <Show when={!isVideo() && mediaSrc()}>
          <img
            src={mediaSrc()}
            alt={filename()}
            class="max-w-[90vw] max-h-[90vh] object-contain transition-opacity duration-150"
            style={{ opacity: loaded() ? "1" : "0" }}
            onLoad={() => setLoaded(true)}
            draggable={false}
          />
        </Show>

        <Show when={!mediaSrc()}>
          <div class="text-neutral-500 text-sm">Loading...</div>
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
        {filename()} — {props.currentIndex + 1} / {props.paths.length}
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
  const [thumbInfo, setThumbInfo] = createSignal<CachedThumbnailInfo | null>(null);

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
        setThumbInfo(null);
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
          }
        } catch {}
        try {
          const ti = await getCachedThumbnailInfo(path);
          setThumbInfo(ti);
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
    } catch (err) {
      console.error("Failed to set rating:", err);
    }
  };

  const userTags = () => tags().filter((t) => t.namespace === "user");
  const otherTags = () => tags().filter((t) => t.namespace !== "user");

  return (
    <div
      class="absolute right-0 top-0 h-full w-80 p-4 overflow-y-auto"
      style={{
        background: "rgba(10, 10, 10, 0.9)",
        "backdrop-filter": "blur(12px)",
        "border-left": "1px solid rgba(255,255,255,0.05)",
      }}
      onClick={(e) => e.stopPropagation()}
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

        {/* Thumbnail */}
        <Show when={thumbInfo()}>
          <div class="border-t border-neutral-800 pt-3 mt-3 space-y-3">
            <span class="text-neutral-500 font-medium">Thumbnail</span>
            <InfoRow label="Dimensions" value={`${thumbInfo()!.width} × ${thumbInfo()!.height}`} />
            <InfoRow label="Format" value={thumbInfo()!.format.toUpperCase()} />
            <InfoRow label="Resize" value={thumbInfo()!.resize_filter.charAt(0).toUpperCase() + thumbInfo()!.resize_filter.slice(1)} />
            <InfoRow label="Cache size" value={formatBytes(thumbInfo()!.size_bytes)} />
          </div>
        </Show>

        {/* Rating */}
        <div class="border-t border-neutral-800 pt-3 mt-3">
          <span class="text-neutral-500">Rating:</span>
          <div class="flex gap-1 mt-1">
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

