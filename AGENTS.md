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

### 1. Plan before you build

Thinking is cheap in a plan and expensive once it's code — catch a wrong
assumption or a bad approach before anything is written, not after.

**No code changes without a written, approved plan.** No size exception —
depth scales with the change instead: a typo fix might be one sentence, a new
subsystem a paragraph per section. A short plan is the rule working, not a
workaround.

**Record the plan** in chat if nothing about the change would trigger the
update rules in principle 5 (no new or changed subsystem, interface, or data
flow). Otherwise record it as `docs/_planning/<slug>/requirements.md` and
`design.md` (layout in principle 5). Either way, answer in writing before
presenting for approval:

- **Approach** — what you're going to do.
- **Alternatives** — what else could work, and why each one lost.
- **Impact** — what this touches, what could break, what depends on it.
- **Assumptions** — what you're taking on faith, and what happens if it's wrong.

**Review it — independently when you can.** A subagent or fresh session scoped
to just the plan catches what self-review won't; use one if available.
Otherwise review it yourself, adversarially. Fix what you find.

**Get explicit approval** before creating `tasks.md` or touching code. Revise
and re-present on feedback — silence isn't approval.

**Disclose every departure** from the approved plan when you report progress.
Stop and get approval if a departure leaves any of the four questions above
without a confident answer.

**On completion**, fold what's durable into the permanent docs (principle 5)
and delete `docs/_planning/<slug>/`, if one exists. Git history is the record
of what was tried.

### 2. Simplest thing that works

Complexity must be earned by a demonstrated need, not an anticipated one. The
simplest solution that satisfies the requirement is correct.

- One function beats a class; one class beats a hierarchy; a hierarchy beats a
  framework.
- No abstraction, interface, or plugin point for a single implementation — write
  the concrete thing, and extract the abstraction on the second real use case.
- No config option, flag, or parameter that wasn't asked for. Every knob is a
  permanent maintenance surface and a test case.
- No error handling for conditions that can't occur, no defensive checks for
  invariants the type system already guarantees, no retries without a transient
  failure mode.
- Standard library over a new dependency; an existing dependency over a new one.
  Justify any addition by what it removes.
- Deletion is a valid fix. If a change orphans code, remove it in the same change
  — don't comment it out.

Complexity needs a named requirement. If you can't name the one that forces it,
write the simple version.

### 3. Performance is a design property, not a pass at the end

Think about cost where it's expensive to change later: algorithmic complexity,
allocation patterns, I/O and syscall boundaries, data layout, work per iteration
of a hot loop. Get these right the first time.

Don't micro-optimize, don't restructure readable code for speculative gains, and
don't trade clarity for performance without a measurement showing the trade is
real — unmeasured optimization is complexity without justification (principle 2).

When a fast path needs complexity, isolate it: one clearly marked place, behind a
simple interface, with a comment naming the measurement that motivated it.

### 4. Comments explain why

A comment carries what the code can't recover on its own.

- Rationale, constraints, and rejected alternatives — not mechanics. If a comment
  restates the line below it, delete it.
- Non-obvious decisions: why this algorithm, this ordering, this buffer size,
  this apparent inefficiency.
- Invariants, caller assumptions, and units/frames/coordinate conventions on
  anything numeric.
- Anything surprising — if a future reader would be tempted to "fix" it, say why
  it's that way.
- Every module gets a doc comment stating its purpose and boundaries. That's
  where per-file explanation lives, not `docs/`.

### 5. Keep the docs current

`docs/` describes how the system works now, and why. It's part of every change,
not a follow-up.

**Layout**

```
docs/
  README.md              entry point; links to every subsystem
  architecture.md         component map, data flow, dependency direction
  _planning/<slug>/       active feature plans (principle 1); not part of this tree
  <subsystem>/
    README.md             subsystem overview
    <topic>.md             only when a topic outgrows the README
```

**Granularity.** Pages describe subsystems, not files. Give a component its own
directory when it has its own responsibility and interface — not one page per
source file. Split a topic out of a README only when it would otherwise dominate
it. Per-file explanation belongs in module doc comments (principle 4).

**Every subsystem README covers:** responsibility, explicit non-responsibility,
public interface, dependencies (named, not implied by nesting), dependents, and
the invariants callers must uphold.

**Linking.** Relative markdown links only. Every page links back to
`docs/README.md` and to the subsystems it names — nothing should be reachable
only by browsing the filesystem.

**Update rules.** In the same change:
- add, remove, rename, or move a subsystem → update `architecture.md` and its
  links
- change a data flow, interface contract, or file/wire format → update the
  subsystem READMEs on both sides
- code contradicts what the docs say → fix the docs

Prose over bullet fragments. Link to code instead of pasting it — it will drift.
An outdated doc is a bug; fix it in the same change that caused it.