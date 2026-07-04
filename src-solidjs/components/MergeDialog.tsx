import { createSignal, createMemo, Show, For, onMount, onCleanup } from "solid-js";
import {
  getMergeCandidates,
  mergeDuplicates,
  thumbUrl,
  type MergeCandidate,
  type MergeGps,
  type MergePlan,
} from "../lib/ipc";

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function formatRes(w: number | null, h: number | null): string {
  if (w == null || h == null) return "";
  return `${w}×${h}`;
}

function formatDate(ts: number | null): string {
  if (ts == null) return "—";
  return new Date(ts * 1000).toLocaleString(undefined, {
    day: "numeric",
    month: "short",
    year: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function fileName(path: string): string {
  return path.split("/").pop() || path;
}

function gpsEq(a: MergeGps | null, b: MergeGps | null): boolean {
  if (a == null || b == null) return a === b;
  return a.lat === b.lat && a.lon === b.lon && a.alt === b.alt;
}

function formatGps(g: MergeGps): string {
  return `${g.lat.toFixed(4)}, ${g.lon.toFixed(4)}`;
}

/**
 * Merge dialog: pick a keeper file, resolve per-field conflicts, then fold the
 * selected data into the keeper and trash the rest. Tags union by default;
 * scalar fields (rating/color/notes/location/mtime) are pick-one.
 */
export function MergeDialog(props: {
  paths: string[];
  bestPath: string | null;
  onCancel: () => void;
  onMerged: (keeper: string, discarded: string[]) => void;
}) {
  const [candidates, setCandidates] = createSignal<MergeCandidate[]>([]);
  const [loading, setLoading] = createSignal(true);
  const [merging, setMerging] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const [keeper, setKeeper] = createSignal<string>(props.bestPath ?? props.paths[0]);

  // Resolved scalar selections. Set once candidates load, then user-editable.
  const [rating, setRating] = createSignal<number | null>(null);
  const [colorLabel, setColorLabel] = createSignal<string | null>(null);
  const [notes, setNotes] = createSignal<string | null>(null);
  const [location, setLocation] = createSignal<MergeGps | null>(null);
  const [mtime, setMtime] = createSignal<number | null>(null);

  // Tag union with per-tag keep flags.
  const [droppedTags, setDroppedTags] = createSignal<Set<string>>(new Set());

  const candFor = (path: string) => candidates().find((c) => c.path === path);

  const tagUnion = createMemo(() => {
    const seen = new Set<string>();
    const out: string[] = [];
    for (const c of candidates()) {
      for (const t of c.user_tags) {
        if (!seen.has(t)) {
          seen.add(t);
          out.push(t);
        }
      }
    }
    return out;
  });

  // Locations available across copies (companion + exif), de-duplicated.
  const locationOptions = createMemo(() => {
    const out: { gps: MergeGps; from: string; source: "companion" | "exif" }[] = [];
    for (const c of candidates()) {
      if (c.companion_location && !out.some((o) => gpsEq(o.gps, c.companion_location))) {
        out.push({ gps: c.companion_location, from: c.path, source: "companion" });
      }
      if (c.exif_location && !out.some((o) => gpsEq(o.gps, c.exif_location))) {
        out.push({ gps: c.exif_location, from: c.path, source: "exif" });
      }
    }
    return out;
  });

  // Defaults keyed off the keeper: keeper's value wins, else the only non-empty.
  const applyDefaults = (cands: MergeCandidate[], keep: string) => {
    const k = cands.find((c) => c.path === keep);
    const firstNonNull = <T,>(get: (c: MergeCandidate) => T | null): T | null => {
      if (k && get(k) != null) return get(k);
      const vals = cands.map(get).filter((v): v is T => v != null);
      return vals.length ? vals[0] : null;
    };
    setRating(firstNonNull((c) => c.rating));
    setColorLabel(firstNonNull((c) => c.color_label));
    setNotes(firstNonNull((c) => c.notes));
    setLocation(firstNonNull((c) => c.companion_location ?? c.exif_location));
    // Default mtime: the earliest across copies (usually the original).
    const times = cands.map((c) => c.mtime).filter((t): t is number => t != null);
    setMtime(times.length ? Math.min(...times) : (k?.mtime ?? null));
  };

  onMount(async () => {
    try {
      const cands = await getMergeCandidates(props.paths);
      setCandidates(cands);
      applyDefaults(cands, keeper());
    } catch (e) {
      setError(String(e));
    }
    setLoading(false);
  });

  const chooseKeeper = (path: string) => {
    setKeeper(path);
    applyDefaults(candidates(), path);
  };

  const toggleTag = (tag: string) => {
    setDroppedTags((prev) => {
      const next = new Set(prev);
      if (next.has(tag)) next.delete(tag);
      else next.add(tag);
      return next;
    });
  };

  const keeperMtime = () => candFor(keeper())?.mtime ?? null;

  const doMerge = async () => {
    setMerging(true);
    setError(null);
    const discard = props.paths.filter((p) => p !== keeper());
    const finalTags = tagUnion().filter((t) => !droppedTags().has(t));
    // Only stamp mtime when it actually differs from the keeper's current time.
    const setMt = mtime() != null && mtime() !== keeperMtime() ? mtime() : null;
    const plan: MergePlan = {
      keeper: keeper(),
      discard,
      user_tags: finalTags,
      rating: rating(),
      color_label: colorLabel(),
      notes: notes(),
      location: location(),
      set_mtime: setMt,
    };
    try {
      await mergeDuplicates(plan);
      props.onMerged(keeper(), discard);
    } catch (e) {
      setError(String(e));
      setMerging(false);
    }
  };

  const handleKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      props.onCancel();
    }
  };
  window.addEventListener("keydown", handleKey, true);
  onCleanup(() => window.removeEventListener("keydown", handleKey, true));

  return (
    <div
      class="fixed inset-0 z-[220] flex items-center justify-center p-6"
      style={{ background: "rgba(0, 0, 0, 0.75)" }}
      onClick={props.onCancel}
    >
      <div
        class="flex flex-col max-w-[880px] w-full max-h-[88vh] rounded-xl overflow-hidden"
        style={{ background: "rgb(20, 20, 22)", border: "1px solid rgba(255,255,255,0.08)" }}
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header */}
        <div class="flex items-center justify-between px-5 py-3.5 border-b border-neutral-800/60">
          <span class="text-sm font-medium text-neutral-200">Merge duplicates</span>
          <button
            onClick={props.onCancel}
            class="w-7 h-7 flex items-center justify-center text-neutral-400 hover:text-neutral-200 rounded transition-colors cursor-pointer"
          >
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M18 6L6 18M6 6l12 12" />
            </svg>
          </button>
        </div>

        <Show when={!loading()} fallback={
          <div class="flex items-center justify-center py-16 gap-3">
            <div class="w-4 h-4 border-2 border-teal-400 border-t-transparent rounded-full animate-spin" />
            <span class="text-sm text-neutral-400">Reading metadata…</span>
          </div>
        }>
          <div class="overflow-y-auto px-5 py-4 flex flex-col gap-5">
            {/* Keeper selection */}
            <section class="flex flex-col gap-2">
              <span class="text-[11px] uppercase tracking-wider text-neutral-500 font-medium">
                Keep which file
              </span>
              <div class="flex gap-3 flex-wrap">
                <For each={candidates()}>
                  {(c) => (
                    <button
                      onClick={() => chooseKeeper(c.path)}
                      class="relative flex flex-col rounded-lg overflow-hidden transition-all cursor-pointer text-left"
                      style={{
                        width: "150px",
                        border: keeper() === c.path
                          ? "2px solid rgba(45, 212, 191, 0.8)"
                          : "2px solid rgba(255,255,255,0.06)",
                        background: "rgba(30,30,30,0.6)",
                      }}
                    >
                      <Show when={keeper() === c.path}>
                        <div class="absolute top-1.5 left-1.5 z-10 px-1.5 py-0.5 rounded text-[10px] font-medium bg-teal-600/80 text-teal-50">
                          Keeper
                        </div>
                      </Show>
                      <div class="w-full h-[100px] bg-neutral-900 flex items-center justify-center overflow-hidden">
                        <img
                          src={thumbUrl(c.path)}
                          class="max-w-full max-h-full object-contain pointer-events-none"
                          loading="lazy"
                        />
                      </div>
                      <div class="px-2 py-1.5 flex flex-col gap-0.5">
                        <span class="text-[11px] text-neutral-300 truncate" title={c.path}>
                          {fileName(c.path)}
                        </span>
                        <div class="flex items-center justify-between text-[10px] text-neutral-500">
                          <span>{formatRes(c.width, c.height)}</span>
                          <span>{formatSize(c.file_size)}</span>
                        </div>
                      </div>
                    </button>
                  )}
                </For>
              </div>
            </section>

            {/* Tags */}
            <Show when={tagUnion().length > 0}>
              <FieldRow label="Tags" hint="union — click to drop">
                <div class="flex gap-1.5 flex-wrap">
                  <For each={tagUnion()}>
                    {(tag) => {
                      const dropped = () => droppedTags().has(tag);
                      return (
                        <button
                          onClick={() => toggleTag(tag)}
                          class="px-2 py-0.5 text-[11px] rounded cursor-pointer transition-colors"
                          classList={{
                            "bg-teal-700/50 text-teal-200": !dropped(),
                            "bg-neutral-800 text-neutral-600 line-through": dropped(),
                          }}
                        >
                          {tag}
                        </button>
                      );
                    }}
                  </For>
                </div>
              </FieldRow>
            </Show>

            {/* Rating */}
            <Show when={candidates().some((c) => c.rating != null)}>
              <FieldRow label="Rating">
                <PickChips
                  options={[
                    { key: "none", label: "None", selected: rating() == null, onPick: () => setRating(null) },
                    ...uniqueValues(candidates().map((c) => c.rating)).map((v) => ({
                      key: `r${v}`,
                      label: "★".repeat(v),
                      selected: rating() === v,
                      onPick: () => setRating(v),
                    })),
                  ]}
                />
              </FieldRow>
            </Show>

            {/* Color label */}
            <Show when={candidates().some((c) => c.color_label != null)}>
              <FieldRow label="Color label">
                <PickChips
                  options={[
                    { key: "none", label: "None", selected: colorLabel() == null, onPick: () => setColorLabel(null) },
                    ...uniqueValues(candidates().map((c) => c.color_label)).map((v) => ({
                      key: v,
                      label: v,
                      selected: colorLabel() === v,
                      onPick: () => setColorLabel(v),
                    })),
                  ]}
                />
              </FieldRow>
            </Show>

            {/* Notes */}
            <Show when={candidates().some((c) => c.notes != null && c.notes !== "")}>
              <FieldRow label="Notes" hint="pick one">
                <div class="flex flex-col gap-1.5 w-full">
                  <button
                    onClick={() => setNotes(null)}
                    class="px-2 py-1 text-[11px] rounded cursor-pointer transition-colors text-left"
                    classList={{
                      "bg-teal-700/40 text-teal-200": notes() == null,
                      "bg-neutral-800 text-neutral-400 hover:bg-neutral-700": notes() != null,
                    }}
                  >
                    None
                  </button>
                  <For each={uniqueValues(candidates().map((c) => c.notes))}>
                    {(v) => (
                      <button
                        onClick={() => setNotes(v)}
                        class="px-2 py-1 text-[11px] rounded cursor-pointer transition-colors text-left whitespace-pre-wrap"
                        classList={{
                          "bg-teal-700/40 text-teal-200": notes() === v,
                          "bg-neutral-800 text-neutral-400 hover:bg-neutral-700": notes() !== v,
                        }}
                      >
                        {v}
                      </button>
                    )}
                  </For>
                </div>
              </FieldRow>
            </Show>

            {/* Location */}
            <Show when={locationOptions().length > 0}>
              <FieldRow label="Location" hint="companion + EXIF">
                <PickChips
                  options={[
                    { key: "none", label: "None", selected: location() == null, onPick: () => setLocation(null) },
                    ...locationOptions().map((o, i) => ({
                      key: `loc${i}`,
                      label: `${formatGps(o.gps)}${o.source === "exif" ? " (EXIF)" : ""}`,
                      selected: gpsEq(location(), o.gps),
                      onPick: () => setLocation(o.gps),
                    })),
                  ]}
                />
              </FieldRow>
            </Show>

            {/* File time */}
            <Show when={candidates().some((c) => c.mtime != null)}>
              <FieldRow label="File time" hint="stamped on keeper">
                <PickChips
                  options={uniqueValues(candidates().map((c) => c.mtime)).map((v) => ({
                    key: `t${v}`,
                    label: formatDate(v),
                    selected: mtime() === v,
                    onPick: () => setMtime(v),
                  }))}
                />
              </FieldRow>
            </Show>
          </div>

          {/* Footer */}
          <div class="flex items-center justify-between px-5 py-3.5 border-t border-neutral-800/60">
            <span class="text-[11px] text-neutral-500">
              {props.paths.length - 1} {props.paths.length - 1 === 1 ? "copy" : "copies"} will be trashed
            </span>
            <div class="flex items-center gap-2">
              <Show when={error()}>
                <span class="text-[11px] text-red-400 max-w-[300px] truncate" title={error()!}>{error()}</span>
              </Show>
              <button
                onClick={props.onCancel}
                class="px-3 py-1.5 text-xs rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-300 hover:bg-neutral-700"
              >
                Cancel
              </button>
              <button
                onClick={doMerge}
                disabled={merging()}
                class="px-4 py-1.5 text-xs rounded cursor-pointer transition-colors bg-teal-700/70 text-teal-100 hover:bg-teal-600/70 disabled:opacity-50 disabled:cursor-not-allowed"
              >
                {merging() ? "Merging…" : "Merge"}
              </button>
            </div>
          </div>
        </Show>
      </div>
    </div>
  );
}

function FieldRow(props: { label: string; hint?: string; children: any }) {
  return (
    <section class="flex flex-col gap-1.5">
      <div class="flex items-baseline gap-2">
        <span class="text-[11px] uppercase tracking-wider text-neutral-500 font-medium">
          {props.label}
        </span>
        <Show when={props.hint}>
          <span class="text-[10px] text-neutral-600 normal-case tracking-normal">{props.hint}</span>
        </Show>
      </div>
      {props.children}
    </section>
  );
}

function PickChips(props: {
  options: { key: string; label: string; selected: boolean; onPick: () => void }[];
}) {
  return (
    <div class="flex gap-1.5 flex-wrap">
      <For each={props.options}>
        {(o) => (
          <button
            onClick={o.onPick}
            class="px-2 py-0.5 text-[11px] rounded cursor-pointer transition-colors"
            classList={{
              "bg-teal-700/50 text-teal-200": o.selected,
              "bg-neutral-800 text-neutral-400 hover:bg-neutral-700": !o.selected,
            }}
          >
            {o.label}
          </button>
        )}
      </For>
    </div>
  );
}

function uniqueValues<T>(vals: (T | null)[]): T[] {
  const out: T[] = [];
  for (const v of vals) {
    if (v != null && !out.includes(v)) out.push(v);
  }
  return out;
}
