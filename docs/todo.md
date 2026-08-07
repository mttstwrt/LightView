# Open work

[← docs index](README.md)

Known gaps, in rough priority order. Anything here is understood but not done;
anything with enough shape to be designed belongs in a subsystem page instead.

## The tree is not rustfmt-formatted

`cargo fmt --check` fails on ~70 files, and formatting would be a ~4,200-line
diff. Until that lands as its own commit, the gate advertised in `AGENTS.md` is
aspirational and `cargo clippy --fix` is a trap: its let-chain rewrites leave
bodies at the old indentation and only look right after a `cargo fmt` you cannot
scope to one change. Worth doing when no branches are in flight, together with
the ~60 remaining clippy style warnings (`collapsible_if` dominates). See
[`build-and-verify.md`](build-and-verify.md).

## ~250–300 duplicated lines between the two grids

Structurally identical fetch loops, eviction, pruning, and URL versioning, with
small policy differences that make a naive merge unsafe.
[`frontend/grid-loading.md`](frontend/grid-loading.md) inventories the
differences and proposes a four-step extraction ordered smallest-risk-first.
Each step needs browser verification, not just `tsc`.

## Colour-label filtering is silently inert

`color_label` lives only in the companion file, so `FilterExpr::ColorLabel`
compiles to a tautology and the term is ignored rather than rejected. Fixing it
means indexing the column into `media_meta` at scan time — a migration plus a
change to the indexer. Until then the parser accepts a query the evaluator
cannot honour, which is the worst of the available states. See
[`query/`](query/README.md).

## `reindex_gallery` does not regenerate thumbnails

Re-indexing rebuilds the media and tag indexes but does not kick off background
thumbnail regeneration, so a re-index after a bulk external edit leaves stale
thumbnails until something else asks for them.

## Several backend commands have no consumer

Each of these is a complete chain — a registered `#[tauri::command]`, a typed
wrapper in `lib/ipc.ts`, and in some cases an `/api/invoke` arm — with no call
site anywhere in the tree. They are listed together because the decision is the
same for all of them and it is a product decision, not a cleanup: either wire up
the UI or delete the chain end to end. Deleting only the client binding would
be worse than leaving it, since it strands the backend half.

| Command | What it would drive |
|---|---|
| `get_timeline_index` | The scrollbar's date markers. `CacheDb::compute_timeline` and `TimelineEntry` exist and work; nothing asks for them. |
| `get_transformed_media` | Viewer rotate/exposure/colour. This one is the largest: it also strands `gpu_pipeline::transform_image`, its WGSL shader, and the `apply_cpu_transforms` fallback. |
| `get_gallery_stats`, `clear_cache`, `reindex_gallery` | A cache-maintenance surface in settings. `rebuild_thumbnails` next to them *is* wired, so the gap is visible in one screen. |
| `get_hardware_profile` | Showing the detected profile that already drives pool sizing. |
| `get_cached_thumbnail_info`, `get_thumbnail` | Per-path thumbnail introspection; the batch forms are used. |
| `get_thumbhashes`, `thumbhashUrl` | Superseded — the ThumbHash now rides inline on each sorted item. |
| `close_gallery`, `get_gallery_info` | Superseded by `get_boot_state`, which returns gallery info with the first payload. |

`apply_plugin_tags` looks like one of these and is not: `lightview-worker` calls
it over HTTP, and its TS wrapper carries a comment saying it exists to document
that contract.

## Absolute paths as primary keys

Every path-keyed row stores an absolute path, which is why `rebase_root` and
`infer_old_root` exist. Storing gallery-relative paths would delete that entire
mechanism, but it is a migration touching every table and every query, and the
current machinery works and is tested. Recorded as a structural observation, not
a recommendation — see
[decision 0001](decisions/0001-one-cache-per-gallery.md).

## `detect_legacy_version` is probably dead

It only runs for databases with no `schema_version` stamp, and versioning has
been in place since v1 of the schema. The ladder is now cheap and correct so
there is no cost to keeping it, but if it could be established that no such
database exists in the wild, deleting it would remove the whole class of
over-reporting bug that [decision 0003](decisions/0003-derive-schema-version-from-migrations.md)
was written about.

## Smaller items

- **A worker pool for image decode on the client.** A single decode worker is
  fine in practice — the browser parallelizes `createImageBitmap` — but a small
  pool would remove the JS orchestration bottleneck under burst load.
- **A virtual folder view.** Default hierarchy: plugin names and user folders at
  the top level, then a folder per tag inside. Entirely virtual — it would never
  copy or move files.
- **`too_many_arguments` on the thumbnail write paths.** `write_standard_row`
  and `write_tier_row` take nine positional arguments each, two of them adjacent
  `u32`s (`width`, `height`). Transposing them at a call site would compile and
  store a wrong aspect ratio. A small `ThumbRow` struct removes the class.
- **The `?fit=` resize route has no coalescer**, unlike the tier serve path, so
  concurrent requests for the same resize each do the work.
