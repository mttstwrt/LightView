# AGENTS.md

## Development Commands
- **Full App (Dev):** `cargo tauri dev`
- **Frontend Dev:** `npm run dev`
- **Build (Production):** `npm run tauri build`
- **Rust Checks:** `cargo check` (from `src-tauri/`)
- **Rust Tests:** `cargo test` (use `-- --exact` for single tests)
- **Benchmarks:** `cargo bench --bench <name>` (from `src-tauri/`) or `npm run bench` (frontend)

## Quality & Verification
- **Rust Linting:** `cargo clippy --all-targets --all-features` (clean of errors; ~60 style warnings remain)
- **Frontend Types:** `npx tsc --noEmit`
- **Note:** There is no `npm run lint` script.
- **`cargo fmt --check` currently FAILS** on ~70 files — the tree has never been rustfmt-formatted. Do not run `cargo fmt` inside an unrelated change, and avoid `cargo clippy --fix` (its let-chain rewrites need a reformat you can't scope). See [`docs/build-and-verify.md`](docs/build-and-verify.md).
- **Build prerequisites:** `cargo check` fails in a build script without GTK/WebKitGTK and `libheif >= 1.21` (Ubuntu 24.04 ships 1.17 — needs a source build), and every Rust target additionally needs `dist/` to exist (`npm run build`) — the SPA is embedded into the library, not just read by the `lightview` binary. Same doc.

## Deeper Reference
[`docs/`](docs/README.md) — subsystem maps and cross-module invariants. Start at [`docs/architecture.md`](docs/architecture.md); each subsystem README states the invariants its callers must uphold, so read the one covering whatever you are about to change.

## Core Architecture
- **Boundary:** Frontend calls Rust via `src-solidjs/lib/ipc.ts` (canonical IPC).
- **Protocol:** Media/thumbnails are served via `lightview://` (custom URI protocol).
- **State:** Global state is in `src-tauri/src/lib.rs` (`AppState`) via `tauri::State`.
- **Lifecycle:** `commands/gallery.rs::open_gallery` manages provider registration, DB connection, and FS watching.

## Critical Implementation Notes
- **Cargo Features:** Default features include `gpu` and `custom-protocol`.
- **Linux Stability:** `main.rs` sets `GDK_BACKEND=x11` and `WEBKIT_DISABLE_DMABUF_RENDERER=1` for WebKit stability.
- **Performance:** `[profile.dev.package."*"] opt-level = 2` is intentional for image/DB workloads. Do not change.

## Engineering Principles

These are ordered. When they conflict, the earlier one wins.

### 1. Simplest thing that works

The simplest solution that satisfies the requirement is the correct solution.
Complexity must be earned by a demonstrated need, not an anticipated one.

- Prefer a function to a class, a class to a hierarchy, a hierarchy to a framework.
- Do not add abstraction layers, interfaces, or plugin points that have exactly one
  implementation. Write the concrete thing. Extract the abstraction on the second
  real use case, not the first hypothetical one.
- Do not add configuration options, feature flags, or parameters that were not asked
  for. Every knob is a permanent maintenance surface and a combinatorial test case.
- Do not add error handling for conditions that cannot occur, defensive checks for
  invariants the type system already guarantees, or retry logic where there is no
  transient failure mode.
- Prefer the standard library. Prefer an existing dependency to a new one. Justify any
  new dependency in terms of what it removes.
- Deleting code is a valid and preferred solution. If a change makes existing code
  unreachable, remove it in the same change; do not leave it commented out.

If you believe a more complex approach is warranted, state the specific requirement
that forces it before writing the code. If you cannot name the requirement, use the
simple version.

### 2. Performance is a design property, not a pass at the end

Think about cost at the level where it matters — algorithmic complexity, allocation
patterns, I/O and syscall boundaries, data layout and locality, work done per
iteration of a hot loop. Get these right the first time; they are expensive to change.

Do not micro-optimize. Do not restructure readable code for speculative gains, and do
not trade clarity for performance without a measurement showing the trade is real.
Unmeasured optimization is complexity without justification and violates principle 1.

When a fast path genuinely requires complexity, isolate it: keep the complex code in
one clearly marked place behind a simple interface, with a comment explaining the
measurement that motivated it.

### 3. Comments explain why

Comments carry the information that is not recoverable from the code itself.

- Explain rationale, constraints, and rejected alternatives — not mechanics. If a
  comment restates what the line does, delete it.
- Document non-obvious decisions: why this algorithm, why this ordering, why this
  buffer size, why this apparent inefficiency is deliberate.
- Document invariants, assumptions about caller behavior, and units/frames/coordinate
  conventions on anything numeric.
- Flag anything surprising. If a future reader would be tempted to "fix" the code,
  say why it is that way.
- Every module gets a module-level doc comment stating its purpose and boundaries.
  This is where per-file explanation lives — not in `docs/`.

### 4. Keep the docs current

`docs/` is a set of linked markdown pages describing how the system works and why.
It is part of every change, not a follow-up.

**Layout**

```
docs/
  README.md              entry point; map of the docs with links to each subsystem
  architecture.md        component map, data flow, dependency direction
  decisions/
    0001-<slug>.md       one decision per file, numbered, append-only
  <subsystem>/
    README.md            subsystem overview
    <topic>.md           only when a topic outgrows the README
```

**Granularity.** Pages describe subsystems, not files. Create a subsystem directory
when a component has its own responsibility and interface; do not create a page per
source file. Split a topic into its own page only when it is long enough that it
would dominate the subsystem README. Per-file explanation belongs in module doc
comments (principle 3).

**Every subsystem README covers:** what it is responsible for, what it explicitly is
not responsible for, its public interface, what it depends on, what depends on it,
and the invariants callers must uphold. State dependencies by name rather than
relying on directory nesting to imply them.

**Linking.** Use relative markdown links. Every page links back to `docs/README.md`
and to the subsystems it names. `architecture.md` and each subsystem README are hubs;
no page should be reachable only by browsing the filesystem.

**Decisions.** When a choice has alternatives worth recording, add
`docs/decisions/NNNN-<slug>.md` with: context, options considered, the choice, and
the consequences. Decision files are never edited after the fact. If a decision is
reversed, write a new one and add a superseding link to the old.

**Update rules.** In the same change, whenever you:
- add, remove, rename, or move a subsystem — update `architecture.md` and fix links
- change a data flow, interface contract, or file/wire format — update the affected
  subsystem READMEs on both sides of the boundary
- make a decision with real alternatives — add a decision file
- write code that contradicts something the docs currently state — fix the docs

Prose over bullet fragments. Do not paste code that will drift; link to it and explain
the shape. If a change makes a doc wrong, fixing it is not optional and not deferred.