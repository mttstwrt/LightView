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

## Where your data lives

**Everything durable is in your gallery; everything derived is not.**

Tags, ratings, notes and sets go into `.lightview/companions/` beside your
photos — plain JSON, per directory, safe to grep, rsync and back up. Thumbnails
and the index go into `$XDG_CACHE_HOME/lightview/`, keyed by a hash of the
gallery's path, and deleting all of it costs you time and nothing else.

## Building

```sh
npm ci && npm run build            # dist/ must exist before any cargo command
cargo build --manifest-path src-rust/Cargo.toml --release
```

Needs Rust (2024 edition), Node 20+, `libheif` ≥ 1.21 at build time, and
`ffmpeg` at runtime. The libheif version is the one that bites on Debian-family
distributions — see
[docs/build-and-verify.md](docs/build-and-verify.md).

## Documentation

[**docs/**](docs/README.md) describes how the system works and why.
[docs/architecture.md](docs/architecture.md) is the place to start: the three
modes, the trust model, the layers, and where a request and a file each go.

Writing a tagger is [plugins/README.md](plugins/README.md).
