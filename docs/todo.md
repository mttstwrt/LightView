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
site anywhere in the tree. Deleting only the client binding would be worse than
leaving it, since it strands the backend half; each chain has to go, or get
wired up, end to end.

They are not all the same kind of thing, and the kind determines the answer.

### Superseded — a better path already exists

Deleting these loses no capability. Each was replaced by something that does the
same job and is what the app actually calls.

| Command | Replaced by |
|---|---|
| `get_timeline_index` | The scrollbar indicators are computed client-side in `App.tsx` from `sortedItems`, which the frontend already holds. That version also does name and size indicators, not just dates, and needs no `items_per_row` round-trip. `CacheDb::compute_timeline`, `TimelineEntry`, and the unused `timeline` signal in `galleryStore` all belong to the old design. |
| `get_hardware_profile` | `get_debug_info` returns the same facts (storage type, cores, RAM, thumbnail threads, GPU active) already shaped for display, and is what `DebugOverlay` and `DevtoolsApp` call. |
| `get_thumbhashes`, `thumbhashUrl` | The ThumbHash now rides inline on every `SortedItem`, which is the whole point of the `LEFT JOIN thumbnails` in `get_sorted_items` — one payload instead of a second round-trip. |
| `get_gallery_info` | `get_boot_state`, which returns gallery info with the first payload rather than as its own call. |
| `get_cached_thumbnail_info` | `get_all_thumbnail_tiers`, sitting next to it and returning per-tier rows instead of only the Standard one. `InfoPanel` uses the richer form. |
| `get_thumbnail` | `get_thumbnails_batch` for the grid, and the 404-driven serve path for everything else. The singular form predates both. |

### Never finished — the capability is unreachable

| Command | State |
|---|---|
| `get_transformed_media` | Viewer rotate/exposure/colour. The largest of these by far: it is the only caller of `gpu_pipeline::transform_image` (~170 lines) and its 66-line WGSL shader, plus `apply_cpu_transforms` and roughly a third of `commands/viewer.rs`. No UI exposes any of it. |
| `get_gallery_stats`, `clear_cache` | A cache-maintenance surface in settings. `rebuild_thumbnails` next to them *is* wired, so the gap sits inside one screen. `get_gallery_stats` also reports `index_size_bytes` as a row count times 100, which would need fixing before it is shown to anyone. |

### Orphaned by omission

`close_gallery` is never called because the frontend switches galleries by
calling `open_gallery` again. That is mostly fine — `start_fs_watcher` retires
the previous watcher itself, and the old `CacheDb` and connection pool are
dropped when replaced. The one thing only `close_gallery` does is drop the
read-only pool *and then* checkpoint the WAL, so switching galleries leaves the
previous gallery's WAL to SQLite's own auto-checkpoint. Minor, but it is the
reason the command should be called on switch rather than deleted.

`reindex_gallery` is listed separately above: it has a second problem beyond
having no caller.

### Not one of these

`apply_plugin_tags` looks like one and is not: `lightview-worker` calls it over
HTTP, and its TS wrapper carries a comment saying it exists to document that
contract.

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
