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
```

## Development commands

- **Build:** `npm ci && npm run build` then
  `cargo build --manifest-path src-rust/Cargo.toml`
- **Frontend dev server:** `npm run dev`
- **Rust checks:** `cargo check` / `cargo test` (from `src-rust/`; `-- --exact`
  for a single test)
- **Frontend tests:** `npm test` — vitest, the layout and scroll arithmetic only
- **Run it:** `lightview <dir>` prints a URL and opens a browser at it

## Quality and verification

- **Rust linting:** `cargo clippy --all-targets --all-features` — clean, and
  expected to stay clean.
- **Frontend types:** `npx tsc --noEmit` from `src-solidjs/` — clean. There is
  no `npm run lint` script.
- **Rust doc links:** `cargo doc --no-deps --document-private-items` checks
  every intra-doc link in the headers; a broken one is a header that has drifted
  from its code.
- **`cargo fmt --check` FAILS** on most files — the tree has never been
  rustfmt-formatted. Do not run `cargo fmt` inside an unrelated change, and
  avoid `cargo clippy --fix` (its let-chain rewrites need a reformat you cannot
  scope).
- **`dist/` must exist before any `cargo` command.** Not just `build` — the SPA
  is embedded into the *library*, so `check`, `test` and `clippy` fail without
  it. This is the first thing to check when a fresh clone will not compile.
- **`libheif` >= 1.21** is a build dependency and Ubuntu 24.04 ships 1.17, so a
  Debian-family host needs a source build. `ffmpeg` is a runtime dependency for
  video. See [`README.md`](README.md#building).

**Driving the whole stack without a display** is two scripts, and it is the only
way to exercise the grid, which `tsc` cannot cover:

```sh
bash .claude/skills/verify/drive.sh    # the real binary, over curl
node .claude/skills/verify/grid.mjs    # the built SPA, in headless Chromium
```

## Reading the code

There is no wiki: each module's header states what it is responsible for, what
it is not, and the rules it upholds. Start at
[`src-rust/src/lib.rs`](src-rust/src/lib.rs) for the layers and the rule that
keeps them honest, and at [`src-solidjs/App.tsx`](src-solidjs/App.tsx) for the
shell and boot. `cargo doc --no-deps --document-private-items --open` renders
the Rust side with its cross-links.

## Things that are true and easy to get wrong

Each rule is explained in the header of the module named after it.

- **Trust is a property of the bind, never of the peer.** `0.0.0.0` includes
  `127.0.0.1`, so "is this peer local?" is the wrong question.
  → `server/listen.rs`
- **A process has one gallery, bound at startup.** There is no command that
  moves it to another folder. → `cli/mod.rs`
- **Paths on the wire are gallery-relative, percent-encoded per segment**, `/`
  left literal. → `server/routes.rs`
- **The cache is outside the gallery and fully derived.** A `format_version`
  bump deletes and rebuilds it, and that is the only mechanism for a *schema*
  change. A **reader** learning to extract something new is not one: it stamps
  its version in `gallery_meta` and clears `exif_read` for the rows it owns, so
  a library catches up without losing `date_added` and `last_viewed` — which a
  rebuild only restores where sidecars already exist. → `cache/db.rs`
- **Companion sidecars are the only durable data.** Never write one outside
  `modify_companion`, and never drop a field without keeping `extra`.
  → `companion/writer.rs`
- **A header read is recorded separately from what it found.** `exif_read`
  means "looked", not "found something" — a photo with no GPS and a screenshot
  with no EXIF block leave identical rows, so any gate phrased over the result
  columns either re-reads forever or excludes forever. → `services/gallery.rs`
- **Sort and group use `COALESCE(date_taken, mtime)`; filters use
  `date_taken`.** Ordering has to place every file somewhere; `date=2024` has
  to mean *taken* in 2024. → `sort/sorter.rs`
- **`Cargo.lock` is committed**, and `PKGBUILD` builds `--locked`. → `PKGBUILD`

## Engineering Principles

These are ordered; when they conflict, the earlier one wins. They share one
idea: **a system should be describable by simple rules with few exceptions** —
the architecture, each module, each function. A rule that cannot be stated
simply is a design that is not finished.

### 1. Settle the architecture before the code

Changes are planned in plan mode and approved before build mode touches code.
There are no planning documents in the repository; the plan is the approved
plan. Depth scales with the change — a sentence for a one-line fix, a paragraph
per question for a new subsystem — but no change skips it, because most bad code
is not badly written: it is correctly written in the wrong place, against the
wrong contract, or adds a concept that did not need to exist.

Every plan answers these, in order. The first three are the architectural ones:

- **Placement** — which module, and which way the dependencies point. A lower
  layer must not learn about a higher one ([`lib.rs`](src-rust/src/lib.rs)
  states the layers); if the change needs it to, the plan is wrong before it
  starts.
- **Contract** — what interface, wire format, schema, or invariant changes, and
  who is on each side of it. A format other installations read, or state a user
  cannot regenerate, is a much larger commitment than a cache, and is argued for
  as one.
- **Cost in concepts** — what a reader will have to hold in their head
  afterwards: a code path, a table, a knob, a second way to do something that
  already has one. Check the other direction first: **could the requirement be
  met by deleting something?** Name every new *except* — each is permanent until
  someone removes it.
- **Alternatives** — what else would work, and why each one lost.
- **Assumptions** — what is taken on faith, and what happens if it is wrong.
  Anything unmeasured is named as unmeasured.

Two checks, because both failures are common and quiet. **Second
implementation:** an abstraction, interface, or plugin point names its second
real consumer, or the concrete thing is written instead. **Seam:** a change that
is hard to place, or a process with no single function whose header can describe
it, usually means the seam is in the wrong spot — say so rather than working
around it.

Argue against the plan before presenting it; the `plan-reviewer` agent does this
with nothing but the plan. While building, **disclose every departure from the
approved plan**, and stop for approval if one changes the Placement or Contract
answer — those are the expensive ones to undo once code exists.

### 2. Simple rules, few exceptions

Complexity must be earned by a demonstrated need, not an anticipated one.

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

**Robust means the rule holds without anyone remembering it.** Enforce an
invariant at the one point every path must pass through — a single write
function, a type that cannot hold the bad state, a value fixed at startup —
rather than in each caller. `modify_companion` as the only way to write a
sidecar, and trust fixed by the bind rather than checked per peer, are the
pattern.

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

### 4. The code is the documentation

There is no wiki. Prose kept apart from the code drifts from it and nothing
checks it, so each rule is written once, on the code that enforces it.

**Every module opens with a header** stating what it is responsible for, what it
deliberately is not, and the rules it upholds for its callers.

**Every function has a header.** Its first sentence states what the function
does as one rule; anything after that is an exception, a reason, or an
obligation on the caller. A function that drives a process describes the
process — the steps, their order, and why that order. If the header will not
come out simply, the function does too much or sits in the wrong place: fix the
design, not the prose. Two kinds of function have their contract written
elsewhere already: a trait implementation inherits the trait's, and a test's
name is its header.

Beyond headers, a comment carries only what the code cannot:

- Why, not what — rationale, constraints, the rejected alternative and why it
  lost. A comment that restates the line below it is deleted.
- The failure a line prevents, stated in the present tense. How it was found and
  what the code used to do belong in the commit message.
- Invariants, caller assumptions, and units, frames, or coordinate conventions
  on anything numeric.
- Anything a future reader would be tempted to "fix" — say why it is that way.
- References point at code — Rust intra-doc links, which `cargo doc` checks —
  never at a prose file.

A header changes in the same commit as its code; one that contradicts its code
is a bug.

**Three prose files remain, each for a reader who does not read the code:**
[`README.md`](README.md) for people running and building LightView,
[`plugins/README.md`](plugins/README.md) for plugin authors, and this file for
the rules and the commands. This file may name a rule in one line; the
explanation lives in the header it points to.
