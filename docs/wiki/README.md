# LightView engineering wiki

Durable knowledge about *why* the code is shaped the way it is — the reasoning
that outlives any single change, and the rules a change must not quietly break.

## What lives where

| Document | Purpose |
|---|---|
| [`invariants.md`](invariants.md) | The load-bearing rules. Read before changing the cache, the thumbnail pipeline, or the remote API. |
| [`thumbnail-pipeline.md`](thumbnail-pipeline.md) | The tier system end to end: what each tier is for, who generates it, how it's bounded. |
| [`cache-schema.md`](cache-schema.md) | SQLite schema, the migration contract, and how to add a table or column safely. |
| [`grid-loading.md`](grid-loading.md) | The two grids, the primitives they share, and the duplication still between them. |
| [`build-and-verify.md`](build-and-verify.md) | Getting a compiling checkout, and how to exercise the whole stack without a display. |
| [`review-2026-08.md`](review-2026-08.md) | Findings from the August 2026 codebase review: what was fixed, what is still open. |

## Relationship to the other docs

- **`CLAUDE.md` / `AGENTS.md`** (repo root) are the orientation documents: what
  the project is, which commands to run, where modules live, and a dense set of
  hard-won operational gotchas. They are the first thing to read.
- **This wiki** is the second thing. It goes deeper on the subsystems that are
  intricate enough that a newcomer will otherwise reconstruct the reasoning by
  trial and error, and it is where a review's conclusions are recorded so they
  are not rediscovered later.
- **`docs/*.md`** (the flat files alongside this directory) are per-feature
  design notes written at the time a feature was built —
  [`workerTagging.md`](../workerTagging.md),
  [`pluginExtensibility.md`](../pluginExtensibility.md),
  [`scrollLoadingRedesign.md`](../scrollLoadingRedesign.md),
  [`mergeDuplicates.md`](../mergeDuplicates.md),
  [`jpegDecodePerformance.md`](../jpegDecodePerformance.md). They are
  point-in-time records; this wiki is maintained.

## Conventions for editing this wiki

- **Record the reason, not the behavior.** The code says what it does; a wiki
  page earns its place by saying why the obvious alternative is wrong. Where a
  choice was measured, give the numbers.
- **Say when something is reasoned rather than measured.** Several performance
  decisions in this codebase apply to WebKitGTK, which no automated harness
  here can measure. Those are marked as such in the source; keep that honesty.
- **Date the findings pages.** `review-*.md` files are snapshots and should not
  be edited in place once a later review supersedes them — add a new one and
  link back.
