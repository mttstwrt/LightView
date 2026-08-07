# Duplicates

[← docs index](../README.md) · [architecture](../architecture.md)

**Responsible for:** finding near-identical copies of the same image, and
resolving a group down to one file without losing the metadata the other copies
carried. Detection lives in `cache/duplicates.rs`; the user-facing operations —
`find_duplicates`, `mark_not_duplicates`, `get_merge_candidates`,
`merge_duplicates` — live in `commands/duplicates.rs`.

**Not responsible for:** deleting files (`commands/trash.rs`), writing the
`.lightview` sidecar ([`companion/`](../companion/README.md)), or generating the
thumbnails detection reads from ([`pipeline/`](../pipeline/README.md)).

**Depends on:** [`cache/`](../cache/README.md) for the `thumbnails` and
`not_duplicates` tables, `companion/` for reading and merging metadata, and the
trash path for discarding. **Depended on by:** `DuplicatesPanel.tsx` and
`MergeDialog.tsx`, and four arms of the
[`/api/invoke` allowlist](../remote/README.md).

**Invariant:** a merge writes the keeper's *companion* file and, optionally, its
mtime. It never rewrites image bytes. See the EXIF boundary below — this is the
reason the feature is shaped the way it is.

## Detection

Detection is perceptual, not byte-exact, because the interesting duplicates are
re-encodes and re-exports rather than literal copies.

Each cached Standard thumbnail gets a 64-bit **dHash**: downscale to 9×8
greyscale, compare each pixel to its right neighbour, and emit the 8×8 = 64
comparison bits. Working from the *thumbnail* rather than the original is the
whole trick — the bytes are already decoded and already in the cache, so
hashing an entire gallery costs no source decodes at all. The hash is stored in
a `phash` column on the `thumbnails` table, so it is discarded and recomputed
along with the thumbnail it describes.

`compute_phashes_batch` fills in missing hashes a bounded batch at a time. It is
called from two places: the duplicate finder itself (so opening the panel makes
progress even on a cold gallery) and the idle backfill worker (so on a headless
server the work is already done by the time anyone asks).

`find_duplicates(threshold)` then compares every pair by Hamming distance —
0 is byte-identical pixels, ~10 is loose. This is quadratic in the number of
hashed files, which is acceptable at gallery scale and is the reason the
threshold is a parameter rather than a constant: a tighter threshold is not
cheaper, so the knob exists for precision, not for cost.

Pairs the user has explicitly rejected are recorded in `not_duplicates` with
`path_a < path_b` so lookups are canonical. That table is deliberately *outside*
`path_keyed_tables()` — its paths are not in a `path` column — so every sweep
that removes or relocates files handles it separately. See the
[cache invariants](../cache/README.md#invariants-callers-must-uphold).

## Merge

Trashing a duplicate discards whatever metadata that copy carried. Near-identical
duplicates usually differ *only* in metadata: one copy has user tags, another has
a rating or the original file timestamp, a third has GPS. Merge lets the user
pick one file to keep, fold the others' metadata into its companion, and trash
the rest.

### What is mergeable

| Field | Source | Merge rule | Write target |
|---|---|---|---|
| User tags | companion `tags.user` | union, editable (drop via checkbox) | keeper companion |
| Auto / plugin tags | companion `tags.auto`, `tags.plugins` | union, auto-folded (no per-tag UI) | keeper companion |
| Rating | companion `meta.core.rating` | per-field pick (default keeper, else sole non-empty) | keeper companion |
| Color label | companion `meta.core.color_label` | per-field pick | keeper companion |
| Notes | companion `meta.core.notes` | **pick one** (non-chosen shown so nothing is lost silently) | keeper companion |
| Companion location | `meta.core.location` | per-field pick | keeper companion |
| File mtime | filesystem / `media_meta.mtime` | per-field pick (default: earliest) | `filetime` on the keeper file |
| Embedded EXIF GPS | `media_meta.gps_lat/lon` | read-only; if the keeper has no companion location, offer to promote a copy's GPS into it | keeper companion |
| Embedded EXIF (date, camera, …) | image bytes | **not touched** — shown as read-only context | — |

### The EXIF boundary

LightView has no EXIF *write* path, and acquiring one to inject a discarded
copy's metadata would mean rewriting the keeper's image bytes — risking the
original in exchange for fields nothing in the app reads back. GPS is the single
exception, and only because it can be captured *without* touching image bytes:
it is written into the keeper's companion `location` instead.

That asymmetry is the thing to understand before extending this feature. Anything
that can be expressed in the companion is mergeable; anything that would require
re-encoding the file is not.

### The operation

`get_merge_candidates(paths)` gathers, per path in one round-trip, everything the
dialog needs to show: the companion (`tags.*` and `meta.core.{rating,
color_label, notes, location}`), the file mtime, indexed EXIF GPS from
`media_meta`, and size/dimensions for display.

`merge_duplicates(plan)` then applies a fully-resolved `MergePlan` — the dialog
does the resolving, the backend does no conflict logic of its own:

1. `modify_companion(&keeper, …)` applies the resolved tags and meta, folding in
   the auto and plugin tag unions. This is the same helper `commands/tags.rs`
   uses, so a merge and a hand-edit write the file identically.
2. If the plan sets an mtime, stamp the keeper file.
3. `reindex_tags_for_file` plus an autocomplete-count refresh, mirroring
   `add_user_tag_impl`, so the tag index reflects the merge immediately rather
   than at the next gallery open.
4. Trash the discards through `trash_files_impl` — the existing,
   capability-gated path, so merge inherits its confinement and its undo.

The keeper's image bytes are unchanged, so no thumbnail cache-bust is needed;
the discards leave the index via the trash path's normal sweep.

`merge_duplicates` sits behind the same `delete` capability as the Trash button,
because step 4 is a delete.

### Out of scope

Rewriting embedded EXIF into image bytes, and merging across arbitrary
selections that are not a detected duplicate group.
