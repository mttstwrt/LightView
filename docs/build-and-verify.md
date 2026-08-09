# Build and verify

[← docs index](README.md)

## System libraries

`cargo check` fails at the *build-script* stage without these — before any of
our code is compiled, so the error looks unrelated to whatever you changed.

| Library | Needed by | Notes |
|---|---|---|
| `gdk-3.0`, `webkit2gtk-4.1`, `libsoup-3.0` | `tauri` → `gdk-sys` | in every distro's repos |
| `libheif >= 1.21` | `libheif-rs 2` → `libheif-sys` | **not** in Ubuntu 24.04 (ships 1.17) |
| `ffmpeg` / `ffprobe` | `pipeline/video.rs` at runtime | without them every clip is a grey placeholder |

On Debian/Ubuntu:

```bash
apt-get install -y libgtk-3-dev libwebkit2gtk-4.1-dev libsoup-3.0-dev ffmpeg
```

`libheif` needs a source build on Ubuntu 24.04 — the packaged 1.17.6 does not
satisfy the `>= 1.21` the crate's build script asks pkg-config for:

```bash
curl -sSL -o libheif.tar.gz \
  https://github.com/strukturag/libheif/releases/download/v1.21.0/libheif-1.21.0.tar.gz
tar xzf libheif.tar.gz && cd libheif-1.21.0
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release \
  -DWITH_EXAMPLES=OFF -DBUILD_TESTING=OFF -DCMAKE_INSTALL_PREFIX=/usr/local
cmake --build build -j"$(nproc)" && cmake --install build && ldconfig

export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib/x86_64-linux-gnu/pkgconfig
```

The container image (`Dockerfile`) uses Arch, where `pacman -S libheif` is
current enough; this only bites on Debian-family hosts.

## Every Rust build needs `dist/`

Run `npm ci && npm run build` once before any `cargo` command. Two things read
`dist/` at *compile* time, so its absence fails the build before any of our code
is reached — a confusing error, since nothing about the Rust source is wrong:

- `tauri::generate_context!()` reads `frontendDist` (`../dist`) and fails the
  `lightview` binary with `error: proc macro panicked … this path doesn't
  exist`.
- `#[derive(RustEmbed)]` in `http_server::web_assets` embeds the same directory
  into the *library*, and so into `lightview-headless` and `lightview-worker`
  too: `folder '…/dist' does not exist`.

This used to be the desktop binary's problem alone. Embedding the SPA
(`docs/remote/README.md`) extended it to the library, which is the price of a
self-contained server binary. A rebuilt frontend does *not* need a Rust rebuild
to take effect in a debug build — rust-embed reads from disk there — but the
directory has to exist when the crate is first compiled.

## Current state of the quality gates

`AGENTS.md` lists `cargo fmt --check` and `cargo clippy --all-targets
--all-features` as the Rust gates. As of August 2026:

- **`cargo clippy`** — clean of hard errors, ~60 warnings remaining, all
  style-level (`collapsible_if` dominates, then `new_without_default`,
  `too_many_arguments`, a few `map_or`/`redundant_closure`).
- **`cargo fmt --check`** — **fails on ~70 files.** The tree has never been
  rustfmt-formatted, so this gate cannot pass today and running `cargo fmt`
  would produce a ~4,200-line diff touching nearly everything.

The practical consequence, and the reason it is written down here: **do not run
`cargo fmt` as part of an unrelated change**, and be careful with
`cargo clippy --fix`. The autofix converts nested `if`s into Rust 2024
let-chains but leaves the bodies at their original indentation, which only
looks right if you then run `cargo fmt` — which you cannot do in a scoped
change. Either fix those lints by hand with correct indentation, or leave them.

Formatting the tree is a worthwhile one-off (its own commit, no logic changes,
merged when no branches are in flight), after which the gate becomes real.

## Exercising the whole stack without a display

`lightview-headless` boots the same backend and axum server as the desktop app
with no WebKitGTK, so every route can be driven with `curl`. `CLAUDE.md` has
the canonical recipe; this is the short form plus the browser step.

```bash
npm ci && npm run build                          # dist/ — needed to compile at all
cd src-tauri && cargo build --bin lightview-headless

G=$(mktemp -d)                                   # throwaway gallery
python3 -c "
from PIL import Image
import sys
for i, c in enumerate([(220,60,60),(60,180,90),(70,110,230)]):
    Image.new('RGB', (800,600), c).save(f'$G/img{i}.jpg', quality=85)
"

./target/debug/lightview-headless serve "$G" --port 8799 &

PIN=$(./target/debug/lightview-headless pair "$G" | sed -n 's/Pairing PIN: //p')
# The cookie name carries a per-gallery suffix (`lv_device_<id>`), so match the
# base name rather than `lv_device=`. See docs/remote/README.md.
COOKIE=$(curl -sk -D- -o/dev/null -X POST https://localhost:8799/pair/redeem \
  -H 'Content-Type: application/json' -d "{\"code\":\"$PIN\",\"device_name\":\"test\"}" \
  | sed -nE 's/.*(lv_device[^=]*=[^;]+).*/\1/p')
```

Everything is HTTPS with a self-signed cert, so `curl -k` throughout. Useful
probes:

```bash
# Boot state. NB: groupBy is an internally-tagged enum — {"type":"none"},
# not the bare string "none", which is a common first mistake.
curl -sk --cookie "$COOKIE" -X POST https://localhost:8799/api/invoke \
  -H 'Content-Type: application/json' \
  -d '{"command":"get_boot_state","args":{"sortField":"name","sortOrder":"asc","groupBy":{"type":"none"}}}'

# A thumbnail at any tier — note the leading '/' of the absolute path is stripped
curl -sk --cookie "$COOKIE" -o /dev/null -w '%{http_code} %{size_download}B\n' \
  "https://localhost:8799/thumb/j$G/img0.jpg"

# The SSE change stream, then trigger the watcher
curl -skN --cookie "$COOKIE" https://localhost:8799/api/events &
cp "$G/img0.jpg" "$G/new.jpg"       # → event: fs-changed

# Auth boundary: no cookie must be 401
curl -sk -o /dev/null -w '%{http_code}\n' -X POST https://localhost:8799/api/invoke \
  -H 'Content-Type: application/json' -d '{"command":"get_gallery_info","args":{}}'
```

### Driving the real SPA

Chromium is preinstalled at `/opt/pw-browsers/chromium`. This is the only way
to verify grid changes, which `tsc` cannot cover:

```js
import { chromium } from 'playwright';
const browser = await chromium.launch({ executablePath: '/opt/pw-browsers/chromium' });
const ctx = await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 1280, height: 900 } });
// $COOKIE is already `name=value`; split it, since the name is per-gallery.
await ctx.addCookies([{ name: '<name from $COOKIE>', value: '<value from $COOKIE>', domain: 'localhost', path: '/' }]);
const page = await ctx.newPage();
page.on('pageerror', e => console.log('PAGEERROR', e.message));
await page.goto('https://localhost:8799/', { waitUntil: 'networkidle' });
await page.waitForTimeout(4000);
// Two <img> per item is correct: the thumbhash placeholder plus the real tier.
console.log(await page.$$eval('img', els =>
  els.filter(e => e.naturalWidth > 0).map(e => `${e.naturalWidth}x${e.naturalHeight}`)));
await page.screenshot({ path: '/tmp/grid.png' });
```

One expected console error in this setup: `An SSL certificate error occurred
when fetching the script.` — the service worker will not register against a
self-signed cert in headless Chromium. It does not affect the grid.

### Checking the cache directly

```bash
python3 -c "
import sqlite3; c = sqlite3.connect('$G/.lightview/cache.db')
print(c.execute('SELECT key, value FROM gallery_meta').fetchall())
for t in ['thumbnails','thumbnails_micro','thumbnails_justified','thumbnails_justified_high']:
    print(t, c.execute(f'SELECT COUNT(*) FROM {t}').fetchone()[0])
"
```

The fs-watcher only catches changes made *after* startup; a restart re-indexes.
Worker pairings live in the gallery's `cache.db`, so a new test gallery needs a
fresh `lightview-worker pair`.
