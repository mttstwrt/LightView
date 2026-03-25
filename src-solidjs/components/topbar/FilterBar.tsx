import { createSignal, For, Show } from "solid-js";
import {
  acQuery, setAcQuery,
  acSuggestions, setAcSuggestions,
  acOpen, setAcOpen,
  acSelectedIndex, setAcSelectedIndex,
  filterPills, addPill, removePill, clearPills,
  ratingFilter, setRatingFilter,
  buildFilterQuery,
} from "../../stores/filterStore";
import { setDisplayPaths } from "../../stores/galleryStore";
import { sortField, sortOrder, groupBy } from "../../stores/settingsStore";
import { autocompleteTags, applyFilter, clearFilter, getSortedItems } from "../../lib/ipc";

export function FilterBar() {
  let debounceTimer: number | undefined;

  const handleInput = (value: string) => {
    setAcQuery(value);
    setAcSelectedIndex(0);

    if (debounceTimer) clearTimeout(debounceTimer);
    debounceTimer = window.setTimeout(async () => {
      if (value.trim().length > 0) {
        try {
          const suggestions = await autocompleteTags(value.trim());
          setAcSuggestions(suggestions);
          setAcOpen(suggestions.length > 0);
        } catch {
          setAcSuggestions([]);
        }
      } else {
        setAcSuggestions([]);
        setAcOpen(false);
      }
    }, 150);
  };

  const selectSuggestion = async (namespace: string, tag: string) => {
    addPill(namespace, tag);
    setAcQuery("");
    setAcOpen(false);
    setAcSuggestions([]);
    await applyCurrentFilter();
  };

  const handleRemovePill = async (index: number) => {
    removePill(index);
    await applyCurrentFilter();
  };

  const handleClear = async () => {
    clearPills();
    setAcQuery("");
    try {
      const paths = await clearFilter();
      const sorted = await getSortedItems(sortField(), sortOrder(), groupBy());
      setDisplayPaths(sorted.items.map((item) => item.path));
    } catch {}
  };

  const applyCurrentFilter = async () => {
    const query = buildFilterQuery();
    try {
      if (query) {
        const filteredPaths = await applyFilter(query);
        const sorted = await getSortedItems(sortField(), sortOrder(), groupBy(), filteredPaths);
        setDisplayPaths(sorted.items.map((item) => item.path));
      } else {
        const sorted = await getSortedItems(sortField(), sortOrder(), groupBy());
        setDisplayPaths(sorted.items.map((item) => item.path));
      }
    } catch (e) {
      console.error("Filter error:", e);
    }
  };

  const handleKeyDown = (e: KeyboardEvent) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setAcSelectedIndex((i) => Math.min(i + 1, acSuggestions().length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setAcSelectedIndex((i) => Math.max(i - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      const suggestions = acSuggestions();
      const idx = acSelectedIndex();
      if (suggestions[idx]) {
        selectSuggestion(suggestions[idx].namespace, suggestions[idx].tag);
      }
    } else if (e.key === "Escape") {
      setAcOpen(false);
    }
  };

  const handleSetRatingFilter = async (value: number) => {
    if (ratingFilter()?.value === value && ratingFilter()?.op === ">=") {
      // Clicking the same star clears the filter
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
        {/* Active filter pills */}
        <For each={filterPills()}>
          {(pill, index) => (
            <span class="inline-flex items-center gap-1 px-2 py-0.5 text-xs rounded bg-neutral-700 text-neutral-200">
              <span class="text-teal-400/70">{pill.namespace}:</span>
              {pill.tag}
              <button
                class="text-neutral-400 hover:text-neutral-200 cursor-pointer ml-0.5"
                onClick={() => handleRemovePill(index())}
              >
                &times;
              </button>
            </span>
          )}
        </For>

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
          type="text"
          value={acQuery()}
          onInput={(e) => handleInput(e.currentTarget.value)}
          onKeyDown={handleKeyDown}
          onFocus={() => {
            if (acSuggestions().length > 0) setAcOpen(true);
          }}
          onBlur={() => {
            // Delay to allow click on suggestions
            setTimeout(() => setAcOpen(false), 200);
          }}
          placeholder={filterPills().length > 0 ? "" : "Filter..."}
          class="flex-1 bg-transparent border-none outline-none text-sm text-neutral-200 placeholder-neutral-500 min-w-[80px]"
        />

        <Show when={filterPills().length > 0 || ratingFilter()}>
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
          class="absolute top-full left-0 right-0 mt-1 rounded overflow-hidden shadow-lg z-50 max-h-64 overflow-y-auto"
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
                  selectSuggestion(suggestion.namespace, suggestion.tag);
                }}
              >
                <div class="flex items-center gap-2">
                  <span class="px-1.5 py-0.5 text-xs rounded bg-teal-800/40 text-teal-300/80">
                    {suggestion.namespace}
                  </span>
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
