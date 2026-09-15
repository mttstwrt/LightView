# Design

Principle 1's five questions, in order.

## Placement

**One function in `pipeline/`, called from the branch that already exists in
`services/gallery.rs::probe_and_store`.**

`probe_and_store` gained a `match media_type_of(&path)` when videos started
being probed at index time. Its image arm currently reads EXIF and nothing
else; it gains a dimensions read beside that, exactly as the video arm reads
duration and dimensions together. No new seam, no new caller, and the
dependency direction is unchanged: `services` → `pipeline` → `cache::meta`.

**The format branch lives in `pipeline/thumbnailer.rs`, not in `services`.**
Which reader can parse which container is knowledge that already lives there —
`decode_heic_*` is there because HEIC is a pipeline concern. `services` asks for
the dimensions of a path and is told, or not. One entry point:

```
pipeline::thumbnailer::dimensions(path) -> Option<(u32, u32)>
```

trying `image::image_dimensions` first and libheif second. `header_dimensions`,
deleted three commits ago for having no caller, comes back as the first half of
it — it was dead because its consumer had never been written, which is the
finding rather than an argument against restoring it.

**Not in the thumbnailer's write path.** `pipeline/serve.rs` already writes
width and height when it decodes a frame. That is where they come from today and
is precisely the defect: a file's shape should not depend on whether anyone has
scrolled to it. That write stays — it is correct when it happens, and
`set_probed` is COALESCE-first-wins so the two writers cannot disagree (both
report display dimensions with rotation applied).

## Contract

| what changes | who is on the other side |
|---|---|
| `media_meta.width` / `height` populated for images at index time | `sort::sorter`, `services::media`, the wire, `galleryStore.aspectByPath`, `JustifiedGrid` |
| `pipeline::thumbnailer::dimensions` is new (and `header_dimensions` restored, private to it) | in-crate, one caller |

No schema change: `ProbedMedia` already carries `width` and `height`, and the
thumbnailer already writes them. No wire change: the fields are already
serialized and already typed nullable on the frontend. **The frontend needs no
edit at all** — `aspectByPath` reads what the index provides, and the 1:1
fallback simply stops being reached.

**No reader stamp, and no `format_version` bump.** Existing caches catch up
through the idle worker, which thumbnails everything it finds missing a tier and
writes dimensions on the way past. See the requirements' out-of-scope note.

## Cost in concepts

One function with a two-format branch, replacing a guess. The 1:1 placeholder
stays in the grid as a fallback but stops being reachable for JPEG, PNG, WebP,
TIFF, GIF and HEIC — what remains behind it is RAW, AVIF, and video on a host
with no ffprobe.

Nothing is added that has to be explained with the word *except*. The change is
net subtractive in behaviour: a code path that produced wrong layouts stops
being entered.

## Alternatives

**Fix it in the grid: stop relaying out when a measured aspect arrives.** Batch
the corrections, or defer them until the user is idle. Rejected on layer: the
aspect ratio of a file is knowable from the file, cheaply, before anything is
drawn. Teaching the UI to hide a missing fact leaves the fact missing — every
other consumer of `width`/`height` keeps getting NULL.

**A better placeholder — the gallery's mean aspect instead of 1:1.** Cheaper
still, and reduces the size of each jump. Rejected: it makes the symptom
smaller and the cause permanent, and a grid that is subtly wrong everywhere is
harder to trust than one that is obviously wrong in one place.

**Scroll anchoring: compensate `scrollTop` when rows above the viewport
change height.** This is the right answer for the residue — the formats neither
reader handles — and it is deliberately *not* in this plan. It is a second
mechanism, it only helps once the first has shrunk the problem to its edges, and
adding both at once would make it impossible to tell which one worked.

**Let the idle worker fix it by thumbnailing sooner.** It already does this, and
it is why no stamp is needed for existing caches. It does not help the case that
matters: a gallery opened for the first time, where nothing is thumbnailed and
everything is a square.

## Assumptions

- **(unmeasured, and the one that could make this worse rather than better)**
  libheif's `ImageHandle::width()`/`height()` return *display* dimensions, with
  the container's rotation already applied. libheif has applied `irot`/`imir`
  by default since 1.16 and this project requires ≥ 1.21, and `decode_heic_*`
  already treats those values as the source dimensions it reports alongside a
  decoded frame — so the two paths agree by construction. **To verify against a
  real rotated HEIC before the commit lands**: if they are stored rather than
  display dimensions, every portrait iPhone photo would lay out landscape, which
  is worse than the square it replaces.
- **(unmeasured)** `image::image_dimensions` costs a file open and a header
  parse. To be measured over a few hundred files; the pass is background,
  batched and resumable, and the comparable ffprobe spawn measured 60 ms, so the
  bar to clear is low.
- **(measured, this session)** The gallery exhibiting the reported symptom is
  mostly JPEG/PNG, so the `image` half addresses the library the bug was
  reported from; the HEIC half addresses the other gallery.
- HEIC EXIF already works: `kamadak-exif`'s `read_from_container` covers HEIF,
  so dates and coordinates are not part of this change.

## The two checks principle 1 asks for

**Second-implementation test** — no abstraction is introduced. `dimensions` is
a concrete function with one caller; the two-format branch inside it is a
`match`, not a plugin point.

**Seam test** — passes. `ProbedMedia` has the fields, `probe_and_store` has the
branch, and the thumbnailer already owns format knowledge. Nothing has to move.

## The work

Two commits.

**1. Read an image's shape when it is indexed.** `dimensions()` in
`pipeline/thumbnailer.rs` — `image::image_dimensions`, falling back to a libheif
handle read with no decode. Wired into `probe_and_store`'s image arm. Tests: a
JPEG and a PNG probe to their real dimensions; a HEIC does too; a rotated HEIC
reports display dimensions, not stored ones; a file neither reader can parse
leaves them NULL and is still recorded as looked-at.

**2. Prove the grid stops moving.** A check in `grid.mjs` that samples the
scroll host's `scrollHeight` across a scroll through a gallery with no
thumbnails yet, and asserts it does not change. That check fails on the current
build — it is the regression test for this bug, and writing it first is what
stops the fix being declared working because the churn happened to land below
the fold.

Docs: `docs/gallery/` (what an index-time read now covers),
`docs/pipeline/` (the dimensions entry point and what it cannot read), and
`docs/frontend/grid-loading.md` (why the layout no longer converges).
