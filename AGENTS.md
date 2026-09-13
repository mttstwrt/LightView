# AGENTS.md

The single guidance file for this repository. `CLAUDE.md` is a symlink to it —
there is no second copy to drift.

LightView is a local media gallery: it opens a folder of images and videos,
indexes it into SQLite, generates thumbnails at four resolutions, and presents a
browsable grid plus a full-resolution viewer. The same process serves that
gallery to phones and laptops on the LAN. **One Rust binary, with a SolidJS
bundle compiled into it.**

**Stack:** Rust 2024 · axum · rusqlite (bundled SQLite) · rayon · tokio · image ·
fast_image_resize · libheif-rs — and TypeScript · SolidJS · Tailwind v4 · Vite 5.

## Layout

```
src-rust/      the crate: one library, one binary
src-solidjs/   the SPA, built into dist/ and embedded at compile time
plugins/       the example tagger, and the protocol a plugin author reads
docs/          how the system works, and why
```

## Development commands

- **Build:** `npm ci && npm run build` then
  `cargo build --manifest-path src-rust/Cargo.toml`
- **Frontend dev server:** `npm run dev`
- **Rust checks:** `cargo check` / `cargo test` (from `src-rust/`; `-- --exact`
  for a single test)
- **Run it:** `lightview <dir>` prints a URL and opens a browser at it

## Quality and verification

- **Rust linting:** `cargo clippy --all-targets --all-features` — clean, and
  expected to stay clean.
- **Frontend types:** `npx tsc --noEmit` from `src-solidjs/` — clean. There is
  no `npm run lint` script.
- **`cargo fmt --check` FAILS** on most files — the tree has never been
  rustfmt-formatted. Do not run `cargo fmt` inside an unrelated change, and
  avoid `cargo clippy --fix` (its let-chain rewrites need a reformat you cannot
  scope).
- **`dist/` must exist before any `cargo` command.** Not just `build` — the SPA
  is embedded into the *library*, so `check`, `test` and `clippy` fail without
  it. This is the first thing to check when a fresh clone will not compile.
- **`libheif` >= 1.21** is a build dependency and Ubuntu 24.04 ships 1.17, so a
  Debian-family host needs a source build. `ffmpeg` is a runtime dependency for
  video. See [`docs/build-and-verify.md`](docs/build-and-verify.md).

**Driving the whole stack without a display** is two scripts, and it is the only
way to exercise the grid, which `tsc` cannot cover:

```sh
bash .claude/skills/verify/drive.sh    # the real binary, over curl
node .claude/skills/verify/grid.mjs    # the built SPA, in headless Chromium
```

## Deeper reference

[`docs/`](docs/README.md) — subsystem maps and cross-module invariants. Start at
[`docs/architecture.md`](docs/architecture.md); each subsystem README states the
invariants its callers must uphold, so read the one covering whatever you are
about to change.

## Things that are true and easy to get wrong

- **Trust is a property of the bind, never of the peer.** `0.0.0.0` includes
  `127.0.0.1`, so "is this peer local?" is the wrong question.
- **A process has one gallery, bound at startup.** There is no command that
  moves it to another folder.
- **Paths on the wire are gallery-relative, percent-encoded per segment**, `/`
  left literal.
- **The cache is outside the gallery and fully derived.** A `format_version`
  bump deletes and rebuilds it, and that is the only mechanism for a *schema*
  change. A **reader** learning to extract something new is not one: it stamps
  its version in `gallery_meta` and clears `exif_read` for the rows it owns, so
  a library catches up without losing `date_added` and `last_viewed` — which a
  rebuild only restores where sidecars already exist.
- **Companion sidecars are the only durable data.** Never write one outside
  `modify_companion`, and never drop a field without keeping `extra`.
- **A header read is recorded separately from what it found.** `exif_read`
  means "looked", not "found something" — a photo with no GPS and a screenshot
  with no EXIF block leave identical rows, so any gate phrased over the result
  columns either re-reads forever or excludes forever.
- **Sort and group use `COALESCE(date_taken, mtime)`; filters use
  `date_taken`.** Ordering has to place every file somewhere; `date=2024` has
  to mean *taken* in 2024.
- **`Cargo.lock` is committed**, and `PKGBUILD` builds `--locked`.

## Engineering Principles

These are ordered. When they conflict, the earlier one wins.

### 1. Work out the architecture before you write code

Thinking is cheap in a plan and expensive once it's code. The point of planning
is not the document — it's forcing the structural questions to be answered while
they are still free to answer differently. Most bad code in a codebase is not
badly written; it is correctly written in the wrong place, against the wrong
contract, or adding a concept that did not need to exist.

**No code changes without a written, approved plan.** No size exception — depth
scales with the change instead: a one-line fix might be one sentence, a new
subsystem a paragraph per section. A short plan is the rule working, not a
workaround.

Answer these in writing, in this order, before presenting anything for approval.
The first three are the architectural ones and are the reason this principle is
first:

- **Placement** — where does this sit, and which way do the dependencies point?
  Name the module. Domain modules must not learn about their callers; if the
  change makes a lower layer aware of a higher one, that is the finding, and the
  plan is wrong before it starts.
- **Contract** — what interface, wire format, schema, or invariant changes, and
  who is on the other side of it? Name both sides. A format other installations
  read, or state a user cannot regenerate, is a much larger commitment than a
  cache and should be argued for as one.
- **Cost in concepts** — what does this add that a reader will have to hold in
  their head? A code path, a table, a config knob, a second way to do something
  that already has one. Count it honestly, and check the opposite direction
  first: **could the requirement be met by deleting something instead?** If a
  change adds a case that has to be explained with the word *except*, say so in
  the plan; every such case is permanent until someone removes it.
- **Alternatives** — what else could work, and why each one lost. "It was the
  first thing I thought of" is not a reason.
- **Assumptions** — what you are taking on faith, and what happens if it is
  wrong. Anything unmeasured belongs here, named as unmeasured.

Two checks worth running against your own plan, because both failures are
common and quiet. **The second-implementation test:** if the plan introduces an
abstraction, an interface, or a plugin point, name the second real consumer. If
there isn't one, write the concrete thing (principle 2). **The seam test:** if
the change is hard to place, that is usually the architecture telling you the
seam is in the wrong spot — say so rather than working around it.

**Record the plan** in chat if nothing about the change would trigger the update
rules in principle 5 (no new or changed subsystem, interface, or data flow).
Otherwise record it as `docs/_planning/<slug>/requirements.md` and `design.md`
(layout in principle 5).

**Review it — independently when you can.** A subagent or fresh session scoped
to just the plan catches what self-review won't; use one if available. Otherwise
review it yourself, adversarially: argue the plan is wrong and see what survives.
Fix what you find.

**Get explicit approval** before creating `tasks.md` or touching code. Revise and
re-present on feedback — silence isn't approval.

**Disclose every departure** from the approved plan when you report progress.
Stop and get approval if a departure leaves any of the five questions above
without a confident answer — especially Placement or Contract, since those are
the ones that are expensive to undo once code exists.

**On completion**, fold what's durable into the permanent docs (principle 5) and
delete `docs/_planning/<slug>/`, if one exists. Git history is the record of what
was tried.

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