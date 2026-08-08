# 0003 — Derive the schema version from the migration list

[← docs index](../README.md) · [cache](../cache/README.md)

## Context

The cache database carries a `schema_version` in `gallery_meta` and applies a
list of migrations on open. `SCHEMA_VERSION` was a hand-maintained constant
alongside `MIGRATIONS`.

The two drifted: the constant read 14 while `MIGRATIONS` already contained a v15
entry. That is harmless in most designs, but not this one. The constant's only
consumer was `detect_legacy_version`, which returned it to mean "this unstamped
database is fully current" — and `run_migrations` **skips every migration at or
below the version it is handed**. An unstamped database reaching that branch
would be stamped 14 without the justified-tier tables ever being created, and
every subsequent `jm`/`jh` query against it would fail.

Only databases predating the versioning scheme could reach it, so it was
probably never hit in the field. But the failure is total and silent, and the
drift was guaranteed to recur.

## Options considered

**Keep the constant, add a test asserting it equals the last migration's
version.** Minimal change; the test catches the drift at CI time.

**Derive the constant from `MIGRATIONS` with a `const fn`.** The two cannot
separate at all.

**Delete the constant** and have every caller read the last migration's version
directly.

## Decision

Derive it: `SCHEMA_VERSION` is a `const fn` over `MIGRATIONS`, evaluated at
compile time.

A test would have caught the drift, but only after someone wrote the test and
only at the moment CI ran. Deriving removes the class of bug rather than
detecting instances of it, at no cost — the value is still a compile-time
constant with the same type and the same call sites.

Separately, and as part of the same reasoning: `detect_legacy_version` now
returns `MAX_DETECTABLE_VERSION` (10, the last feature its ladder actually
probes for) rather than `SCHEMA_VERSION`. The asymmetry that makes this correct
is that migrations are idempotent, so **under-reporting is harmless** —
migrations simply re-run — while over-reporting silently leaves tables
uncreated. Extending the ladder is optional; raising its return value beyond
what it verifies is not.

## Consequences

- Adding a migration requires no second edit. Append the entry; the version
  follows.
- Four tests hold the surrounding invariants:
  `fresh_db_reaches_the_latest_migration` (versions strictly increasing, since
  an out-of-order entry would never run), `migrations_are_idempotent` (a full
  re-run over a current database is a no-op),
  `legacy_detection_never_over_reports`, and the path-keyed-table sweep.
- `run_migrations` warns when the final version does not equal
  `SCHEMA_VERSION`. The only way to land short is a database stamped ahead of
  this build — opened by a newer LightView, then downgraded — which is worth a
  log line and is not otherwise recoverable.
- `detect_legacy_version` is now cheap and correct but very likely dead:
  versioning has been in place since v1 of the schema. It is kept because
  proving no such database exists in the wild is not possible; see
  [`todo.md`](../todo.md).
