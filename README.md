# LightView

A local media gallery. Point it at a folder of images and videos: LightView
indexes it, generates thumbnails, extracts metadata, and gives you a browsable
grid and a full-resolution viewer — in a browser on the same machine, or on
phones and laptops over the LAN.

One Rust binary, with a SolidJS bundle compiled into it.

```
lightview ~/photos                 open a gallery, and launch a browser at it
lightview --serve /mnt/nas/photos  serve it over the LAN, with pairing and TLS
lightview tag ~/photos --plugin wd-tagger
```

## What it does

- **A justified grid** over any local folder, virtualized, with a resolution
  ladder that trades sharpness for latency mid-scroll and upgrades off-screen.
- **A full-resolution viewer** with keyboard and touch navigation, ratings,
  tags, and an info panel.
- **A filter language** over tags, sets, ratings, dates, dimensions, file size,
  media type and colour labels, compiled into the same query the sort orders.
- **Companion sidecars** holding tags, ratings, notes and location, written
  under a lock so two machines can edit one gallery.
- **Auto-tagging plugins** — a subprocess speaking newline-delimited JSON,
  run locally or from the machine that has the GPU.
- **Duplicate detection** by perceptual hash, with a merge that folds a group
  onto one keeper.
- **Access from a phone**, with device pairing, an optional password, uploads,
  and a self-signed certificate you can install.

**Images:** JPEG · PNG · WebP · AVIF · HEIC/HEIF · BMP · TIFF · GIF
**Videos:** MP4 · WebM · MKV · MOV · AVI · M4V

HEIC/HEIF is transcoded on the fly when served at full resolution.

## Two ways to run it

### Serve a gallery over the LAN

```sh
cp docker-compose.yml .            # point the gallery mount at your folder
docker compose up -d --build
docker compose exec lightview lightview pair --data-dir /state
```

That prints a six-digit PIN. Open `https://<host>:8443/pair` on the phone and
enter it. Set `--tls-san <your LAN address>` in the compose command first, or
the certificate will not cover the address the phone actually dials.

### Open a gallery locally

```sh
makepkg -si                        # Arch; see PKGBUILD
lightview ~/photos
```

It binds a random loopback address, prints the URL, and opens a browser at it.
The same package carries `lightview tag`.

Everything else — a password, revoking a device, trimming the cache — is a verb:

```
lightview pair                     mint a one-time pairing code
lightview devices [revoke <id>]    list or revoke paired devices
lightview password [--clear]       set the gallery password, read from stdin
lightview cache [--prune]          show or trim the derived-cache directory
```

## Filtering

The search box takes a small query language:

```
vacation                          any namespace
user::vacation                    one namespace
set::kellys-comic                 set membership
plugin.wd::beach                  a plugin's tags
"two words"                       a tag containing a space
NOT plugin.wd::indoor             negation
rating>=4                         rating
date=2024   date>=2024-01-01      capture date — a year, a year-month, or a day
added<=2024-06   viewed>=2023     date added, last viewed
width>=1920   height<=1080        pixel dimensions
size>=10mb   size<=500kb          file size (b/kb/mb/gb)
type:video                        media type
color:red                         colour label
has::user   has::set              namespace existence
has:geo     missing:geo           whether coordinates exist at all
(a OR b) AND NOT set::burst       grouping
```

`OR` binds loosest, then `AND`, then `NOT`; parentheses override. Years are
always four digits. A **set** is just a tag in the `set` namespace, so a burst,
a comic or a face cluster is grouped by tagging its members.

`date=` means the date a photo was *taken*, so a screenshot or a video with no
capture date never matches it. The grid still sorts those files by their
modification time, and the info panel says which of the two dates you are
seeing.

## Where your data lives

**Everything durable is in your gallery; everything derived is not.**

Tags, ratings, notes and sets go into `.lightview/companions/` beside your
photos — plain JSON, per directory, safe to grep, rsync and back up. Thumbnails
and the index go into `$XDG_CACHE_HOME/lightview/`, keyed by a hash of the
gallery's path, and deleting all of it costs you time and nothing else.

## Building

| Need | Why |
|---|---|
| Rust (2024 edition) | the binary |
| Node 20+ | the SPA, which is **embedded into the binary** |
| `libheif` ≥ 1.21 | linked, for HEIC/HEIF |
| `ffmpeg` + `ffprobe` | at runtime, for video thumbnails and frame extraction |
| `xdg-utils` | at runtime, to open a browser in local mode |

```sh
npm ci && npm run build            # dist/ must exist before any cargo command
cargo build --manifest-path src-rust/Cargo.toml --release
```

The order is not optional: `dist/` is read by a macro in the library, so every
Rust target — `check`, `test` and `clippy` included — fails without it.

**`libheif` ≥ 1.21 is the one that bites.** Arch tracks a current release, which
is most of why the container image and the package target it; Ubuntu 24.04
ships 1.17, so a Debian-family host needs a source build:

```sh
git clone --depth 1 --branch v1.21.2 https://github.com/strukturag/libheif
cmake -S libheif -B libheif/build -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_PREFIX=/usr/local -DWITH_EXAMPLES=OFF -DWITH_GDK_PIXBUF=OFF
cmake --build libheif/build --parallel && sudo cmake --install libheif/build
sudo ldconfig
export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib64/pkgconfig
```

## How it works

There is no separate design wiki: each module opens with a header saying what
it is responsible for and the rules it keeps, and each function with the rule
it implements. Start at [`src-rust/src/lib.rs`](src-rust/src/lib.rs) for the
layers, and [`src-solidjs/App.tsx`](src-solidjs/App.tsx) for the web client.
`cargo doc --no-deps --document-private-items --open` renders the Rust side.

Writing a tagger is [plugins/README.md](plugins/README.md).
