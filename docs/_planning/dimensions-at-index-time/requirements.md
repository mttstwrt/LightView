# Know a file's shape before anyone looks at it

## Background

The grid lays out by aspect ratio. `aspectByPath` is built from
`media_meta.width`/`height`, and where those are NULL `JustifiedGrid` falls back
to a **1:1 square guess**, recording the real ratio when the thumbnail finally
loads (`recordMeasuredAspect`). Each correction recomputes the whole justified
layout, so every row after the corrected one moves.

Nothing writes an image's dimensions at index time. `probe_and_store`'s image
branch reads EXIF — a date and coordinates — and `pipeline/serve.rs` writes
width and height as a *side effect of decoding a frame for a thumbnail*. So a
file that has never been thumbnailed has no shape, and the grid guesses.

Measured in headless Chromium at phone size, on a gallery whose files never
change: **seven layout recomputes in 333 ms**, total content height wandering
about 110 px, while `scrollTop` stayed pinned at 1400 and the container width
stayed at 390. Rows move under the reader.

This is the same defect A1 fixed for capture dates, in a different column: a
fact about a file, discovered when someone happens to scroll past it rather than
when the file is indexed.

## Requirements

### R1 — An indexed image has its dimensions

After a gallery is indexed, every image row carries `width` and `height`, read
from the file's header without decoding it. A file dropped into a running
gallery gets the same treatment. This holds for the formats the `image` crate
parses (JPEG, PNG, WebP, TIFF, GIF) **and for HEIC**, which it does not: one of
the two galleries this targets is entirely HEIC, so a fix that skipped it would
not fix the library it was written for.

### R2 — The grid does not move while thumbnails load

Scrolled to a fixed offset in a gallery whose thumbnails have not been generated
yet, the content height and every row's position stay put as thumbnails arrive.
The 1:1 placeholder becomes unreachable for any format either reader can read.

### R3 — Nothing pays a decode for it

Dimensions come from a header or a container handle. No image is decoded, and no
thumbnail is generated, to satisfy R1.

## Out of scope

- **A reader stamp to re-read existing caches.** Established by measurement
  during planning: `warm_thumbnails` walks every file missing a `J` or `Js` tier
  and generates it, which writes dimensions through `set_probed`. Existing
  caches converge on their own; the stamp would only make them converge sooner,
  and "sooner" does not earn a permanent mechanism.
- **The jump that returns *exactly* where it started.** The reported symptom on
  a phone is a jump up and then back down to precisely the original position.
  The defect above cannot produce that: `recordMeasuredAspect` returns early for
  an already-measured path, so each correction happens once and sticks, moving
  the grid one way. Either those corrections happen to cancel on that device, or
  there is a second mechanism. This plan fixes what it can prove and treats the
  remaining symptom as diagnostic: if it survives, the cause reverts rather than
  settles, which is a different search.
