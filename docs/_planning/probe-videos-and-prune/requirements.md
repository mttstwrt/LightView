# Probe videos at index time, and prune what nothing calls

Two unrelated findings from a review pass over the whole tree, kept in one plan
because they were found together and land as separate commits.

The first is a feature that cannot work. The second is the surface a reader has
to hold in their head for no return: functions nothing calls, dependencies
nothing imports, comments describing an engine that was deleted.

## Background

`media_meta.duration` is NULL for every video that has ever been indexed.
`set_probed` has two production callers — `services/gallery.rs` (the header
pass) and `pipeline/serve.rs` (the thumbnailer) — and neither passes one: the
first reads an EXIF header, which a video does not have, and the second writes
only the dimensions it decoded. `pipeline::video::probe` has exactly one caller
in the crate, `plugin/input.rs`, on the plugin path.

Downstream of that, `ThumbnailCell.isShortVideo()` requires a known duration, so
it is permanently false, so the **autoplay short videos in the grid** toggle and
its seconds threshold are two settings a user can change with no possible
effect. The comment at `sort/sorter.rs:83` — "probed lazily during
thumbnailing" — describes something that does not happen.

## Requirements

### R1 — A video in the index carries what its container knows

After a gallery is indexed, every video row has its duration, its
rotation-corrected dimensions, and — when the container carries them — its
capture time and its coordinates. A clip dropped into a running gallery gets
the same treatment without a restart, as photos already do.

A machine with no `ffmpeg` installed must not record that it has looked at a
video it could not open; installing `ffmpeg` later and reopening must fill the
gaps in.

### R2 — The autoplay setting does something

With **autoplay short videos in the grid** on and a threshold of *n* seconds, a
clip shorter than *n* plays in the grid and a longer one does not. The setting
is either honoured or removed; it does not stay inert.

Bounded by what the browser can play: `ThumbnailCell.isShortVideo` also requires
`isPlayableVideo()`, so `.avi`, `.mkv`, `.wmv` and `.flv` never autoplay however
short. That is existing, deliberate behaviour — those containers go to the
external player — and this requirement does not change it.

### R3 — A session whose window has closed exits without waiting for enrichment

Closing the last window ends the session within the grace period even while a
first-open enrichment pass is still running, except across an operation that
writes durable data. An interrupted pass resumes on the next open rather than
restarting.

Every unattended `modify_companion` caller holds a guard across its write, and
no caller holds one across work that writes only the derived cache. "Unattended"
means not covered by the HTTP graceful shutdown: the enrichment pass, the
companion sweep, and the detached plugin job spawned by `run_plugin`.

### R4 — Nothing public is uncalled

No public function in the crate is without a caller, no exported frontend
binding is unreferenced, and no declared dependency is unimported. Where a
function's own doc comment names a caller, that caller exists.

### R5 — No comment describes something that was deleted

Specifically: the viewer no longer renders GIFs from a backend frame atlas, and
one comment still says it does, while another in the same file says it does not.

### R6 — Launched subprocesses are reaped

`xdg-open` at launch and the external application behind **Open with** are
spawned and never waited on, so each leaves a defunct entry for the lifetime of
the process. Opening ten files in an external viewer leaves ten.

## Out of scope

- **Filtering or sorting by duration.** Nothing asks for it, and a column the
  grid reads is not a query surface until someone wants one.
- **A QR code for pairing.** `devices.rs` describes the secret as travelling in
  one and the `qrcode` package is installed for it, but nothing renders it. That
  is a missing feature, not a dangling one; this plan removes the unused
  dependency and leaves the decision alone.
- **A general migration mechanism.** Whether *this* change earns a
  `format_version` bump or a narrower one-shot reset is decided in the design,
  and it is the plan's central question rather than an out-of-scope one.

## Two facts that reframe the cost, found by the plan review

**A format bump loses more than time.** `cache/db.rs` says at its head that
`date_added` and `last_viewed` "are mirrored into the companion … so the bump
loses time and nothing else", and twice lower down (`db.rs:308`, `db.rs:533`)
says a wipe "costs `date_added` and `last_viewed` for everything". Both are
right about different galleries: the mirror only reaches files that already have
a sidecar, because `reindex_companions` skips any path whose companion cannot be
stat'd. An untagged camera roll has no sidecars at all, so a bump resets when
every file was added and when it was last seen. The head comment is the one that
needs fixing.

This already applies to the bump shipped earlier in this branch (1 → 2, for
`exif_read`). Anyone opening an untagged library on that build has already paid
it once.

**The geocoder does not filter by media type.** `backfill_locations` selects
`WHERE gps_lat IS NOT NULL` with no predicate on kind. The moment videos carry
coordinates, the first enrichment pass creates a sidecar in the user's gallery
for every geotagged clip that never had one. That is new durable data on the
tree the design promises is safe to copy around, so it is a contract decision,
not a side effect.
