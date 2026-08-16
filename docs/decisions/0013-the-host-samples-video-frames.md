# 0013 — The host samples video frames; plugins only ever see stills

[← docs index](../README.md) · [plugins](../plugins/README.md) · [pipeline](../pipeline/README.md) · [worker tagging](../remote/worker-tagging.md)

## Context

Sending videos to a remote tagging worker produced no tags, no error, and no
worker log line — the job was marked **Done**. `resolve_target` intersected the
candidate list with `SELECT path FROM media_meta WHERE media_type = 'image'`,
and did so for *both* target kinds, so a selection of clips resolved to an empty
list and the claim loop reads empty as "nothing left to tag". This never worked
remotely; the filter arrived in the same commit that introduced the worker.

Native frame-splitting *did* work, because each ML tagger carried its own
`is_video`, `get_video_duration`, `extract_frame`, `sample_video_timestamps`
and `predict_video` — four copies of one policy, in Python scripts on their own
release cadence, each shelling out to ffmpeg once per frame. Which made the
remote gap read as a regression rather than as something that had never
existed.

So there were two questions: where do frames come from, and who merges them.

## Options considered

**Ship whole clips to the worker.** The smallest change: drop the `media_type`
narrowing and let the existing download path carry the file. Rejected on the
numbers. `?fit=` only resizes `jpg`/`png`/`webp`, so a video transfers whole,
and the worker's disk bound is a *count* of 64 files, not a byte budget — 64
phone clips is plausibly tens of gigabytes on the wire and on disk. Frames are
images, which makes that bound safe again and costs a few hundred KB per clip.

**Keep plugin-side sampling for local runs, add server-side sampling for remote
ones.** Also small, and it institutionalises the bug: two sampling policies
that agree only by coincidence, drifting per plugin, with the same clip tagged
differently depending on where the job happened to run. The reason the remote
gap survived a year is that nobody could see the two paths side by side.

**Decode every frame.** Rejected without much thought: sampling a handful is
cheap enough that the weak-server argument does not apply, and decoding every
frame of a camera roll is a different product.

**Have the plugin merge its own frames.** It cannot — under host-side sampling
each frame is an independent request, and the plugin has no idea two of them
came from one clip. Telling it would put the whole notion of a video back into
every plugin, which is precisely what this removes.

## Decision

`plugin::input` plans a media item into *parts*: one for a still, `video_frames`
(manifest-declared, default 5, clamped to 16) for a clip, evenly spaced across
the middle 90% of the duration — the sampling the plugins were already doing, so
converging on one implementation changed no output.

Parts are produced where the file is. The local drivers decode on the shared
thumbnail pool; a remote worker fetches `GET /media/<path>?frame=i&frames=n`,
which extracts and encodes one frame server-side. That route exists for this and
nothing else — the thumbnail tiers already answer "a picture of this clip", and
only a tagger asks for "the i-th of n samples across it".

The host merges. `merge_parts` unions the per-frame tag sets in first-seen
order, which is *exact* rather than approximate: the taggers aggregate frames by
taking an element-wise maximum of their score vectors and thresholding once, and
`max(s) > T` holds precisely when some `s > T`. So a union reproduces
plugin-side sampling tag for tag.

`rating:` is the one exception and the reason `merge_parts` is not a
`flat_map`. It is an argmax over a score vector, not a set, so a union would
give a clip up to five ratings where there should be one. The per-frame
`rating_scores` already ride along in `meta`, so the host redoes the argmax over
the per-frame maxima — the same computation the plugin would have done had it
seen every frame. A plugin that emits a bare `rating:` label with no scores
falls back to a plurality across frames, which is the best a set of labels
supports.

A single-part item — every still, which is almost every item — passes through
`merge_parts` untouched: same tags, same meta, same order.

The `media_meta` join in `resolve_target` stays, widened rather than dropped. It
is doing two jobs and only one was wrong: it also confines a remote-supplied
path list to files this gallery has actually indexed, which is what stops a
paired browser naming arbitrary filesystem paths for a worker to download.

## Consequences

- Videos tag on every path — desktop, in-process executor, remote worker — with
  one sampling policy. Verified end to end: two clips and six images through a
  remote worker and again through the in-process executor, 8/8 both times, each
  clip's companion carrying one merged entry with `video_frames_sampled`.
- `is_video`, `predict_video`, `get_video_duration`, `extract_frame`, the
  sampling function and the ffmpeg dependency leave all four bundled plugins.
  That is the point as much as the bug fix is: it is what makes the taggers
  movable to their own repository without taking a copy of the host's video
  policy with them.
- A clip costs `video_frames` inferences rather than one, and the same number of
  disk slots in a worker's window. All of an item's parts must be in flight
  together, so `video_frames` is clamped below the smallest window — a manifest
  asking for more would deadlock the job by construction.
- Progress is counted in media items, not requests: a five-frame clip is one
  step of the progress bar, and one unit of the job's total.
- `video_frames_sampled` in the merged meta records how many frames actually
  contributed, which is not the requested count when a sample point failed to
  decode. A clip that loses one frame to a fade still gets tagged from the rest.
- `merge_parts` knows the `rating:` convention, which is a bundled-tagger
  convention rather than part of the protocol. Documented as such; a plugin that
  never emits the prefix is unaffected. If a second such convention appears, the
  right answer is probably a manifest declaration rather than a second special
  case here.
- `GET /media?frame=` is a new authenticated route that can make a paired device
  spend ffmpeg time. It is bounded the same way every other decode is — the
  shared thumbnail pool, and ffmpeg's own timeout — and it refuses non-video
  paths outright.
