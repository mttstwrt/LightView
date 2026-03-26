import { createSignal } from "solid-js";
import type { TagSuggestion } from "../lib/types";

// The raw filter query string (e.g. "user AND example OR rating>=3")
const [filterQuery, setFilterQuery] = createSignal("");

// Rating filter (0 = no rating filter) — appended to query on apply
const [ratingFilter, setRatingFilter] = createSignal<{ op: string; value: number } | null>(null);

// Autocomplete state
const [acQuery, setAcQuery] = createSignal("");
const [acSuggestions, setAcSuggestions] = createSignal<TagSuggestion[]>([]);
const [acSelectedIndex, setAcSelectedIndex] = createSignal(0);
const [acOpen, setAcOpen] = createSignal(false);

export {
  filterQuery, setFilterQuery,
  ratingFilter, setRatingFilter,
  acQuery, setAcQuery,
  acSuggestions, setAcSuggestions,
  acSelectedIndex, setAcSelectedIndex,
  acOpen, setAcOpen,
};

/// Build the full filter query from the text input and rating filter.
export function buildFilterQuery(): string {
  const parts: string[] = [];

  const q = filterQuery().trim();
  if (q) {
    parts.push(q);
  }

  const rf = ratingFilter();
  if (rf) {
    parts.push(`rating${rf.op}${rf.value}`);
  }

  return parts.join(" AND ");
}

/// Clear all filter state.
export function clearAllFilters() {
  setFilterQuery("");
  setRatingFilter(null);
  setAcQuery("");
  setAcSuggestions([]);
  setAcOpen(false);
}
