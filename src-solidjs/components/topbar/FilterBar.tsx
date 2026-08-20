import { createSignal, For, Show, onMount, onCleanup } from "solid-js";
import { isMobile } from "../../lib/runtime";
import {
  acQuery, setAcQuery,
  acSuggestions, setAcSuggestions,
  acOpen, setAcOpen,
  acSelectedIndex, setAcSelectedIndex,
  filterQuery, setFilterQuery,
  ratingFilter, setRatingFilter,
  refreshFilteredItems,
  clearAllFilters,
} from "../../stores/filterStore";
import { autocompleteTags } from "../../lib/ipc";

interface FilterBarProps {
  onInputRef?: (el: HTMLInputElement) => void;
  /** Open the autocomplete list and rating popover upward instead of down —
   *  used when the bar sits in a bottom sheet (otherwise they'd fall off the
   *  bottom of the screen, behind the keyboard). */
  dropUp?: boolean;
  /** Called after Enter applies the filter. The mobile sheet passes this to
   *  dismiss itself (and the keyboard with it) so the results are visible. */
  onSubmit?: () => void;
}

export function FilterBar(props: FilterBarProps) {
  let inputRef: HTMLInputElement | undefined;
  let debounceTimer: number | undefined;

  // Mobile: the five inline rating stars don't fit alongside sort/settings, so
  // they collapse into a single star button that opens a small popover.
  const [ratingMenuOpen, setRatingMenuOpen] = createSignal(false);
  let ratingRef: HTMLDivElement | undefined;
  onMount(() => {
    const onDocClick = (e: MouseEvent) => {
      if (ratingMenuOpen() && ratingRef && !ratingRef.contains(e.target as Node)) {
        setRatingMenuOpen(false);
      }
    };
    document.addEventListener("click", onDocClick);
    onCleanup(() => document.removeEventListener("click", onDocClick));
  });

  // Extract the last "word" being typed (the token after the last space/operator)
  const getCurrentToken = (value: string, cursorPos: number): { token: string; start: number } => {
    const before = value.slice(0, cursorPos);
    // Split on spaces to find the current token
    const match = before.match(/(\S+)$/);
    if (!match) return { token: "", start: cursorPos };
    return { token: match[1], start: cursorPos - match[1].length };
  };

  const handleInput = (value: string) => {
    setAcQuery(value);
    setAcSelectedIndex(0);

    if (debounceTimer) clearTimeout(debounceTimer);
    debounceTimer = window.setTimeout(async () => {
      const cursorPos = inputRef?.selectionStart ?? value.length;
      const { token } = getCurrentToken(value, cursorPos);

      // Skip autocomplete for operators and empty tokens
      const upper = token.toUpperCase();
      if (!token || upper === "AND" || upper === "OR" || upper === "NOT") {
        setAcSuggestions([]);
        setAcOpen(false);
        return;
      }

      // Strip leading NOT/namespace prefix for autocomplete lookup
      const lookupToken = token.includes("::") ? token.split("::").pop()! : token;
      if (!lookupToken) {
        setAcSuggestions([]);
        setAcOpen(false);
        return;
      }

      try {
        const suggestions = await autocompleteTags(lookupToken);
        setAcSuggestions(suggestions);
        setAcOpen(suggestions.length > 0);
      } catch {
        setAcSuggestions([]);
      }
    }, 150);
  };

  const insertSuggestion = (suggestion: { namespace: string; tag: string }) => {
    const value = acQuery();
    const cursorPos = inputRef?.selectionStart ?? value.length;
    const { start } = getCurrentToken(value, cursorPos);

    // Both namespace and tag suggestions insert the bare value.
    // Namespace suggestions insert e.g. "plugin.wd", tag suggestions
    // insert the bare tag name. Users can manually type namespace::tag
    // to narrow to a specific namespace.
    const replacement = suggestion.tag;

    const before = value.slice(0, start);
    const after = value.slice(cursorPos);
    const newValue = before + replacement + (after.startsWith(" ") ? "" : " ") + after;

    setAcQuery(newValue);
    setAcOpen(false);
    setAcSuggestions([]);

    // Apply the filter
    setFilterQuery(newValue.trim());
    applyCurrentFilter();

    // Restore focus and cursor
    requestAnimationFrame(() => {
      if (inputRef) {
        inputRef.focus();
        const pos = before.length + replacement.length + 1;
        inputRef.setSelectionRange(pos, pos);
      }
    });
  };

  const handleKeyDown = (e: KeyboardEvent) => {
    if (acOpen()) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setAcSelectedIndex((i) => Math.min(i + 1, acSuggestions().length - 1));
        return;
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setAcSelectedIndex((i) => Math.max(i - 1, 0));
        return;
      } else if (e.key === "Tab") {
        e.preventDefault();
        const suggestions = acSuggestions();
        const idx = acSelectedIndex();
        if (suggestions[idx]) {
          insertSuggestion(suggestions[idx]);
        }
        return;
      } else if (e.key === "Escape") {
        setAcOpen(false);
        return;
      }
    }

    // Enter without autocomplete open → apply filter
    if (e.key === "Enter") {
      e.preventDefault();
      setFilterQuery(acQuery().trim());
      setAcOpen(false);
      applyCurrentFilter();
      props.onSubmit?.();
    } else if (e.key === "Escape") {
      (e.target as HTMLInputElement).blur();
    }
  };

  const applyCurrentFilter = refreshFilteredItems;

  // Clearing is just "no filter", so it goes through the same refresh every
  // other filter change uses: with an empty query it asks the backend for the
  // unfiltered sort directly. The old shape fetched every path in the gallery
  // first and then discarded the list.
  const handleClear = async () => {
    clearAllFilters();
    await refreshFilteredItems();
  };

  const handleSetRatingFilter = async (value: number) => {
    if (ratingFilter()?.value === value && ratingFilter()?.op === ">=") {
      setRatingFilter(null);
    } else {
      setRatingFilter({ op: ">=", value });
    }
    await applyCurrentFilter();
  };

  const handleClearRatingFilter = async () => {
    setRatingFilter(null);
    await applyCurrentFilter();
  };

  // The five 1–5 star buttons, shared by the desktop inline row and the mobile
  // popover. `closeAfter` dismisses the mobile popover once a star is tapped;
  // that variant also gets real padding — a bare ~14px glyph is far below a
  // usable touch target.
  const ratingStars = (closeAfter: boolean) => (
    <For each={[1, 2, 3, 4, 5]}>
      {(star) => (
        <button
          class="cursor-pointer transition-colors leading-none"
          classList={{ "text-base p-2": closeAfter, "text-sm": !closeAfter }}
          style={{
            color: ratingFilter() && star <= ratingFilter()!.value ? "#f59e0b" : "#525252",
          }}
          onClick={() => {
            handleSetRatingFilter(star);
            if (closeAfter) setRatingMenuOpen(false);
          }}
          title={`Filter: rating >= ${star}`}
        >
          &#9733;
        </button>
      )}
    </For>
  );

  return (
    <div class="flex-1 relative min-w-0">
      <div class="flex items-center gap-1 bg-neutral-800/60 rounded px-2 py-1 border border-neutral-700/40">
        <Show
          when={isMobile()}
          fallback={
            <>
              {/* Rating filter pill */}
              <Show when={ratingFilter()}>
                <span class="inline-flex items-center gap-1 px-2 py-0.5 text-xs rounded bg-amber-900/40 text-amber-300">
                  {ratingFilter()!.op}{ratingFilter()!.value}&#9733;
                  <button
                    class="text-amber-400/60 hover:text-amber-200 cursor-pointer ml-0.5"
                    onClick={handleClearRatingFilter}
                  >
                    &times;
                  </button>
                </span>
              </Show>

              {/* Rating star quick-filter */}
              <div class="flex items-center gap-0 ml-0.5">{ratingStars(false)}</div>
            </>
          }
        >
          {/* Mobile: single star button opening a compact popover. */}
          <div class="relative shrink-0" ref={ratingRef}>
            <button
              class="flex items-center gap-0.5 px-1.5 py-0.5 rounded cursor-pointer leading-none text-sm"
              classList={{
                "text-amber-300 bg-amber-900/30": !!ratingFilter(),
                "text-neutral-500": !ratingFilter(),
              }}
              onClick={() => setRatingMenuOpen((v) => !v)}
              title="Filter by rating"
            >
              <span>&#9733;</span>
              <Show when={ratingFilter()}>
                <span class="text-xs">{ratingFilter()!.value}</span>
              </Show>
            </button>
            <Show when={ratingMenuOpen()}>
              <div
                class="absolute left-0 p-1.5 rounded shadow-lg z-50 flex items-center gap-0.5"
                classList={{ "top-full mt-1": !props.dropUp, "bottom-full mb-1": props.dropUp }}
                style={{
                  background: "rgba(20, 20, 20, 0.97)",
                  "backdrop-filter": "blur(12px)",
                  border: "1px solid rgba(255,255,255,0.08)",
                }}
              >
                {ratingStars(true)}
                <Show when={ratingFilter()}>
                  <button
                    class="text-neutral-400 hover:text-neutral-200 text-sm p-2 ml-1 cursor-pointer"
                    aria-label="Clear rating filter"
                    onClick={() => { handleClearRatingFilter(); setRatingMenuOpen(false); }}
                  >
                    &times;
                  </button>
                </Show>
              </div>
            </Show>
          </div>
        </Show>

        <input
          ref={(el) => { inputRef = el; props.onInputRef?.(el); }}
          type="text"
          value={acQuery()}
          onInput={(e) => handleInput(e.currentTarget.value)}
          onKeyDown={handleKeyDown}
          onFocus={() => {
            if (acSuggestions().length > 0) setAcOpen(true);
          }}
          onBlur={() => {
            setTimeout(() => setAcOpen(false), 200);
          }}
          placeholder={
            // The full syntax tour truncates uselessly in a phone-width input.
            isMobile()
              ? "Filter tags…"
              : "Filter... (e.g. user AND example, date>=2024-01-01, width>=1920, size>=10mb)"
          }
          class="flex-1 bg-transparent border-none outline-none text-sm text-neutral-200 placeholder-neutral-500 min-w-0"
        />

        <Show when={filterQuery() || ratingFilter()}>
          <button
            class="text-neutral-500 hover:text-neutral-300 text-xs cursor-pointer"
            onClick={handleClear}
          >
            Clear
          </button>
        </Show>
      </div>

      {/* Autocomplete dropdown */}
      <Show when={acOpen()}>
        <div
          class="absolute left-0 right-0 rounded overflow-hidden shadow-lg z-50 max-h-64 overflow-y-auto hide-scrollbar"
          classList={{ "top-full mt-1": !props.dropUp, "bottom-full mb-1": props.dropUp }}
          style={{
            background: "rgba(20, 20, 20, 0.95)",
            "backdrop-filter": "blur(12px)",
            border: "1px solid rgba(255,255,255,0.08)",
          }}
        >
          <For each={acSuggestions()}>
            {(suggestion, index) => (
              <div
                class="flex items-center justify-between px-3 py-2 cursor-pointer text-sm"
                style={{
                  background: acSelectedIndex() === index() ? "rgba(255,255,255,0.06)" : "transparent",
                }}
                onMouseEnter={() => setAcSelectedIndex(index())}
                onMouseDown={(e) => {
                  e.preventDefault();
                  insertSuggestion(suggestion);
                }}
              >
                <div class="flex items-center gap-2">
                  <Show when={suggestion.namespace === "_namespace"}>
                    <span class="px-1.5 py-0.5 text-xs rounded bg-violet-800/40 text-violet-300/80">
                      source
                    </span>
                  </Show>
                  <span class="text-neutral-200">{suggestion.tag}</span>
                </div>
                <span class="text-neutral-500 text-xs">{suggestion.count}</span>
              </div>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}
