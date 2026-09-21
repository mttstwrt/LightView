// Choose a directory on the machine the server is running on.
//
// **The replacement for the native file dialog, and `Owner`-only.** A browser
// cannot return a filesystem path — the File System Access API yields a handle,
// not a path, and only in Chromium — so the picker walks the server's
// directories through `GET /api/dirs`, which returns names and nothing else:
// never media, never file contents. That listing is loopback-only and shows
// what the local user can already enumerate with any file manager.
//
// The alternative was `rfd`, and it loses: a new dependency that links GTK or a
// desktop portal, on a process that may have no display at all.
//
// Navigation is by the paths the server hands back — `parent` and `places`
// travel with each listing — so this component never builds a path out of
// string pieces, and there is no spelling of a request it can produce that the
// server did not just name.
//
// **The sidebar is the reason this is usable at all.** Walking one level at a
// time from the gallery root to `~/Pictures/Archive` is four clicks up and
// three down, which is why every native dialog has a list of places. The
// server decides what is on it: it is the only side that can check a directory
// exists, and the only side allowed to name a path.

import { Show, For, createSignal, onCleanup, onMount } from "solid-js";

import { api } from "../../lib/ipc";

interface Entry {
  name: string;
  path: string;
}

interface Place {
  label: string;
  path: string;
}

export function DirectoryPicker(props: {
  title: string;
  confirmLabel: string;
  onPick: (path: string) => void;
  onCancel: () => void;
}) {
  // Opens at the gallery root — the server picks it when no path is given, so
  // the client needs no absolute path to start from and does not learn one it
  // has no other use for.
  const [path, setPath] = createSignal("");
  const [parent, setParent] = createSignal<string | null>(null);
  const [entries, setEntries] = createSignal<Entry[]>([]);
  const [places, setPlaces] = createSignal<Place[]>([]);
  const [error, setError] = createSignal<string | null>(null);
  const [loading, setLoading] = createSignal(true);

  const go = async (to?: string) => {
    setLoading(true);
    setError(null);
    try {
      const listing = await api.listDirs(to);
      setPath(listing.path);
      setParent(listing.parent);
      setEntries(listing.entries);
      setPlaces(listing.places);
    } catch (e) {
      // The previous level stays on screen: a directory that cannot be read is
      // a dead end to back out of, not a reason to empty the dialog.
      setError(String(e));
    }
    setLoading(false);
  };

  onMount(() => void go());

  const handleKey = (e: KeyboardEvent) => {
    if (e.key !== "Escape") return;
    e.stopPropagation();
    props.onCancel();
  };
  window.addEventListener("keydown", handleKey, true);
  onCleanup(() => window.removeEventListener("keydown", handleKey, true));

  return (
    <div
      class="fixed inset-0 z-[260] flex items-center justify-center p-6"
      style={{ background: "rgba(0, 0, 0, 0.75)" }}
      onClick={props.onCancel}
    >
      <div
        class="flex flex-col w-full max-w-2xl max-h-[80vh] rounded-xl overflow-hidden"
        style={{ background: "rgb(20, 20, 22)", border: "1px solid rgba(255,255,255,0.08)" }}
        onClick={(e) => e.stopPropagation()}
      >
        <div class="flex items-center justify-between px-5 py-3.5 border-b border-neutral-800/60">
          <span class="text-sm font-medium text-neutral-200">{props.title}</span>
          <button
            onClick={props.onCancel}
            aria-label="Cancel"
            class="w-7 h-7 flex items-center justify-center text-neutral-400 hover:text-neutral-200 rounded transition-colors cursor-pointer"
          >
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M18 6L6 18M6 6l12 12" />
            </svg>
          </button>
        </div>

        <div class="px-5 py-2 border-b border-neutral-800/60">
          <span class="text-[11px] text-neutral-500 break-all">{path()}</span>
        </div>

        <Show when={error()}>
          <div class="px-5 py-2 text-xs text-red-400 border-b border-red-900/40 bg-red-950/20">
            {error()}
          </div>
        </Show>

        <div class="flex flex-col sm:flex-row flex-1 min-h-[12rem] overflow-hidden">
          {/* Places. A column beside the listing on a wide window, a
              scrollable strip above it on a narrow one — the picker is
              Owner-only and so usually desktop, but a narrow window should
              lose the layout rather than the shortcuts. */}
          <Show when={places().length > 0}>
            <div
              class="flex flex-row sm:flex-col gap-1 shrink-0 overflow-x-auto sm:overflow-x-visible sm:overflow-y-auto
                     px-2 py-2 sm:w-44 border-b sm:border-b-0 sm:border-r border-neutral-800/60"
            >
              <For each={places()}>
                {(place) => (
                  <button
                    onClick={() => void go(place.path)}
                    title={place.path}
                    class="flex items-center gap-2 px-2.5 py-1.5 rounded text-left text-xs whitespace-nowrap cursor-pointer transition-colors"
                    classList={{
                      "bg-neutral-800 text-neutral-100": place.path === path(),
                      "text-neutral-400 hover:bg-neutral-800/60 hover:text-neutral-200":
                        place.path !== path(),
                    }}
                  >
                    <svg class="w-3.5 h-3.5 shrink-0 text-neutral-600" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                      <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
                    </svg>
                    <span class="truncate">{place.label}</span>
                  </button>
                )}
              </For>
            </div>
          </Show>

          <div class="flex-1 overflow-y-auto px-2 py-2">
          <Show when={parent()}>
            {(up) => (
              <button
                onClick={() => void go(up())}
                class="flex w-full items-center gap-2 px-3 py-1.5 rounded text-left text-xs text-neutral-400 hover:bg-neutral-800 cursor-pointer"
              >
                <span class="text-neutral-600">↑</span>
                <span>Parent folder</span>
              </button>
            )}
          </Show>
          <Show
            when={!loading() && entries().length === 0}
            fallback={
              <For each={entries()}>
                {(entry) => (
                  <button
                    onClick={() => void go(entry.path)}
                    class="flex w-full items-center gap-2 px-3 py-1.5 rounded text-left text-xs text-neutral-300 hover:bg-neutral-800 cursor-pointer"
                  >
                    <svg class="w-3.5 h-3.5 shrink-0 text-neutral-600" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                      <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
                    </svg>
                    <span class="truncate">{entry.name}</span>
                  </button>
                )}
              </For>
            }
          >
            <div class="px-3 py-2 text-xs text-neutral-600">No subfolders here.</div>
          </Show>
          </div>
        </div>

        <div class="flex items-center justify-end gap-2 px-5 py-3.5 border-t border-neutral-800/60">
          <button
            onClick={props.onCancel}
            class="px-3 py-1.5 text-xs rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-300 hover:bg-neutral-700"
          >
            Cancel
          </button>
          <button
            onClick={() => props.onPick(path())}
            class="px-4 py-1.5 text-xs rounded cursor-pointer transition-colors bg-teal-700/70 text-teal-100 hover:bg-teal-600/70"
          >
            {props.confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
