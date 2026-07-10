# Worker-based remote tagging — design

**Status:** implemented (2026-07-07; server-local execution, job pinning and
the plugin streaming contract added 2026-07-08).
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
  `type:image AND NOT has::plugin.<prefix>` resolved at *claim time* — so a
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
    created_at: i64, updated_at: i64,
}

pub struct WorkerEntry { worker_id, worker_name, plugins: Vec<PluginInfo>, last_seen, busy_job, local: bool }
```

Constants: worker TTL **45 s** (worker announces every 15 s), job stall
**90 s** (worker updates at least every 10 s), last **50** finished jobs
retained. Stalled running jobs are requeued lazily inside claim/status calls
— the worker's poll makes this run constantly, so there is no reaper task.

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
| `claim_tagging_job` | `{workerId, pluginNames}` → `TaggingJob \| null` | oldest queued job the worker can run, **with resolved `paths`**; skips jobs pinned to another worker; filter targets run `apply_filter_impl` at claim, restricted to `media_type = 'image'`; sets Running; requeues stalled jobs first |
| `update_tagging_job` | `{jobId, workerId, completed, failed}` → `{cancelled: bool}` | progress + heartbeat; unknown job also returns `cancelled: true` (covers server restart / pruned job) |
| `complete_tagging_job` | `{jobId, workerId, succeeded, failed}` → `{}` | |
| `fail_tagging_job` | `{jobId, workerId, error}` → `{}` | |
| `enqueue_tagging_job` | `{pluginName, paths?: string[], filter?: string, workerId?: string}` (exactly one target) → `TaggingJob` | web UI entry point; `workerId` pins the job to that worker (validated: the worker must be connected and offer the plugin) |
| `cancel_tagging_job` | `{jobId}` → `{}` | queued → cancelled immediately; running → worker learns on next update and kills the plugin |
| `get_tagging_status` | `{}` → `{workers, jobs}` | web UI sync / re-sync after SSE lag |

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
  · `run [--plugins-dir <dir>] [--fit 1024] [--poll 3]` · `status`.
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
  uses (shared `scan_plugins()`).
- **Job loop** (one job at a time):
  1. Background task announces every 15 s.
  2. Poll `claim_tagging_job` every `poll_secs`.
  3. On a job: spawn **one plugin subprocess for the whole job** via
     `runner::run_plugin_stream_channel` (so e.g. wd-tagger loads its model
     once), created in a per-job temp dir.
  4. Downloader task: `GET /media/<encoded path>?fit=1024` per item (WebP;
     PIL sniffs content so extension doesn't matter; HEIC arrives transcoded),
     bounded by a `Semaphore(64)` on files-on-disk; feeds temp paths into the
     plugin's request channel; download failure counts as failed and skips.
  5. Result loop: map temp → server path, batch 32 →
     `apply_plugin_tags` → `update_tagging_job`; delete temp files as results
     land. A 10 s tick sends a pure-heartbeat update (first-run model
     downloads can take minutes before any result).
  6. `{cancelled: true}` → kill the plugin, wipe the temp dir, resume polling.
     Stream end → flush + `complete_tagging_job`. Errors →
     `fail_tagging_job` + exponential backoff, keep looping.

## Plugin streaming contract

The downloader's files-on-disk bound (64) is released only as results land, so
**a plugin that buffers stdin to EOF before tagging deadlocks any job larger
than the bound** — the plugin waits for EOF, EOF waits for the downloader,
the downloader waits for results. (This is exactly how the original ML
taggers failed: they read the whole request list up front to size their GPU
instance pool.) The rules, also documented in `docs/pluginExtensibility.md`
and `plugin/runner.rs`:

- Plugins **must consume requests as they arrive and emit each result as soon
  as it's ready**. The bundled taggers use a stdin-reader thread feeding a
  bounded queue, with worker threads that flush partial batches whenever the
  queue runs dry.
- The host exports **`LIGHTVIEW_JOB_TOTAL`** (expected request count) to the
  plugin subprocess, replacing the read-to-EOF sizing pattern.
- The worker can't reliably distinguish a buffering plugin from a slow first
  run (model downloads), so it doesn't fail the job — but once the bound is
  full with zero results it logs a pointed warning naming the plugin.

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
- Jobs run via `runner::run_plugin_stream` on the original file paths — no
  downloads, no temp files. Results batch (32) into
  `apply_plugin_tags_impl` directly; a 10 s heartbeat flushes partial
  batches and picks up cancellation, mirroring the worker binary.
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
- `SettingsMenu.tsx`: web-only **Remote Tagging** section — connected
  workers (the in-process executor gets a `server` badge), per-action
  "Tag All Untagged"
  (`filter: "type:image AND NOT has::plugin.<tagPrefix>"`, pinned when the
  action is), job list with cancel, and a no-worker hint.

## Testing without ML

`plugins/example-auto-tagger` (dependency-free `python3`, `tag_prefix:
"example"`) exercises the full loop: headless server + worker on localhost,
enqueue via curl, assert `tagging-job` SSE transitions, companions gaining
`tags.plugins.example`, `apply_filter "NOT has::plugin.example"` → `[]`,
idempotent re-enqueue (claim resolves 0 paths → completes instantly), and
mid-run cancel killing the plugin. See the headless recipe in `CLAUDE.md`.

## Future work (out of scope here)

- Per-device scopes/roles (today all paired devices are metadata-write).
- Worker consuming SSE for instant job pickup instead of the 3 s poll.
- An in-browser tagger (onnxruntime-web) plugging into the same
  `apply_plugin_tags` + job/progress plumbing.
- Multiple concurrent jobs per worker (v1: one at a time).
