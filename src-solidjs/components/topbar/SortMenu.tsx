// Sort field, direction, sub-sort, and grouping.
//
// Changing any of them re-runs the filter *and* the sort together rather than
// re-sorting what is on screen: the backend returns the ordered list, so a
// client-side re-sort would have to reimplement the tiebreakers and would drift
// from them.

import { createSignal, Show, For, onCleanup } from "solid-js";
import { isMobile } from "../../lib/runtime";
import { sortField, setSortField, sortOrder, setSortOrder, subSortField, setSubSortField, subSortOrder, setSubSortOrder, groupBy } from "../../stores/settingsStore";
import { setDisplayPaths, setSortedItems } from "../../stores/galleryStore";
import { buildFilterQuery } from "../../stores/filterStore";
import { getSortedItems, applyFilter } from "../../lib/ipc";
import type { SortField, SortOrder } from "../../lib/types";

const SORT_OPTIONS: { field: SortField; label: string }[] = [
  { field: "date", label: "Date" },
  { field: "name", label: "Name" },
  { field: "size", label: "Size" },
  { field: "rating", label: "Rating" },
  { field: "media_type", label: "Type" },
  { field: "lastviewed", label: "Recently Viewed" },
  { field: "dateadded", label: "Date Added" },
  { field: "lastrated", label: "Recently Rated" },
];

function defaultOrder(field: SortField): SortOrder {
  return field === "name" || field === "media_type" ? "asc" : "desc";
}

export function SortMenu(props: { dropUp?: boolean }) {
  const [open, setOpen] = createSignal(false);

  const toggle = () => setOpen((v) => !v);

  const handleKey = (e: KeyboardEvent) => {
    if (e.key === "Escape" && open()) {
      e.stopPropagation();
      setOpen(false);
    }
  };
  window.addEventListener("keydown", handleKey, true);
  onCleanup(() => window.removeEventListener("keydown", handleKey, true));

  const reSort = async (
    field: SortField,
    order: SortOrder,
    subField: SortField,
    subOrder: SortOrder,
  ) => {
    setSortField(field);
    setSortOrder(order);
    setSubSortField(subField);
    setSubSortOrder(subOrder);
    try {
      const query = buildFilterQuery();
      if (query) {
        const filteredPaths = await applyFilter(query);
        const sorted = await getSortedItems(field, order, groupBy(), filteredPaths, subField, subOrder);
        setSortedItems(sorted.items);
        setDisplayPaths(sorted.items.map((item) => item.path));
      } else {
        const sorted = await getSortedItems(field, order, groupBy(), undefined, subField, subOrder);
        setSortedItems(sorted.items);
        setDisplayPaths(sorted.items.map((item) => item.path));
      }
    } catch (e) {
      console.error("Sort error:", e);
    }
  };

  const handleSelectField = (field: SortField) => {
    if (field === sortField()) {
      // Toggle primary order
      const newOrder = sortOrder() === "desc" ? "asc" : "desc";
      reSort(field, newOrder, subSortField(), subSortOrder());
    } else {
      // New primary field — reset sub-sort to date desc
      reSort(field, defaultOrder(field), "date", "desc");
    }
    setOpen(false);
  };

  const handleSelectSubField = (field: SortField) => {
    if (field === subSortField()) {
      // Toggle sub order
      const newOrder = subSortOrder() === "desc" ? "asc" : "desc";
      reSort(sortField(), sortOrder(), field, newOrder);
    } else {
      reSort(sortField(), sortOrder(), field, defaultOrder(field));
    }
  };

  const currentLabel = () =>
    SORT_OPTIONS.find((o) => o.field === sortField())?.label ?? "Sort";

  const subLabel = () =>
    SORT_OPTIONS.find((o) => o.field === subSortField())?.label ?? "";

  const orderIcon = (order: SortOrder) => (order === "desc" ? "\u2193" : "\u2191");

  // Sub-sort options: everything except the current primary field
  const subOptions = () => SORT_OPTIONS.filter((o) => o.field !== sortField());

  return (
    <div class="relative shrink-0">
      <button
        onClick={toggle}
        class="shrink-0 flex items-center gap-1 px-2.5 py-1.5 text-xs text-neutral-400 hover:text-neutral-200 bg-neutral-800 hover:bg-neutral-700 rounded transition-colors cursor-pointer"
        title="Sort"
      >
        <span>{currentLabel()}</span>
        <span class="text-neutral-500">{orderIcon(sortOrder())}</span>
        {/* Sub-sort is secondary detail — hidden on mobile to keep the bar
            narrow enough for the settings/map buttons to fit. */}
        <Show when={!isMobile()}>
          <span class="text-neutral-600 mx-0.5">/</span>
          <span class="text-neutral-500">{subLabel()}</span>
          <span class="text-neutral-600">{orderIcon(subSortOrder())}</span>
        </Show>
      </button>

      <Show when={open()}>
        <div class="fixed inset-0 z-40" onClick={() => setOpen(false)} />

        <div
          class="absolute right-0 min-w-[220px] rounded-lg overflow-hidden shadow-xl z-50"
          classList={{
            "top-full mt-2": !props.dropUp,
            "bottom-full mb-2": props.dropUp,
          }}
          style={{
            background: "rgba(18, 18, 18, 0.96)",
            "backdrop-filter": "blur(16px)",
            border: "1px solid rgba(255,255,255,0.08)",
          }}
        >
          {/* Primary sort */}
          <div class="px-3 pt-2 pb-1 text-[10px] uppercase tracking-wider text-neutral-500">
            Sort by
          </div>
          <For each={SORT_OPTIONS}>
            {(opt) => {
              const isActive = () => sortField() === opt.field;
              return (
                <button
                  class="w-full text-left px-3 py-1.5 text-xs flex items-center justify-between cursor-pointer transition-colors"
                  classList={{
                    "text-teal-300 bg-teal-900/20": isActive(),
                    "text-neutral-300 hover:bg-neutral-700/50": !isActive(),
                  }}
                  onClick={() => handleSelectField(opt.field)}
                >
                  <span>{opt.label}</span>
                  <Show when={isActive()}>
                    <span class="text-teal-400/70">{orderIcon(sortOrder())}</span>
                  </Show>
                </button>
              );
            }}
          </For>

          {/* Divider */}
          <div class="mx-2 my-1 border-t border-neutral-700/50" />

          {/* Sub-sort */}
          <div class="px-3 pt-1 pb-1 text-[10px] uppercase tracking-wider text-neutral-500">
            Then by
          </div>
          <For each={subOptions()}>
            {(opt) => {
              const isActive = () => subSortField() === opt.field;
              return (
                <button
                  class="w-full text-left px-3 py-1.5 text-xs flex items-center justify-between cursor-pointer transition-colors"
                  classList={{
                    "text-teal-300 bg-teal-900/20": isActive(),
                    "text-neutral-300 hover:bg-neutral-700/50": !isActive(),
                  }}
                  onClick={() => handleSelectSubField(opt.field)}
                >
                  <span>{opt.label}</span>
                  <Show when={isActive()}>
                    <span class="text-teal-400/70">{orderIcon(subSortOrder())}</span>
                  </Show>
                </button>
              );
            }}
          </For>
        </div>
      </Show>
    </div>
  );
}
