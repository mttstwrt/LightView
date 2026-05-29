import { Show, For, createSignal, createEffect, on, onCleanup } from "solid-js";
import {
  getMediaMeta,
  getTags,
  addUserTag,
  removeUserTag,
  setRating as setRatingIpc,
  getAllThumbnailTiers,
} from "../../lib/ipc";
import type { ThumbnailTierInfo } from "../../lib/ipc";
import { ScrollBar } from "../shared/ScrollBar";
import { isWeb } from "../../lib/runtime";

// The web client is read-only; editing controls are hidden.
const readOnly = isWeb();

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

export function InfoPanel(props: { path: string; filename: string }) {
  const [meta, setMeta] = createSignal<MetaInfo | null>(null);
  const [tags, setTags] = createSignal<{ namespace: string; tag: string }[]>([]);
  const [newTag, setNewTag] = createSignal("");
  const [rating, setRating] = createSignal(0);
  const [lastRated, setLastRated] = createSignal<number | null>(null);
  const [thumbTiers, setThumbTiers] = createSignal<ThumbnailTierInfo[]>([]);
  const [thumbExpanded, setThumbExpanded] = createSignal(false);
  const [expandedNamespaces, setExpandedNamespaces] = createSignal<Set<string>>(
    new Set(["user"]),
  );

  const toggleNamespace = (ns: string) => {
    setExpandedNamespaces((prev) => {
      const next = new Set(prev);
      if (next.has(ns)) next.delete(ns);
      else next.add(ns);
      return next;
    });
  };

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

  const tagsByNamespace = () => {
    const groups = new Map<string, { namespace: string; tag: string }[]>();
    for (const t of tags()) {
      const list = groups.get(t.namespace);
      if (list) list.push(t);
      else groups.set(t.namespace, [t]);
    }
    if (!groups.has("user")) groups.set("user", []);
    return Array.from(groups.entries()).sort(([a], [b]) => {
      if (a === "user") return -1;
      if (b === "user") return 1;
      return a.localeCompare(b);
    });
  };

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
                      class="text-base transition-colors"
                      classList={{ "cursor-pointer": !readOnly, "cursor-default": readOnly }}
                      style={{ color: star <= rating() ? "#f59e0b" : "#525252" }}
                      disabled={readOnly}
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

          {/* Tags — one collapsible section per namespace */}
          <For each={tagsByNamespace()}>
            {([namespace, nsTags]) => (
              <div class="border-t border-neutral-800 pt-3 mt-3">
                <button
                  class="flex items-center gap-1.5 text-neutral-500 font-medium cursor-pointer hover:text-neutral-400 transition-colors w-full text-left"
                  onClick={() => toggleNamespace(namespace)}
                >
                  <svg
                    class="w-3 h-3 transition-transform"
                    style={{
                      transform: expandedNamespaces().has(namespace)
                        ? "rotate(90deg)"
                        : "rotate(0deg)",
                    }}
                    fill="none"
                    stroke="currentColor"
                    viewBox="0 0 24 24"
                    stroke-width="2.5"
                  >
                    <path stroke-linecap="round" stroke-linejoin="round" d="M9 5l7 7-7 7" />
                  </svg>
                  {namespace}
                  <span class="text-neutral-600 font-normal text-[11px]">
                    ({nsTags.length})
                  </span>
                </button>
                <Show when={expandedNamespaces().has(namespace)}>
                  <div class="mt-2">
                    <Show when={nsTags.length > 0}>
                      <div class="flex flex-wrap gap-1">
                        <For each={nsTags}>
                          {(t) => (
                            <Show
                              when={namespace === "user"}
                              fallback={
                                <span class="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-neutral-400 bg-neutral-800/50">
                                  {t.tag}
                                </span>
                              }
                            >
                              <span class="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-neutral-300 bg-neutral-800">
                                {t.tag}
                                <Show when={!readOnly}>
                                  <button
                                    class="text-neutral-500 hover:text-neutral-200 cursor-pointer"
                                    onClick={() => handleRemoveTag(t.tag)}
                                  >
                                    &times;
                                  </button>
                                </Show>
                              </span>
                            </Show>
                          )}
                        </For>
                      </div>
                    </Show>
                    <Show when={namespace === "user" && !readOnly}>
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
                    </Show>
                  </div>
                </Show>
              </div>
            )}
          </For>
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
