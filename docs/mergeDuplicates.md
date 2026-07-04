# Merge Duplicates

Adds a **Merge** action to the duplicate-detection panel. Where "Trash" simply
discards a duplicate copy (losing whatever metadata that copy carried), **Merge**
lets the user pick one file to *keep*, fold selected metadata from the other
copies into the keeper's companion (and optionally its file mtime), then trash
the rest.

## Motivation

Near-identical duplicates usually differ only in their *metadata*, not their
pixels: one copy has user tags, another has a rating or the original file
timestamp, a third has GPS. Plain deletion throws that away. Merge preserves it
on a single surviving file.

## Data model — what is mergeable

| Field | Source | Merge rule | Write target |
|---|---|---|---|
| User tags | companion `tags.user` | union, editable (drop via checkbox) | keeper companion |
| Auto / plugin tags | companion `tags.auto`, `tags.plugins` | union, auto-folded (no per-tag UI) | keeper companion |
| Rating | companion `meta.core.rating` | per-field pick (default keeper, else sole non-empty) | keeper companion |
| Color label | companion `meta.core.color_label` | per-field pick | keeper companion |
| Notes | companion `meta.core.notes` | **pick one** (non-chosen shown so nothing is lost silently) | keeper companion |
| Companion location | `meta.core.location` | per-field pick | keeper companion |
| File mtime | filesystem / `media_meta.mtime` | per-field pick (default: earliest) | `filetime` on keeper file |
| Embedded EXIF GPS | `media_meta.gps_lat/lon` | read-only; if keeper has no companion location, offer "promote copy's GPS → keeper companion location" | keeper companion |
| Embedded EXIF (date / camera / etc.) | image bytes | **not touched** — shown as read-only context only | — |

Rationale for the EXIF boundary: LightView has no EXIF-write path, and rewriting
a keeper's image bytes to inject a discarded copy's EXIF risks corrupting the
original. GPS is the exception only because we can capture it *without* touching
image bytes — by writing it into the keeper's companion `location`.

## Backend (`src-tauri`)

### 1. `get_merge_candidates(paths) -> Vec<MergeCandidate>`
New read command in `commands/duplicates.rs`. Per path, one round-trip gathers
everything the popup needs:
- companion via `reader::read_companion_optional` → `tags.user/auto/plugins`,
  `meta.core.{rating, color_label, notes, location}`
- file mtime via `std::fs::metadata().modified()`
- EXIF GPS from `media_meta.gps_lat/gps_lon` (already indexed)
- `file_size`, `width`, `height` for display

### 2. `merge_duplicates(plan) -> Result<(), String>`
```
MergePlan {
  keeper: String,
  discard: Vec<String>,
  user_tags: Vec<String>,     // final resolved union
  rating: Option<u8>,
  color_label: Option<String>,
  notes: Option<String>,
  location: Option<Location>, // may be promoted from a copy's EXIF GPS
  set_mtime: Option<i64>,     // epoch secs to stamp on keeper, if changed
}
```
Steps:
1. `modify_companion(&keeper, |c| { … })` — reuse the helper from `tags.rs`
   (promoted to shared visibility), applying resolved tags/meta. Auto + plugin
   tag unions from the candidates are folded in here.
2. If `set_mtime` is set → stamp the keeper file (`filetime` crate).
3. `reindex_tags_for_file(&keeper, &companion)` + refresh autocomplete counts
   (mirrors `add_user_tag_impl`).
4. Trash the discards via `trash_files_impl` (reuses the existing,
   capability-gated trash path).

### 3. Registration & gating
Register both commands in `main.rs` and `http_server/api.rs` (web-client
parity). Gate `merge_duplicates` behind the same `delete` capability the Trash
button already uses.

### 4. New dependency
`filetime` in `src-tauri/Cargo.toml` (small, well-established, for stamping the
keeper's mtime).

## Frontend (`src-solidjs`)

### 1. `lib/ipc.ts`
Add `MergeCandidate` / `MergePlan` types and `getMergeCandidates(paths)` /
`mergeDuplicates(plan)` wrappers.

### 2. `MergeDialog.tsx` (new)
Modal opened from a group:
- Top row: each copy as a thumbnail with a **keeper radio** (default = current
  `is_best`).
- **Tags** section: unified chip list (union), each removable.
- **Conflict fields** (rating / color / notes / location / mtime): one row each,
  every copy's value shown as a selectable chip; auto-selects the keeper's or
  the sole non-empty value. Location row also surfaces any EXIF-only GPS with a
  "use this" affordance.
- Read-only context line per copy: embedded EXIF date / GPS (informational).
- Footer: **Merge** (calls `mergeDuplicates`, closes, updates group state) /
  Cancel.

### 3. `DuplicatesPanel.tsx`
Add a **Merge** button beside "Not duplicates" in each group header (shown only
when `capabilities().delete`). On success, reuse the existing
group-dissolution / `displayPaths` / `totalCount` update logic from
`handleTrash`, removing all discarded paths at once.

## Cache correctness
- Keeper companion write → `reindex_tags_for_file` (live).
- Discarded copies removed from the index by the existing trash path.
- Keeper's image bytes are unchanged → no thumbnail `?v=` cache-bust needed.

## Testing
- Rust unit test for `merge_duplicates`: temp gallery, two companions with
  disjoint tags + conflicting ratings; run a plan; assert the keeper companion
  holds the union / resolved values and the discards are trashed.
- Manual via `lightview-headless` to exercise the web path + capability gating.

## Out of scope
- Rewriting embedded EXIF into image bytes.
- Merging across arbitrary (non-duplicate-group) selections.
