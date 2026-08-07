# 0006 — Plugins are subprocesses speaking NDJSON

[← docs index](../README.md) · [plugins](../plugins/README.md)

## Context

The feature that motivated a plugin system is ML auto-tagging: run a model over
the gallery and write the resulting tags into each file's companion. The useful
implementations of that are Python programs with large native dependencies
(PyTorch, ONNX Runtime) and multi-second model load times.

The host is a Rust process that must stay responsive while this runs, and must
not be taken down by a plugin that crashes, leaks, or hangs.

## Options considered

**Native dynamic libraries** (`dlopen` + a C ABI). Fastest possible calls, no
serialization. A plugin crash takes the host with it, and the ABI would have to
be stable across Rust releases.

**An embedded scripting runtime** (Python via pyo3, or a JS engine). Convenient
authoring, but the whole point is that these plugins bring their own heavyweight
native stack — embedding a Python interpreter does not remove the dependency
problem, it inherits it, and pins the host to one interpreter version.

**WebAssembly.** Real sandboxing and a stable ABI, which is genuinely the right
long-term answer for untrusted plugins. But no PyTorch, so it cannot run the one
workload the system exists for.

**A subprocess with a line-delimited protocol on stdin/stdout.**

## Decision

A subprocess. A plugin is a directory with a `manifest.json`; the host spawns it
and writes one JSON request per line to its stdin, reading one JSON result per
line from its stdout.

Process isolation is the decisive property: a plugin that segfaults, wedges, or
allocates without bound costs the host one child process. Dependency isolation
follows for free — the plugin brings its own interpreter and its own wheels, and
the host needs to know nothing about either. NDJSON was chosen over a
length-prefixed framing because it is trivially implementable from any language
with a `print` statement, and debuggable by piping the stream to a terminal.

**Results must be streamed, never batched.** This is the part of the protocol
that is easiest to get wrong and hardest to diagnose. A plugin must consume
requests as they arrive and emit each result as soon as it is ready — it must
never read stdin to EOF before starting work. Remote hosts
(`lightview-worker`) keep only a bounded number of downloaded files on disk and
fetch more only as results come back, so an EOF-buffering plugin deadlocks any
job larger than that bound. For plugins that legitimately need the total up
front to size a batch or a worker pool, the host advertises it in the
`LIGHTVIEW_JOB_TOTAL` environment variable instead.

`ExecutionConfig::Wasm` exists in the manifest enum and returns
`WasmNotSupported`. It is a placeholder for the sandboxing story, not a
half-built feature.

## Consequences

- One process spawn per run, not per image, so model load is amortized across
  the whole batch by the plugin itself.
- Serialization cost is one JSON line per image — negligible against a model
  inference, and the reason this shape would be wrong for a per-pixel operation.
- **Capabilities are declarative, not enforced.** `ReadImage` and
  `NetworkAccess` in the manifest are advisory; there is no sandbox around the
  subprocess. A plugin runs with the host's privileges. This is acceptable only
  because plugins are installed by hand into the host's data directory, and it
  is the reason plugin execution is absent from the
  [`/api/invoke` allowlist](0005-remote-invoke-is-an-allowlist.md).
- The protocol has exactly one verb. `action` is plumbed but never varies from
  `"tag"`, and `tag_prefix` is mandatory because the data model assumes the
  output *is* tags. A plugin cannot add a view, a panel, a filter operator, or a
  sort key. [`plugins/`](../plugins/README.md) is the honest inventory of what a
  real extension host would need.
