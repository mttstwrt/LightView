import { createSignal, Show, For, onCleanup } from "solid-js";
import { sortField, setSortField, sortOrder, setSortOrder, groupBy } from "../../stores/settingsStore";
import { setDisplayPaths } from "../../stores/galleryStore";
import { filterPills, buildFilterQuery } from "../../stores/filterStore";
import { getSortedItems, applyFilter } from "../../lib/ipc";
import type { SortField, SortOrder } from "../../lib/types";

const SORT_OPTIONS: { field: SortField; label: string }[] = [
  { field: "date", label: "Date" },
  { field: "name", label: "Name" },
  { field: "size", label: "Size" },
  { field: "rating", label: "Rating" },
  { field: "media_type", label: "Type" },
];

export function SortMenu() {
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

  const reSort = async (field: SortField, order: SortOrder) => {
    setSortField(field);
    setSortOrder(order);
    try {
      const query = buildFilterQuery();
      if (query) {
        const filteredPaths = await applyFilter(query);
        const sorted = await getSortedItems(field, order, groupBy(), filteredPaths);
        setDisplayPaths(sorted.items.map((item) => item.path));
      } else {
        const sorted = await getSortedItems(field, order, groupBy());
        setDisplayPaths(sorted.items.map((item) => item.path));
      }
    } catch (e) {
      console.error("Sort error:", e);
    }
  };

  const handleSelectField = (field: SortField) => {
    if (field === sortField()) {
      // Toggle order
      const newOrder = sortOrder() === "desc" ? "asc" : "desc";
      reSort(field, newOrder);
    } else {
      // New field, default desc for date/rating/size, asc for name/type
      const defaultOrder: SortOrder =
        field === "name" || field === "media_type" ? "asc" : "desc";
      reSort(field, defaultOrder);
    }
    setOpen(false);
  };

  const currentLabel = () =>
    SORT_OPTIONS.find((o) => o.field === sortField())?.label ?? "Sort";

  const orderIcon = () => (sortOrder() === "desc" ? "\u2193" : "\u2191");

  return (
    <div class="relative">
      <button
        onClick={toggle}
        class="shrink-0 flex items-center gap-1 px-2.5 py-1.5 text-xs text-neutral-400 hover:text-neutral-200 bg-neutral-800 hover:bg-neutral-700 rounded transition-colors cursor-pointer"
        title="Sort"
      >
        <span>{currentLabel()}</span>
        <span class="text-neutral-500">{orderIcon()}</span>
      </button>

      <Show when={open()}>
        <div class="fixed inset-0 z-40" onClick={() => setOpen(false)} />

        <div
          class="absolute top-full right-0 mt-2 min-w-[140px] rounded-lg overflow-hidden shadow-xl z-50"
          style={{
            background: "rgba(18, 18, 18, 0.96)",
            "backdrop-filter": "blur(16px)",
            border: "1px solid rgba(255,255,255,0.08)",
          }}
        >
          <For each={SORT_OPTIONS}>
            {(opt) => {
              const isActive = () => sortField() === opt.field;
              return (
                <button
                  class="w-full text-left px-3 py-2 text-xs flex items-center justify-between cursor-pointer transition-colors"
                  classList={{
                    "text-teal-300 bg-teal-900/20": isActive(),
                    "text-neutral-300 hover:bg-neutral-700/50": !isActive(),
                  }}
                  onClick={() => handleSelectField(opt.field)}
                >
                  <span>{opt.label}</span>
                  <Show when={isActive()}>
                    <span class="text-teal-400/70">{orderIcon()}</span>
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
