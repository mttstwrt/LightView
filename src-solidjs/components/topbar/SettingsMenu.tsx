// Configuration only — the things you *set*.
//
// Everything you *do* moved out to the command list (`CommandMenu.tsx`);
// opening this panel is itself the last entry in that list. What is left is one
// ordered list of `Section`s, and the order is source order: the `order` prop
// this used to carry let thirteen call sites each pick a magic number, four of
// which collided, because nobody ever saw all thirteen at once.
//
// **Three sections, down from nine.** The ones that went are not a trim for
// tidiness — each described a mechanism that no longer exists:
//
//   - *Remote Access* (a third of this file: the enable/disable toggle, port
//     field, QR pairing, device roster, password form, inactivity selector,
//     upload and delete switches) — a served bind is `lightview --serve`, and
//     nothing is `Owner` under it, so the administration moved to
//     `lightview pair`, `lightview devices` and `lightview password`, which are
//     the only place it could live.
//   - *Views* — there is one grid and it is justified.
//   - *Storage* — the companion location is one place now, and the cache moved
//     out of the gallery entirely.
//   - *GPU acceleration* — the compositing path it toggled went with the
//     desktop webview.
//   - *Reset connection* — it unregistered the service worker so iOS would
//     re-prompt for the certificate; there is no service worker, and reloading
//     is the whole recovery.
//
// **Display preferences are per client, for every client.** They live in
// `clientPrefs` (localStorage), not in a file inside the gallery: a file in the
// gallery is per *gallery*, so two desktops mounting one share would fight over
// thumbnail size — the exact thing a per-client preference exists to prevent.

import { createSignal, Show, For, onCleanup, onMount } from "solid-js";
import { Portal, Dynamic } from "solid-js/web";

import { CloseIcon } from "./icons";
import { api } from "../../lib/ipc";
import { isMobile } from "../../lib/runtime";
import { versionLabel, GIT_SHA } from "../../lib/version";
import { setThumbWork } from "../../stores/activityStore";
import { displayPaths, settingsOpen, setSettingsOpen } from "../../stores/galleryStore";
import { refreshFilteredItems } from "../../stores/filterStore";
import {
  gallerySettings,
  prefs,
  saveDefaultFilter,
  setPrefs,
  type DisplayPrefs,
} from "../../stores/settingsStore";
import { viewerOpen } from "../../stores/viewerStore";
import type { ThumbTier } from "../../lib/types";

const THUMB_PRESETS = [
  { label: "S", value: 120 },
  { label: "M", value: 200 },
  { label: "L", value: 300 },
  { label: "XL", value: 400 },
] as const;

// On mobile the thumbnail-size picker is a column count rather than a pixel
// bucket: 200px gives a desktop six columns and a 390px phone exactly one.
const MOBILE_COL_PRESETS = [1, 2, 3, 4, 5] as const;

const GAP_PRESETS = [
  { label: "None", value: 0 },
  { label: "Tight", value: 2 },
  { label: "Normal", value: 4 },
  { label: "Wide", value: 8 },
] as const;

/** How many paths to hand the server per precache call. Bounded so one
 *  maintenance pass cannot occupy the whole thumbnail pool for minutes while
 *  somebody is scrolling. */
const PRECACHE_BATCH = 48;

/** The tiers a maintenance pass fills. `js` derives from the same decode as
 *  `j` and the two high tiers are generated for what is actually viewed zoomed
 *  in — warming those across a whole library is disk spent on cells nobody has
 *  opened. */
const PRECACHE_TIERS: ThumbTier[] = ["j"];

/** The settings panel. It has no trigger of its own — the command list opens
 *  it, on both surfaces. `onRequestShow` lets the keyboard shortcut reveal the
 *  auto-hiding chrome before the panel appears over it. */
export function SettingsMenu(props: { onRequestShow?: () => void }) {
  // Open state lives in the store so the command list can open this panel, and
  // so `App` can hide the grid behind the full-screen mobile page.
  const open = settingsOpen;
  const setOpen = setSettingsOpen;
  onCleanup(() => setSettingsOpen(false));

  const toggle = () => setOpen((v) => !v);

  const handleKey = (e: KeyboardEvent) => {
    if (e.key === "Escape" && open()) {
      e.stopPropagation();
      setOpen(false);
      return;
    }
    if (
      e.key === "i" &&
      !e.ctrlKey && !e.metaKey && !e.altKey &&
      !viewerOpen() &&
      !(e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement)
    ) {
      e.preventDefault();
      if (!open()) props.onRequestShow?.();
      toggle();
    }
  };
  window.addEventListener("keydown", handleKey, true);
  onCleanup(() => window.removeEventListener("keydown", handleKey, true));

  const set = <K extends keyof DisplayPrefs>(key: K, value: DisplayPrefs[K]) =>
    setPrefs({ [key]: value } as Partial<DisplayPrefs>);

  /** Set the cell size to whatever renders `cols` columns at this width. */
  const setColumns = (cols: number) => {
    const gap = prefs().grid_gap;
    set("thumbnail_size", Math.round((window.innerWidth + gap) / cols - gap));
  };

  /** Roughly how many columns the current size gives, for the mobile picker's
   *  active state. Rounded, because the justified layout varies the count row
   *  by row and an exact match would light up nothing. */
  const currentColumns = () => {
    const gap = prefs().grid_gap;
    return Math.max(1, Math.round((window.innerWidth + gap) / (prefs().thumbnail_size + gap)));
  };

  // ── Default filter ────────────────────────────────────────────────────────
  // The one setting here that is *not* per client: it lives in the gallery's
  // own settings file so it survives a cache rebuild and applies to whichever
  // client opens the gallery next. Committed on blur rather than per keystroke
  // — each save is a durable write to a file inside the gallery.
  const [filterDraft, setFilterDraft] = createSignal<string | null>(null);
  const filterValue = () => filterDraft() ?? gallerySettings().default_filter;
  const commitFilter = async () => {
    const next = filterDraft();
    if (next === null || next === gallerySettings().default_filter) return;
    setFilterDraft(null);
    try {
      await saveDefaultFilter(next);
    } catch (e) {
      console.error("Could not save the default filter:", e);
    }
  };

  // ── Thumbnails ────────────────────────────────────────────────────────────
  const [tierBytes, setTierBytes] = createSignal<{ tier: ThumbTier; bytes: number }[]>([]);
  const refreshTotals = () =>
    api.tierTotals().then(setTierBytes).catch(() => setTierBytes([]));
  onMount(() => void refreshTotals());

  const [working, setWorking] = createSignal(false);
  let cancelWork = false;

  /** Generate the base tier for the whole gallery, in bounded batches.
   *
   *  Deliberately unfiltered: the active view filter must not hide paths from a
   *  whole-gallery maintenance pass. Already-cached paths are skipped by the
   *  server, so a re-run is cheap and cancelling loses nothing. */
  const generateMissing = async () => {
    if (working()) {
      cancelWork = true;
      return;
    }
    setWorking(true);
    cancelWork = false;
    try {
      const all = await api.items({
        sort: "name",
        order: "asc",
        filter: "",
        group_by: { type: "none" },
      });
      const paths = all.items.map((it) => it.path);
      const total = paths.length * PRECACHE_TIERS.length;
      let done = 0;
      setThumbWork({ done, total });
      for (const tier of PRECACHE_TIERS) {
        for (let i = 0; i < paths.length && !cancelWork; i += PRECACHE_BATCH) {
          const batch = paths.slice(i, i + PRECACHE_BATCH);
          await api.precache(tier, batch);
          done += batch.length;
          setThumbWork({ done, total });
        }
      }
    } catch (e) {
      console.error("Thumbnail generation failed:", e);
    }
    setThumbWork(null);
    setWorking(false);
    void refreshTotals();
  };

  /** Discard every cached tier for every file and let them regenerate on
   *  demand. Restorable by definition — the source images are untouched — so
   *  it is `Device` like every other thumbnail operation. */
  const rebuildAll = async () => {
    try {
      const all = await api.items({
        sort: "name",
        order: "asc",
        filter: "",
        group_by: { type: "none" },
      });
      await api.regenerate(all.items.map((it) => it.path));
      window.dispatchEvent(new CustomEvent("lightview:thumbnails-invalidated"));
      void refreshTotals();
    } catch (e) {
      console.error("Rebuild failed:", e);
    }
  };

  const totalBytes = () => tierBytes().reduce((sum, t) => sum + t.bytes, 0);

  return (
    <Show when={open()}>
      {/* Backdrop — click to close (desktop only; the mobile page is opaque
          and covers the whole viewport, so there is nothing to click behind). */}
      <Show when={!isMobile()}>
        <div class="fixed inset-0 z-40" onClick={() => setOpen(false)} />
      </Show>

      {/* On mobile the panel is a full-screen opaque page rather than a
          translucent drawer: phones do not reliably honour `backdrop-filter`,
          so the gallery bled through and the panel read as empty.

          The mobile page is portalled to <body> so its `position: fixed`
          resolves against the viewport. Rendered in place it would be trapped
          by the top bar's `backdrop-filter`, which establishes a containing
          block for fixed descendants and clips the page to the bar's height.
          Desktop stays in place — its `absolute` panel is positioned by the
          wrapper it shares with the command button in `TopBar`, so it drops
          from the control that opened it. */}
      <Dynamic component={isMobile() ? Portal : InPlace}>
        <div
          class={
            isMobile()
              ? "fixed inset-0 z-[60] overflow-hidden flex flex-col safe-panel"
              : "absolute top-full right-0 mt-2 w-72 rounded-lg overflow-hidden shadow-xl z-50 flex flex-col max-h-[calc(100vh-5rem)]"
          }
          style={
            isMobile()
              ? { background: "#121212" }
              : {
                  background: "rgba(18, 18, 18, 0.96)",
                  "backdrop-filter": "blur(16px)",
                  border: "1px solid rgba(255,255,255,0.08)",
                }
          }
        >
          <div class="px-4 py-3 border-b border-neutral-800/60 flex items-center justify-between shrink-0">
            <div class="flex items-baseline gap-2">
              <span class="text-sm font-medium text-neutral-200">Settings</span>
              {/* Image count for the current filter. On desktop this sits in
                  the top bar; on mobile the bar is too cramped. */}
              <Show when={isMobile()}>
                <span class="text-xs text-neutral-500 tabular-nums">
                  {displayPaths().length.toLocaleString()} images
                </span>
              </Show>
            </div>
            <Show when={isMobile()}>
              <button
                onClick={() => setOpen(false)}
                class="w-10 h-10 -mr-2 flex items-center justify-center rounded text-neutral-400 hover:text-neutral-200 hover:bg-neutral-800 cursor-pointer"
                title="Close"
                aria-label="Close settings"
              >
                <CloseIcon size={16} />
              </button>
            </Show>
          </div>

          <div
            class="px-4 py-3 flex flex-col gap-4 overflow-y-auto overscroll-contain flex-1 min-h-0"
            classList={{ "hide-scrollbar": isMobile(), "dupes-scroll": !isMobile() }}
          >
            {/* ── Display ── */}
            <Section label="Display">
              <Show
                when={isMobile()}
                fallback={
                  <Field label="Thumbnail size">
                    <div class="flex gap-1">
                      <For each={THUMB_PRESETS}>
                        {(p) => (
                          <Chip
                            active={prefs().thumbnail_size === p.value}
                            onClick={() => set("thumbnail_size", p.value)}
                          >
                            {p.label}
                          </Chip>
                        )}
                      </For>
                    </div>
                  </Field>
                }
              >
                <Field label="Columns">
                  <div class="flex gap-1">
                    <For each={MOBILE_COL_PRESETS}>
                      {(cols) => (
                        <Chip
                          active={currentColumns() === cols}
                          onClick={() => setColumns(cols)}
                        >
                          {cols}
                        </Chip>
                      )}
                    </For>
                  </div>
                </Field>
              </Show>

              <Field label="Grid spacing">
                <div class="flex gap-1">
                  <For each={GAP_PRESETS}>
                    {(p) => (
                      <Chip
                        active={prefs().grid_gap === p.value}
                        onClick={() => set("grid_gap", p.value)}
                      >
                        {p.label}
                      </Chip>
                    )}
                  </For>
                </div>
              </Field>

              <Field label="Zoom range (px)">
                <div class="flex items-center gap-2">
                  <NumberInput
                    value={prefs().thumb_size_min}
                    title="Smallest row size the zoom control reaches"
                    onChange={(n) => set("thumb_size_min", Math.min(n, prefs().thumb_size_max - 1))}
                  />
                  <span class="text-xs text-neutral-500">to</span>
                  <NumberInput
                    value={prefs().thumb_size_max}
                    title="Largest row size the zoom control reaches"
                    onChange={(n) => set("thumb_size_max", Math.max(n, prefs().thumb_size_min + 1))}
                  />
                </div>
              </Field>

              <Field label="Background">
                <div class="flex items-center gap-2">
                  <input
                    type="color"
                    value={prefs().background_color}
                    onInput={(e) => set("background_color", e.currentTarget.value)}
                    class="w-6 h-6 rounded cursor-pointer border border-neutral-700 bg-transparent"
                  />
                  <span class="text-xs text-neutral-500 font-mono">
                    {prefs().background_color}
                  </span>
                </div>
              </Field>

              <Toggle
                label="Start at bottom"
                checked={prefs().start_at_bottom}
                onChange={(v) => set("start_at_bottom", v)}
              />
              <Note>
                Opens at the end of the grid and scrolls up. To change which end
                is oldest, flip the sort direction instead.
              </Note>

              <Toggle
                label="GIF autoplay in grid"
                checked={prefs().gif_autoplay_grid}
                onChange={(v) => set("gif_autoplay_grid", v)}
              />
              <Toggle
                label="Autoplay short videos in grid"
                checked={prefs().video_autoplay_grid}
                onChange={(v) => set("video_autoplay_grid", v)}
              />
              <Show when={prefs().video_autoplay_grid}>
                <Field label="Max video length (seconds)">
                  <NumberInput
                    value={prefs().video_autoplay_max_seconds}
                    title="Videos at or under this length autoplay in the grid"
                    onChange={(n) => set("video_autoplay_max_seconds", n)}
                  />
                </Field>
              </Show>
              <Toggle
                label="Video hover preview"
                checked={prefs().video_hover_preview}
                onChange={(v) => set("video_hover_preview", v)}
              />
              <Toggle
                label="Video auto-replay"
                checked={prefs().video_autoplay_loop}
                onChange={(v) => set("video_autoplay_loop", v)}
              />
              <Toggle
                label="Autoplay videos in viewer"
                checked={prefs().video_autoplay_viewer}
                onChange={(v) => set("video_autoplay_viewer", v)}
              />
              <Note>Starts muted — tap the pill or the speaker button for sound.</Note>

              <Toggle
                label="Thumbnail fade-in"
                checked={prefs().scroll_blur}
                onChange={(v) => set("scroll_blur", v)}
              />
              <Toggle
                label="High-detail zoom"
                checked={prefs().justified_high_detail}
                onChange={(v) => set("justified_high_detail", v)}
              />
              <Note>
                Serves a larger tier when you zoom in, for visible cells only.
                Uses more disk for the photos you actually look at closely.
              </Note>

              <Show when={isMobile()}>
                <Field label="Filter sheet position">
                  <div class="flex items-center gap-1 p-0.5 rounded bg-neutral-800/60">
                    <For each={["top", "bottom"] as const}>
                      {(where) => (
                        <button
                          class="flex-1 px-2 py-1 text-xs rounded cursor-pointer transition-colors capitalize"
                          classList={{
                            "bg-neutral-700 text-white": prefs().mobile_filter_sheet === where,
                            "text-neutral-300": prefs().mobile_filter_sheet !== where,
                          }}
                          onClick={() => set("mobile_filter_sheet", where)}
                        >
                          {where}
                        </button>
                      )}
                    </For>
                  </div>
                </Field>
                <Note>Bottom is easier to reach one-handed on large phones.</Note>
              </Show>
            </Section>

            {/* ── Thumbnails ── */}
            <Section label="Thumbnails">
              <button
                onClick={() => void generateMissing()}
                class="px-3 py-1.5 text-xs rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
              >
                {working() ? "Cancel generation" : "Generate missing thumbnails"}
              </button>
              <Note>
                Fills the base tier for the whole gallery up front, so cold
                regions do not burst-generate while you scroll them. Safe to
                cancel and re-run — cached files are skipped.
              </Note>

              <Show when={tierBytes().length > 0}>
                <Field label={`Cached (${formatBytes(totalBytes())})`}>
                  <div class="flex flex-col gap-0.5">
                    <For each={tierBytes()}>
                      {(t) => (
                        <div class="flex items-baseline justify-between text-[11px]">
                          <span class="text-neutral-500 uppercase tracking-wider">{t.tier}</span>
                          <span class="text-neutral-400 tabular-nums">{formatBytes(t.bytes)}</span>
                        </div>
                      )}
                    </For>
                  </div>
                </Field>
              </Show>

              <button
                onClick={() => void rebuildAll()}
                disabled={working()}
                class="px-3 py-1.5 text-xs rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300 disabled:opacity-50 disabled:cursor-not-allowed"
              >
                Rebuild all thumbnails
              </button>
              <Note>
                Discards every cached size and regenerates on demand. Your
                photos are untouched.
              </Note>
            </Section>

            {/* ── Default filter ── */}
            <Section label="Default filter">
              <input
                type="text"
                value={filterValue()}
                onInput={(e) => setFilterDraft(e.currentTarget.value)}
                onBlur={() => void commitFilter()}
                onKeyDown={(e) => {
                  if (e.key === "Enter") e.currentTarget.blur();
                }}
                placeholder="e.g. rating>=3 AND NOT set::scans"
                class="w-full px-2 py-1 bg-neutral-800 border border-neutral-700 rounded text-xs text-neutral-200 placeholder-neutral-600 outline-none focus:border-neutral-500"
              />
              <Note>
                Applied whenever this gallery is opened, by any client. It lives
                in the gallery's own settings file, so it survives a cache
                rebuild. Leave it empty for no default.
              </Note>
              <Show when={filterValue().trim()}>
                <button
                  onClick={() => void refreshFilteredItems()}
                  class="px-3 py-1.5 text-xs rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
                >
                  Apply now
                </button>
              </Show>
            </Section>

            {/* ── Connection ── */}
            <Section label="Connection">
              {/* Plain <a>, no `download`: iOS routes the response to the
                  profile installer off the navigation itself. */}
              <a
                href="/cert"
                class="px-3 py-1.5 text-xs rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300 text-center"
              >
                Install server certificate
              </a>
              <Note>
                Replaces the browser's temporary security exception with real
                trust, so it stops expiring. On iPhone/iPad open this in Safari
                (not the home-screen app), install from Settings → Profile
                Downloaded, then enable it under General → About → Certificate
                Trust Settings.
              </Note>
            </Section>

            {/* Build identity — always last. Lets a running client be matched
                to a build ("did the container actually update?"). */}
            <AboutFooter />
          </div>
        </div>
      </Dynamic>
    </Show>
  );
}

// ── Helpers ────────────────────────────────────────────────────────────────

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

/** Renders children where they sit (no portal). Paired with `Dynamic` so the
 *  desktop dropdown stays in place while the mobile page portals out. */
function InPlace(props: { children: any }) {
  return props.children;
}

/** Build identity, pinned to the bottom. Tap to copy the full label so it can
 *  be pasted into a bug report or checked against what a deploy should run. */
function AboutFooter() {
  const [copied, setCopied] = createSignal(false);
  const label = versionLabel();
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(GIT_SHA ? `${label} (${GIT_SHA})` : label);
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    } catch {
      /* clipboard blocked (insecure context) — the text is on screen anyway */
    }
  };
  return (
    <button
      onClick={copy}
      title="Copy build info"
      class="mt-1 pt-3 border-t border-neutral-800/60 text-left text-[10px] font-mono text-neutral-600 hover:text-neutral-400 transition-colors cursor-pointer select-text"
    >
      <span class="text-neutral-500">LightView</span> {copied() ? "copied ✓" : label}
    </button>
  );
}

/** One group of related settings. Position is source order — there is no
 *  `order` prop, deliberately; see the note at the top of this file. */
function Section(props: { label: string; children: any }) {
  return (
    <div class="flex flex-col gap-2.5">
      <span class="text-[11px] uppercase tracking-wider text-neutral-500 font-medium">
        {props.label}
      </span>
      {props.children}
    </div>
  );
}

function Field(props: { label: string; children: any }) {
  return (
    <div class="flex flex-col gap-1">
      <span class="text-xs text-neutral-400">{props.label}</span>
      {props.children}
    </div>
  );
}

function Note(props: { children: any }) {
  return (
    <p class="text-[10px] text-neutral-500 -mt-1 pl-0.5 leading-relaxed">{props.children}</p>
  );
}

function Chip(props: { active: boolean; onClick: () => void; children: any }) {
  return (
    <button
      onClick={props.onClick}
      class="px-2 py-0.5 text-xs rounded cursor-pointer transition-colors"
      classList={{
        "bg-teal-700/60 text-teal-200": props.active,
        "bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300":
          !props.active,
      }}
    >
      {props.children}
    </button>
  );
}

function NumberInput(props: { value: number; title: string; onChange: (n: number) => void }) {
  return (
    <input
      type="number"
      min="1"
      max="4000"
      value={props.value}
      title={props.title}
      onInput={(e) => {
        const n = parseInt(e.currentTarget.value, 10);
        if (Number.isFinite(n) && n > 0) props.onChange(n);
      }}
      class="w-20 px-2 py-1 bg-neutral-800 border border-neutral-700 rounded text-xs text-neutral-200 outline-none focus:border-neutral-500"
    />
  );
}

function Toggle(props: { label: string; checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <label class="flex items-center justify-between cursor-pointer group">
      <span class="text-xs text-neutral-400 group-hover:text-neutral-300 transition-colors">
        {props.label}
      </span>
      <button
        role="switch"
        aria-checked={props.checked}
        onClick={() => props.onChange(!props.checked)}
        class={`relative w-8 h-4.5 rounded-full transition-colors cursor-pointer ${
          props.checked ? "bg-teal-600" : "bg-neutral-700"
        }`}
      >
        <span
          class="absolute top-0.5 left-0.5 w-3.5 h-3.5 rounded-full bg-white transition-transform"
          style={{ transform: props.checked ? "translateX(14px)" : "translateX(0)" }}
        />
      </button>
    </label>
  );
}
