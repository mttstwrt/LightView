# 0004 — Bound the zoom tiers by bytes, with hysteresis

[← docs index](../README.md) · [pipeline](../pipeline/README.md)

## Context

Five of the seven thumbnail tiers are generated for everything in the gallery
and are small enough that their total is proportional to the library — bounding
them would only mean regenerating what the user is certain to want again.

The two zoom tiers are different. `jm` (1280) and `jh` (2560) cache whatever the
user happens to view zoomed in, so they grow with browsing rather than with the
library, and their rows are one to two orders of magnitude larger than the
others'. Left alone they grow without limit.

## Options considered

**Row-capped LRU** — keep the newest N rows. One number, trivial to implement.

**Byte-budgeted LRU** — keep rows until an accumulated size budget is reached.

**Time-to-live** — drop rows older than N days.

For "warmest", the ordering key could be insertion order (rowid, already
present) or a real access timestamp (a new column, therefore a migration).

## Decision

A byte budget per tier — 10% of free disk, floored at 512 MiB, capped at 8 GiB —
with LRU eviction keyed on an `accessed_at` column (schema v15), and eviction
firing only past 1.25× the budget.

**Bytes, not rows,** because row size varies by more than an order of magnitude
across these tiers, so any fixed row count maps to a wildly variable footprint.
A cap that is right for a gallery of phone snapshots is wrong by ~10× for one of
camera raws. A window function accumulates sizes warmest-first and drops
everything past the budget.

**A real `accessed_at`, not rowid order,** because rowid encodes when a row was
*inserted*. Scrolling back over cells you were just looking at could evict them:
they were inserted early, which under a rowid FIFO is exactly what "cold" means.
The migration was worth it.

**Hysteresis at 1.25×** because there are two write paths — on-demand serves and
batch generation — and originally only the batch path evicted. The table grew
unbounded between batch calls and then shed the entire overshoot in one
multi-second `DELETE` that held the cache mutex and dropped the whole working
set at once. Both write paths now call `enforce_tier_budget`, and the high-water
mark keeps each pass small rather than making every write pay for a trim.

TTL was rejected because it answers the wrong question: a tier the user has not
touched in a month should be evicted whether it is old or new, and a tier they
use daily should survive regardless of age.

## Consequences

- `LIGHTVIEW_TIER_BUDGET_MB` overrides the budget per tier, in MiB, bypassing
  the floor. It exists so the cache can be pinned small on a constrained host or
  in a test — this is one of the few knobs in the codebase, and it earns its
  place by making eviction testable at all.
- Freshly written rows must be seeded warm. `write_tier_row` sets `accessed_at`
  to now for capped tiers; leaving it at the column default (0) would mark every
  new row maximally cold, so the next eviction would delete exactly what was
  just generated for the current viewport.
- The serve path cannot write, so access marks are buffered in
  `AppState::pending_tier_accesses` and flushed by `enforce_tier_budget`
  immediately before it evicts. **The order is load-bearing:** evicting before
  flushing drops precisely the rows the user is currently looking at.
- Buffered marks are themselves bounded (`MAX_PENDING_TIER_ACCESSES`), so a
  runaway cannot grow the map without limit.
