// The active filter: the query text, the rating control, and autocomplete state.
//
// The rating filter is kept separate from the query string and appended at
// apply time, because it is a discrete control rather than something the user
// typed — round-tripping it through the text would mean parsing the query back
// out to know where the slider should sit.

import { createSignal } from "solid-js";
import type { TagSuggestion } from "../lib/types";
import { refresh } from "./galleryStore";

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

/// Apply the current filter state (query + rating) to the gallery.
///
/// One query does it: the filter compiles into the same statement the sort
/// orders, so the round trip that handed a list of matched paths back to the
/// server to be re-expanded is gone.
export async function refreshFilteredItems() {
  try {
    await refresh({ filter: buildFilterQuery() });
  } catch (e) {
    console.error("Filter error:", e);
  }
}

/// Replace the filter with `query` and apply it. Also mirrors the query into
/// the filter bar's input so the active filter is visible and editable there.
/// Used by tappable tag chips in the viewer's info panel.
export async function applyQueryAndRefresh(query: string) {
  setFilterQuery(query);
  setAcQuery(query);
  setAcOpen(false);
  setAcSuggestions([]);
  await refreshFilteredItems();
}

/// Clear all filter state.
export function clearAllFilters() {
  setFilterQuery("");
  setRatingFilter(null);
  setAcQuery("");
  setAcSuggestions([]);
  setAcOpen(false);
}
