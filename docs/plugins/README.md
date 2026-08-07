# Plugins

[← docs index](../README.md) · [architecture](../architecture.md)

**Responsible for:** the plugin manifest format, spawning a plugin as a
subprocess, and the NDJSON protocol spoken over its stdin/stdout
(`plugin/manifest.rs`, `plugin/runner.rs`, `commands/plugins.rs`).

**Not responsible for:** deciding *where* a plugin runs. That is
[`tagging/`](../remote/worker-tagging.md), which routes a job either to the
server's own in-process executor or to a paired `lightview-worker`. It is also
not responsible for writing results: `apply_plugin_output` hands tags to
[`companion/`](../companion/README.md), which owns the file format.

**Depends on:** `companion/` and `util::paths` for the plugin directory.
**Depended on by:** `tagging/`, `commands/plugins.rs`, and `lightview-worker`.

The protocol shape — a subprocess and a line of JSON, rather than an embedded
runtime — is [decision 0006](../decisions/0006-plugins-are-ndjson-subprocesses.md).
The rest of this page is the honest inventory of what that protocol can express
today and what a real extension host would need.

**Status:** design discussion / roadmap. Nothing here is built yet.
**Audience:** maintainers deciding how far to take LightView's plugin system.

---

## 1. What we actually have today

Despite the "plugin-extensible" framing, the plugin system is, in practice, an
**auto-tagger runner**. The full surface is two files — `src-tauri/src/plugin/manifest.rs`
and `src-tauri/src/plugin/runner.rs` — plus the command wrappers in
`src-tauri/src/commands/plugins.rs`.

How it works:

- A plugin is a directory with a `manifest.json` (`name`, `execution`, `capabilities`,
  `tag_prefix`, optional `ui`).
- The host spawns the plugin as a **subprocess** and speaks a streaming NDJSON
  protocol over stdin/stdout:
  - Request line: `{"action": "tag", "path": "/abs/path/img.jpg"}`
  - Result line: `{"path": "...", "tags": [...], "meta": {...}}` or `{"path": "...", "error": "..."}`
  - **Plugins MUST emit results incrementally** — consume requests as they arrive
    and emit each result as soon as it's ready, never buffer stdin to EOF before
    tagging. Remote hosts (`lightview-worker`) keep only a bounded number of
    downloaded files on disk and download more only as results come back, so an
    EOF-buffering plugin deadlocks any job larger than that bound. For pool/batch
    sizing decisions that used to need the full request list, the host advertises
    the expected request count in the `LIGHTVIEW_JOB_TOTAL` env var.
- `apply_plugin_output` writes the returned tags into the companion file under
  `tags.plugins[tag_prefix]`, and any `meta` object under `meta.plugins[tag_prefix]`.

What that means for extensibility:

- **One verb.** The host only ever sends `action: "tag"`. The `action` field is
  plumbed but never varies. `tag_prefix` is mandatory — the data model assumes the
  plugin's output *is* tags.
- **No UI extension.** `PluginUiConfig` has `settings_schema` and `context_menu_items`,
  but `settings_schema` is unused and the only context action that resolves is "tag".
  A plugin cannot add a view, a panel, a command, a filter operator, or a sort key.
- **The "daemon" mode does not exist.** Older docs referenced a long-running daemon
  plugin; there is no `daemon.rs`. Each run is a fresh subprocess (the plugin may batch
  internally across the NDJSON stream, which is how the ML taggers amortize model load).
- **`Wasm` execution is a stub.** `ExecutionConfig::Wasm` exists in the manifest enum
  but `run_plugin_stream` returns `WasmNotSupported`.
- **Capabilities are declarative, not enforced.** `ReadImage` / `NetworkAccess` are
  advisory. There is no actual sandbox around the subprocess yet.

So the honest summary: **a capability-scoped (in intent), out-of-process, data-in /
data-out tagging protocol.** That's a perfectly good shape — it just isn't a general
extension host, and the docs shouldn't imply it is.

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

## 6. Suggested first step

If we pursue this, start with **Track A**, because it is self-contained, low-risk, and
immediately useful, and it forces the "available vs. enabled" split that every later track
depends on. Track B follows naturally (the protocol already has the hooks). Track C is a
deliberate, separate project to schedule only if third-party views become a real goal.

Docs to keep in sync when any of this lands: `README.md` (Plugins section + opening
tagline) and `CLAUDE.md` (the `plugin/` module row and the Plugin-system design note).
