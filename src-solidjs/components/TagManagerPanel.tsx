import { createEffect, createMemo, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import {
  deleteUserTags,
  listUserTags,
  mergeUserTags,
  pathsForUserTags,
  renameUserTag,
  thumbUrl,
  type TagEditResult,
  type UserTagSummary,
} from "../lib/ipc";
import { ConfirmButton } from "./shared/ConfirmButton";
import { isMobile } from "../lib/runtime";

type SortMode = "count" | "name";

/** Gallery-wide user-tag management: rename, merge, and delete tags across
 *  every file that carries them.
 *
 *  Laid out as a wrapping cloud of chips rather than a list of rows: tag names
 *  are short, so one-per-line burned most of the width and put the actions in
 *  10px buttons no thumb can hit. Chips pack the full width, each one shaded
 *  in proportion to how many files carry it — so "most used" stays readable
 *  even in A–Z order — and the actions move to a single bar that follows the
 *  selection. One target field serves both rename (one tag selected) and merge
 *  (several), which is the same operation on the backend.
 *
 *  The space below the cloud shows the files the selection actually covers —
 *  these edits rewrite every file carrying the tag, and the count alone is a
 *  thin thing to confirm a gallery-wide delete against.
 *
 *  Edits go straight to the companion sidecars on disk, so the panel reloads
 *  its list from the backend after each one rather than patching local state —
 *  a merge can change several chips at once, and the counts must stay honest.
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
  const [target, setTarget] = createSignal("");
  // Once the user types a target of their own, stop tracking the selection.
  const [targetEdited, setTargetEdited] = createSignal(false);
  const [preview, setPreview] = createSignal<string[]>([]);
  let targetRef: HTMLInputElement | undefined;

  // Enough to fill the preview area on a large screen without pulling tens of
  // thousands of paths back for a tag that covers the whole gallery.
  const PREVIEW_LIMIT = 120;

  const load = async () => {
    setError(null);
    try {
      const list = await listUserTags();
      setTags(list);
      // Drop selections for tags that no longer exist (merged away, deleted,
      // or renamed) so the action bar can't act on a stale name.
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
    // Escape drops the selection first, then closes the panel.
    if (selected().size > 0) clearSelection();
    else props.onClose();
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

  const maxCount = createMemo(() => tags().reduce((m, t) => Math.max(m, t.count), 1));
  /** Selected tags in backend order, i.e. most-used first. */
  const selectedTags = createMemo(() => tags().filter((t) => selected().has(t.tag)));

  const syncTarget = () => {
    if (!targetEdited()) setTarget(selectedTags()[0]?.tag ?? "");
  };

  const clearSelection = () => {
    setSelected(new Set<string>());
    setTargetEdited(false);
    setTarget("");
  };

  const toggle = (tag: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(tag)) next.delete(tag);
      else next.add(tag);
      return next;
    });
    // Default the target to the most-used selected tag: renaming pre-fills
    // with the current name, merging folds strays into the established
    // spelling. Tracks the selection until the user types something else.
    syncTarget();
  };

  const selectOnly = (tag: string) => {
    setSelected(new Set([tag]));
    setTargetEdited(false);
    syncTarget();
    // Desktop double-click is the "rename this one" shortcut, so put the
    // cursor where the typing goes. Never on touch — it would throw up the
    // keyboard over the tags on every tap.
    if (!isMobile()) requestAnimationFrame(() => targetRef?.select());
  };

  const selectAllVisible = () => {
    setSelected(new Set(visible().map((t) => t.tag)));
    syncTarget();
  };

  // What the selection covers. Re-fetched whenever it changes — including
  // after an edit, since `selected` is rebuilt against the reloaded tag list.
  // The token drops a slow response that lands after a newer one.
  let previewToken = 0;
  createEffect(() => {
    const tagList = [...selected()];
    const token = ++previewToken;
    if (tagList.length === 0) {
      setPreview([]);
      return;
    }
    pathsForUserTags(tagList, PREVIEW_LIMIT)
      .then((paths) => token === previewToken && setPreview(paths))
      .catch(() => token === previewToken && setPreview([]));
  });

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

  // A single selected tag renamed onto an existing name is a merge — same
  // command either way, but the button should say what will happen.
  const isMerge = () =>
    selected().size > 1 || tags().some((t) => t.tag === target().trim() && !selected().has(t.tag));

  const canApply = () => {
    const to = target().trim();
    if (busy() || !to || selected().size === 0) return false;
    // Renaming a lone tag to itself is the pre-filled state — nothing to do.
    return selected().size > 1 || selectedTags()[0]?.tag !== to;
  };

  const apply = () => {
    if (!canApply()) return;
    const to = target().trim();
    const sources = selectedTags().map((t) => t.tag);
    const merging = isMerge();
    if (sources.length === 1) {
      const from = sources[0];
      run(
        () => renameUserTag(from, to),
        (r) =>
          merging
            ? `Merged "${from}" into "${to}" across ${fileWord(r.filesChanged)}`
            : `Renamed "${from}" to "${to}" across ${fileWord(r.filesChanged)}`,
      );
    } else {
      run(
        () => mergeUserTags(sources, to),
        (r) => `Merged ${sources.length} tags into "${to}" across ${fileWord(r.filesChanged)}`,
      );
    }
    clearSelection();
  };

  const deleteSelected = () => {
    const tagList = selectedTags().map((t) => t.tag);
    if (tagList.length === 0) return;
    run(
      () => deleteUserTags(tagList),
      (r) =>
        tagList.length === 1
          ? `Deleted "${tagList[0]}" from ${fileWord(r.filesChanged)}`
          : `Deleted ${tagList.length} tags from ${fileWord(r.filesChanged)}`,
    );
    clearSelection();
  };

  // ── Pieces shared by both layouts ──────────────────────────────────────

  const searchBox = (extraClass: string) => (
    <input
      value={search()}
      onInput={(e) => setSearch(e.currentTarget.value)}
      placeholder="Search tags..."
      // 16px keeps iOS Safari from zooming the page on focus.
      style={isMobile() ? { "font-size": "16px" } : undefined}
      class={`px-2.5 rounded bg-neutral-900 text-neutral-200 placeholder-neutral-600 border border-neutral-800 focus:border-neutral-600 focus:outline-none ${extraClass}`}
    />
  );

  const sortToggle = () => (
    <div class="flex shrink-0 rounded overflow-hidden border border-neutral-800">
      <For each={[["count", "Most used"], ["name", "A–Z"]] as const}>
        {([mode, label]) => (
          <button
            onClick={() => setSortMode(mode)}
            class="px-2.5 text-[11px] cursor-pointer transition-colors whitespace-nowrap"
            classList={{
              "h-9": isMobile(),
              "h-7": !isMobile(),
              "bg-neutral-700 text-neutral-100": sortMode() === mode,
              "bg-neutral-900 text-neutral-500 hover:text-neutral-300": sortMode() !== mode,
            }}
          >
            {label}
          </button>
        )}
      </For>
    </div>
  );

  const closeButton = () => (
    <button
      onClick={props.onClose}
      class="w-9 h-9 shrink-0 flex items-center justify-center text-neutral-400 hover:text-neutral-200 rounded transition-colors cursor-pointer"
    >
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M18 6L6 18M6 6l12 12" />
      </svg>
    </button>
  );

  // The "edits apply to the whole gallery" caveat is the point of the screen,
  // but it doesn't survive a phone's header width — there the action bar's
  // hint carries the meaning instead.
  const summary = () => (
    <span class="text-xs text-neutral-500 truncate">
      {tags().length} user {tags().length === 1 ? "tag" : "tags"}
      {search().trim() ? ` · ${visible().length} shown` : ""}
      {isMobile() ? "" : " · edits apply to the whole gallery"}
    </span>
  );

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
      {/* ── Header. One row on desktop; on a phone the controls need the full
             width, so the title/close row sits above search + sort. ── */}
      <div class="shrink-0 border-b border-neutral-800/60">
        <Show
          when={isMobile()}
          fallback={
            <div class="flex items-center gap-4 px-6 py-3">
              <span class="text-sm font-medium text-neutral-200 whitespace-nowrap">Manage Tags</span>
              <Show when={!loading()}>{summary()}</Show>
              <div class="flex-1" />
              {searchBox("w-52 h-7 text-xs")}
              {sortToggle()}
              {closeButton()}
            </div>
          }
        >
          <div class="flex flex-col gap-2 px-3 py-2.5">
            <div class="flex items-center gap-3">
              <span class="text-sm font-medium text-neutral-200 whitespace-nowrap">Manage Tags</span>
              <Show when={!loading()}>{summary()}</Show>
              <div class="flex-1" />
              {closeButton()}
            </div>
            <div class="flex items-center gap-2">
              {searchBox("flex-1 min-w-0 h-9 text-sm")}
              {sortToggle()}
            </div>
          </div>
        </Show>
      </div>

      <Show when={error()}>
        <div class="shrink-0 px-6 py-2 text-xs text-red-400 border-b border-red-900/40 bg-red-950/20">
          {error()}
        </div>
      </Show>
      <Show when={notice() && !error()}>
        <div class="shrink-0 px-6 py-2 text-xs text-teal-300/80 border-b border-teal-900/30 bg-teal-950/10">
          {notice()}
        </div>
      </Show>

      {/* ── Tag cloud, over a preview of what the selection covers ── */}
      <div class="flex-1 min-h-0 flex flex-col">
      <div class="flex-1 min-h-0 overflow-y-auto px-3 sm:px-6 py-4">
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
          <div class="flex flex-wrap content-start gap-2 max-w-[1400px] mx-auto">
            <For each={visible()}>
              {(entry) => {
                const isSelected = () => selected().has(entry.tag);
                return (
                  <button
                    onClick={() => toggle(entry.tag)}
                    onDblClick={() => selectOnly(entry.tag)}
                    title={`${entry.tag} — ${entry.count} ${entry.count === 1 ? "file" : "files"}`}
                    class="relative overflow-hidden flex items-center gap-2 rounded-full border cursor-pointer transition-colors max-w-full"
                    classList={{
                      "px-3.5 h-9 text-[13px]": isMobile(),
                      "px-3 h-7 text-xs": !isMobile(),
                      "border-teal-500/70 bg-teal-500/15 text-teal-100": isSelected(),
                      "border-neutral-800 bg-neutral-900/60 text-neutral-300 hover:border-neutral-600 hover:text-neutral-100":
                        !isSelected(),
                    }}
                  >
                    {/* Usage bar: how much of the gallery's most-used tag this
                        one reaches. Redundant with the count beside it — it
                        just makes the shape of the tag set scannable. */}
                    <span
                      aria-hidden="true"
                      class="absolute inset-y-0 left-0 pointer-events-none"
                      style={{
                        width: `${(entry.count / maxCount()) * 100}%`,
                        background: isSelected()
                          ? "rgba(45, 212, 191, 0.18)"
                          : "rgba(255, 255, 255, 0.05)",
                      }}
                    />
                    <span class="relative truncate">{entry.tag}</span>
                    <span
                      class="relative text-[10px] tabular-nums shrink-0"
                      classList={{
                        "text-teal-300/70": isSelected(),
                        "text-neutral-500": !isSelected(),
                      }}
                    >
                      {entry.count}
                    </span>
                  </button>
                );
              }}
            </For>
          </div>
        </Show>
      </div>

      {/* Preview: the files a rename/merge/delete would rewrite. Fills the
          space the old row list wasted, and turns "22 files" into something
          you can actually check before a gallery-wide edit. On a phone the
          vertical space isn't there, so it's a scrolling strip instead. */}
      <Show when={selected().size > 0 && preview().length > 0}>
        <div
          class="shrink-0 min-h-0 flex flex-col border-t border-neutral-800/60 bg-neutral-950/40"
          // Sized to its content, capped at 45% — a single row of thumbnails
          // shouldn't reserve half the panel, and a hundred shouldn't push the
          // cloud off screen.
          style={isMobile() ? { height: "7.5rem" } : { "max-height": "45%" }}
        >
          <div class="shrink-0 px-3 sm:px-6 pt-2 pb-1.5 text-[11px] text-neutral-500">
            {preview().length === PREVIEW_LIMIT
              ? `First ${PREVIEW_LIMIT} files`
              : `${preview().length} ${preview().length === 1 ? "file" : "files"}`}
            {selected().size === 1
              ? ` tagged "${[...selected()][0]}"`
              : ` carrying ${selected().size} selected tags`}
          </div>
          <div
            class="flex-1 min-h-0 overflow-auto px-3 sm:px-6 pb-3"
            classList={{ "flex gap-1.5": isMobile() }}
          >
            <div
              classList={{
                "flex gap-1.5": isMobile(),
                "grid gap-1.5 max-w-[1400px] mx-auto": !isMobile(),
              }}
              style={
                isMobile()
                  ? undefined
                  : { "grid-template-columns": "repeat(auto-fill, minmax(88px, 1fr))" }
              }
            >
              <For each={preview()}>
                {(path) => (
                  <img
                    src={thumbUrl(path, "s")}
                    alt=""
                    loading="lazy"
                    title={path}
                    class="rounded object-cover bg-neutral-900"
                    classList={{
                      "h-full w-auto aspect-square shrink-0": isMobile(),
                      "w-full aspect-square": !isMobile(),
                    }}
                  />
                )}
              </For>
            </div>
          </div>
        </div>
      </Show>
      </div>

      {/* ── Action bar. Always present so the controls live in one place: with
             nothing selected it explains the gesture and offers select-all;
             with a selection it renames (one tag) or merges (several) through
             the same target field, since that's one command on the backend. ── */}
      <div
        class="shrink-0 border-t border-neutral-800/60 px-3 sm:px-6 py-2.5"
        style={{
          background: "rgba(16, 16, 16, 0.95)",
          "padding-bottom": isMobile()
            ? "calc(env(safe-area-inset-bottom, 0px) + 0.625rem)"
            : undefined,
        }}
      >
        <Show
          when={selected().size > 0}
          fallback={
            <div class="flex items-center gap-3 max-w-[1400px] mx-auto">
              <span class="text-[11px] text-neutral-500">
                {busy()
                  ? "Rewriting companion files..."
                  : "Select tags to rename, merge, or delete"}
              </span>
              <div class="flex-1" />
              <Show when={visible().length > 0}>
                <button
                  onClick={selectAllVisible}
                  class="px-2.5 text-[11px] rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-200 whitespace-nowrap"
                  classList={{ "h-9": isMobile(), "h-7": !isMobile() }}
                >
                  Select all {visible().length}
                </button>
              </Show>
            </div>
          }
        >
          <div class="flex items-center gap-2 flex-wrap max-w-[1400px] mx-auto">
            <span class="text-xs text-teal-300 tabular-nums whitespace-nowrap">
              {selected().size} selected
            </span>
            <span class="text-[11px] text-neutral-500 whitespace-nowrap">
              {selected().size === 1 ? "rename to" : "merge into"}
            </span>
            <input
              ref={targetRef}
              value={target()}
              onInput={(e) => {
                setTargetEdited(true);
                setTarget(e.currentTarget.value);
              }}
              onKeyDown={(e) => e.key === "Enter" && apply()}
              placeholder="target tag"
              style={isMobile() ? { "font-size": "16px" } : undefined}
              class="flex-1 min-w-[8rem] max-w-xs px-2.5 rounded bg-neutral-900 text-neutral-200 placeholder-neutral-600 border border-neutral-800 focus:border-neutral-600 focus:outline-none"
              classList={{ "h-9 text-sm": isMobile(), "h-7 text-xs": !isMobile() }}
            />
            <button
              onClick={apply}
              disabled={!canApply()}
              class="px-3 text-[11px] rounded cursor-pointer transition-colors bg-teal-700/50 text-teal-200 hover:bg-teal-600/60 disabled:opacity-40 disabled:cursor-default whitespace-nowrap"
              classList={{ "h-9": isMobile(), "h-7": !isMobile() }}
            >
              {isMerge() ? "Merge" : "Rename"}
            </button>
            <ConfirmButton
              label={selected().size === 1 ? "Delete" : `Delete ${selected().size}`}
              confirmLabel="Remove from all files?"
              disabled={busy()}
              onConfirm={deleteSelected}
              class={isMobile() ? "h-9" : "h-7"}
            />
            <button
              onClick={clearSelection}
              class="px-2.5 text-[11px] rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-200"
              classList={{ "h-9": isMobile(), "h-7": !isMobile() }}
            >
              Clear
            </button>
          </div>
        </Show>
      </div>
    </div>
  );
}
