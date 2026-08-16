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

Group A is now mostly closed: A1–A5 have landed and A6's blocking half with
them, leaving one deferred move and one large design. B, C and E are where the
open work is.

Items marked **(small)** are hours, not days. Those, plus C3 and D2, were
previously collected under a single "Smaller items" heading; they now sit in the
group each one belongs to, because size turned out to be the least useful thing
about them — B1 is something a much larger item is waiting on, and D2 turned out
not to be small at all once its premise was checked against the code.

---

## A. Remote tagging and plugins

**Mostly done.** A1–A5 have landed, and A6's protocol half with them. What is
left is one deliberate deferral (moving the ML taggers out of this repository)
and one designed-but-unbuilt item (A7, plugin output that is not tags). The
chain the original ordering described — you could not tell what a worker was running, a job that
went wrong hung forever, and videos silently produced nothing — is closed.

Three decisions came out of it and are the place to start if you need the
reasoning rather than the code:
[0012](decisions/0012-plugins-declare-the-contract-they-were-built-for.md) on
why plugins declare an `api_version`,
[0013](decisions/0013-the-host-samples-video-frames.md) on why the host samples
video frames, and
[0014](decisions/0014-ship-the-worker-with-the-release.md) on shipping the
worker binary.

### ~~A1. Two ways a tagging job can still hang forever~~ — done

Both timer bugs are fixed — the worker's silence timer counts only results that
*matched* a request, and `update_job` reaps before applying the heartbeat, which
is the only traffic a worker inside a wedged job produces. But the real repair
was the one the item named underneath them: a request the plugin will not answer
is now abandoned rather than holding its disk slot forever.

It is a count rather than a deadline, which the original entry did not
anticipate. A slow CPU-only tagger legitimately leaves a file queued for a long
time, so any wall-clock per-file timeout is either too tight for it or too loose
to help; instead a request is abandoned once the plugin has answered 128
*other* requests since it was sent. At most 64 are outstanding, so watching
twice that many others complete means this one was skipped. A five-minute idle
rule covers the tail of a job, where no further results arrive to drive the
count, and stays off until the plugin has answered something so a first-run
model download is never mistaken for a wedge.

The consequence worth stating plainly, because it was the actual requirement:
**a job of any size can be started and walked away from.** The window slides,
misbehaviour costs one image at a time, and `/api/invoke` no longer caps a
selection at axum's 2 MB default.

### ~~A2. Nothing updates an installed plugin, and a stale one deadlocks silently~~ — done

`api_version` in the manifest, checked against how the host delivers requests:
an undeclared plugin is refused by `lightview-worker`, where reading stdin to
EOF deadlocks, and still runs on the desktop, where it always has. That turns
the deadlock into one line at startup naming the plugin and the fix.

`lightview-worker install` and `plugins` replace `cp -r`; the copy logic moved
to `plugin::install` so the desktop command and the worker share it, venv
rewriting included. Plugin stderr is retained and quoted in the failure a job
reports — including "the plugin wrote nothing to stderr", which is the diagnosis
for a plugin waiting on an EOF that will never come.

### ~~A3. `cargo tauri build` never builds `lightview-worker`~~ — done

Shipped with the release rather than left as a documented step, because the
failure mode is someone running a months-old worker and telling people to build
it themselves guarantees no two machines are on the same one. See
[decision 0014](decisions/0014-ship-the-worker-with-the-release.md) and
[`build-and-verify.md`](build-and-verify.md).

### ~~A4. Taggers decode full-resolution originals on the local path~~ — mostly done

`input.max_edge` in the manifest, honoured on both paths: as the `?fit=` value a
worker requests, and as a scaled temp file the local executor and the desktop
batch hand over. The bundled taggers declare 1024, which is what a worker was
already pulling, so remote behaviour is unchanged and the local path stops
decoding 60-megapixel files for a 448-pixel model.

**One half deliberately not done:** pointing those downloads at the cached `jm`
tier where the requested size allows it. It needs the tier's existence and edge
checked per request against what the plugin asked for, and the redundant-decode
problem it was aimed at is mostly [B1](#b1-the-fit-resize-route-has-no-coalescer-small)'s
— a coalescer on `?fit=` covers concurrent taggers without a second lookup path.
Worth revisiting only after B1, and only with a measurement.

### ~~A5. Videos are dropped from every remote tagging job~~ — done

`resolve_target` now intersects with images, videos and GIFs; the join itself
stayed, because it is what confines a remote-supplied path list to files this
gallery has indexed. The host extracts frames (`plugin/input.rs`, served to
workers by `GET /media?frame=`), merges them with the exact union the item
described plus the redone `rating:` argmax, and `AutoTagPanel`'s "Tag untagged"
dropped its `type:image` clause.

The payoff landed as specified: `is_video`, `predict_video`,
`get_video_duration`, `extract_frame`, the sampling policy and the ffmpeg
dependency all left the four bundled plugins, and the desktop converged onto the
same host-side extraction rather than keeping a second policy. See
[decision 0013](decisions/0013-the-host-samples-video-frames.md).

### A6. Move the real plugins to their own repository

**Deferred deliberately; its blocking half is done.** The protocol now carries a
version the manifest declares
([0012](decisions/0012-plugins-declare-the-contract-they-were-built-for.md)),
which was the part that had to land before a repository boundary could exist —
across one, drift between host and plugin stops being a mistake and becomes the
normal case. `lightview-worker install` is the install path the split needs, and
it exists.

What remains is the move itself. `plugins/` should keep only
`example-auto-tagger`; the three ML taggers (`wd`, `camie`, `pixai`) move out.
They are 11 tracked files and ~200 KB, so this is not about size — it is that
they are personal tools on their own release cadence, pinned to model
repositories and CUDA stacks the gallery knows nothing about.

`example-auto-tagger` stays because it is load-bearing: dependency-free
`python3`, and the thing the headless recipe in `CLAUDE.md`, the testing section
of [`remote/worker-tagging.md`](remote/worker-tagging.md) and the `verify` skill
all drive to exercise the job queue without ML.

Two things still make it more than a `git mv`:

**The three taggers share a virtualenv that is not in the repo.** Each manifest
runs `{plugin_dir}/../.venv/bin/python`, a sibling of the plugin directory in
the *install* root, so they must be installed into a common parent and something
must build that venv from their three `requirements.txt` files.
`plugin::install` handles the manifest rewriting when a venv sits beside the
source; it does not *create* one, and an installer that treats plugins as
independent directories will produce three plugins that all fail to spawn.

**References to fix in the same change:** the plugin-system note in `CLAUDE.md`
and the `plugins/` line in `README.md` (both name `wd-tagger` as *the* example),
the inventory at `README.md`'s directory listing, and the worker's own error
message, which now points at `lightview-worker install <path>` but still assumes
a path exists to point at.

Whether the new repository is a submodule or fully independent is open.
Independent looks right, given that nothing in the build or the tests reads the
ML plugins today.

### A7. Plugin-driven UI, and naming what a plugin found

**Designed and settled; not built.**
[`plugins/findings-and-ui.md`](plugins/findings-and-ui.md) is the spec —
wire format, storage, screens and build order — and
[decision 0015](decisions/0015-plugin-ui-is-fixed-shapes-not-a-declared-layout.md)
records why the UI is a fixed vocabulary rather than a declared layout. This
entry is the summary and what is left open.

Two plugins the current protocol cannot express, wanting the same three things.
Recognising faces: the plugin emits a cluster, and nothing can tell it that
cluster is a particular person's. Finding an image's original source: a
reverse-search plugin returns ranked *candidates* — URLs, sites, artists,
similarity scores — of which one or none is right. Neither output is a tag, both
need somewhere for a person to resolve them, and in both cases the verdict has
to reach the plugin's next run or it re-asks forever.

What was decided:

- **Three host-drawn shapes** — `choice`, `confirm`, `label` — declared in the
  manifest, content supplied per result. The host owns all layout; a fourth
  shape is a host release. A general renderer is the thing to extract on the
  third real case, not the first.
- **No new review panel, and no companion schema bump.** A `pending::plugin.<name>`
  filter term plus a Findings section in the viewer's info panel *is* the queue,
  built from parts that already virtualize and already work on a phone. A
  confirmation writes the option's tags to `tags.user` (indexed, filterable,
  survives re-tagging, removable like any user tag) and the provenance to
  `meta.plugins[prefix]` — both destinations already exist.
- **Pending findings and rejections live in the cache DB**, following the
  `not_duplicates` precedent: regenerable scaffolding, not user edits. The
  confirmation in the companion is the durable record.
- **`settings_schema` ships in the same pass**, as a per-gallery configuration
  form reaching the plugin through `LIGHTVIEW_PLUGIN_SETTINGS` in its
  environment — no protocol change, the same mechanism as `LIGHTVIEW_JOB_TOTAL`.
- **`api_version: 2`**, not because any addition breaks a version 1 plugin —
  none do — but because a plugin declaring findings needs a host that renders
  them, and silently doing nothing is the failure the version exists to prevent.

Build order is settings form → findings backend (drivable from `curl` before any
UI exists) → the info-panel section → `known` verdicts and the source plugin.

Deliberately deferred: a dedicated review panel (build it when bulk review is a
real chore and it is clear what the chore is), and **cross-file groups**. A face
cluster's identity cannot live in any one companion, so it needs a
`plugin_groups` table, a merge/rename surface, and an answer to what happens
when a re-run reshapes a cluster the user already named. The source case has
none of that, which is why it goes first; the `label` shape exists so faces are
additive when they come.

## B. Thumbnails, cache, and serving

Three independent items with no dependency between them, so the order is by
what unblocks work elsewhere and by size. **B1 first** because it is the
smallest of the three and because [A4](#a4-taggers-decode-full-resolution-originals-on-the-local-path--mostly-done)
has made the `?fit=` route hot — every remote tagging job now pulls every image
through it at the plugin's declared edge, and concurrent taggers repeat the same
decode. It also absorbs the one half of A4 that was left undone. **B2 next**, a user-visible
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
well — [D1](#d1-250300-duplicated-lines-between-the-two-grids--done) extracted
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
