import { createMemo, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import {
  deleteUserTags,
  listUserTags,
  mergeUserTags,
  renameUserTag,
  type TagEditResult,
  type UserTagSummary,
} from "../lib/ipc";
import { ConfirmButton } from "./shared/ConfirmButton";

type SortMode = "count" | "name";

/** Gallery-wide user-tag management: rename, merge, and delete tags across
 *  every file that carries them.
 *
 *  Edits go straight to the companion sidecars on disk, so the panel reloads
 *  its list from the backend after each one rather than patching local state —
 *  a merge can change several rows at once, and the counts must stay honest.
 *  `onChanged` re-runs the active filter so the grid behind reflects the new
 *  tags (a tag-based filter may now match a different set of files). */
export function TagManagerPanel(props: { onClose: () => void; onChanged?: () => void }) {
  const [tags, setTags] = createSignal<UserTagSummary[]>([]);
  const [loading, setLoading] = createSignal(true);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [search, setSearch] = createSignal("");
  const [sortMode, setSortMode] = createSignal<SortMode>("count");
  const [selected, setSelected] = createSignal<Set<string>>(new Set());
  const [editing, setEditing] = createSignal<string | null>(null);
  const [editValue, setEditValue] = createSignal("");
  const [mergeTarget, setMergeTarget] = createSignal("");
  // Once the user types a target of their own, stop auto-filling it.
  const [targetEdited, setTargetEdited] = createSignal(false);

  const load = async () => {
    setError(null);
    try {
      const list = await listUserTags();
      setTags(list);
      // Drop selections for tags that no longer exist (merged away, deleted,
      // or renamed) so the merge bar can't act on a stale name.
      const live = new Set(list.map((t) => t.tag));
      setSelected((prev) => new Set([...prev].filter((t) => live.has(t))));
    } catch (e) {
      setError(String(e));
    }
    setLoading(false);
  };
  onMount(load);

  const handleKey = (e: KeyboardEvent) => {
    if (e.key !== "Escape") return;
    e.stopPropagation();
    // Escape backs out of the rename field first, then out of the panel.
    // Clearing the draft makes the blur that follows a no-op commit.
    if (editing()) {
      setEditValue("");
      setEditing(null);
    } else {
      props.onClose();
    }
  };
  window.addEventListener("keydown", handleKey, true);
  onCleanup(() => window.removeEventListener("keydown", handleKey, true));

  const visible = createMemo(() => {
    const q = search().trim().toLowerCase();
    const list = q ? tags().filter((t) => t.tag.toLowerCase().includes(q)) : tags();
    return sortMode() === "name"
      ? [...list].sort((a, b) => a.tag.localeCompare(b.tag))
      : list; // backend already returns count desc, tag asc
  });

  const selectedTags = () => tags().filter((t) => selected().has(t.tag));

  const toggle = (tag: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(tag)) next.delete(tag);
      else next.add(tag);
      return next;
    });
    // Default the merge target to the most-used selected tag — the usual
    // intent is folding strays into the established spelling. Tracks the
    // selection until the user types a target of their own.
    if (!targetEdited()) setMergeTarget(selectedTags()[0]?.tag ?? "");
  };

  /** Run one edit, then reload the list and refresh the gallery behind. */
  const run = async (action: () => Promise<TagEditResult>, describe: (r: TagEditResult) => string) => {
    if (busy()) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const result = await action();
      setNotice(describe(result));
      if (result.filesFailed > 0) {
        setError(
          `${result.filesFailed} file${result.filesFailed === 1 ? "" : "s"} could not be updated — ` +
            "the companion sidecar is missing or not writable.",
        );
      }
      await load();
      props.onChanged?.();
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  const fileWord = (n: number) => `${n} file${n === 1 ? "" : "s"}`;

  const commitRename = (from: string) => {
    const to = editValue().trim();
    setEditing(null);
    if (!to || to === from) return;
    const merging = tags().some((t) => t.tag === to);
    run(
      () => renameUserTag(from, to),
      (r) =>
        merging
          ? `Merged "${from}" into "${to}" across ${fileWord(r.filesChanged)}`
          : `Renamed "${from}" to "${to}" across ${fileWord(r.filesChanged)}`,
    );
  };

  const handleMerge = () => {
    const target = mergeTarget().trim();
    const sources = selectedTags().map((t) => t.tag);
    if (!target || sources.length === 0) return;
    run(
      () => mergeUserTags(sources, target),
      (r) => `Merged ${sources.length} tags into "${target}" across ${fileWord(r.filesChanged)}`,
    );
    setSelected(new Set<string>());
    setTargetEdited(false);
  };

  const handleDelete = (tagList: string[]) => {
    if (tagList.length === 0) return;
    run(
      () => deleteUserTags(tagList),
      (r) =>
        tagList.length === 1
          ? `Deleted "${tagList[0]}" from ${fileWord(r.filesChanged)}`
          : `Deleted ${tagList.length} tags from ${fileWord(r.filesChanged)}`,
    );
    setSelected((prev) => {
      const next = new Set(prev);
      for (const t of tagList) next.delete(t);
      return next;
    });
  };

  return (
    <div
      ref={(el) => {
        // Capture wheel events so the gallery grid underneath never scrolls
        const stop = (e: WheelEvent) => e.stopPropagation();
        el.addEventListener("wheel", stop, { passive: false });
        onCleanup(() => el.removeEventListener("wheel", stop));
      }}
      class="fixed inset-0 z-[200] flex flex-col"
      style={{ background: "rgba(10, 10, 10, 0.98)" }}
    >
      {/* Header */}
      <div class="flex items-center justify-between gap-4 px-6 py-4 border-b border-neutral-800/60">
        <div class="flex items-center gap-4 min-w-0">
          <span class="text-sm font-medium text-neutral-200 whitespace-nowrap">Manage Tags</span>
          <Show when={!loading()}>
            <span class="text-xs text-neutral-500 whitespace-nowrap">
              {tags().length} user {tags().length === 1 ? "tag" : "tags"} · edits apply to the whole
              gallery
            </span>
          </Show>
        </div>
        <div class="flex items-center gap-3">
          <input
            value={search()}
            onInput={(e) => setSearch(e.currentTarget.value)}
            placeholder="Search tags..."
            class="w-44 px-2.5 py-1 text-xs rounded bg-neutral-900 text-neutral-200 placeholder-neutral-600 border border-neutral-800 focus:border-neutral-600 focus:outline-none"
          />
          <button
            onClick={() => setSortMode(sortMode() === "count" ? "name" : "count")}
            class="px-2.5 py-1 text-[10px] rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-200 whitespace-nowrap"
          >
            Sort: {sortMode() === "count" ? "Most used" : "A–Z"}
          </button>
          <button
            onClick={props.onClose}
            class="w-8 h-8 flex items-center justify-center text-neutral-400 hover:text-neutral-200 rounded transition-colors cursor-pointer"
          >
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M18 6L6 18M6 6l12 12" />
            </svg>
          </button>
        </div>
      </div>

      {/* Merge bar — only meaningful with a selection */}
      <Show when={selected().size > 0}>
        <div class="flex items-center gap-3 px-6 py-2 border-b border-neutral-800/60 bg-neutral-900/40 flex-wrap">
          <span class="text-xs text-neutral-400 whitespace-nowrap">
            {selected().size} selected
          </span>
          <span class="text-[11px] text-neutral-500 whitespace-nowrap">merge into</span>
          <input
            value={mergeTarget()}
            onInput={(e) => {
              setTargetEdited(true);
              setMergeTarget(e.currentTarget.value);
            }}
            onKeyDown={(e) => e.key === "Enter" && handleMerge()}
            placeholder="target tag"
            class="w-48 px-2.5 py-1 text-xs rounded bg-neutral-900 text-neutral-200 placeholder-neutral-600 border border-neutral-800 focus:border-neutral-600 focus:outline-none"
          />
          <button
            onClick={handleMerge}
            disabled={busy() || !mergeTarget().trim()}
            class="px-2.5 py-1 text-[10px] rounded cursor-pointer transition-colors bg-teal-700/50 text-teal-200 hover:bg-teal-600/60 disabled:opacity-40 disabled:cursor-default whitespace-nowrap"
          >
            Merge
          </button>
          <ConfirmButton
            label="Delete Selected"
            confirmLabel="Delete from all files?"
            disabled={busy()}
            onConfirm={() => handleDelete([...selected()])}
          />
          <button
            onClick={() => {
              setSelected(new Set<string>());
              setTargetEdited(false);
              setMergeTarget("");
            }}
            class="px-2.5 py-1 text-[10px] rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-200"
          >
            Clear
          </button>
        </div>
      </Show>

      <Show when={error()}>
        <div class="px-6 py-2 text-xs text-red-400 border-b border-red-900/40 bg-red-950/20">
          {error()}
        </div>
      </Show>
      <Show when={notice() && !error()}>
        <div class="px-6 py-2 text-xs text-teal-300/80 border-b border-teal-900/30 bg-teal-950/10">
          {notice()}
        </div>
      </Show>

      {/* Tag list */}
      <div class="flex-1 overflow-y-auto px-6 py-4">
        <Show when={loading()}>
          <div class="flex items-center justify-center h-full text-neutral-500 text-sm">
            Loading...
          </div>
        </Show>
        <Show when={!loading() && tags().length === 0}>
          <div class="flex items-center justify-center h-full text-neutral-600 text-sm">
            No user tags in this gallery yet
          </div>
        </Show>
        <Show when={!loading() && tags().length > 0 && visible().length === 0}>
          <div class="flex items-center justify-center h-full text-neutral-600 text-sm">
            No tags match "{search()}"
          </div>
        </Show>
        <Show when={visible().length > 0}>
          <div class="flex flex-col gap-1 max-w-3xl mx-auto">
            <For each={visible()}>
              {(entry) => (
                <div
                  class="flex items-center gap-3 px-3 py-2 rounded-lg transition-colors hover:bg-neutral-900/60"
                  style={{ border: "1px solid rgba(255,255,255,0.04)" }}
                >
                  <input
                    type="checkbox"
                    checked={selected().has(entry.tag)}
                    onChange={() => toggle(entry.tag)}
                    class="cursor-pointer accent-teal-600"
                  />
                  <div class="flex-1 min-w-0">
                    <Show
                      when={editing() === entry.tag}
                      fallback={
                        <span class="text-xs text-neutral-200 truncate" title={entry.tag}>
                          {entry.tag}
                        </span>
                      }
                    >
                      <input
                        ref={(el) => requestAnimationFrame(() => { el.focus(); el.select(); })}
                        value={editValue()}
                        onInput={(e) => setEditValue(e.currentTarget.value)}
                        onKeyDown={(e) => {
                          if (e.key === "Enter") commitRename(entry.tag);
                        }}
                        onBlur={() => commitRename(entry.tag)}
                        class="w-full px-2 py-0.5 text-xs rounded bg-neutral-900 text-neutral-100 border border-neutral-700 focus:border-neutral-500 focus:outline-none"
                      />
                    </Show>
                  </div>
                  <span class="text-[10px] text-neutral-500 whitespace-nowrap">
                    {entry.count} {entry.count === 1 ? "file" : "files"}
                  </span>
                  <button
                    onClick={() => {
                      setEditValue(entry.tag);
                      setEditing(entry.tag);
                    }}
                    disabled={busy()}
                    class="px-2.5 py-1 text-[10px] rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-200 disabled:opacity-40 disabled:cursor-default"
                  >
                    Rename
                  </button>
                  <ConfirmButton
                    label="Delete"
                    confirmLabel={`Remove from ${entry.count}?`}
                    disabled={busy()}
                    onConfirm={() => handleDelete([entry.tag])}
                  />
                </div>
              )}
            </For>
          </div>
        </Show>
      </div>

      <Show when={busy()}>
        <div class="px-6 py-2 text-xs text-neutral-400 border-t border-neutral-800/60">
          Rewriting companion files...
        </div>
      </Show>
    </div>
  );
}
