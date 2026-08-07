# 0001 — One SQLite cache per gallery, not one global index

[← docs index](../README.md) · [cache](../cache/README.md)

## Context

LightView needs somewhere to keep a media index, thumbnail blobs, a tag index,
duplicate-detection state, and — later — remote device pairings. Thumbnails
dominate the byte count and can reach several gigabytes for a large gallery.

A gallery is a directory the user picks. There is no notion of a library the
application owns; the user may open a folder on an external drive, a NAS mount,
or a directory they will move next week.

## Options considered

**One global database in the application's data directory,** keyed by absolute
path. This is what most photo managers do. It survives the gallery directory
moving, it makes cross-gallery queries possible, and there is exactly one file
to open at startup.

**One database per gallery, stored inside it** at `<gallery>/.lightview/cache.db`.

**No database; a per-file sidecar plus a thumbnail directory tree.** Nothing to
migrate, trivially inspectable, and the storage the user already understands.

## Decision

One SQLite database per gallery, inside the gallery, at
`<gallery>/.lightview/cache.db`.

The deciding property is that the cache travels with the data it describes.
Copying a gallery to another machine, backing it up, or handing it to another
LightView installation carries the thumbnails and the index along with it, with
no export step and no orphaned state left behind on the original machine. A
global index gets the inverse of every one of those: it accumulates rows for
galleries that no longer exist, and a gallery moved to another machine arrives
cold.

The per-file option was rejected on I/O cost. A grid scroll needs hundreds of
small reads; served from one memory-mapped SQLite file in WAL mode those are
page reads, and served from a directory tree they are hundreds of `open`/`read`
syscalls. Sorting and filtering would additionally have no index at all.

## Consequences

- Storage tracks the gallery. Deleting a gallery deletes its cache.
- Cross-gallery queries are not possible. Nothing has needed them.
- **Absolute paths become primary keys**, because the database has no reason to
  know it is inside the tree it describes. Moving the gallery directory
  therefore orphans every row, which is why `adopt_gallery_root` /
  `rebase_root` / `infer_old_root` exist and must run before the index is
  populated. Storing gallery-relative paths would delete that machinery, but it
  is a migration touching every table and every query; recorded in
  [`todo.md`](../todo.md) as the structural observation it is.
- Per-gallery scope propagates to things that were not obviously per-gallery:
  device pairings and the remote password live here too, which turns out to be
  the behaviour users expect — sharing one gallery does not share another.
- The database can always be rebuilt from the media files plus their companion
  sidecars, but "just delete it" is not free: view history, ratings not yet
  flushed to a companion, and dedup decisions live only here.
