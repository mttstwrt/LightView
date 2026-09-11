# pipeline/

[← docs](../README.md)

**Responsible for** turning a file on disk into bytes a browser can show: image
and video decoding, HEIC transcoding, EXIF extraction, the four thumbnail tiers,
the coalescer that keeps a scroll from generating the same thumbnail forty
times, the byte budget, and the idle worker that fills in what nobody has asked
for yet.

**Not responsible for** what a request is *allowed* to see — that is
[server/](../server/README.md) — or for what a tier is *stored in*, which is
[cache/](../cache/README.md).

**Depends on** `image`, `fast_image_resize`, `libheif-rs`, `rayon`, and `ffmpeg`
as a subprocess. **Depended on by** the media and thumbnail routes, the
duplicate hasher, and [plugins/](../plugins/README.md).

## Four tiers, one family

| Tier | Longest edge | Budgeted | Used for |
|---|---|---|---|
| `js` | 128 | no | the cheap rung during a fling, and the panels that address arbitrary files |
| `j` | 512 | no | the grid's base, the perceptual hash, and a plugin's default input |
| `jm` | 1280 | yes | a zoomed-in cell, and the viewer's underlay |
| `jh` | 2560 | yes | a heavily zoomed cell |

Every one of them is `generate_for_path_fit(path, edge)` — **aspect-preserving,
WebP, one encoder, one render path**. The two families this replaces (square
crops and fitted images, JPEG and WebP, seven sizes) meant two render paths,
two sets of dimensions to reason about, and a tier whose only consumer was a
grid that no longer exists.

Four rungs cost a table and a row in `ThumbTier::ALL`. The ladder is what lets
the grid trade sharpness for latency mid-scroll without the backend knowing.

`js` and `j` are **not** budgeted: they are small, every cell needs one, and
evicting them means regenerating them on the next scroll. The two large tiers
are, because they are generated for what a person actually zoomed into.

## The hot path, and the three things that keep it standing

A scrolling grid asks for a few hundred thumbnails a second, aborts most of
those requests before they finish, and asks again a moment later. Each of these
replaces a failure that actually happened:

- **Reads go through the read-only pool, never the writer**, so a scroll is not
  queued behind the index pass.
- **Generation is coalesced**, and the slot is an RAII guard, so a cancelled
  request releases it rather than wedging every later asker. A waiter that wakes
  to find the generator gone becomes the generator; the attempt count bounds
  that, so a persistently failing source degrades to a miss instead of spinning.
- **Access marks buffer in memory and drain immediately before an eviction
  pass.** The read path holds a read-only connection and cannot stamp
  `accessed_at` itself, and draining *after* an eviction rather than before
  would evict exactly what the user is looking at.

`RelPath` goes in; `GalleryPath` exists only inside the generate branch. A
request answered from the database never pays for a `realpath` walk, and the one
branch that opens an arbitrary file **cannot compile** without the
canonicalizing check.

## The byte budget

Evict warmest-first down to the budget, but only once a tier is past **1.25 ×**
it. Evicting at exactly the budget would run a delete pass on essentially every
generation, which is the pathology hysteresis exists for.

A freshly written row is stamped with the current time, which is what stops an
eviction pass deleting exactly what was just generated.

## Video

`ffmpeg` is a **runtime** dependency, not a build one. Without it a clip shows a
grey cell rather than a hole the grid re-requests on every scroll pass — a
still that will not decode is a broken file, and inventing a cell for it would
hide that, but a missing subprocess is a deployment fact.

Frame extraction is best-effort by design. `-ss` lands on the nearest decodable
point, and a timestamp past a clip's real end — durations in container metadata
are routinely a little long — yields nothing at all, so a failed seek falls back
to the start of the clip. For tagging that is the right trade: a duplicate frame
costs one redundant inference, an error costs the file its tags.

## The idle worker

Three backlogs in one loop, all cheap to abandon: the two unbounded tiers
newest-first, then perceptual hashes decoded from the cached `j` bytes, then any
missing ThumbHash while those pixels are decoded anyway. Every unit re-checks
idleness before starting, so a user who touches the grid gets the pool back
within one batch rather than at the end of a sweep.

**"Is anyone looking" is an activity timestamp and nothing else.** The signal it
replaces was "no client is subscribed to the change stream", which worked only
while the desktop user was a different *kind* of client. With one runtime the
local user **is** a subscriber, so that counter is true whenever anybody has the
gallery open — and this worker would never run at all. No perceptual hashes,
and therefore no duplicate detection, ever.

The hourly companion sweep is deliberately **not** part of this loop: it skips
its units whenever somebody is touching the grid, which is right for thumbnail
backfill and wrong for a tag written over the share. See
[gallery/](../gallery/README.md#the-hourly-sweep).

## Invariants a caller must uphold

- **Never generate on the writer connection.** The read pool answers hits; the
  generate branch takes the writer only to store the result.
- **A tier is the bytes at an edge, not a crop.** Anything that wants a
  different *shape* wants `?fit=` on the media route, which is an on-demand
  decode and is not cached.
- **`NULL` means "not hashed", never a sentinel.** A genuinely flat image
  legitimately hashes to zero — see [duplicates/](../duplicates/README.md).
- **Speculative work must not mark activity.** A precache that stamps the
  activity clock makes the idle worker believe someone is looking, permanently.
