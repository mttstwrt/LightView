# Building and verifying

[← docs](README.md)

## Prerequisites

| Need | Why |
|---|---|
| Rust (2024 edition) | the binary |
| Node 20+ | the SPA, which is **embedded into the binary** |
| `libheif` ≥ 1.21 | linked, for HEIC/HEIF |
| `ffmpeg` + `ffprobe` | subprocesses, for video thumbnails and frame extraction |
| `xdg-utils` | the browser launch in local mode |

**`libheif` ≥ 1.21 is the one that bites.** Ubuntu 24.04 ships 1.17, so a
Debian-family host needs a source build:

```sh
git clone --depth 1 --branch v1.21.2 https://github.com/strukturag/libheif
cmake -S libheif -B libheif/build -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_PREFIX=/usr/local -DWITH_EXAMPLES=OFF -DWITH_GDK_PIXBUF=OFF
cmake --build libheif/build --parallel && sudo cmake --install libheif/build
sudo ldconfig
export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib64/pkgconfig
```

Arch tracks a current release, which is most of why the container image and the
package both target it.

## The build order is not optional

```sh
npm ci
npm run build          # → dist/ — must exist before any cargo command
cargo build --manifest-path src-rust/Cargo.toml
```

The SPA is embedded into the **library**, not read from disk by the binary, so
**every** Rust target — `check`, `test`, `clippy`, `build` — fails without
`dist/`. It is the first thing to check when a fresh clone will not compile.

**`Cargo.lock` is committed.** This crate ships a binary, and the package
build, the container image and the release workflow all start from a clean
checkout — without the lock file each of them re-resolves every dependency,
and a semver-compatible upstream release can break a build with no change here
and nothing in git to bisect. `PKGBUILD` passes `--locked`, which fails
outright rather than quietly resolving something else, so a lock file that
drifts from `Cargo.toml` is a build error and not a surprise later.

## Checks

```sh
cargo test    --manifest-path src-rust/Cargo.toml --all-targets
cargo clippy  --manifest-path src-rust/Cargo.toml --all-targets --all-features
npx tsc --noEmit          # from src-solidjs/
```

All three are expected to be clean. `cargo fmt` has never been run over this
tree, so `--check` fails on almost every file; formatting it is its own change,
not something to fold into another one.

## Driving the whole stack with no display

Two recipes, both in `.claude/skills/verify/`. They build nothing and supply
nothing — each creates its own throwaway gallery with `ffmpeg`, starts the real
binary, and drives it.

```sh
bash .claude/skills/verify/drive.sh    # the binary, over curl
node .claude/skills/verify/grid.mjs    # the built SPA, in headless Chromium
```

### `drive.sh` — every route, both modes

Argument errors and exit codes, the administrative verbs, then `lightview <dir>`
on a random loopback address: the launch token redeemed once and refused twice,
all four tiers as WebP, ETag/304, Range/206, traversal refused in both
spellings, the companion round trip, `set::` and `user::` filters, the watcher
ingesting a file, and a second launch finding the lock and printing the running
URL. Then `--serve` over TLS: pairing, the certificate, every `Owner` command
refused, a cross-site POST refused, and a revoked device. Then a plugin run,
including a clip, a re-run that skips, and a manifest version bump that
re-tags.

### `grid.mjs` — the part `tsc` cannot see

Whether the justified layout produces cells, whether those cells fetch
thumbnails that actually arrive, whether the viewer opens, whether the event
stream connects, whether a plugin run started from the panel finishes, and
whether any of it logs a console error or a failed request — at desktop width
and again at 390px.

It launches the pre-installed Chromium **by path**. Playwright resolves a
browser by revision, and the revision a given version wants is often not the one
an image ships, so the default launch fails with "run npx playwright install" —
which is exactly what cannot happen in a sandbox with no network.

## Two ways a check can pass while testing nothing

Both of these happened here, and both are worth recognising:

- **A traversal check.** `curl` normalizes `/media/../../etc/passwd` to
  `/etc/passwd` before the request leaves, so it never reached the route under
  test. `--path-as-is` is what makes it a real test, and an encoded spelling
  belongs beside it because the two are rejected by different code.
- **A filtered-out 404.** The browser check excluded favicon requests as noise,
  which hid the fact that every page load produced one. Filtering a failure
  because it is familiar is how it stays.
