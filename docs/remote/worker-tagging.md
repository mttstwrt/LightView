# Worker-based remote tagging

[← docs index](../README.md) · [remote](README.md)

**Status:** implemented (2026-07-07; server-local execution, job pinning and
the plugin streaming contract added 2026-07-08; host-side input preparation,
video support and the self-healing file window added 2026-08-15).
**Problem:** the headless server (e.g. an Intel N100) can't run ML taggers, and
remote clients must never execute code on the server. The web client should
still be able to trigger and monitor tagging.

## Architecture at a glance

```
┌────────────┐  enqueue job / watch SSE   ┌──────────────────────┐
│ web client │ ─────────────────────────► │ lightview-headless   │
│ (browser)  │ ◄───────────────────────── │  · job queue (RAM)   │
└────────────┘  tagging-job / -workers    │  · worker registry   │
                                          │  · /media, /api/...  │
┌───────────────────┐  claim job, pull    └──────────┬───────────┘
│ lightview-worker  │  image bytes, push             │
│ (capable machine) │ ◄──────────────────────────────┘
│  · local plugins  │   apply_plugin_tags + update_tagging_job
│  · runs ML tagger │
└───────────────────┘
```

- A new **`lightview-worker`** binary runs on a capable paired machine. It
  pairs with the server like any device (`lv_device` cookie), pulls image
  bytes over HTTPS, runs the existing plugin subprocess protocol
  (`plugin/runner.rs` NDJSON) locally, and pushes tags back through the
  already-allowlisted `apply_plugin_tags` command. **The server never receives
  or executes code** — a job carries a plugin *name*; the worker only runs
  manifests installed under its own `data_dir()/plugins`.
- The server keeps a small **in-memory job queue** + **worker registry**
  (`src-tauri/src/tagging/`). The web UI enqueues jobs ("tag selected",
  "tag all untagged"); the worker polls to claim them and reports progress;
  web clients watch live via SSE.
- The server can also **run plugins itself**: an in-process executor
  (`tagging/local.rs`) registers in the same worker registry under the
  reserved id `local-server` and claims jobs like any worker — no HTTP, no
  downloads, files read in place. It only ever runs plugins installed under
  the *server's own* `data_dir()/plugins`, so the no-remote-code rule holds;
  with an empty plugins dir it never registers. This is the opt-in "run it on
  the server" choice for hosts that can afford it.
- When a plugin is offered by more than one worker (say, the server *and* a
  remote machine), jobs can be **pinned** to a specific worker at enqueue —
  the web UI shows one entry per place-to-run-it.

## Why these choices

- **In-memory queue, no persistence.** Tag writes are idempotent
  (`apply_plugin_output` replaces the prefix's tag list), and the flagship
  "tag all untagged" job targets the filter
  `NOT has::plugin.<prefix>` resolved at *claim time* — so a
  server restart loses only the queue entry, and re-enqueueing resumes exactly
  where it stopped. Persistence would buy one saved click at the cost of a
  cache.db schema and cross-restart stale-claim semantics.
- **Worker polls (every ~3 s) instead of consuming SSE.** One tiny LAN POST
  per 3 s is negligible; it avoids hand-rolled SSE parsing/reconnect in the
  worker, and the poll doubles as the lazy stale-job reaper. SSE job events
  exist anyway — for web clients.
- **Progress is not piggybacked on `apply_plugin_tags`.** Apply carries no
  job identity, and per-image plugin *errors* produce no writes at all.
  `update_tagging_job` is progress + heartbeat + the cancellation
  back-channel in one call.
- **Capabilities stay static.** `get_server_capabilities` is per-gallery
  policy; worker presence is dynamic and lives in `get_tagging_status` + the
  `tagging-workers` SSE event. With no live worker the web UI hides the
  tagging actions and shows a hint instead.
- **Desktop untouched.** The local `run_plugin_batch` path and its Tauri
  events stay as-is. The queue lives in `lightview_lib` wired through the
  HTTP allowlist, so a *desktop* host with remote access enabled serves
  workers automatically.

## Server state (`src-tauri/src/tagging/mod.rs`)

```rust
pub struct TaggingState { jobs: Vec<TaggingJob>, workers: HashMap<String, WorkerEntry> }

pub struct TaggingJob {            // serialized camelCase
    id: String,                    // uuid v4
    plugin_name: String, tag_prefix: String, display_name: String,
    target: JobTarget,             // Paths(Vec<String>) | Filter(String)
    state: JobState,               // Queued | Running | Done | Failed | Cancelled
    total: usize,                  // fixed at claim (0 while queued for filter jobs)
    completed: usize, failed: usize,
    claimed_by: Option<String>,    // workerId
    worker_name: Option<String>,
    pinned_worker: Option<String>, // only this workerId may claim (validated at enqueue)
    error: Option<String>,
    created_at: i64,
    updated_at: i64,               // last contact from the worker (liveness)
    progressed_at: i64,            // last time completed+failed actually moved
}

pub struct WorkerEntry { worker_id, worker_name, plugins: Vec<PluginInfo>, last_seen, busy_job, local: bool }
```

```rust
pub struct WorkerEntry {
    worker_id, worker_name, plugins: Vec<PluginInfo>,
    worker_version: Option<String>,   // the binary the machine is running
    last_seen, local: bool,
}
```

Constants: worker TTL **45 s** (worker announces every 15 s), job stall
**90 s** (worker updates at least every 10 s), no-progress **30 min**, last
**50** finished jobs retained. Stalled running jobs are requeued lazily inside
announce/claim/enqueue/status **and heartbeat** calls, so there is no reaper
task.

The heartbeat is on that list because of the one case where nothing else is: a
worker inside a running job stops announcing and stops claiming, so during
exactly the wedge `JOB_NO_PROGRESS_SECS` exists to catch, the heartbeat is the
only traffic arriving. Leaving it out meant the deadline was never evaluated
for the jobs that needed it.

**Versions travel with the announce.** `PluginInfo` carries the plugin's
`api_version` and the worker reports its own binary version, both surfaced in
the web UI's worker list. "What is that machine actually running?" used to be
unanswerable, which is how a rebuilt worker binary next to a year-old plugin
copy survived as a configuration —
[decision 0012](../decisions/0012-plugins-declare-the-contract-they-were-built-for.md).

**Liveness and progress are separate clocks, deliberately.** The heartbeat
refreshes `updated_at` unconditionally — it proves the worker *process* is
alive, not that the job is moving — because a tagger's first run legitimately
produces nothing for minutes while it downloads and loads its model. So a
worker that is alive but wedged would hold a job in `Running` forever, with no
error anywhere: it keeps the 90 s stall reaper permanently satisfied. Hence
`progressed_at`, advanced only when `completed + failed` actually increases,
and a 30-minute backstop that marks such a job **Failed** (not requeued —
requeueing just hands the same wedge to the same worker, and the job would
loop re-claiming itself forever). Tag writes are idempotent and filter targets
skip what's already tagged, so re-enqueueing resumes rather than redoes.

AppState additions (`src-tauri/src/lib.rs`):

```rust
pub tagging: Arc<tokio::sync::Mutex<tagging::TaggingState>>,
pub tagging_event_tx: broadcast::Sender<tagging::TaggingSseEvent>,   // channel(64)
```

`fs_change_tx` is untouched — it is typed and its `receiver_count()` feeds
the idle-backfill heuristic (`pipeline/idle.rs`).

## Protocol — new `/api/invoke` commands

All metadata-write tier (same trust as `apply_plugin_tags` /
`add_user_tag_batch`), behind the existing `lv_device` cookie auth. Args and
results are camelCase.

| Command | Args → Result | Notes |
|---|---|---|
| `worker_announce` | `{workerId, workerName, plugins}` → `{}` | registers/refreshes registry entry; broadcasts `tagging-workers`; the reserved id `local-server` is rejected so a remote device can't impersonate the in-process executor |
| `claim_tagging_job` | `{workerId, pluginNames}` → `TaggingJob \| null` | oldest queued job the worker can run, **with resolved `paths`**; skips jobs pinned to another worker; filter targets run `apply_filter_impl` at claim, intersected with the gallery's indexed images, videos and GIFs; sets Running; requeues stalled jobs first |
| `update_tagging_job` | `{jobId, workerId, completed, failed}` → `{cancelled: bool}` | progress + heartbeat; unknown job also returns `cancelled: true` (covers server restart / pruned job) |
| `complete_tagging_job` | `{jobId, workerId, succeeded, failed}` → `{}` | |
| `fail_tagging_job` | `{jobId, workerId, error}` → `{}` | |
| `enqueue_tagging_job` | `{pluginName, paths?: string[], filter?: string, workerId?: string}` (exactly one target) → `TaggingJob` | web UI entry point; `workerId` pins the job to that worker (validated: the worker must be connected and offer the plugin) |
| `cancel_tagging_job` | `{jobId}` → `{}` | queued → cancelled immediately; running → worker learns on next update and kills the plugin |
| `get_tagging_status` | `{}` → `{workers, jobs}` | web UI sync / re-sync after SSE lag |

`worker_announce` also accepts an optional `workerVersion`.
`update`/`complete`/`fail` verify `workerId == claimed_by` — an integrity
guard against a second worker stomping a job, not a security boundary (all
paired devices are equally trusted today; per-device scopes are future work).

Tag writes themselves go through the pre-existing `apply_plugin_tags`
(`{entries: [{path, tagPrefix, version, tags, meta?}]}`), which canonicalizes
and confines paths to the gallery root, writes companions, and re-indexes.

## SSE — `/api/events` additions

Two new event types alongside `fs-changed`:

- `event: tagging-job` — full job snapshot JSON on every state transition
  (enqueue, claim, progress update, complete/fail/cancel).
- `event: tagging-workers` — worker registry snapshot on announce/expiry.

Implemented as a second broadcast channel merged into the existing SSE
stream with `futures::stream::select`. A lagged subscriber just re-syncs via
`get_tagging_status`.

## `lightview-worker` binary

`src-tauri/src/bin/lightview-worker/{main,config,http,job}.rs` — same crate,
reusing `lightview_lib::plugin::{manifest, runner}`. Feature-gated so
desktop/headless builds never compile it:

```toml
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"], optional = true }
[features] worker = ["dep:reqwest"]
[[bin]]   name = "lightview-worker"   required-features = ["worker"]
```

- **CLI:**
  `pair --server https://host:8787 [--pin N] [--name X] [--yes] [--trust-new]`
  · `run [--plugins-dir <dir>] [--fit 1024] [--poll 3]`
  · `install <path> [--plugins-dir <dir>]` · `plugins` · `status`.
  `install` is how a plugin gets here — re-running it over the same source is
  the update, since the install directory is replaced. `plugins` lists what is
  installed with versions, and any the worker refuses to run. Released
  alongside the headless server
  ([decision 0014](../decisions/0014-ship-the-worker-with-the-release.md)), so
  a stale worker binary has something to be stale *against*.
  The PIN is minted host-side with the existing `lightview-headless pair`;
  the worker redeems it at `POST /pair/redeem` with
  `device_name: "worker:<name>"` and stores the cookie.
- **TLS = trust-on-first-use pinning.** The server cert is self-signed, so
  `pair` captures `sha256(end_entity_der)`, prints the fingerprint for
  confirmation, and pins it in config. Cert rotation (LAN-IP change — see
  `http_server/tls.rs`) produces a clear error telling the user to re-run
  `pair --trust-new` (re-pin, keep cookie).
- **Config:** `data_dir()/worker.toml` (exe-relative, mode 0600):
  `server_url, cookie, cert_sha256, worker_id, worker_name, fit_edge, poll_secs`.
- **Plugins:** the same `data_dir()/plugins/*/manifest.json` scan the desktop
  uses (shared `scan_plugins()`), then `check_api_version` against
  `RequestDelivery::Incremental`. A plugin declaring no `api_version` predates
  the streaming contract and is **refused at startup** with a message naming it
  and the fix, rather than tagging 64 images and hanging.
- **Job loop** (one job at a time):
  1. Background task announces every 15 s.
  2. Poll `claim_tagging_job` every `poll_secs`.
  3. On a job: spawn **one plugin subprocess for the whole job** via
     `runner::run_plugin_stream_channel` (so e.g. wd-tagger loads its model
     once), created in a per-job temp dir.
  4. Downloader task: one request per still, `input.video_frames` per clip.
     `GET /media/<encoded path>?fit=<edge>` for a still and
     `…?fit=<edge>&frame=i&frames=n` for a frame (WebP; PIL sniffs content so
     extension doesn't matter; HEIC arrives transcoded). `<edge>` is the
     plugin's declared `input.max_edge`, falling back to the worker's `--fit`
     when the manifest says nothing. Bounded by a `Semaphore(64)` on
     files-on-disk; feeds temp paths into the plugin's request channel;
     download failure resolves that part as failed. The client sets a 60 s
     **idle-read** timeout (resets on every byte, so a big slow download is
     fine) — without it a stalled connection parks the downloader forever while
     the job loop keeps heartbeating.
  5. Result loop: `PartTracker` maps each result back to its media item and
     merges an item's parts once all have resolved; batch 32 →
     `apply_plugin_tags` → `update_tagging_job`; delete temp files as results
     land. A 10 s tick sends a pure-heartbeat update (first-run model
     downloads can take minutes before any result) and sweeps abandoned
     requests. Progress counts **media items**, not requests: a five-frame clip
     is one unit of the job total.
  6. `{cancelled: true}` → kill the plugin, wipe the temp dir, resume polling.
     Stream end → flush + `complete_tagging_job`. Errors →
     `fail_tagging_job` + exponential backoff, keep looping.

## What the plugin receives

Not the file. Under `api_version: 1` the host decides a plugin's input, and the
worker's downloader is one of three implementations of the same policy
(`plugin/input.rs`; the other two are the in-process executor and the desktop's
batch command):

- A **still** is fetched at the plugin's declared `input.max_edge`, so a
  448-pixel model never pulls a 60-megapixel original across the LAN.
- A **video** becomes `input.video_frames` requests, each fetched from
  `?frame=i&frames=n`, which the *server* extracts and encodes. That is why the
  worker needs no ffmpeg. Shipping the clip whole was the alternative and was
  rejected: `?fit=` cannot resize a video, so 64 files on disk would be tens of
  gigabytes rather than a few hundred KB each.
- The host merges an item's parts back into one tag write — a union of the
  per-frame tag sets, plus a redone argmax for `rating:`. See
  [decision 0013](../decisions/0013-the-host-samples-video-frames.md).

## Plugin streaming contract

The downloader's files-on-disk bound (64) is released only as results land, so
**a plugin that buffers stdin to EOF before tagging deadlocks any job larger
than the bound** — the plugin waits for EOF, EOF waits for the downloader,
the downloader waits for results. (This is exactly how the original ML
taggers failed: they read the whole request list up front to size their GPU
instance pool.) The rules, also documented in [`plugins/`](../plugins/README.md)
and `plugin/runner.rs`:

- Plugins **must consume requests as they arrive and emit each result as soon
  as it's ready**. The bundled taggers use a stdin-reader thread feeding a
  bounded queue, with worker threads that flush partial batches whenever the
  queue runs dry.
- The host exports **`LIGHTVIEW_JOB_TOTAL`** (expected *request* count, so a
  clip counts as its frames) to the plugin subprocess, replacing the
  read-to-EOF sizing pattern.
- Plugins **must emit exactly one result per request** — including a
  `{"path": ..., "tags": [], "error": ...}` line for anything they can't
  process.
- A plugin that claims none of this — no `api_version` in its manifest — is
  **refused at worker startup**. That is the enforcement the rules above lacked
  for a year, and the reason a stale copy can no longer produce a silent hang.

## The file window is a window, not a job size

Each request in flight holds one of the 64 semaphore permits until its result
comes back. Two ways a permit used to leak, both of which stalled the job
permanently once 64 accumulated — the downloader blocks on `acquire_owned()`,
the plugin stops receiving stdin lines, and no result ever arrives again:

1. **The plugin never answers a request.**
2. **The plugin answers with a path the worker can't match.** Easy to hit by
   accident: a plugin that canonicalizes its input (`os.path.realpath`) under a
   symlinked `TMPDIR` echoes back a different string for the same file. Requests
   are therefore keyed on the temp **file name** — unique within a job — not the
   full path handed to the plugin, so directory-level rewriting is harmless. A
   result that still doesn't match logs `plugin result for unknown path`.

Because it only bit once 64 slots were gone, jobs under ~64 images completed
normally and larger ones hung — which read as "batches over 64 fail" and is why
this looked like a size limit rather than a leak.

**Permits now always come back.** `PartTracker::take_stale` abandons a request
once either rule fires, costing one failed image and returning its slot:

- **The plugin has answered `STALE_AFTER_RESULTS` (128) *other* requests since
  this one was sent.** A count, not a clock, deliberately: a slow CPU-only
  tagger legitimately leaves a file queued for a long time, so any wall-clock
  per-file deadline is either too tight for it or too loose to be useful. At
  most 64 requests are outstanding at once, so watching twice that many others
  get answered means this one was skipped.
- **Both the request and the plugin have been idle for `IDLE_RECLAIM` (5 min).**
  The count rule cannot fire at the *tail* of a job, where no further results
  arrive to count. This clears it, the sender drops, the plugin sees EOF, and
  the job finishes. It only applies once the plugin has answered something, so a
  first-run model download — minutes of silence with every slot full — is never
  mistaken for a wedge.

`NO_RESULT_STALL_SECS` (20 min) remains the outer backstop for a plugin that is
not working at all, and it is refreshed **only by results that matched**.
Counting unmatched ones kept it permanently fresh in exactly the case it exists
to catch.

Together with the server-side heartbeat reap, that is what makes "select several
thousand photos and walk away" a supported thing to do: the window slides,
misbehaviour costs one image at a time, and every remaining path to a permanent
hang has a timer that actually evaluates.

Measured against a headless server with a deliberately broken plugin. A
200-image job with a well-behaved tagger finishes in under 4 s. The same job
with a plugin that answers only *every other* request — 100 stranded slots
against a 64-slot window, the configuration that used to hang forever — parks at
64/200 as expected, reclaims all 64 slots on the idle rule at ~5 min, and
completes at **100 tagged / 100 failed** in 331 s. A plugin that drops a single
request needs no reclaim at all: the final drain catches it when the stream
ends, and the job reports 199/1 in five seconds.

So the worst realistic case costs one five-minute pause per window's worth of
skips, and the common case costs nothing. What it never does is stop.

## Server-local execution (`tagging/local.rs`)

The in-process executor gives small hosts the *option* of running taggers
themselves — e.g. the dependency-free `example-auto-tagger`, or a real one on
a desktop host serving remote access. It is the same queue protocol as
`lightview-worker` minus the HTTP leg:

- Started once per process by `lightview-headless serve` and the desktop's
  `enable_remote_access` (`ensure_started`, guarded by an `AtomicBool`).
- Rescans the server's `data_dir()/plugins` every 30 s; with no plugins it
  stays out of the registry entirely. Otherwise it announces as
  `local-server` / "Server (<hostname>)" with `local: true` (the announce
  doubles as the TTL keep-alive) and claims jobs in a poll loop.
- Jobs run through the same `plugin::input` preparation the worker uses — no
  downloads, but not "hand over the original" either: a still larger than the
  plugin's declared `input.max_edge` is decoded and scaled on the shared
  thumbnail pool, and a video is split into frames the same way. That is the
  half of the input policy that used to be missing, and it is why a server-local
  run of a 448-pixel model no longer decodes 60-megapixel files in Python.
  Results batch (32) into `apply_plugin_tags_impl` directly; a 10 s heartbeat
  flushes partial batches, picks up cancellation and sweeps abandoned requests,
  mirroring the worker binary.
- Remote devices cannot announce as `local-server` (rejected in the API
  dispatch), and the executor never runs anything a remote device sent —
  only manifests already installed on the server's disk.

## Web UI

- `lib/ipc.ts`: `enqueueTaggingJob`, `cancelTaggingJob`, `getTaggingStatus`
  (+ `TaggingJob` / `WorkerStatus` types). `applyPluginTags` already existed.
- New `stores/taggingStore.ts`: `workers`/`jobs` signals, `workerPlugins()`
  (deduped union of live workers' plugins) and `taggingActions()` — one
  runnable entry per (plugin, place-to-run-it): a plugin offered by a single
  worker gets one unpinned entry; offered by several (e.g. server + remote),
  one *pinned* entry per worker labeled "on <workerName>". Bridges the active
  job into the existing `pluginStore` toast, remembering the active `jobId`.
- `App.tsx`: `tagging-job` / `tagging-workers` listeners on the existing
  `EventSource`; `PluginToast` cancel calls `cancelTaggingJob(jobId)` when a
  worker job drives the toast.
- `ContextMenu.tsx`: on web (where `capabilities().plugins` is false), fetch
  `getTaggingStatus()` on open; render the plugins submenu from
  `taggingActions()` → `enqueueTaggingJob(name, {paths}, workerId?)`
  (batch-aware via the existing selection handling). Desktop path unchanged.
- `AutoTagPanel.tsx`: the web body of the **Auto-tagging** panel, reached from
  the command list ([`frontend/chrome.md`](../frontend/chrome.md)) — connected
  workers (the in-process executor gets a `server` badge), per-action
  "Tag untagged"
  (`filter: "NOT has::plugin.<tagPrefix>"`, pinned when the action is), job list
  with cancel, and a no-worker hint. No `type:image` clause: the host samples
  frames from videos now, and leaving it would have kept excluding them after
  the backend stopped. Each worker row shows its reported binary version. The panel's desktop
  body is the installed-plugin list instead; the two never coexist.

## Testing without ML

`plugins/example-auto-tagger` (dependency-free `python3`, `tag_prefix:
"example"`) exercises the full loop: headless server + worker on localhost,
enqueue via curl, assert `tagging-job` SSE transitions, companions gaining
`tags.plugins.example`, `apply_filter "NOT has::plugin.example"` → `[]`,
idempotent re-enqueue (claim resolves 0 paths → completes instantly), and
mid-run cancel killing the plugin. Put an `.mp4` in the gallery too: a clip's
companion should gain one merged entry with `video_frames_sampled` in its meta,
which is the difference between "videos are tagged" and "videos are silently
skipped" — the failure this loop shipped with for a year. See the headless
recipe in `CLAUDE.md`.

## Future work (out of scope here)

- Per-device scopes/roles (today all paired devices are metadata-write).
- Worker consuming SSE for instant job pickup instead of the 3 s poll.
- An in-browser tagger (onnxruntime-web) plugging into the same
  `apply_plugin_tags` + job/progress plumbing.
- Multiple concurrent jobs per worker (v1: one at a time).
