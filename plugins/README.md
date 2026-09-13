# Plugins

A plugin tags media. LightView hands it images and it hands back tags; it never
touches a companion file, a database or a video.

## Installing one

A plugin is a directory under `$XDG_DATA_HOME/lightview/plugins/` (usually
`~/.local/share/lightview/plugins/`) whose name matches the `name` in its
`manifest.json`. Copy the directory there and it is installed — there is no
install command, for the same reason the password and pairing are administered
from a shell.

The directory name is the identity, and the manifest's `name` is checked against
it. Nothing resolves a plugin by joining a caller-supplied string onto a path:
the host scans the install root and a name either matches something it found or
matches nothing. A request carries a plugin *name*, never a command, so there is
no shape of request that can name something to execute.

```
lightview tag ~/photos --plugin example-auto-tagger
lightview tag /mnt/nas/photos --plugin wd-tagger --filter 'NOT has::plugin.wd'
```

## The protocol

One JSON object per line, both directions, over stdin and stdout.

The host sends `{"action":"tag","path":"/abs/path.webp"}` and expects **exactly
one** result per request:

```json
{"path": "/abs/path.webp", "tags": ["dog", "beach"], "meta": {"…": "…"}}
{"path": "/abs/path.webp", "error": "could not read"}
```

An error result is an answer, not a failure — it costs that file its tags and
nothing else.

**Emit each result as soon as it is ready. Never buffer stdin to EOF.** The host
keeps a bounded number of requests in flight and releases a slot only when a
result comes back, so a plugin that waits for EOF deadlocks any job larger than
that window. `LIGHTVIEW_JOB_TOTAL` in the environment carries the expected
request count, which is what a plugin that wants to size a progress bar should
read instead.

The host will decide a plugin has stopped answering if it moves 128 requests
past one without answering it, or if it goes quiet for a long time *after* having
answered something. A first run that spends ten minutes downloading a model is
not a stall — no clock runs until the first result.

## The manifest

```json
{
  "name": "example-auto-tagger",
  "display_name": "Example Auto-Tagger",
  "version": "1.0.0",
  "api_version": 1,
  "description": "…",
  "execution": { "type": "cli", "command": "python3", "args": ["{plugin_dir}/tagger.py"] },
  "tag_prefix": "example",
  "input": { "max_edge": 512, "video_frames": 5 }
}
```

`api_version` must be `1`. `{plugin_dir}` expands to the installed directory.

`tag_prefix` is the namespace the tags land in — `plugin.example` — and the
bucket is **replaced wholesale** on each run, which is what makes re-running
under a newer version a re-tag rather than a union with what the old model
thought.

`version` is the skip predicate. A file already carrying this plugin's tags at
this version **or higher** is skipped, so a retrained model ships as a version
bump and the next run re-tags the gallery.

### `max_edge` is a cost, and 512 is the free one

The host serves the smallest cached thumbnail tier at least `max_edge` across —
128, 512, 1280 or 2560 — **rounding up, never down**, because a model handed a
smaller image than it trained on has lost information it cannot recover.

The tier the background worker warms is **512**. A plugin declaring more than
that pays one full thumbnail generation per image, on whatever machine is
running the job. The bundled taggers declare 512 for that reason; models
downsize internally, so the loss is nil. If a tagger genuinely needs 1280, warm
that tier for the gallery rather than absorbing the decode silently.

### A plugin never sees a video

The host samples `video_frames` stills across a clip (default 5, capped at 16)
and sends them as ordinary requests. It merges the answers itself: a **union**
of the tag sets, except that `rating:` is one choice rather than a set and gets a
fresh argmax across the frames.

## What is not here

`camie-tagger`, `pixai-tagger` and `wd-tagger` used to live in this directory.
They are personal tools on their own release cadence, pinned to model
repositories and CUDA stacks the gallery knows nothing about, and they share a
virtualenv that was never in this repository — each manifest runs
`{plugin_dir}/../.venv/bin/python`, a sibling in the install root. They are
installed the same way any other plugin is.

## The example

[`example-auto-tagger/`](example-auto-tagger/) is dependency-free `python3` and
about sixty lines. It is what the verification recipe drives, so it is also the
shortest complete statement of the protocol above.
