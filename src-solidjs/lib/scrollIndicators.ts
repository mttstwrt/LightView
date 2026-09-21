// Labels for the scrollbar's track, derived from the item list and the sort.
//
// Pure functions of `(items, sortField)`: which markers sit where on the rail,
// and what the drag thumb reads at a given fraction. They live here rather than
// in `App` because `App` is the shell — what is on screen and how it is wired —
// and this is gallery logic with its own tuning (the hoisted `Intl` formatters
// below are a measured fix, not a style preference).

import type { ScrollIndicator } from "../components/shared/ScrollBar";
import type { SortedItem, SortField } from "./types";

/** The timestamp column a sort field orders by, for the date-shaped fields.
 *  Returns null for fields that aren't dates — those get their own labelling
 *  below. Keeps "Recently Viewed" and friends navigable on the scrollbar
 *  instead of falling through to a blank track. */
function dateAccessor(field: SortField): ((item: SortedItem) => number | null) | null {
  switch (field) {
    case "date":
      return (it) => it.date;
    case "lastviewed":
      return (it) => it.last_viewed;
    case "dateadded":
      return (it) => it.date_added;
    case "lastrated":
      return (it) => it.last_rated;
    default:
      return null;
  }
}

export function buildScrollIndicators(items: SortedItem[], field: SortField): ScrollIndicator[] {
  if (items.length === 0) return [];

  const getDate = dateAccessor(field);
  if (getDate) return buildDateIndicators(items, getDate);

  switch (field) {
    case "name":
      return buildNameIndicators(items);
    case "size":
      return buildSizeIndicators(items);
    case "rating":
      return buildRatingIndicators(items);
    default:
      return [];
  }
}

// Lazily-built, reused date formatters. `Date#toLocaleDateString(locale, opts)`
// constructs a fresh Intl.DateTimeFormat on every call — ~100µs each — so
// calling it once per item turned this O(n) walk into seconds of blocked main
// thread on a large gallery. Hoisting the formatter makes the same walk ~40x
// cheaper; the loop below additionally only formats at a month boundary.
let _monthFmt: Intl.DateTimeFormat | undefined;
const monthFormat = (d: Date) =>
  (_monthFmt ??= new Intl.DateTimeFormat(undefined, { month: "short", year: "numeric" })).format(d);

let _dayFmt: Intl.DateTimeFormat | undefined;
const dayFormat = (d: Date) =>
  (_dayFmt ??= new Intl.DateTimeFormat(undefined, {
    day: "numeric",
    month: "short",
    year: "numeric",
  })).format(d);

function buildDateIndicators(
  items: SortedItem[],
  getDate: (item: SortedItem) => number | null,
): ScrollIndicator[] {
  const indicators: ScrollIndicator[] = [];
  // Compare a cheap numeric month key rather than the rendered label, so the
  // formatter runs once per month boundary (a couple of dozen times) instead
  // of once per item.
  let lastKey = Number.NaN;
  for (let i = 0; i < items.length; i++) {
    const ts = getDate(items[i]);
    if (!ts) continue;
    const d = new Date(ts * 1000);
    const key = d.getFullYear() * 12 + d.getMonth();
    if (key !== lastKey) {
      indicators.push({ position: i / items.length, label: monthFormat(d) });
      lastKey = key;
    }
  }
  return dedupeIndicators(indicators);
}

function buildNameIndicators(items: SortedItem[]): ScrollIndicator[] {
  const indicators: ScrollIndicator[] = [];
  let lastChar = "";
  for (let i = 0; i < items.length; i++) {
    const name = items[i].path.split("/").pop() ?? "";
    const ch = name.charAt(0).toUpperCase();
    if (ch && ch !== lastChar) {
      indicators.push({ position: i / items.length, label: ch });
      lastChar = ch;
    }
  }
  return dedupeIndicators(indicators);
}

function formatSizeShort(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

function buildSizeIndicators(items: SortedItem[]): ScrollIndicator[] {
  // Place indicators at size order-of-magnitude boundaries
  const thresholds = [
    100 * 1024,        // 100 KB
    500 * 1024,        // 500 KB
    1024 * 1024,       // 1 MB
    5 * 1024 * 1024,   // 5 MB
    10 * 1024 * 1024,  // 10 MB
    50 * 1024 * 1024,  // 50 MB
    100 * 1024 * 1024, // 100 MB
  ];
  const indicators: ScrollIndicator[] = [];
  let tIdx = 0;
  // Determine sort direction from first vs last
  const ascending = items.length > 1 && items[0].file_size <= items[items.length - 1].file_size;

  for (let i = 0; i < items.length && tIdx < thresholds.length; i++) {
    const size = items[i].file_size;
    const threshold = thresholds[ascending ? tIdx : thresholds.length - 1 - tIdx];
    const crossed = ascending ? size >= threshold : size <= threshold;
    if (crossed) {
      indicators.push({ position: i / items.length, label: formatSizeShort(threshold) });
      tIdx++;
    }
  }
  return dedupeIndicators(indicators);
}

function buildRatingIndicators(items: SortedItem[]): ScrollIndicator[] {
  const indicators: ScrollIndicator[] = [];
  let lastRating = -1;
  for (let i = 0; i < items.length; i++) {
    const r = items[i].rating ?? 0;
    if (r !== lastRating) {
      indicators.push({ position: i / items.length, label: r === 0 ? "Unrated" : "\u2605".repeat(r) });
      lastRating = r;
    }
  }
  return dedupeIndicators(indicators);
}

/** Thin out indicators so they don't overlap — keep at most ~15, evenly spaced. */
function dedupeIndicators(indicators: ScrollIndicator[]): ScrollIndicator[] {
  if (indicators.length <= 15) return indicators;
  const step = Math.ceil(indicators.length / 15);
  const result: ScrollIndicator[] = [];
  for (let i = 0; i < indicators.length; i += step) {
    result.push(indicators[i]);
  }
  return result;
}

export function getThumbLabelForItems(items: SortedItem[], field: SortField, fraction: number): string {
  if (items.length === 0) return "";
  const idx = Math.min(Math.floor(fraction * items.length), items.length - 1);
  const item = items[idx];

  const getDate = dateAccessor(field);
  if (getDate) {
    const ts = getDate(item);
    // "Never" reads right for the un-stamped tail of viewed/rated/added sorts,
    // which SQL parks at the end via NULLS LAST.
    // The date sort coalesces to the file time server-side, so every item has
    // one; "No date" survives only for a row the server could not date at all.
    if (!ts) return field === "date" ? "No date" : "Never";
    return dayFormat(new Date(ts * 1000));
  }

  switch (field) {
    case "name": {
      const name = item.path.split("/").pop() ?? "";
      return name.length > 20 ? name.slice(0, 20) + "\u2026" : name;
    }
    case "size":
      return formatSizeShort(item.file_size);
    case "rating": {
      const r = item.rating ?? 0;
      return r === 0 ? "Unrated" : "\u2605".repeat(r);
    }
    default:
      return "";
  }
}
