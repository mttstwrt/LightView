import { createSignal } from "solid-js";
import type { TagSuggestion } from "../lib/types";

// Active filter pills (accumulated tag/rating selections)
const [filterPills, setFilterPills] = createSignal<
  { namespace: string; tag: string }[]
>([]);

// Rating filter (0 = no rating filter)
const [ratingFilter, setRatingFilter] = createSignal<{ op: string; value: number } | null>(null);

// Autocomplete state
const [acQuery, setAcQuery] = createSignal("");
const [acSuggestions, setAcSuggestions] = createSignal<TagSuggestion[]>([]);
const [acSelectedIndex, setAcSelectedIndex] = createSignal(0);
const [acOpen, setAcOpen] = createSignal(false);
const [recentTags, setRecentTags] = createSignal<string[]>([]);

export {
  filterPills, setFilterPills,
  ratingFilter, setRatingFilter,
  acQuery, setAcQuery,
  acSuggestions, setAcSuggestions,
  acSelectedIndex, setAcSelectedIndex,
  acOpen, setAcOpen,
  recentTags, setRecentTags,
};

/// Build the filter query string from the current pills and rating filter.
export function buildFilterQuery(): string {
  const parts: string[] = [];

  const pills = filterPills();
  if (pills.length > 0) {
    parts.push(
      pills.map((p) => `${p.namespace}:${p.tag}`).join(" AND ")
    );
  }

  const rf = ratingFilter();
  if (rf) {
    parts.push(`rating${rf.op}${rf.value}`);
  }

  return parts.join(" AND ");
}

export function addPill(namespace: string, tag: string) {
  setFilterPills((prev) => {
    if (prev.some((p) => p.namespace === namespace && p.tag === tag)) {
      return prev;
    }
    return [...prev, { namespace, tag }];
  });
}

export function removePill(index: number) {
  setFilterPills((prev) => prev.filter((_, i) => i !== index));
}

export function clearPills() {
  setFilterPills([]);
  setRatingFilter(null);
}
