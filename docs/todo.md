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

## Memory-pressure polling 403s on the web client

`lib/memoryPressure.ts` polls `get_memory_status`, which is not in the
`/api/invoke` allowlist and never has been. The poll is wrapped in a bare
`try/catch`, so every cycle fails silently and the viewer cache's
pressure-based eviction never engages in a browser — only on the desktop.

Adding it to the allowlist is the wrong fix: the command reports the *server's*
RAM, and sizing a phone's image cache from the host's free memory is
meaningless. The web client should either use its own signal
(`performance.memory`, `navigator.deviceMemory`) or not poll at all. Either way
the empty `catch` should stop hiding it.

Found by driving the SPA against `lightview-headless`; it is invisible from
`tsc` and from the Rust tests.

## Absolute paths as primary keys

Every path-keyed row stores an absolute path, which is why `rebase_root` and
`infer_old_root` exist. Storing gallery-relative paths would delete that entire
mechanism, but it is a migration touching every table and every query, and the
current machinery works and is tested. Recorded as a structural observation, not
a recommendation — see
[decision 0001](decisions/0001-one-cache-per-gallery.md).

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
