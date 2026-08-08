# 0005 — Remote command dispatch is a hand-written allowlist

[← docs index](../README.md) · [remote](../remote/README.md)

## Context

The desktop frontend reaches the backend through Tauri's `invoke()`, which can
call any registered `#[tauri::command]`. The same SPA bundle also runs in a
browser on the LAN, where it has no Tauri IPC and instead posts to
`/api/invoke`.

The registered command set includes operations that must never be reachable
from a phone on the network: copying and moving files anywhere on the host,
executing plugin subprocesses, and changing process-level render configuration.

Device pairing already decides *whether* a browser may talk to the server. It
says nothing about what it may then do.

## Options considered

**Expose every registered command,** relying on pairing as the only boundary.

**A denylist** — forward everything except an enumerated set of dangerous
commands.

**A `#[remote]` attribute** on the command functions, with the bridge generated
from it. Adding a command to the remote surface becomes a one-word edit.

**A hand-written match**, one arm per permitted command, spelling out the
argument shape locally.

## Decision

A hand-written match in `http_server/api.rs`. Anything not named there is 403,
even if the client forges the name.

Pairing is authentication; this is authorization, and collapsing the two would
mean any command added for the desktop is immediately reachable from the
network. A denylist inverts the failure mode in the wrong direction: forgetting
to add an entry silently *exposes* a command, whereas forgetting to add an
allowlist arm merely means a feature does not work remotely — noticed
immediately, and safe until it is.

The attribute-macro option was rejected on the same reasoning that recommends
it. Making the remote surface easy to extend makes widening it easy too, and
moves the audit from one readable file to a grep across every command module.
The match is ~500 lines and reads like boilerplate; **the verbosity is the
security property.** One arm per command, with the argument struct written out
next to it, is what makes the reachable surface auditable by reading it.

Two refinements fall out of the same principle:

- Delete-shaped commands sit behind an additional per-gallery
  `remote.allow_delete` flag, checked in **match guards** — before the arm that
  implements the command, not inside it, so the gate cannot be bypassed by a
  future edit to the body.
- `worker_announce` explicitly rejects the reserved id
  `tagging::local::LOCAL_WORKER_ID`, so a paired device cannot impersonate the
  server's own in-process executor in the worker registry.

## Consequences

- Every new remotely-available command needs two edits: the command itself and
  an allowlist arm. This is intended friction.
- The bridge deserializes into per-command structs with
  `rename_all = "camelCase"`, matching the JSON Tauri's `invoke()` already
  produces. That is what lets `lib/ipc.ts` swap transports without changing a
  call site.
- Desktop-only features are absent from the allowlist rather than hidden in the
  UI. The frontend asks `capabilitiesStore` what to render, but the enforcement
  does not depend on the client behaving.
- Refactoring the match into anything more compact should be treated as a
  security change, not a cleanup.
