# 0012 — Plugins declare the host contract they were built for

[← docs index](../README.md) · [plugins](../plugins/README.md) · [worker tagging](../remote/worker-tagging.md)

## Context

A plugin is a directory that gets **copied** into an install root. Nothing
afterwards compares that copy against wherever it came from. A worker binary
can be rebuilt and re-paired ten times while the plugin beside it stays exactly
what it was on the day someone ran `cp -r`.

That is not a hypothetical cost. Taggers written before commit `1eaa7ed` read
stdin to EOF before loading their model. Under `lightview-worker` that is a
guaranteed deadlock past the disk window: the host holds stdin open waiting for
results, the plugin waits for EOF before starting, and neither moves. The
signature is unusually misleading — the model never loads, so no VRAM is ever
allocated; jobs under 64 images finish normally, because the downloader drains
and stdin closes; and the same plugin works at any batch size in the desktop
app, which writes every request up front. It reads as "large batches fail" and
it cost a debugging session.

The streaming requirement that would have prevented it was documented and
enforced by nothing. Meanwhile two further changes wanted to alter what a
plugin receives — stills scaled to the size the model actually wants, and
videos arriving as host-extracted frames — and neither is safe to apply to a
plugin that was not written expecting it. A version 0 plugin handed frames
would sample frames of a frame; a version 1 plugin handed a raw `.mp4` returns
nothing.

## Options considered

**Keep the streaming rule as documentation.** Free, and demonstrably
insufficient: the rule was already written down in three places when the
deadlock happened. It also gives the host no way to change what a plugin
receives, because there is nothing to distinguish a plugin that expects the new
behaviour from one that does not.

**Detect the behaviour instead of declaring it.** Run the plugin, watch whether
it answers before EOF, and refuse it if not. Attractive because it needs no
cooperation from plugin authors — and wrong, because "has not answered yet" is
exactly what a legitimate first-run model download looks like. Any timeout
short enough to be a useful gate would fail honest plugins on cold start, and
any timeout long enough not to would sit inside the hang it is meant to
prevent.

**Version the plugin's own release and infer.** Plugins already declare
`version`, which is stamped into every companion file they write. Reusing it
would conflate two independent questions — "is this a newer build of the same
plugin?" and "which host does it expect?" — and a plugin's own numbering is its
author's business.

**Refuse an undeclared plugin everywhere.** Simple and symmetric, and it breaks
the desktop for plugins that work there today and always have. The desktop
writes every request up front and closes stdin; a stdin-to-EOF plugin completes
fine. Refusing it would be punishing a plugin for a hazard that does not exist
on that host.

## Decision

A manifest declares `api_version`, the host contract it was built against,
separately from its own `version`. Absent means `0`.

- **0** — the pre-1 contract: original file paths, every request written up
  front, the plugin does its own video sampling.
- **1** — current: the plugin consumes requests as they arrive, emits exactly
  one result per request, and reads `LIGHTVIEW_JOB_TOTAL` rather than sizing
  itself off the request list. Stills arrive scaled to `input.max_edge`, videos
  arrive as extracted frames, and the plugin never learns a video was involved.

The check is asymmetric, because the hazard is. `check_api_version` takes how
the host delivers requests: an undeclared plugin is refused only by a host that
releases requests as results come back (`lightview-worker`), and runs normally
on one that writes them all up front. A plugin declaring a version *newer* than
the host is refused everywhere — it may expect input this host cannot produce.

`InputPolicy::for_manifest` turns the same declaration into what the host
actually does, so the version is not advisory metadata: it selects behaviour.

`PluginInfo` carries `api_version` into the worker registry, and workers report
their binary version on announce. Both surface in the web UI's worker list, so
"what is that machine actually running?" is answerable without walking over to
it.

## Consequences

- The deadlock becomes one line at worker startup naming the plugin and the
  fix, instead of a job that hangs at 64 images with nothing in any log.
- Plugin authors have one field to add, and adding it is a claim they must
  actually satisfy — a plugin that declares 1 and buffers stdin still hangs,
  now with no excuse. The abandoned-request reclaim
  ([0013](0013-the-host-samples-video-frames.md) shares its machinery) keeps
  even that from being fatal.
- `install_plugin`'s generated manifest for a bare `.py` deliberately declares
  nothing. Wrapping an arbitrary script cannot promise it streams, so the
  desktop runs it and a worker refuses it until its author says otherwise.
- Bumping `PLUGIN_API_VERSION` is now a real event with a migration cost, and
  the bar is correspondingly high: only for a change that breaks a plugin
  following the current rules.
- The version does not describe the *wire format* of a result, which has not
  changed and is still validated by parsing. It describes what the host
  guarantees about input and what it requires of output timing.
- This is what makes moving the ML taggers to their own repository tractable
  (A6 in [`todo.md`](../todo.md)). Across a repository boundary, drift between
  host and plugin stops being a mistake and becomes the normal case; a version
  is the thing that lets the two release independently without guessing.
