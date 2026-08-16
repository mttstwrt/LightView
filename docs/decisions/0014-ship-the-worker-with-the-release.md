# 0014 — Ship `lightview-worker` with the release

[← docs index](../README.md) · [build and verify](../build-and-verify.md) · [worker tagging](../remote/worker-tagging.md)

## Context

`cargo tauri build` produces no worker binary, and that is not a build failure:
the bin declares `required-features = ["worker"]`, so Cargo skips it unless the
feature is on, and the Tauri bundle only carries the app's own binary. It needs
its own `cargo build --release --bin lightview-worker --features worker`.

The quiet consequence is a developer — or a user — running a months-old worker
against a current server and debugging the wrong code. That is the same class of
problem as an install directory nobody updates
([0012](0012-plugins-declare-the-contract-they-were-built-for.md)), one level
down: the plugin's version and the binary's version are the two halves of "what
is actually running on that machine", and neither was visible.

The release workflow already builds and uploads a standalone
`lightview-headless` tarball for exactly the audience that needs a worker, so
the marginal cost of a second binary is one `cargo build` against an already
warm cache.

## Options considered

**Document it as a separate step.** Zero infrastructure, and it guarantees no
two machines run the same build. It also makes the version the worker now
reports on announce far less useful: knowing a worker is on v0.1.0 only helps if
there is a v0.1.1 to point someone at.

**Bundle it inside the desktop installer.** Puts the binary where it is least
likely to be wanted — the machine running the desktop app is the one that does
*not* need a worker, since it can run plugins directly. It would also inflate
every installer with a binary most users never invoke.

**Build it in the container image.** The image is the headless server, and the
whole premise of the worker is that the server is too weak to run taggers. A
worker inside the server image is the configuration the feature exists to avoid.

## Decision

`release.yml` builds `lightview-worker` alongside the headless server and
uploads it as its own `lightview-worker-<tag>-<os>.tar.gz`, with
`--no-default-features --features custom-protocol,worker` — no GPU stack, no
webview. It downloads prepared frames from the server and shells out to a
plugin, so it needs neither.

A separate archive rather than one combined download: the two run on different
machines by design, and pairing them in one tarball implies otherwise.

Ordinary development still uses the documented `cargo build --bin
lightview-worker --features worker`; this decision is about what a *user* can
obtain without a Rust toolchain.

## Consequences

- The version a worker announces becomes actionable — there is a published build
  to compare it against and to update to.
- The release job gains a Rust build. It shares the workspace cache with the
  headless build immediately before it, so the cost is close to link time.
- Nothing in CI *tests* the worker binary. It compiles under the release feature
  set and is exercised by hand against `lightview-headless` (see
  [`build-and-verify.md`](../build-and-verify.md)); an automated end-to-end run
  of the job queue would be worth having and is not part of this.
- The `worker` feature stays opt-in for local builds, so a routine
  `cargo check`/`cargo build` is unchanged. `--all-features` in the clippy gate
  already covers it.
