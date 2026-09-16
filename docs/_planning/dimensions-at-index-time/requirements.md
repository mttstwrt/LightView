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
- **A second mechanism behind the jump.** Investigated and not found; recorded
  here so nobody re-opens it on the same hunch. See "What the instrumented runs
  settled" below.

## What the instrumented runs settled

The cause was established by building the SPA with a probe inside the
`aspectArray` memo and driving the real binary at phone size, rather than by
reading the code. Three things came out of it, and two of them contradict
earlier conclusions in this session.

**The defect is the reported bug.** Every height change coincided with an aspect
correction, `rows` constant at 18 while `total` moved 3139 → 3166 → 3139 → 3104
as four files went from the square guess to their measured shape. An earlier run
had appeared to separate the two — the corrections finished before the viewer
closed and the height still moved afterwards — and that reading was wrong. It is
a race between thumbnail arrival and the close, so the two look independent
whenever the timing happens to separate them.

**No layout input other than the aspects ever changes.** The probe logged
container width, target row height, gap and the group boundaries on every
recompute, and none of them moved once. This rules out the group boundaries
shifting as dates are written during enrichment, which had been the leading
suspicion and would have implicated a shipped commit.

**There is no double correction.** A per-path counter across a full run:
**0 of 21** paths that changed aspect changed more than once, and the paths array
was never reordered. The write-once guard in `recordMeasuredAspect` holds. An
earlier count of "51 changes for 36 files" had suggested otherwise; that counter
incremented once per *recompute containing any difference*, which is not the same
quantity as a per-path change, and comparing the two was the error.

So the fix needs no second mechanism. Removing the placeholder removes every
correction, and with them every recompute.

**What remains unexplained** is the precision of the reported symptom: the grid
returning to exactly its starting position. One-way corrections wander the total
height in both directions — a row that gains a wide image gets shorter, one that
gains a tall image gets taller — so passing back through the starting value is
possible rather than surprising, but "exactly" was a person's description of a
moving grid and is not evidence of an exact return. This is worth re-checking
after the fix rather than designing around now.
