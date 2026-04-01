import { createSignal, For, Show } from "solid-js";
import {
  acQuery, setAcQuery,
  acSuggestions, setAcSuggestions,
  acOpen, setAcOpen,
  acSelectedIndex, setAcSelectedIndex,
  filterQuery, setFilterQuery,
  ratingFilter, setRatingFilter,
  buildFilterQuery,
  clearAllFilters,
} from "../../stores/filterStore";
import { setDisplayPaths, setSortedItems } from "../../stores/galleryStore";
import { sortField, sortOrder, groupBy } from "../../stores/settingsStore";
import { autocompleteTags, applyFilter, clearFilter, getSortedItems } from "../../lib/ipc";

export function FilterBar() {
  let inputRef: HTMLInputElement | undefined;
  let debounceTimer: number | undefined;

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
    } else if (e.key === "Escape") {
      (e.target as HTMLInputElement).blur();
    }
  };

  const applyCurrentFilter = async () => {
    const query = buildFilterQuery();
    try {
      if (query) {
        const filteredPaths = await applyFilter(query);
        const sorted = await getSortedItems(sortField(), sortOrder(), groupBy(), filteredPaths);
        setSortedItems(sorted.items);
        setDisplayPaths(sorted.items.map((item) => item.path));
      } else {
        const sorted = await getSortedItems(sortField(), sortOrder(), groupBy());
        setSortedItems(sorted.items);
        setDisplayPaths(sorted.items.map((item) => item.path));
      }
    } catch (e) {
      console.error("Filter error:", e);
    }
  };

  const handleClear = async () => {
    clearAllFilters();
    try {
      await clearFilter();
      const sorted = await getSortedItems(sortField(), sortOrder(), groupBy());
      setSortedItems(sorted.items);
      setDisplayPaths(sorted.items.map((item) => item.path));
    } catch {}
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

  return (
    <div class="flex-1 relative">
      <div class="flex items-center gap-1 bg-neutral-800/60 rounded px-2 py-1 border border-neutral-700/40">
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
        <div class="flex items-center gap-0 ml-0.5">
          <For each={[1, 2, 3, 4, 5]}>
            {(star) => (
              <button
                class="cursor-pointer text-sm transition-colors leading-none"
                style={{
                  color: ratingFilter() && star <= ratingFilter()!.value ? "#f59e0b" : "#525252",
                }}
                onClick={() => handleSetRatingFilter(star)}
                title={`Filter: rating >= ${star}`}
              >
                &#9733;
              </button>
            )}
          </For>
        </div>

        <input
          ref={inputRef}
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
          placeholder="Filter... (e.g. user AND example, NOT auto::indoor)"
          class="flex-1 bg-transparent border-none outline-none text-sm text-neutral-200 placeholder-neutral-500 min-w-[80px]"
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
          class="absolute top-full left-0 right-0 mt-1 rounded overflow-hidden shadow-lg z-50 max-h-64 overflow-y-auto hide-scrollbar"
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
