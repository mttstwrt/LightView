# LightView documentation

LightView is a local media gallery: it opens a folder of images and videos,
indexes it into a per-gallery SQLite cache, generates thumbnails at several
resolutions, and presents a browsable grid plus a full-resolution viewer. The
same application can serve that gallery to phones and laptops on the LAN.

These pages describe how the system works and why it is shaped the way it is.
Per-file explanation lives in the module doc comments in the source; these
pages describe subsystems, the contracts between them, and the reasoning that
is not recoverable from reading any single file.

## Start here

[`architecture.md`](architecture.md) is the map: the component inventory, how
data flows through it, and which direction the dependencies point. Read it
before the subsystem pages — they assume you know where they sit.

[`build-and-verify.md`](build-and-verify.md) is the practical companion: the
system libraries a checkout needs before `cargo check` reaches any of our code,
the current state of the quality gates, and how to drive the whole stack —
server, web client, real browser — without a display.

## Subsystems

| Page | Responsibility |
|---|---|
| [`cache/`](cache/README.md) | The per-gallery SQLite database: schema, migrations, connection strategy, and what happens when the gallery moves. |
| [`pipeline/`](pipeline/README.md) | Thumbnail generation and serving: the tier ladder, the four entry points, coalescing, and the disk budget. |
| [`query/`](query/README.md) | Turning a filter string into a set of paths, then ordering and grouping them. Also tag autocomplete. |
| [`companion/`](companion/README.md) | The `.lightview` sidecar files that hold user metadata, and their relationship to the cache index. |
| [`remote/`](remote/README.md) | The axum HTTP server, device pairing, TLS, and the `/api/invoke` allowlist that bounds what a remote client may do. |
| [`plugins/`](plugins/README.md) | The subprocess/NDJSON plugin protocol, what it can and cannot express today, and the path to a real extension host. |
| [`duplicates/`](duplicates/README.md) | Perceptual-hash duplicate detection and the metadata-preserving merge. |
| [`frontend/`](frontend/README.md) | The SolidJS SPA: stores, the IPC boundary, and the desktop/web split. |

Deeper topics that outgrew their subsystem page:

- [`pipeline/jpeg-decode.md`](pipeline/jpeg-decode.md) — where thumbnail
  generation actually spends its time, and the options for making it faster.
- [`remote/worker-tagging.md`](remote/worker-tagging.md) — the job queue and
  worker protocol that let a capable machine run taggers for a weak server.
- [`frontend/grid-loading.md`](frontend/grid-loading.md) — the machinery both
  grids use to stream thumbnails into a virtual scroller.
- [`frontend/chrome.md`](frontend/chrome.md) — the planned split between
  commands and settings, and the space they compete for on a phone.

## Decisions

[`decisions/`](decisions/) records choices that had real alternatives, one file
per decision, numbered and append-only. A decision file is never edited after
the fact; if a decision is reversed, a new file supersedes it. The subsystem
pages describe what the system does today, and link to the decision when the
answer to "why not the obvious thing?" is longer than a sentence.

- [0001 — One SQLite cache per gallery, not one global index](decisions/0001-one-cache-per-gallery.md)
- [0002 — Seven thumbnail tiers in two families](decisions/0002-two-families-of-thumbnail-tiers.md)
- [0003 — Derive the schema version from the migration list](decisions/0003-derive-schema-version-from-migrations.md)
- [0004 — Bound the zoom tiers by bytes, not rows](decisions/0004-byte-budgeted-lru-for-zoom-tiers.md)
- [0005 — Remote command dispatch is an allowlist](decisions/0005-remote-invoke-is-an-allowlist.md)
- [0006 — Plugins are subprocesses speaking NDJSON](decisions/0006-plugins-are-ndjson-subprocesses.md)
- [0007 — Two nested render windows in the grids](decisions/0007-two-zone-render-window.md)
- [0008 — No view-module API; enablement plus code-splitting instead](decisions/0008-no-view-module-api.md)
- [0009 — Three kinds of chrome: commands, panels, and configuration](decisions/0009-commands-panels-and-configuration.md)

## Open work

[`todo.md`](todo.md) is the running list of known gaps — things that are
understood but not done. It is deliberately short; anything with enough shape
to be designed belongs in a subsystem page instead. Items are grouped by the
part of the system they touch and ordered *within* a group by the sequence they
should be done in, with the reasoning for that sequence stated; the groups
themselves are independent of one another.
