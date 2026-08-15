# Open work

[← docs index](README.md)

Known gaps — understood but not done. Anything with enough shape to be designed
belongs in a subsystem page instead.

Items are grouped by the part of the system they touch, and **within a group
they are listed in the order they should be done**: each group's opening
paragraph says why that order and not another. The groups are near-independent —
nothing in *Frontend structure* waits on *Remote tagging* — so they can largely
be worked in parallel. The one cross-group dependency was D1 before C2 —
building the infinite canvas before the two grids were de-duplicated would have
made it a third copy of their loading machinery — and D1 has landed, so C2 is
free to start. Ids (`A1`, `B2`, …) are stable references; they encode position
within a group, not global priority.

Items marked **(small)** are hours, not days. Those, plus C3 and D2, were
previously collected under a single "Smaller items" heading; they now sit in the
group each one belongs to, because size turned out to be the least useful thing
about them — B1 is something a much larger item is waiting on, and D2 turned out
not to be small at all once its premise was checked against the code.

---

## A. Remote tagging and plugins

The largest cluster, and one chain: today you cannot tell what a worker is
actually running, a job that goes wrong can hang forever without saying so, and
videos silently produce nothing.

The order is driven by verification cost and by two hard dependencies. **A1
first** because every later item in this group is verified by running jobs, and
a job that wedges forever turns each of those into a debugging session — and
because A5 multiplies files-per-job about fivefold, which makes the 64-file
permit leak far more likely to bite. **A2 and A3 next** as a pair: they are the
two halves of "what version is actually running on that machine", plugins and
binary respectively, and A2's installer is a hard prerequisite for A6. **A4
before A5**, because A4 introduces the manifest-declared input size that A5's
frame extraction is specified to reuse. **A5 before A6**, because it is what
takes ffmpeg and video handling out of every plugin, which is worth having
before they move across a repository boundary. **A7 last** — it is the largest
and most speculative, and it wants the versioned protocol A6 delivers.

### A1. Two ways a tagging job can still hang forever

Distinct from the stale-plugin deadlock in [A2](#a2-nothing-updates-an-installed-plugin-and-a-stale-one-deadlocks-silently),
which the 20-minute fail-out does catch. These two paths defeat the timers
themselves.

On the worker, `last_result` is refreshed before the result is matched against
`pending` (`bin/lightview-worker/job.rs`), so a plugin that answers with paths
the worker cannot match keeps the 20-minute `NO_RESULT_STALL_SECS` timer alive
while leaking a disk permit per file. Downloads stop at 64, the plugin goes
quiet, and nothing ever trips. Only count a result that matched an entry.

On the server, `TaggingState::reap` is called from `announce_worker`,
`enqueue_job`, `claim_job` and `get_status` — but not from `update_job`
(`tagging/mod.rs`), and a worker inside a running job stops announcing and
claiming. So during exactly the situation the no-progress deadline exists for,
the only traffic reaching the server is the heartbeat that skips the reaper, and
`JOB_NO_PROGRESS_SECS` never evaluates. The comment claiming commands "arrive
constantly, so there is no background reaper task" is wrong for a wedged job.

The real repair underneath both is a per-file deadline: a downloaded file that
has gone unanswered for its own timeout releases its permit and counts as one
failed image, so the batch continues instead of dying at 64.

### A2. Nothing updates an installed plugin, and a stale one deadlocks silently

Plugins are *copied* into a worker's `data_dir()/plugins`, and nothing
afterwards compares that copy against the repo. A worker binary can be rebuilt
and re-paired while the plugin beside it stays whatever it was on the day it was
copied — which is how the 64-image stall survives a "latest worker binary".

Any tagger predating commit `1eaa7ed` reads stdin to EOF before loading its
model. Under `lightview-worker` that is a guaranteed deadlock past
`MAX_PENDING_FILES`: the host holds stdin open while it waits for results, the
plugin waits for EOF before it starts, and neither moves. The signature is
distinctive — the model never loads, so *no VRAM is ever allocated*, jobs under
64 images finish normally (the downloader drains, stdin closes, EOF arrives),
and the same plugin works for any batch size in the desktop app, which writes
every request up front and closes stdin immediately. Reproduced end to end: the
current `example-auto-tagger` tags 100 images through a remote worker without
stalling, so the host path itself is sound.

The streaming contract is documented in
[`remote/worker-tagging.md`](remote/worker-tagging.md) but only enforced by
convention. Worth having: an install/update command that refreshes plugins from
a known source rather than leaving `cp -r` as the mechanism, and a version the
worker reports so a server can see what its workers are actually running.

Making it diagnosable matters as much as preventing it. Plugin stderr is logged
at `log::debug!` (`plugin/runner.rs`), so at the default level the one channel
that would have explained this — the plugin saying nothing at all — is
invisible. It should surface at info, or at least be replayed when a job fails.
That half is a few lines and pays for itself immediately; do it first, before
the installer.

### A3. `cargo tauri build` never builds `lightview-worker`

`npm run tauri build` produces no worker binary. That is not a build failure:
the bin declares `required-features = ["worker"]` (`src-tauri/Cargo.toml`), so
Cargo skips it unless the feature is on, and the Tauri bundle only carries the
app's own binary. It needs its own
`cargo build --release --bin lightview-worker --features worker`.

Decide whether that stays a documented separate step or the worker is added to
the release build and shipped alongside the app. Either way it should be
written down, because the quiet failure mode is a developer running a
months-old worker binary against a current server and debugging the wrong code
— the same class of problem as [A2](#a2-nothing-updates-an-installed-plugin-and-a-stale-one-deadlocks-silently),
one level down, which is why the two belong together.

### A4. Taggers decode full-resolution originals on the local path

Remote workers already pull `?fit=1024` (`bin/lightview-worker/config.rs`), so
this is only half missing: `tagging/local.rs` hands the plugin every original
path, which means a server-local run of a 448-pixel model decodes 60-megapixel
files in Python for nothing.

Rather than a second hard-coded number, let the plugin state the longest edge it
wants in `manifest.json` and honour it on both paths — as the `?fit=` value the
worker requests, and as a downscaled temp file the local executor hands over in
place of the original. Point those downloads at the cached `jm` tier where the
requested size allows it, instead of resizing from source each time; the
`?fit=` route has no coalescer ([B1](#b1-the-fit-resize-route-has-no-coalescer-small)),
so every concurrent tagger request currently repeats the same decode. See
[`plugins/`](plugins/README.md) and [`pipeline/`](pipeline/README.md).

That manifest field is also what [A5](#a5-videos-are-dropped-from-every-remote-tagging-job)
needs for its frame edge, which is why this comes first.

### A5. Videos are dropped from every remote tagging job

Sending mp4s to a remote worker produces no tags and no worker log line,
because the job is never offered to a worker at all. `resolve_target`
(`tagging/mod.rs`) intersects the candidate list with
`SELECT path FROM media_meta WHERE media_type = 'image'`, and it does so for
*both* target kinds — so explicitly selecting videos and right-clicking to tag
them drops them just as surely as a filter would. When every candidate is a
video the resolved list is empty, and the claim loop treats empty as "nothing
left to tag" and marks the job **Done**. The UI therefore reports a job that
succeeded, which is why this reads as silence rather than as an error.

This never worked remotely: the filter arrived in `1eaa7ed`, the commit that
introduced the worker. The desktop path (`commands/plugins.rs`) has no
equivalent restriction, so native frame-splitting still works — which is what
makes it look like a regression. The plugin side has been ready all along
(`is_video` → `predict_video`).

The intersection itself must stay. It is doing two jobs, and only one of them
is wrong: it also confines a remote-supplied path list to files actually
indexed in this gallery, which is the only thing stopping a paired device from
naming arbitrary filesystem paths for a worker to download. Widen the
`media_type` predicate; do not drop the join. The second half is
`AutoTagPanel.tsx`, where "Tag untagged" hardcodes
`type:image AND NOT has::plugin.<prefix>` and would keep excluding videos even
after the backend stops doing so.

**The server extracts the frames, and how many is adjustable.** Shipping whole
videos was the alternative and is rejected: `?fit=` only applies to
`jpg`/`png`/`webp` (`is_fit_resizable`), so a video would transfer whole, and
the worker's disk bound is a count of 64 files rather than a byte budget —
64 videos is plausibly tens of gigabytes on the wire and on disk. Frames are
images, which makes that bound safe again and costs a few hundred KB per clip.
Sampling a handful of frames is cheap enough that the weak-server argument does
not apply; decoding every frame would be a different matter.

`pipeline/video.rs` already has the parts: `probe()` for duration (cached),
`-ss` seeking in `run_frame`, scaling inside the filter graph, display-matrix
rotation, and timeouts on every invocation. `extract_frame` needs to take a
timestamp instead of choosing one. Mirror the sampling the plugins do today —
`VIDEO_FRAME_SAMPLES = 5`, evenly spaced across 5%–95% of duration — as the
default for the adjustable count, so behaviour does not change silently on the
day this lands.

Merging is mostly free, and one part is not. The plugins aggregate frames by
taking an element-wise maximum of the score vectors and thresholding once, and
because `max(s) > T` exactly when some `s > T`, a plain union of per-frame tag
sets reproduces today's general and character tags precisely. `rating:` is the
exception: it is an argmax over the rating scores, so a union would yield up to
five ratings where there is currently one. The per-frame `rating_scores` ride
along in the result `meta` already, so the host can reproduce the argmax over
the per-frame maxima — but it has to be done deliberately.

The payoff beyond fixing video is that plugins stop knowing what a video is.
`is_video`, `predict_video`, `get_video_duration`, the plugin-side
`extract_frame` and the ffmpeg dependency itself all leave every plugin, which
is worth having before the taggers move to their own repository. That argues
for converging the desktop path onto the same host-side extraction rather than
keeping plugin-side sampling for local runs and server-side sampling for remote
ones — two policies that would quietly disagree per plugin. The frame edge
should come from the same manifest-declared input size as still images
([A4](#a4-taggers-decode-full-resolution-originals-on-the-local-path)).

### A6. Move the real plugins to their own repository

`plugins/` should keep only `example-auto-tagger`; the three ML taggers (`wd`,
`camie`, `pixai`) move out. They are 11 tracked files and ~200 KB, so this is
not about size — it is that they are personal tools on their own release
cadence, pinned to model repositories and CUDA stacks the gallery knows nothing
about, and every one of them is a working demonstration of a protocol the host
should be able to change without editing three Python scripts in the same
commit.

`example-auto-tagger` stays because it is load-bearing: dependency-free
`python3`, and the thing the headless recipe in `CLAUDE.md`, the testing
section of [`remote/worker-tagging.md`](remote/worker-tagging.md) and the
`verify` skill all drive to exercise the job queue without ML.

Two things make this more than a `git mv`:

**The install path stops existing.** Today a plugin is installed by copying
`../plugins/<name>` out of the checkout. With no local copy, that instruction
has no source, so the install/update mechanism from
[A2](#a2-nothing-updates-an-installed-plugin-and-a-stale-one-deadlocks-silently)
stops being a nice-to-have and becomes the only way to get a plugin onto a
worker. These two items should land together or the split will strand every
worker on whatever it last copied — which is exactly the failure that has
already cost a debugging session.

**The three taggers share a virtualenv that is not in the repo.** Each manifest
runs `{plugin_dir}/../.venv/bin/python`, a sibling of the plugin directory in
the *install* root, so they must be installed into a common parent and
something must build that venv from their three `requirements.txt` files. An
installer that treats plugins as independent directories will produce three
plugins that all fail to spawn.

Whether the new repository is a submodule or fully independent is open. A
submodule keeps a single clone working and lets CI reach the taggers, at the
cost of re-coupling the two repositories' histories; independent is cleaner and
forces the install path to be real, which is the point. Independent looks
right, given that nothing in the build or the tests reads the ML plugins today.

References to fix in the same change: the plugin-system note in `CLAUDE.md` and
the `plugins/` line in `README.md` (both name `wd-tagger` as *the* example), the
inventory at `README.md`'s directory listing, and the worker's own error
message, which tells the user to install "the repo's plugins/wd-tagger"
(`bin/lightview-worker/main.rs`) — a string that becomes a lie the moment this
lands.

While the two repositories are being separated, give the protocol a version the
manifest can declare. Today a plugin states its own version but nothing about
which host contract it was written against, which is why a script that predates
the streaming requirement installs cleanly and then deadlocks. Across a repo
boundary that drift stops being a mistake and becomes the normal case.

### A7. Plugin-driven UI, and naming what a plugin found

Recognising faces is the case the current protocol cannot express: the plugin
can emit a cluster, but nothing can tell it that cluster is a particular
person's, because there is nowhere for a plugin to put UI.

The direction chosen is host-rendered and declarative — the plugin describes
panels, forms and actions as JSON in its manifest (finally activating the
`PluginUiConfig.settings_schema` field that has always parsed and never
rendered) and the host draws them in Solid. Plugin-supplied HTML in a sandboxed
iframe, [`plugins/`](plugins/README.md) Track C, stays the fallback for
displays a schema genuinely cannot describe; it is a much larger surface to
version and should not be taken on for this.

The face case additionally needs something declarative UI does not give: a host
screen for naming and merging plugin-emitted clusters, which any plugin that
produces groups can feed. That belongs to the host, not to a plugin.

This is the group's largest item and wants the versioned protocol
[A6](#a6-move-the-real-plugins-to-their-own-repository) introduces — a UI
contract that can drift silently across a repository boundary is worse than no
UI contract.

---

## B. Thumbnails, cache, and serving

Three independent items with no dependency between them, so the order is by
what unblocks work elsewhere and by size. **B1 first** because it is the
smallest of the three and because [A4](#a4-taggers-decode-full-resolution-originals-on-the-local-path)
is about to make the `?fit=` route hot; landing the coalescer before that means
the tagger work is measured against a route that behaves like the tier path
rather than against one that redundantly decodes. **B2 next**, a user-visible
staleness bug. **B3 last** — a latent hazard with no current symptom, safe to
do at any point.

### B1. The `?fit=` resize route has no coalescer *(small)*

Unlike the tier serve path, concurrent requests for the same resize each do the
work. Every duplicate is a full source decode.

### B2. `reindex_gallery` does not regenerate thumbnails

Re-indexing rebuilds the media and tag indexes but does not kick off background
thumbnail regeneration, so a re-index after a bulk external edit leaves stale
thumbnails until something else asks for them.

### B3. `too_many_arguments` on the thumbnail write paths *(small)*

`write_standard_row` and `write_tier_row` take nine positional arguments each,
two of them adjacent `u32`s (`width`, `height`). Transposing them at a call site
would compile and store a wrong aspect ratio. A small `ThumbRow` struct removes
the class.

---

## C. Views and browsing

Five views, all native: the two grids and the map exist, the canvas and the
virtual folder hierarchy do not. There is no view-module API and there is not
going to be one — [decision 0008](decisions/0008-no-view-module-api.md) records
why, and what replaced it.

**C1 and C4 first**, in either order: both finish something already shipped, and
an existing feature that misleads is worse than a missing one — C1 offers a
label you cannot see, C4 offers a tag you cannot use. Only then start new views.
**C2 before C3** because the infinite canvas is the view actually wanted; the
virtual folder view is a well-specified idea with no pressure behind it.

### C1. Colour labels stop half way

The column is indexed, `color:` filters, the context menu picks one and
`ThumbnailCell` draws the dot — but the label is invisible everywhere else and
cannot order anything. `SortField` (`lib/types.ts`) has no `color` arm, so
neither `SortMenu` nor the backend sorter offers it, there is no grouping by
label, and `JustifiedGrid` never renders the dot the square grid does. A colour
you can set, search for and not see in the view you actually use is worse than
not having it.

Ordering needs a defined sequence — labels are a fixed list, so sort on that
list's index rather than the stored string, with unlabelled items last.

### C2. The infinite scrolling canvas

The top of the current sort in the centre, later items spiralling outward,
reusing the aspect-preserving justified tiers so it costs no new thumbnails.

A native view, like the other four. The view-module API this item used to be
about is settled and not happening — see
[decision 0008](decisions/0008-no-view-module-api.md). The short version: the
thing an API was wanted for was "a view you never use costs nothing", and that
is a bundling question, not a contract question. Per-gallery enablement
(`views.rs`) plus a dynamic `import()` on the views that are actually expensive
delivers it with no public surface at all — the map, the only view carrying its
own rendering library, is 153 kB of a 445 kB bundle and is already split out;
the canvas and [C3](#c3-a-virtual-folder-view) reuse machinery the main bundle
carries regardless, so splitting them would save single-digit kilobytes.

What the canvas *does* need is shared implementation, which the frontend already
has a pattern for: behaviour lives in `lib/` as factory functions taking
accessors (`scrollDynamics.ts`, `loadPriority.ts`, `thumbSwap.ts`, …), each
component keeping its own policy. That is now true of the loading machine as
well — [D1](#d1-250300-duplicated-lines-between-the-two-grids) extracted
`urlVersions`, `pathIndex`, `thumbQueue`, `fetchLoop` and `cellSources` — so the
canvas is the first view that can consume it rather than a third copy of it. It
supplies its own layout, range calculation, `evict`, `drain` and `speculate`
against spiral geometry; everything else it inherits.

One thing it will have to change rather than reuse: `loadPriority.ts` ranks
against contiguous *index* ranges, which is what a reading-order scroller has. A
spiral shows an off-centre 2D window — several disjoint index runs — and wants
ranking by distance from the viewport centre. Lift the zone→rank mapping into a
parameter and keep the current function as the range-based case.
[`grid-loading.md`](frontend/grid-loading.md) has the full inherit/write split.

### C3. A virtual folder view

Default hierarchy: plugin names and user folders at the top level, then a folder
per tag inside. Entirely virtual — it would never copy or move files. Membership
is *derived*, not curated: every node is a saved query over the existing index,
which means no new tables, nothing to repair when files move or are deleted, and
no second source of truth beside the companion files. Curated albums you drag
files into were considered and set aside for that reason.

### C4. A tag with a space in it cannot be filtered

The filter tokenizer splits on whitespace and has no quoting, so no tag
containing a space can be named by any query. It is not that such a query
matches poorly — it fails outright: `beach trip` parses `beach` as a term,
reaches `trip` with nothing left to do, and the whole expression is rejected as
`Unexpected token: trip`.

The storage layer has no such restriction, and nothing stops the two from
diverging. `add_user_tag`, `add_user_tag_batch`, `rename_user_tag`, and
`merge_user_tags` take the string verbatim; the three UI paths that call them
(`ContextMenu`, `InfoPanel`, `SelectionBar`) only `.trim()`, so interior spaces
pass straight through; and all four are in the `/api/invoke` allowlist, so a
paired browser can do it too. The result is reachable by ordinary use: the tag
is written to the sidecar, indexed, counted, and then **offered by
autocomplete** — so a user who never types a quote still reaches the failure by
clicking a suggestion, which returns a 500.

Quoted strings in `filter::parser::tokenize` are the fix. The tokenizer already
handles one context-sensitive character — `(` is grouping only when it starts a
token, which is what keeps `hatsune_miku_(vocaloid)` intact — so a quote state
belongs in the same loop. Two frontend spots move with it: `getCurrentToken` in
`FilterBar.tsx` finds the current token with `/(\S+)$/` and would split inside a
quoted phrase, and `insertSuggestion` must wrap a suggestion containing
whitespace in quotes. That last line is what makes the fix invisible — the
dropdown stops handing out queries that fail.

Normalising tags to underscores on write was considered as the cheaper answer,
and it does close the path most users take. It was not chosen as the whole fix
because it silently rewrites what someone typed and cannot reach data written
elsewhere: companion files are a wire format that other LightView installations
read and write (see [`companion/`](companion/README.md)), so a sidecar carrying
a space can always arrive from outside. Only quoting makes those tags
addressable.

This is independent of the location tags, which join words with underscores to
match what the ML taggers already emit; that convention is right regardless and
does not change here. See [`query/`](query/README.md), where the constraint is
documented, and [`geocode/`](geocode/README.md).

---

## D. Frontend structure and performance

**D1 is done**, and with it the orchestration half of D2 — the two were the same
code, which is why they were scheduled together.
[`grid-loading.md`](frontend/grid-loading.md) now describes the four extracted
primitives and how the grids stay different through them, and
[C2](#c2-the-infinite-scrolling-canvas) is unblocked: the infinite canvas can
consume `createFetchLoop` / `createThumbQueue` rather than become a third copy
of them.

**D2 is now one item, and it needs a measurement before code.** What is left of
it is the decode-worker pool, whose case is strongest on the platform this
repository cannot measure. It is not blocked by anything; it is blocked on
someone producing a number.

**D3 is done** — the commands/settings split shipped, and
[`chrome.md`](frontend/chrome.md) now describes what exists rather than what was
planned. The two questions that had been holding it up are settled in
[decision 0009](decisions/0009-commands-panels-and-configuration.md).

### ~~D1. ~250–300 duplicated lines between the two grids~~ — done

Four primitives in `src-solidjs/lib/`, extracted in the order the plan proposed
and each one keeping the grids' policy differences as arguments:
`urlVersions.ts` (the `?v=` counter and its epoch), `pathIndex.ts` (path →
position, and pruning), `thumbQueue.ts` (queued / in-flight / failed / warmed,
parameterized on the payload so GalleryGrid carries none and JustifiedGrid
carries a `ThumbTier`), `fetchLoop.ts` (the two single-flight slots and the
order one pass runs them in), and `cellSources.ts` (a cell's URL, its rung and
its in-flight swap, which only make sense together). `markUrlLoaded` moved to
`lib/loadedUrls.ts` on the way, so `lib/` no longer imports from a component.

The two components lost 244 lines (2397 → 2153) and the new modules add 636, so
the tree is *longer* than before. That is the expected shape and not a reason to
be suspicious of the trade: what was duplicated is now written once, and most of
the added bulk is the doc comments explaining why each piece is shaped the way
it is — the invariants that used to live only in the two copies, where they
disagreed. The win is single-sourcing and correct-by-construction teardown, not
a smaller diff.

One thing was deleted rather than moved: GalleryGrid's `coalescedPaths` set was
redundant with the queued and in-flight sets on every path that reached it, and
its only distinguishable behaviour was a bug — `onInvalidate` cleared the queue
but not the coalesced set, blocking a path from re-queueing until the next pass.

### D2. A worker pool for image decode on the client

The original entry said "a single decode worker is fine in practice… but a small
pool would remove the JS orchestration bottleneck under burst load", and **there
is no decode worker to pool**: `new Worker(` appears nowhere in `src-solidjs/`,
the only two `createImageBitmap` calls are `ContextMenu` (clipboard copy) and
`GifCanvas` (atlas frames), both on the main thread, and `thumbSwap.ts` uses
`img.decode()`, a browser facility rather than a worker of ours.

Restating it produced two readings. **The orchestration one has landed** — see
the drain re-arm in [`grid-loading.md`](frontend/grid-loading.md) and
[decision 0010](decisions/0010-re-arm-the-drain-not-widen-it.md). It was not
"widen the drain concurrency": the slots are not the bottleneck, and widening
them would put more un-preemptable work on one bounded pool. What was actually
wrong is that nothing re-armed the loop when a batch settled, and a 404 had no
way to reach the schedule at all, so the queue drained at one batch per 500 ms
poll however fast the backend was. The swap half of that reading was simply
wrong: `createThumbSwapper` already keys in-flight decodes per path and runs
them concurrently.

Checking it in a browser turned up something that bounds the payoff, and is the
more useful finding of the two: **the 404 queue is a recovery path, not how a
gallery fills.** Both thumbnail routes resolve through
`thumb_serve::get_or_generate`, which generates inside the request and 404s only
once generation has failed — measured over a cold 1200-image gallery, 880
thumbnail responses and not one 404. So the re-arm buys recovery latency, and
anyone trying to make browsing faster should be looking at `get_or_generate`'s
coalescer and pool and at the speculative warms instead.

**What remains is moving thumbnail decode off the main thread**, and it is a
large change, not a small one. Every cell becomes a `<canvas>` fed by
`createImageBitmap` in a worker (or a transferred `OffscreenCanvas`), which
carries two consequences the original entry did not name:

- It gives up the browser's own image cache, which is measurably doing work
  today: returning to already-visited scroll positions costs +2 MB rather than
  +55 MB, and that is the cache, not us. Canvas cells re-decode on every reveal.
- It buys back the one lever [`grid-loading.md`](frontend/grid-loading.md) says
  does not exist — "the client cannot free anything". `ImageBitmap.close()` is
  explicit. On iOS, where the ceiling is enforced by killing the tab rather than
  by pruning, that is plausibly a bigger prize than the decode thread, and it is
  a *different* justification than the WebKitGTK one.

So it has two rationales on two platforms, and the stated one is on the platform
no harness here can measure. Support is also a floor — `createImageBitmap` in a
worker needs Safari 16.4+ — so the `<img>` path stays as a fallback either way,
and the change adds a second rendering model to `ThumbnailCell` rather than
replacing the first.

Do it as a measured spike on the web client first, where both the harness and
the iOS memory pain are, and write a decision file for the outcome either way.

### ~~D3. Commands and settings are the same drawer, on both surfaces~~ — done

The command list lives in `components/topbar/CommandMenu.tsx` and renders two
ways: a dropdown behind an overflow icon where the desktop gear was, and a sheet
behind a FAB where the mobile upload button was. Settings is its last entry, and
`SettingsMenu` keeps only configuration — six sections left it, the `Section`
`order` prop is gone, and the remaining seven are read in source order. Neither
surface gained a control; the phone went from four floating buttons to three.

The two open questions were settled in
[decision 0009](decisions/0009-commands-panels-and-configuration.md): Plugins
and Remote Tagging became one `AutoTagPanel` behind one command, and the mobile
view switcher took the corner the gear vacated. Details in
[`frontend/chrome.md`](frontend/chrome.md), which is now a description rather
than a plan.

---

## E. Build and platform integration

**E1 first**, whenever the tree is quiet enough to take it: until it lands, the
gate advertised in `AGENTS.md` cannot pass and `cargo clippy --fix` stays a trap
for every other change in this file. It is a scheduling constraint rather than a
priority — it wants no branches in flight — so **E2** goes first if that window
has not arrived; it is also the one of the three a headless deployment feels
today. **E3** is self-contained and desktop-only, so it waits for neither.

### E1. The tree is not rustfmt-formatted

`cargo fmt --check` fails on ~70 files, and formatting would be a ~4,200-line
diff. Until that lands as its own commit, the gate advertised in `AGENTS.md` is
aspirational and `cargo clippy --fix` is a trap: its let-chain rewrites leave
bodies at the old indentation and only look right after a `cargo fmt` you cannot
scope to one change. Worth doing when no branches are in flight, together with
the ~50 remaining clippy style warnings (`collapsible_if` dominates). See
[`build-and-verify.md`](build-and-verify.md).

### E2. Most host settings are unreachable on a headless server

Every per-gallery host setting is a `#[tauri::command]`, so it exists only in
the desktop app: the gallery password and its inactivity window, the upload
enable/scheme, and the remote-delete flag. A paired browser is deliberately not
allowed to change any of them — they administer the boundary `/api/invoke`
enforces — which is right, but it means a Docker deployment has no way to reach
them at all, short of editing `gallery_meta` in SQLite by hand.

`lightview-headless` has the shape of the answer already: `pair` and `views`
both open the served gallery's `cache.db` from a second process (safe under
WAL) and mutate one key. The remaining settings want the same treatment, and
probably one `config` subcommand rather than one verb each — `views` earned its
own name by being a list rather than a value, and that argument does not
generalise.

The password is the one with a wrinkle: `set_remote_password` argon2-hashes its
input, so the subcommand must read the password from a prompt or stdin rather
than argv, where it would land in shell history and `ps`.

### E3. "Open folder with LightView" on Linux

`lightview.desktop` already declares `MimeType=inode/directory` and
`Exec=lightview %f`, and `main.rs` already opens a directory passed as argv[1] —
but nothing installs the file, and `Exec` names a binary the build does not
produce: `tauri.conf.json` sets `productName` to `Gallery`. Fix the name
mismatch, install the desktop file and an icon from the bundle, and add a
`Desktop Action` so file managers offer "Open LightView here" on a folder's
background as well as "Open With" on the folder itself. Linux only for now.

---

## F. Structural observations

Not scheduled, and deliberately so. Recorded because the shape of the system
only makes sense once you know them.

### F1. Absolute paths as primary keys

Every path-keyed row stores an absolute path, which is why `rebase_root` and
`infer_old_root` exist. Storing gallery-relative paths would delete that entire
mechanism, but it is a migration touching every table and every query, and the
current machinery works and is tested. Recorded as a structural observation, not
a recommendation — see
[decision 0001](decisions/0001-one-cache-per-gallery.md).
