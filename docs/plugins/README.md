# Plugins

[← docs index](../README.md) · [architecture](../architecture.md)

**Responsible for:** the plugin manifest format and its two versions; spawning
a plugin as a subprocess and the NDJSON protocol spoken over its stdin/stdout;
what a plugin receives (scaled stills, sampled video frames) and how several
results become one; and installing a plugin into an install directory.
`plugin/{manifest,runner,input,install}.rs` plus `commands/plugins.rs`.

**Not responsible for:** deciding *where* a plugin runs. That is
[`tagging/`](../remote/worker-tagging.md), which routes a job either to the
server's own in-process executor or to a paired `lightview-worker`. It is also
not responsible for writing results: `apply_plugin_output` hands tags to
[`companion/`](../companion/README.md), which owns the file format.

**Depends on:** `companion/` for the tag namespace and media types,
[`pipeline/`](../pipeline/README.md) for decode/resize and ffmpeg frame
extraction, and `util::paths` for the plugin directory.
**Depended on by:** `tagging/` (both executors), `commands/plugins.rs`,
`http_server::routes` (the `?frame=` route), and `lightview-worker`.

**Invariants callers must uphold.** Run a plugin only after
`check_api_version` has passed for the delivery mode you are about to use — an
undeclared plugin on an incremental host deadlocks by construction. Register a
media item's full part count with `PartTracker` *before* producing any part, or
an item whose first frame answers early will be finalized short. And every part
must be resolved exactly once — by a result, a preparation failure, a staleness
sweep, or the final drain — or its item never completes and its file silently
loses its tags.

The protocol shape — a subprocess and a line of JSON, rather than an embedded
runtime — is [decision 0006](../decisions/0006-plugins-are-ndjson-subprocesses.md).
The rest of this page is the honest inventory of what that protocol can express
today and what a real extension host would need.

**Status:** sections 1 and 6 describe what exists; 2–5 are the analysis behind
the direction, kept because it is what stops it being re-litigated.
**Audience:** maintainers deciding how far to take LightView's plugin system.

---

## 1. What we actually have today

Despite the "plugin-extensible" framing, the plugin system is, in practice, an
**auto-tagger runner**. The surface is `src-tauri/src/plugin/` — `manifest.rs`
(the JSON a plugin declares itself with), `runner.rs` (the subprocess and the
NDJSON), `input.rs` (what the plugin receives and how several results become
one), `install.rs` (getting a plugin into an install directory) — plus the
command wrappers in `src-tauri/src/commands/plugins.rs`.

### The manifest

A plugin is a directory with a `manifest.json`: `name`, `display_name`,
`version`, `api_version`, `execution`, `capabilities`, `tag_prefix`, optional
`input`, optional `ui`.

Two of those are versions and they answer different questions. `version` is the
plugin's own, stamped into every companion file it writes. **`api_version` is
the host contract it was built against**, and it selects behaviour rather than
merely describing it — see
[decision 0012](../decisions/0012-plugins-declare-the-contract-they-were-built-for.md).

| `api_version` | The plugin gets | The plugin must |
|---|---|---|
| `0` (absent) | original file paths, every request written up front | do its own video handling; may read stdin to EOF first |
| `1` | stills scaled to `input.max_edge`; a video arrives as several extracted frames | consume requests as they arrive and answer every one |

A `0` plugin is refused by `lightview-worker`, where reading stdin to EOF is a
guaranteed deadlock, and runs normally on the desktop, where it always has. A
plugin declaring a version *newer* than the host is refused everywhere.

`input` states what the plugin wants to receive: `max_edge` (longest edge for
stills; `0` for the original, never upscaled) and `video_frames` (how many
frames to sample from a clip; default 5, clamped to 16). A 448-pixel model that
declares its size stops decoding 60-megapixel originals in Python, and stops
pulling them across a network.

### The protocol

The host spawns the plugin as a **subprocess** and speaks streaming NDJSON over
stdin/stdout:

- Request line: `{"action": "tag", "path": "/abs/path/img.jpg"}`
- Result line: `{"path": "...", "tags": [...], "meta": {...}}` or
  `{"path": "...", "error": "..."}`
- **Plugins MUST emit results incrementally** — consume requests as they arrive
  and emit each result as soon as it is ready, never buffer stdin to EOF before
  tagging. `LIGHTVIEW_JOB_TOTAL` carries the expected request count for
  pool/batch sizing decisions that used to need the full request list.
- **Exactly one result per request**, including an `error` result for anything
  the plugin cannot process. A skipped request is not free: it holds a disk slot
  in a remote worker's bounded window until the host gives up on it.

`apply_plugin_output` writes the returned tags into the companion file under
`tags.plugins[tag_prefix]`, and any `meta` object under `meta.plugins[prefix]`.

### What a plugin does *not* see

Under `api_version: 1`, the host decides the plugin's input and reassembles its
output — `plugin/input.rs`, and
[decision 0013](../decisions/0013-the-host-samples-video-frames.md):

- A **video** never reaches a plugin. The host samples frames, sends them as
  ordinary still requests, and merges the results: a union of the per-frame tag
  sets, which is exact for thresholded scores, plus an argmax redone over the
  per-frame `rating_scores` because a rating is one choice rather than a set.
- A **still larger than `max_edge`** arrives already scaled.
- A **single-part item passes through untouched** — same tags, same meta, same
  order. Merging is for clips.

All three drivers (`lightview-worker`, the server's in-process executor, the
desktop's `run_plugin_batch`) share that machinery, so a plugin cannot behave
differently depending on where a job happened to run.

### Installing

`plugin::install::install_from_path` copies a plugin directory (or wraps a bare
`.py`) into an install root, replacing what was there — "update" is the same
verb as "install". It skips virtualenvs and `__pycache__`, and rewrites a
venv-relative interpreter to an absolute one, which is what the bundled ML
taggers need. Reachable from the desktop's `install_plugin` command and from
`lightview-worker install`; `lightview-worker plugins` lists what is installed,
with versions and any the worker refuses.

### What that means for extensibility

- **One verb.** The host only ever sends `action: "tag"`. `tag_prefix` is
  mandatory — the data model assumes the plugin's output *is* tags. The class of
  plugin that does not fit is designed in
  [`findings-and-ui.md`](findings-and-ui.md).
- **No UI extension.** `PluginUiConfig` has `settings_schema` and
  `context_menu_items`, but `settings_schema` is unused and the only context
  action that resolves is "tag". A plugin cannot add a view, a panel, a command,
  a filter operator, or a sort key.
- **The "daemon" mode does not exist.** Older docs referenced a long-running
  daemon plugin; there is no `daemon.rs`. Each run is a fresh subprocess (the
  plugin may batch internally across the NDJSON stream, which is how the ML
  taggers amortize model load).
- **`Wasm` execution is a stub.** `ExecutionConfig::Wasm` exists in the manifest
  enum but `run_plugin_stream` returns `WasmNotSupported`.
- **Capabilities are declarative, not enforced.** `ReadImage` / `NetworkAccess`
  are advisory. There is no actual sandbox around the subprocess yet.

So the honest summary: **a capability-scoped (in intent), out-of-process,
data-in / data-out tagging protocol.** That's a perfectly good shape — it just
isn't a general extension host, and the docs shouldn't imply it is.

---

## 2. The core tension: two different kinds of "plugin"

The question "should the map / justified views be plugins?" runs into a fundamental
architectural fork. There are two plugin philosophies, and they are nearly opposites:

| | **Out-of-process plugins (what we have)** | **In-process UI plugins (custom views)** |
|---|---|---|
| Runs where | Separate subprocess | Inside the privileged webview |
| Trust | Low — scoped capabilities, killable | **High — same trust as the app itself** |
| Talks to host via | NDJSON over a pipe | Would need `invoke()` / direct API access |
| Data exchanged | Paths in, tags/meta out | The entire frontend data + selection + viewer model |
| Failure blast radius | Plugin dies, host fine | Can read the filesystem, trash files, hang the UI |

The current taggers are subprocesses — a real isolation boundary. A "view plugin" that
injects SolidJS components is the **opposite**: untrusted code running at maximum trust,
inside the webview that has IPC access to the Rust backend (filesystem, trash, move/copy).

Conflating these two into one "plugin system" is exactly where maintenance pain comes
from. They want different lifecycles, different security models, and different APIs.

---

## 3. Should the built-in views (grid / justified / map) become plugins?

**Recommendation: no.** Keep them native. The reasoning:

1. **Coupling.** To express the justified grid as a plugin, the host would have to expose
   — as a stable, versioned public contract, forever — the windowed item list, multi-tier
   thumbnail URLs, per-item aspect ratios, the selection model, open-viewer callbacks,
   scroll/timeline integration, and geo points. That's essentially the whole internal
   frontend API.

2. **You don't even get to delete the native code.** The built-in views legitimately need
   privileged, high-performance access. Even after building a view-plugin API, you would
   *not* route your own core views through it (perf + privilege), so you'd carry both the
   contract *and* the native implementations. Pure cost, no deletion.

3. **Security regression.** Today's plugins are isolated subprocesses. Turning core views
   into in-webview plugins moves trusted code into the most privileged context.

The framing trap is believing "plugin-extensible" requires the app's own views to be
plugins. They don't. VS Code's text editor isn't an extension; Blender's viewport isn't an
addon. **Core stays native; plugins are additive.**

---

## 4. Recommended path

Three independent tracks, roughly in increasing cost. The first two make the
"plugin-extensible" claim honest at low risk; the third is the real custom-display bet
and should only be taken if third-party views are a genuine roadmap goal.

### Track A — Per-gallery enablement & config *(do this first; clearly worth it)*

The most clearly-good idea, and largely orthogonal to everything else. Today plugins are
global. Make **enablement and configuration** per-gallery, while keeping plugin **code**
global.

Separate two concepts that "install per gallery" wrongly conflates:

- **Available** — the plugin binary/code. Stays **global** (one install, one update path;
  installing taggers per-gallery means duplication and N-place upgrades).
- **Enabled + configured** — lives **per-gallery**, alongside the `settings.toml` you
  already persist in each gallery's `.lightview/` directory.

Concretely:

- Add an `enabled_plugins` list (and a per-plugin settings blob) to the gallery's
  `.lightview/` config — wire it through the existing per-gallery settings mechanism
  (`settingsStore` / `commands/settings.rs`).
- `list_plugins` returns *available* plugins; a new flag/field marks which are enabled
  for the open gallery; `run_plugin*` refuses disabled plugins.
- This finally activates `PluginUiConfig.settings_schema`: render it as a per-gallery
  settings form.

Result: a photo library enables `wd-tagger` + (later) `map`; a meme folder enables
neither. Small change, low risk, fits the existing model.

### Track B — Broaden the plugin verbs *(moderate cost; makes the claim honest)*

The protocol already carries an `action` field and a `meta` passthrough — they're just
unused beyond `tag`. Turn the single hardcoded verb into a small dispatched set so plugins
can do more than tag:

- `tag` — current behavior.
- `enrich` — write structured metadata (rating, notes, color label, or arbitrary
  `meta.plugins[...]`). The `meta` channel already exists; formalize what the host accepts
  and how it maps onto the companion schema.
- `action` — a user-triggered, file-scoped operation surfaced via
  `ui.context_menu_items` (the field is already there). Output is a result/notification
  rather than tags. This is what makes "right-click → run my plugin" real for non-taggers.

Keep `tag_prefix` required only for tagging verbs; make it optional otherwise. Each new
verb is an additive arm in `run_plugin_stream` + `apply_plugin_output`, so the protocol
and isolation model don't change — only the host's dispatch widens.

While here, decide the fate of the two stubs:

- **Capabilities:** either enforce them (real sandbox — seccomp/landlock on Linux,
  network namespacing) or downgrade the docs to "advisory." Don't leave them implying a
  guarantee that isn't there.
- **WASM:** either implement it (a `wasmtime` host gives real in-process sandboxing with
  enforced capabilities — a genuinely good fit for untrusted plugins) or remove the
  variant. A stub that errors is worse than an honest absence.

### Track C — Sandboxed view / panel surface *(high cost; only if custom displays are a real goal)*

This is the *legitimate* way to get third-party custom displays — the VS Code-webview /
Obsidian model — and it is a **separate, additive** surface, not a rewrite of the core:

- A view plugin ships HTML/JS/CSS that runs in a **sandboxed iframe** (CSP-locked, no
  direct `invoke()`).
- It communicates with the host *only* via a narrow, **versioned `postMessage` API**:
  - host → plugin: "here are items N–M (paths, thumb URLs at tier T, aspect ratios,
    selection state)"
  - plugin → host: "open viewer at index K", "set selection", "request more items"
- The iframe sandbox + explicit message protocol *is* the security boundary — it restores
  the isolation property the in-webview core lacks.

Cost is real: you must design, document, and version the message protocol and a plugin
lifecycle (mount/unmount, error handling, resource limits). Take this track only if
third-party views are something you'll actually invest in and maintain — not aspirationally.

---

## 5. Does this make the app better, or just harder to maintain?

| Change | Verdict | Maintenance cost |
|---|---|---|
| **A. Per-gallery enable/config** (code stays global) | **Do it** — clearly better; fits existing per-gallery settings | Low |
| **B. Broaden verbs** (`tag` → `tag`/`enrich`/`action`) | **Do it** — makes "plugin-extensible" honest; additive to the protocol | Moderate |
| Resolve the WASM / capability stubs | **Do it alongside B** — stop shipping stubs that imply guarantees | Low–moderate |
| **C. Sandboxed iframe view surface** | Only if third-party custom displays are a real goal | High |
| **Rewrite core views as plugins** | **Don't** — security regression, perf loss, freezes the whole internal API, and you keep the native code anyway | High, negative value |

The cheapest way to make the tagline true is **not** a view system — it's Tracks A + B:
let plugins be enabled per-gallery and do more than tag. Custom displays (Track C) are a
separate, larger bet with a different security model; pursue it deliberately, not as a
side effect of "making plugins better."

---

## 6. Chosen direction

The tracks above are the analysis; this is what was actually decided, recorded
here so the next reader does not re-litigate it. Open items are tracked in
[`../todo.md`](../todo.md).

**Per-gallery enablement first (Track A), extended to cover the built-in
views.** Enabling a view and generating its thumbnails become the same setting,
so a gallery browsed only in the justified layout stops paying for square
tiers. This is the whole near-term answer to "the square grid costs gigabytes I
don't need" and it requires no plugin machinery.

**Plugin UI is host-rendered and declarative, not an iframe.** Plugins describe
panels, forms and actions as JSON in the manifest — `settings_schema` made real
— and the host renders them in Solid. Track C's sandboxed iframe remains the
fallback for displays a schema cannot express, but it is not the default
mechanism: it is a versioned message protocol and a plugin lifecycle to
maintain forever, and the motivating cases do not need one. Those cases —
naming recognised faces, and confirming which of several candidates is an
image's original source — both need a *host* surface where a person resolves
what a plugin found, which any plugin emitting such findings can feed. The
shape of that is designed in [`findings-and-ui.md`](findings-and-ui.md).

**Plugins declare the host contract they were built for.** `api_version` in the
manifest, and it is not documentation: it selects what the plugin receives, and
it is what lets the host change that without breaking a plugin copied into an
install directory a year ago. See
[decision 0012](../decisions/0012-plugins-declare-the-contract-they-were-built-for.md).

**Videos are the host's problem, not the plugin's.** The host samples frames and
merges the results, so `is_video`/`predict_video`/ffmpeg left every bundled
tagger. See [decision 0013](../decisions/0013-the-host-samples-video-frames.md).

**New views are built native, and there is no view-module API.** Superseded
what this paragraph used to say ("waits for a second consumer") — see
[decision 0008](../decisions/0008-no-view-module-api.md). The API was wanted so
that an unused view would cost nothing, and that turned out to be a bundling
question rather than a contract question: per-gallery enablement plus a dynamic
`import()` on the views that carry their own libraries delivers it with no
public surface. The map is the only such view — 153 kB of a 445 kB bundle — and
is already split out; the canvas and the virtual folder view reuse machinery the
main bundle carries anyway. Section 3 above is why the contract would have been
expensive; 0008 is why it is not needed.

**Native `.so`/`.dll` view plugins are rejected.** Layout runs in a webview, and
the LAN web client cannot load a host dynamic library at all, so a dylib view
would exist on the desktop only. Handing the host layout results over IPC
instead keeps the browser working but puts a round trip inside the scroll loop.
A cargo feature per view was also rejected: it gives up runtime availability and
multiplies the single Docker image the project ships. Both are recorded in
[decision 0008](../decisions/0008-no-view-module-api.md) alongside the option
that was taken.

## 7. Suggested first step

If we pursue this, start with **Track A**, because it is self-contained, low-risk, and
immediately useful, and it forces the "available vs. enabled" split that every later track
depends on. Track B follows naturally (the protocol already has the hooks). Track C is a
deliberate, separate project to schedule only if third-party views become a real goal.

Docs to keep in sync when any of this lands: `README.md` (Plugins section + opening
tagline) and `CLAUDE.md` (the `plugin/` module row and the Plugin-system design note).

## 8. Plugin output that is not tags

The one verb is the real limit, and the plugins that hit it are not exotic:
recognising faces and finding an image's original source both produce
*candidates for a person to confirm* rather than facts. That needs a result kind
that is not a tag, a host screen to resolve it on, and a way for the verdict to
reach the plugin's next run.

[`findings-and-ui.md`](findings-and-ui.md) is the design, and it is settled
enough to build from. The short version: a **finding** takes one of three
host-drawn shapes (`choice`, `confirm`, `label`) declared in the manifest; it is
resolved in the viewer's info panel, reached by a new `pending::plugin.<name>`
filter rather than a dedicated queue; a confirmed answer becomes an ordinary
`tags.user` entry plus provenance in `meta.plugins`, so no companion schema
change is needed; and prior verdicts ride back to the plugin on its next
request so it stops re-asking. `settings_schema` is rendered in the same pass as
a per-gallery configuration form — the one place a real schema *is* interpreted
generically, and [decision 0015](../decisions/0015-plugin-ui-is-fixed-shapes-not-a-declared-layout.md)
is why it is the exception. Tracked as A7 in [`../todo.md`](../todo.md).
