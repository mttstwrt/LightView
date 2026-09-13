# LightView

A local media gallery. It opens a folder of images and videos, indexes it,
generates thumbnails, and presents a browsable grid and a full-resolution
viewer — to a browser on the same machine, or to phones and laptops on the LAN.

One Rust binary, one SolidJS bundle compiled into it.

## Start here

**[architecture.md](architecture.md)** — the three modes, the trust model, the
layers, and where a request and a file each go. Everything else is a subsystem
underneath it.

**[build-and-verify.md](build-and-verify.md)** — what to install, the build
order that is not optional, and how to drive the whole stack with no display.

## Subsystems

Each page states what its subsystem is responsible for, what it deliberately is
*not*, its public interface, what it depends on, what depends on it, and the
invariants a caller has to uphold.

- **[server/](server/README.md)** — the HTTP surface: routes, the two trust
  levels, the one command table, the launch-token session, pairing, TLS, the
  event stream, uploads.
- **[storage/](storage/README.md)** — where everything lives and why: the
  gallery tree, the XDG directories, the trash, the cache ceiling.
- **[cache/](cache/README.md)** — SQLite: the schema, the four tier tables,
  `format_version` instead of migrations, the path-keyed sweep.
- **[pipeline/](pipeline/README.md)** — turning a file into bytes a browser can
  show: one render path, four tiers, the coalescer, the byte budget, the idle
  worker.
- **[gallery/](gallery/README.md)** — how a new file becomes a grid cell: the
  initial scan, the filesystem watcher, the enrichment pass.
- **[companion/](companion/README.md)** — the sidecar files that hold tags,
  ratings and notes, and how two machines write one directory safely.
- **[query/](query/README.md)** — the filter language, sort, grouping,
  autocomplete, and what a *set* is.
- **[duplicates/](duplicates/README.md)** — perceptual hashing, grouping, and
  merging a group onto one keeper.
- **[plugins/](plugins/README.md)** — the tagging protocol and the executor that
  speaks it. The author-facing version is [`plugins/README.md`](../plugins/README.md)
  in the repository root.
- **[geocode/](geocode/README.md)** — coordinates to place names.
- **[frontend/](frontend/README.md)** — the SPA: boot, the stores, the grid, the
  viewer, the chrome. Plus
  **[grid-loading.md](frontend/grid-loading.md)** for how the grid decides what
  to request and when.

## Conventions

These pages describe **how the system works now, and why**. Anything that
explains a single file lives in that file's module doc comment instead — if a
page here restates code, it is drifting and should be deleted rather than
maintained.

Prose over bullet fragments, relative links only, and every page links back
here. An outdated page is a bug, fixed in the change that caused it.
