# plugins/

[← docs](../README.md)

**Responsible for** running a tagger over a gallery: discovering what is
installed, deciding what bytes a plugin sees, speaking the NDJSON protocol to a
subprocess, merging a clip's frames, and writing the results into companions.

**Not responsible for** what a plugin *does* with an image, or for where its
tags end up being queried from — that is
[companion/](../companion/README.md) and [query/](../query/README.md).

The author-facing reference — the manifest, the protocol, what `max_edge` costs
— is [`plugins/README.md`](../../plugins/README.md) in the repository root. This
page is the host side.

**Depends on** [`pipeline/`](../pipeline/README.md) for input bytes and
[`companion/`](../companion/README.md) for the write. **Depended on by** the
`run_plugin` command and `lightview tag`.

## One executor, in process, and no queue between machines

A plugin run is a local operation: read bytes, feed the subprocess, write
companions, report progress. It is driven from the UI on a loopback bind and
from `lightview tag` on the command line, and **those are the same code path
with a different progress sink** — which is why the sink is a closure and not a
trait with one implementation.

The problem the distributed version solved is real: the server is a small box
that cannot run the models, and the desktop has the GPU. But **the desktop can
mount the gallery**, so it does not need a protocol, it needs a path.
`lightview tag /mnt/nas/photos --plugin wd-tagger` opens the gallery the way any
other mode does, runs the plugin locally, and writes companions; the server's
own watcher picks them up. Nothing is claimed, heartbeated or pinned, and
nothing needs a credential — the filesystem already answered the authentication
question.

What went with it: the worker registry with liveness TTLs,
announce/claim/update/complete/fail, job pinning, the requeue-versus-fail
distinction between two staleness clocks, a `remote.toml` credential, a
trust-on-first-use certificate pin, and the fire-and-forget terminal call whose
loss would re-run an entire job.

Two consequences worth stating rather than discovering:

- **Tagging is started from a shell**, unless the gallery is open in a local
  viewer. Consistent with the password, pairing and `server.toml` already being
  administered that way. Unattended tagging of what a phone uploads is a systemd
  timer around the same verb.
- **The bytes move, not the decodes.** The old worker fetched a small image over
  HTTP, so the server paid the decode. `lightview tag` reads full files over the
  mount and decodes on the desktop — more bytes on the wire, far less server
  CPU, which is the right trade when the server is the bottleneck.

## Resolution has no path join, and that is the point

`find` compares a name against what a directory scan produced. An absolute path
or a `..` segment matches nothing, because there is nothing to join it to.

The implementation this replaces did `plugin_dir.join(name)` — and `Path::join`
with an **absolute** argument discards the base entirely, while `..` traverses
out of it. The `manifest.name == name` check afterwards is not a guard, because
whoever writes the manifest writes its name field. Combined with the
[trash-restore primitive](../storage/README.md#the-entry-id-is-not-a-path), that
was a full path from a paired phone to code execution on the server.

So: the directory name is the identity and the manifest's `name` is checked
against *it*; a scan either finds a plugin or does not. **The confinement is
structural rather than a check that could be removed.**

`run_plugin` additionally intersects its paths with the index, so a request
cannot point a plugin at an arbitrary host file even inside the gallery.

## The host decides input, and a plugin never sees a video

A still becomes one request; a clip becomes `video_frames` stills sampled across
it, sent as ordinary requests and merged afterwards. Every executor shares this,
so a plugin cannot behave differently depending on where it ran — and a plugin
author never writes frame extraction, which is where the three bundled taggers
each had their own slightly different version.

**Input is quantized up to a cached tier edge**: the smallest tier at least
`max_edge` across — 128, 512, 1280, 2560 — **rounding up, never down**, because
a model handed a smaller image than it trained on has lost information it cannot
recover. Above the largest tier the source is decoded directly.

The payoff is conditional, and the condition is not automatic: the idle worker
warms **512**, so a plugin declaring more pays one full generation per image, on
whatever machine runs the job. The bundled manifests declare 512 for that
reason; models downsize internally, so the loss is nil. If a tagger genuinely
needs 1280, the answer is to warm `jm` for that gallery, not to absorb the decode
silently.

Merging is a **union** of the per-frame tag sets, with one exception: `rating:`
is a single choice rather than a member of a set, and gets a **redone argmax**
across the frames. Without that, a five-frame clip comes back tagged safe *and*
questionable *and* explicit. A partial failure is not an error — four frames out
of five is a perfectly good answer.

## Why the staleness rules are counts, not clocks

A tagger's first run legitimately produces nothing for minutes while it
downloads and loads a model, so **a live subprocess is not a progressing one**
and liveness is worthless as a signal.

| Rule | Value | Why |
|---|---|---|
| pending window | 32 | also the files on disk, which is what it really bounds |
| stale after | 128 results | a count, so a slow CPU tagger never sheds images for being slow |
| idle reclaim | 5 min | clears a job's tail, where no further results arrive to drive the count — and only once the plugin has answered something |
| no-result stall | 20 min | outer backstop, refreshed **only by results that matched** |
| apply batch | 32 | results per index update |

A compile-time assertion keeps `MAX_VIDEO_FRAMES * 2 <= MAX_PENDING`: a clip
must never fill the window by itself, or a run over one video deadlocks on its
own frames.

A run that stops progressing is failed and reported, and **never retried** —
retrying hands the same wedge to the same plugin.

**Requests are keyed on the temp file *name*, not the full path.** A plugin that
canonicalizes its input under a symlinked `TMPDIR` echoes back a different
string for the same file, and matching on the whole path would silently drop
every result it produced. An unmatched result is logged rather than dropped, and
deliberately does not refresh the clock.

## A run is resumable because writes are per-file and idempotent

Companions are written as results arrive, so an interrupted run — Ctrl-C, a
crash, a closed laptop — leaves the files it finished finished. Re-run it with
the same `--filter` and the already-tagged files are skipped. That is the whole
recovery story, and it replaces a page about requeueing, claim expiry and
partially applied batches.

### The skip predicate is "version or higher"

Checked twice: once while planning, to decide what to send, and **again under
the companion lock immediately before writing**. A run over twenty thousand
files takes hours, and another process — a `lightview tag` over the share, a
phone adding a tag — may have touched the file in between. The plan's answer is
a hint; the answer under the lock is the decision.

Why *or higher* rather than equality: a retrained model ships as a version bump,
so a gallery tagged by v1 must re-tag under v2, while one tagged by v2 must not
be dragged backwards by an older install. An unparseable version on either side
falls back to string equality rather than guessing an ordering — being wrong
costs one re-tag, and guessing that `2.0-rc1` precedes `2.0` could cost a gallery
its re-tag forever.

The plugin's tag bucket is **replaced wholesale**, which is what makes a re-run
under a newer version a re-tag rather than a union with what the old model
thought.

## What is deliberately not here

**Grouping.** A plugin proposing clusters needs four things this does not have:
a way to get proposals off a remote host, a terminal-line contract that keeps
permits recycling (a plugin withholding output until it has seen every face
deadlocks past the window — the exact bug above), durable proposals (a re-run is
hours of GPU), and regions, since `{id, label, paths}` puts a five-person group
shot in five clusters with nothing distinguishing which face is which.

The one decision that had to be made early *is* made: **a confirmed group name
is a `set::` tag**. That is the load-bearing choice, it is independent of how a
grouping gets proposed, and grouping by selection works today through the tag
commands. Build the rest when a plugin exists that needs it — a result kind
nothing receives is the shape of the video-tagging bug.

**Findings**, for the same reason and more comfortably. **Wasm execution**,
advisory `capabilities` and declared UI items — a stub that errors is worse than
an honest absence, and one that silently does nothing is worse still.

## Invariants a caller must uphold

- **Never join a caller-supplied name onto the install root.** Scan, then match.
- **Intersect requested paths with the index** before handing them to a plugin.
- **Re-check the skip predicate under the lock.** The plan is a guess by then.
- **Kill the subprocess on every path out**, including the error ones. A wedged
  tagger outliving its run keeps a GPU busy after the UI says the job failed.
