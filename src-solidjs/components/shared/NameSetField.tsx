// A set-name field with autocomplete over the sets that exist.

import { createSignal, For, onCleanup, Show } from "solid-js";
import { api } from "../../lib/ipc";
import type { TagSuggestion } from "../../lib/types";

/** Name a set, autocompleting over the sets that exist.
 *
 *  First written for the duplicates panel — the replacement for "not
 *  duplicates", and the reason it is a text field rather than a button: the old
 *  gesture recorded a negation nobody could see afterwards, and this one
 *  records a name that shows up in autocomplete, in `set::` filters and in the
 *  tag manager. Autocompleting means a second burst from the same shoot joins
 *  the first rather than founding a near-duplicate name, and a file locked into
 *  an existing set's name appends to its block.
 *
 *  `initiallyOpen` is the dialog form: it starts as the input, and Escape asks
 *  the dialog to close rather than folding back into a button.
 */
export function NameSetField(props: {
  onName: (name: string) => void;
  initiallyOpen?: boolean;
  onCancel?: () => void;
  placeholder?: string;
}) {
  const [open, setOpen] = createSignal(props.initiallyOpen ?? false);
  const close = () => (props.initiallyOpen ? props.onCancel?.() : setOpen(false));
  const [value, setValue] = createSignal("");
  const [suggestions, setSuggestions] = createSignal<TagSuggestion[]>([]);

  let lookup: ReturnType<typeof setTimeout> | undefined;
  const onInput = (next: string) => {
    setValue(next);
    clearTimeout(lookup);
    if (!next.trim()) {
      setSuggestions([]);
      return;
    }
    lookup = setTimeout(async () => {
      try {
        setSuggestions(await api.autocomplete(next.trim(), "set", 6));
      } catch {
        setSuggestions([]);
      }
    }, 150);
  };
  onCleanup(() => clearTimeout(lookup));

  const commit = (name: string) => {
    if (!name.trim()) return;
    if (!props.initiallyOpen) setOpen(false);
    setValue("");
    setSuggestions([]);
    props.onName(name);
  };

  return (
    <Show
      when={open()}
      fallback={
        <button
          onClick={() => setOpen(true)}
          class="px-2 py-0.5 text-[10px] rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-200"
          title="These belong together — name them as a set. Files sharing a set are never offered as duplicates again."
        >
          Name a set
        </button>
      }
    >
      <div class="relative">
        <form
          onSubmit={(e) => {
            e.preventDefault();
            commit(value());
          }}
        >
          <input
            ref={(el) => queueMicrotask(() => el.focus())}
            value={value()}
            onInput={(e) => onInput(e.currentTarget.value)}
            onBlur={() => {
              if (!props.initiallyOpen) setTimeout(() => setOpen(false), 150);
            }}
            onKeyDown={(e) => {
              if (e.key === "Escape") {
                e.stopPropagation();
                close();
              }
            }}
            placeholder={props.placeholder ?? "Set name"}
            class="w-36 px-2 py-0.5 text-[10px] rounded bg-neutral-900 border border-neutral-700 text-neutral-200 outline-none focus:border-teal-700"
          />
        </form>
        <Show when={suggestions().length > 0}>
          <div class="absolute right-0 top-full mt-1 z-10 min-w-36 rounded border border-neutral-800 bg-neutral-950 py-1">
            <For each={suggestions()}>
              {(s) => (
                <button
                  onMouseDown={(e) => {
                    e.preventDefault();
                    commit(s.tag);
                  }}
                  class="flex w-full items-baseline justify-between gap-2 px-2 py-0.5 text-left text-[10px] text-neutral-300 hover:bg-neutral-800"
                >
                  <span class="truncate">{s.tag}</span>
                  <span class="shrink-0 text-neutral-600">{s.count}</span>
                </button>
              )}
            </For>
          </div>
        </Show>
      </div>
    </Show>
  );
}
