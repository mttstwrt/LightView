import { Show, For, createSignal, createEffect, on, onCleanup, onMount } from "solid-js";
import {
  getMediaMeta,
  getTags,
  addUserTag,
  removeUserTag,
  getAllThumbnailTiers,
  setDescription as saveDescription,
} from "../../lib/ipc";
import { rateItem } from "../../stores/galleryStore";
import type { ThumbnailTierInfo } from "../../lib/ipc";
import { ScrollBar } from "../shared/ScrollBar";
import { hasTouch } from "../../lib/runtime";
import { setInfoPanelOpen } from "../../stores/viewerStore";
import { SWIPE_DISMISS_PX, SWIPE_VELOCITY } from "../../lib/touch";

// On touch devices the panel is an Apple-Photos–style opaque bottom sheet that
// slides up and can be flicked down to dismiss; on desktop it's the side panel.
const sheetMode = hasTouch();

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

export function InfoPanel(props: {
  path: string;
  filename: string;
  /** Called when a tag chip is tapped — filters the gallery to that tag.
   *  Tags containing spaces can't be expressed in the query language, so
   *  their chips stay inert. */
  onTagFilter?: (namespace: string, tag: string) => void;
}) {
  const [meta, setMeta] = createSignal<MetaInfo | null>(null);
  const [description, setDescription] = createSignal("");
  // What the last successful write left on disk, so a commit that changes
  // nothing costs no round-trip.
  let savedDescription = "";
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

  // Persist the description if it differs from what is on disk.
  //
  // Takes the path explicitly because the two callers that matter are *not*
  // the blur handler: navigating to the next image leaves the textarea mounted
  // and focused (so no blur fires), and closing the viewer removes it (which
  // also fires no blur). Both commit against the path being left, not the one
  // arriving.
  const commitDescription = async (path: string) => {
    const next = description().trim();
    if (!path || next === savedDescription) return;
    try {
      await saveDescription(path, next || null);
      savedDescription = next;
    } catch (err) {
      console.error("Failed to set description:", err);
    }
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
      async (path, prevPath) => {
        if (prevPath) await commitDescription(prevPath);
        if (!path) return;
        setMeta(null);
        setDescription("");
        savedDescription = "";
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
            setDescription(result.description ?? "");
            savedDescription = result.description ?? "";
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
      // rateItem dispatches lightview:rating-changed; the listener below
      // updates this panel's rating/lastRated signals.
      await rateItem(props.path, newRating);
    } catch (err) {
      console.error("Failed to set rating:", err);
    }
  };

  // The filter tokenizer splits on whitespace — a tag with spaces would parse
  // as garbage, so only offer tap-to-filter for tags the language can express.
  const tagFilterable = (tag: string) => !!props.onTagFilter && !/\s/.test(tag);

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

  // Closing the viewer removes the textarea without blurring it, so an
  // in-progress description would be lost without this.
  onCleanup(() => {
    void commitDescription(props.path);
  });

  let tagInputRef: HTMLInputElement | undefined;
  const onFocusTagInput = () => {
    // Make sure the user section is expanded so the input is actually mounted,
    // then focus it on the next frame once SolidJS has rendered it.
    setExpandedNamespaces((prev) => {
      if (prev.has("user")) return prev;
      const next = new Set(prev);
      next.add("user");
      return next;
    });
    requestAnimationFrame(() => tagInputRef?.focus());
  };
  window.addEventListener("lightview:focus-tag-input", onFocusTagInput);
  onCleanup(() => window.removeEventListener("lightview:focus-tag-input", onFocusTagInput));

  let panelRef: HTMLDivElement | undefined;

  // --- Bottom-sheet drag-to-dismiss (touch) ------------------------------
  // `entered` drives the slide-up on mount; `dragY` follows a downward flick;
  // `smooth` toggles the CSS transition off mid-drag so the sheet tracks the
  // finger 1:1 and eases on release.
  const [entered, setEntered] = createSignal(false);
  const [dragY, setDragY] = createSignal(0);
  const [smooth, setSmooth] = createSignal(true);

  onMount(() => {
    requestAnimationFrame(() => setEntered(true));
  });

  let dragging = false;
  let dragStartY = 0;
  let dragLastY = 0;
  let dragPrevY = 0;
  let dragLastT = 0;
  let dragPrevT = 0;

  const onHandleDown = (e: PointerEvent) => {
    if (e.pointerType !== "touch") return;
    dragging = true;
    dragStartY = dragLastY = dragPrevY = e.clientY;
    dragLastT = dragPrevT = e.timeStamp;
    setSmooth(false);
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  };
  const onHandleMove = (e: PointerEvent) => {
    if (!dragging) return;
    dragPrevY = dragLastY;
    dragPrevT = dragLastT;
    dragLastY = e.clientY;
    dragLastT = e.timeStamp;
    setDragY(Math.max(0, e.clientY - dragStartY));
  };
  const onHandleUp = () => {
    if (!dragging) return;
    dragging = false;
    setSmooth(true);
    const dy = dragLastY - dragStartY;
    const vy = (dragLastY - dragPrevY) / Math.max(1, dragLastT - dragPrevT);
    if (dy > SWIPE_DISMISS_PX || vy > SWIPE_VELOCITY) {
      setDragY(window.innerHeight); // slide fully down, then unmount
      window.setTimeout(() => setInfoPanelOpen(false), 250);
    } else {
      setDragY(0);
    }
  };

  const sheetTransform = () => {
    if (!entered()) return "translateY(100%)";
    return dragY() > 0 ? `translateY(${dragY()}px)` : "translateY(0)";
  };

  // One compact line instead of a label:value row per fact.
  const metaLine = () => {
    const m = meta();
    if (!m) return "";
    const parts = [m.media_type.toUpperCase(), formatBytes(m.file_size)];
    if (m.width && m.height) parts.push(`${m.width} × ${m.height}`);
    if (m.duration_seconds) parts.push(`${m.duration_seconds.toFixed(1)}s`);
    return parts.join(" · ");
  };

  const fields = () => (
    <div class="text-xs text-neutral-400 space-y-3">
          {/* Header block: filename, muted path, condensed facts. */}
          <div>
            <div class="text-sm text-neutral-200 break-all">{props.filename}</div>
            <div class="text-[11px] text-neutral-600 break-all mt-0.5">{props.path}</div>
            <Show when={meta()}>
              <div class="text-neutral-300 mt-1.5">{metaLine()}</div>
              <Show when={meta()!.date_taken}>
                <div class="text-neutral-500 mt-0.5">{formatDate(meta()!.date_taken!)}</div>
              </Show>
            </Show>
          </div>

          {/* Description — prose about what the file shows. Searchable with
              `desc:` or a bare word, and never suggested as a tag; notes stay
              a separate, private field. Saved on blur. */}
          <div class="border-t border-neutral-800 pt-3 mt-3">
            <div class="text-neutral-500 font-medium mb-1.5">Description</div>
            <textarea
              class="w-full bg-neutral-800/50 rounded px-2 py-1.5 text-neutral-300 placeholder-neutral-600 resize-y min-h-14 focus:outline-none focus:ring-1 focus:ring-neutral-600"
              rows="3"
              placeholder="Describe this image…"
              value={description()}
              onInput={(e) => setDescription(e.currentTarget.value)}
              onBlur={() => void commitDescription(props.path)}
            />
          </div>

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

          {/* Rating — single inline row */}
          <div class="border-t border-neutral-800 pt-3 mt-3 flex items-center gap-3">
            <span class="text-neutral-500">Rating</span>
            <div class="flex gap-1">
              <For each={[1, 2, 3, 4, 5]}>
                {(star) => (
                  <button
                    class="text-base transition-colors cursor-pointer leading-none"
                    style={{ color: star <= rating() ? "#f59e0b" : "#525252" }}
                    onClick={() => handleSetRating(star)}
                    aria-label={`Rate ${star} star${star > 1 ? "s" : ""}`}
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
                                <Show
                                  when={tagFilterable(t.tag)}
                                  fallback={
                                    <span class="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-neutral-400 bg-neutral-800/50">
                                      {t.tag}
                                    </span>
                                  }
                                >
                                  <button
                                    class="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-neutral-400 bg-neutral-800/50 cursor-pointer hover:text-neutral-200 hover:bg-neutral-700/60 transition-colors"
                                    onClick={() => props.onTagFilter!(namespace, t.tag)}
                                    title={`Filter: ${namespace}::${t.tag}`}
                                  >
                                    {t.tag}
                                  </button>
                                </Show>
                              }
                            >
                              <span class="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-neutral-300 bg-neutral-800">
                                <Show when={tagFilterable(t.tag)} fallback={t.tag}>
                                  <button
                                    class="cursor-pointer hover:text-white transition-colors"
                                    onClick={() => props.onTagFilter!(namespace, t.tag)}
                                    title={`Filter: ${namespace}::${t.tag}`}
                                  >
                                    {t.tag}
                                  </button>
                                </Show>
                                <button
                                  class="text-neutral-500 hover:text-neutral-200 cursor-pointer"
                                  onClick={() => handleRemoveTag(t.tag)}
                                  aria-label={`Remove tag ${t.tag}`}
                                >
                                  &times;
                                </button>
                              </span>
                            </Show>
                          )}
                        </For>
                      </div>
                    </Show>
                    <Show when={namespace === "user"}>
                      <form onSubmit={handleAddTag} class="flex gap-1 mt-2">
                        <input
                          ref={tagInputRef}
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
  );

  // --- Touch: opaque bottom sheet ----------------------------------------
  // The sheet overlays the photo — no dim layer, no photo re-fit. The photo
  // stays full-screen behind it; a tap or swipe-down on the visible part of
  // the photo dismisses the sheet (handled by the viewer's gestures).
  if (sheetMode) {
    return (
      <div
        class="fixed left-0 right-0 bottom-0 z-[60] rounded-t-2xl flex flex-col max-h-[70vh] shadow-2xl"
        style={{
          // Opaque — Apple-Photos info sheet, not a translucent overlay.
          background: "#1c1c1e",
          transform: sheetTransform(),
          transition: smooth() ? "transform 0.3s ease-out" : "none",
          "padding-bottom": "env(safe-area-inset-bottom)",
        }}
        onClick={(e) => e.stopPropagation()}
      >
        {/* Drag handle / grab area — flick down here to dismiss. */}
        <div
          class="shrink-0 pt-2.5 pb-2 cursor-grab touch-none"
          onPointerDown={onHandleDown}
          onPointerMove={onHandleMove}
          onPointerUp={onHandleUp}
          onPointerCancel={onHandleUp}
        >
          <div class="mx-auto h-1 w-9 rounded-full bg-neutral-600" />
        </div>
        <div class="px-4 pb-6 overflow-y-auto overscroll-contain hide-scrollbar">
          {fields()}
        </div>
      </div>
    );
  }

  // --- Desktop: side panel -----------------------------------------------
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
      <div ref={panelRef} class="h-full w-full p-4 overflow-y-auto hide-scrollbar">
        <h3 class="text-sm font-medium text-neutral-300 mb-4">Info</h3>
        {fields()}
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
