---
name: verify
description: Build, launch, and drive the real binary and the real SPA end-to-end without a display.
---

# Verifying LightView changes

Two recipes. Neither needs a display, a browser window, or a gallery you supply
— each builds its own fixtures with `ffmpeg`, starts the real binary, and drives
it.

```sh
npm ci && npm run build                    # dist/ first — see below
cargo build --manifest-path src-rust/Cargo.toml

bash .claude/skills/verify/drive.sh        # the binary, over curl
node .claude/skills/verify/grid.mjs        # the built SPA, in headless Chromium
```

Both print `N passed, M failed` and exit non-zero on a failure.

## The one thing that will waste your time first

**`dist/` must exist before any `cargo` command.** The SPA is embedded into the
*library*, so `cargo check`, `cargo test` and `cargo clippy` all fail without
it, not just the build. `npm run build` is the fix.

## What each covers

`drive.sh` walks both serving modes. Local mode: the random `127.x.x.x` bind,
the launch token redeemed once and refused twice, the dead-end 401 body, all
four tiers as WebP, ETag/304, Range/206, traversal, the SPA fallback, a video
thumbnail, the companion round trip, `set::` and `user::` filters, the watcher,
and a second launch finding the lock. Served mode: TLS, `/cert`, pairing, five
`Owner` refusals, a cross-site POST, and device revocation. Then plugins: a run
over a gallery containing a clip, a re-run that skips, a version bump that
re-tags, `--filter` scoping, and two plugin names that are paths.

`grid.mjs` covers what `tsc` cannot: cells placed by the justified layout,
thumbnails that decoded, the viewer opening on a click, Escape closing it, a
scroll, the settings sections, a plugin run started from the panel, and a
390px relayout — asserting no console error and no failed request throughout.

## Gotchas, learned the hard way

- **Playwright resolves a browser by revision**, and the revision the installed
  version wants is usually not the one the image ships — the default launch then
  fails telling you to run `npx playwright install`, which needs network.
  `grid.mjs` passes `executablePath: /opt/pw-browsers/chromium` instead.
- **`curl` normalizes `..` out of a path before sending.** A traversal check
  without `--path-as-is` tests nothing; it never reaches the route.
- **Never filter a failure out of the browser check** because it looks like
  noise. The missing favicon link survived a long time behind exactly that.
- **`--data-dir <root>` maps the three XDG roots** to
  `<root>/{cache,data,config}` — so a plugin installs to `<root>/data/plugins/`
  and `server.toml` is at `<root>/config/`.
- **Installing a plugin is copying a directory.** Its name must match the
  `name` in its `manifest.json`, or the scan rejects it.
- **A gallery is locked while it is open.** `lightview tag` against a gallery
  another process is serving refuses, by design.
- **Kill a server with `pkill -f 'lightview --serv[e]'`** — the bracket stops
  `pkill` matching its own shell command line. It does *not* save you if a later
  part of the same compound command contains the literal string; run it as its
  own call.
- **Startup log lines need `RUST_LOG=info`.**

## Adding a check

Append to the relevant script. Both keep a running `pass`/`fail` tally and print
a summary at the end, so a new check is a `check "what it means" "$got"
"$expected"` or an `ok`/`bad` pair. Say what the check *means* rather than what
it calls — `drive.sh` is also the shortest readable statement of the system's
externally visible contract, and it is read that way.
