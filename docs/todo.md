# Open work

[← docs index](README.md)

Known gaps, in rough priority order. Anything here is understood but not done;
anything with enough shape to be designed belongs in a subsystem page instead.

## Two galleries on one host share one device cookie

Pairing a browser with a second gallery silently un-pairs it from the first.
`POST /pair/redeem` sets a fixed cookie name — `lv_device`, `Path=/`
(`http_server/auth_routes.rs`) — and cookies are scoped by host, *not* by port,
so two servers on `server:8787` and `server:8788` write to the same jar and the
second redemption overwrites the first. iOS home-screen web apps escape it only
because each gets its own storage container; a desktop browser has one jar and
loses a pairing every time.

The fix is to make the cookie name carry a per-gallery identifier — mint one
into `gallery_meta` on first open and suffix the name with it — so the two
servers stop colliding. Keep accepting a bare `lv_device` as a fallback and
re-issue it under the new name on the next request, so no already-paired device
has to pair again. See [`remote/`](remote/README.md).

## Nothing updates an installed plugin, and a stale one deadlocks silently

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

## Two ways a tagging job can still hang forever

Distinct from the stale-plugin deadlock above, which the 20-minute fail-out does
catch. These two paths defeat the timers themselves.

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

## `cargo tauri build` never builds `lightview-worker`

`npm run tauri build` produces no worker binary. That is not a build failure:
the bin declares `required-features = ["worker"]` (`src-tauri/Cargo.toml`), so
Cargo skips it unless the feature is on, and the Tauri bundle only carries the
app's own binary. It needs its own
`cargo build --release --bin lightview-worker --features worker`.

Decide whether that stays a documented separate step or the worker is added to
the release build and shipped alongside the app. Either way it should be
written down, because the quiet failure mode is a developer running a
months-old worker binary against a current server and debugging the wrong code.

## The tree is not rustfmt-formatted

`cargo fmt --check` fails on ~70 files, and formatting would be a ~4,200-line
diff. Until that lands as its own commit, the gate advertised in `AGENTS.md` is
aspirational and `cargo clippy --fix` is a trap: its let-chain rewrites leave
bodies at the old indentation and only look right after a `cargo fmt` you cannot
scope to one change. Worth doing when no branches are in flight, together with
the ~60 remaining clippy style warnings (`collapsible_if` dominates). See
[`build-and-verify.md`](build-and-verify.md).

## ~250–300 duplicated lines between the two grids

Structurally identical fetch loops, eviction, pruning, and URL versioning, with
small policy differences that make a naive merge unsafe.
[`frontend/grid-loading.md`](frontend/grid-loading.md) inventories the
differences and proposes a four-step extraction ordered smallest-risk-first.
Each step needs browser verification, not just `tsc`.

## `reindex_gallery` does not regenerate thumbnails

Re-indexing rebuilds the media and tag indexes but does not kick off background
thumbnail regeneration, so a re-index after a bulk external edit leaves stale
thumbnails until something else asks for them.

## Memory-pressure polling 403s on the web client

`lib/memoryPressure.ts` polls `get_memory_status`, which is not in the
`/api/invoke` allowlist and never has been. The poll is wrapped in a bare
`try/catch`, so every cycle fails silently and the viewer cache's
pressure-based eviction never engages in a browser — only on the desktop.

Adding it to the allowlist is the wrong fix: the command reports the *server's*
RAM, and sizing a phone's image cache from the host's free memory is
meaningless. The web client should either use its own signal
(`performance.memory`, `navigator.deviceMemory`) or not poll at all. Either way
the empty `catch` should stop hiding it.

Found by driving the SPA against `lightview-headless`; it is invisible from
`tsc` and from the Rust tests.

## Colour labels stop half way

The column is indexed, `color:` filters, the context menu picks one and
`ThumbnailCell` draws the dot — but the label is invisible everywhere else and
cannot order anything. `SortField` (`lib/types.ts`) has no `color` arm, so
neither `SortMenu` nor the backend sorter offers it, there is no grouping by
label, and `JustifiedGrid` never renders the dot the square grid does. A colour
you can set, search for and not see in the view you actually use is worse than
not having it.

Ordering needs a defined sequence — labels are a fixed list, so sort on that
list's index rather than the stored string, with unlabelled items last.

## Taggers decode full-resolution originals on the local path

Remote workers already pull `?fit=1024` (`bin/lightview-worker/config.rs`), so
this is only half missing: `tagging/local.rs` hands the plugin every original
path, which means a server-local run of a 448-pixel model decodes 60-megapixel
files in Python for nothing.

Rather than a second hard-coded number, let the plugin state the longest edge it
wants in `manifest.json` and honour it on both paths — as the `?fit=` value the
worker requests, and as a downscaled temp file the local executor hands over in
place of the original. Point those downloads at the cached `jm` tier where the
requested size allows it, instead of resizing from source each time; the
`?fit=` route has no coalescer (see Smaller items), so every concurrent tagger
request currently repeats the same decode. See
[`plugins/`](plugins/README.md) and [`pipeline/`](pipeline/README.md).

## Square thumbnails are generated for a view you may never open

`PREWARM_TIERS` is `[Standard, Justified]` (`pipeline/idle.rs`), unconditional
and per gallery. A gallery browsed only in the justified layout still pays the
full square-tier cost — gigabytes, on a large collection — for cells nobody
renders, and the same is true in reverse.

Enablement and generation should be one setting: a per-gallery list of enabled
views, with the idle worker pre-warming only the tiers those views ask for.
This is the cheap half of the modular-views work below and worth doing on its
own; it needs no plugin machinery, only a per-gallery setting and a `const`
that becomes a lookup. Existing rows for a disabled view should stay put rather
than being deleted — re-enabling the view must not re-decode the library — and
the tier's LRU budget already bounds the ones that are genuinely stale. See
[`pipeline/`](pipeline/README.md) and
[decision 0002](decisions/0002-two-families-of-thumbnail-tiers.md).

## Optional views, and a view API only once there are two of them

Beyond disabling the square grid and the map per gallery, the wanted view is an
infinite scrolling canvas: the top of the current sort in the centre, later
items spiralling outward, reusing the aspect-preserving justified tiers so it
costs no new thumbnails.

Build it as a native view first. Extract a view-module API when the canvas
exists and is the *second* real consumer of it, not before — the shape of the
contract is unknowable from one implementation, and
[`plugins/`](plugins/README.md) §3 already sets out why the core views
themselves should not be routed through it. Native dynamic libraries were
considered and rejected for this: layout runs in a webview, the LAN web client
cannot load a host `.so` at all, and an IPC-per-scroll arrangement puts a round
trip in the one loop that must not have one. Whatever lands must stay a single
Docker image, which also rules out a cargo feature per view.

## Plugin-driven UI, and naming what a plugin found

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

## Ship the SPA inside the binary

`lightview-headless` serves `dist/` from disk (`http_server/server.rs`), so the
Docker image and any manual deployment carry a directory that must stay in step
with the executable. `dist/` is 648 KB — embedding it with `rust-embed` costs
almost nothing and makes the binary self-contained. Keep the existing
`--web-root` path as an override so a dev build can still point at a live Vite
output without recompiling.

## "Open folder with LightView" on Linux

`lightview.desktop` already declares `MimeType=inode/directory` and
`Exec=lightview %f`, and `main.rs` already opens a directory passed as argv[1] —
but nothing installs the file, and `Exec` names a binary the build does not
produce: `tauri.conf.json` sets `productName` to `Gallery`. Fix the name
mismatch, install the desktop file and an icon from the bundle, and add a
`Desktop Action` so file managers offer "Open LightView here" on a folder's
background as well as "Open With" on the folder itself. Linux only for now.

## Absolute paths as primary keys

Every path-keyed row stores an absolute path, which is why `rebase_root` and
`infer_old_root` exist. Storing gallery-relative paths would delete that entire
mechanism, but it is a migration touching every table and every query, and the
current machinery works and is tested. Recorded as a structural observation, not
a recommendation — see
[decision 0001](decisions/0001-one-cache-per-gallery.md).

## Smaller items

- **A worker pool for image decode on the client.** A single decode worker is
  fine in practice — the browser parallelizes `createImageBitmap` — but a small
  pool would remove the JS orchestration bottleneck under burst load.
- **A virtual folder view.** Default hierarchy: plugin names and user folders at
  the top level, then a folder per tag inside. Entirely virtual — it would never
  copy or move files. Membership is *derived*, not curated: every node is a
  saved query over the existing index, which means no new tables, nothing to
  repair when files move or are deleted, and no second source of truth beside
  the companion files. Curated albums you drag files into were considered and
  set aside for that reason.
- **`too_many_arguments` on the thumbnail write paths.** `write_standard_row`
  and `write_tier_row` take nine positional arguments each, two of them adjacent
  `u32`s (`width`, `height`). Transposing them at a call site would compile and
  store a wrong aspect ratio. A small `ThumbRow` struct removes the class.
- **The `?fit=` resize route has no coalescer**, unlike the tier serve path, so
  concurrent requests for the same resize each do the work.
