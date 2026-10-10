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
That JPEG is for viewing: **Download** saves the original file, byte for byte,
whatever its type. (**Copy Image** puts a PNG of any image on the clipboard,
without its name or metadata — no web page can put a file there.)

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
set::kellys-comic                 set membership — alone, the set in its own order
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
a comic or a face cluster is grouped by tagging its members — select them and
add them with the selection bar switched to **Set**.

A set can also have an **order** of its own, for a comic saved strip by strip or
anything else whose files don't sort into the right sequence. Filter to exactly
`set::name` and the grid shows the set in its order: drag thumbnails to
rearrange it with a mouse, or use the sort menu's **Reverse**, **Lock** (keep
the order on screen) and **Clear**. Members that have no place yet follow the
ordered ones, in whatever sort you've chosen. Any wider query — `set::name AND
rating>=4`, say — sorts normally. In the sidecar a member's place is a suffix on
its entry, `"set": ["kellys-comic::3"]`, so a set name can't itself end in `::`
and digits.

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

## Writing a plugin

A plugin tags media. LightView hands it images and it hands back tags; it never
touches a companion file, a database or a video. Plugins are developed outside
this repository, and this section is the contract they are written against.

### Installing one

A plugin is a directory under `$XDG_DATA_HOME/lightview/plugins/` (usually
`~/.local/share/lightview/plugins/`) whose name matches the `name` in its
`manifest.json`. Copying the directory there installs it — there is no install
command, for the same reason the password and pairing are administered from a
shell.

The directory name is the identity, and the manifest's `name` is checked against
it. LightView never builds a path from a name it was sent: it scans the install
root, and a name either matches something it found or matches nothing. A request
carries a plugin *name*, never a command, so no request can name something to
execute.

```
lightview tag ~/photos --plugin wd-tagger
lightview tag /mnt/nas/photos --plugin wd-tagger --filter 'NOT has::plugin.wd'
```

### The protocol

One JSON object per line, both directions, over stdin and stdout.

LightView sends `{"action":"tag","path":"/abs/path.webp"}` and expects **exactly
one** result per request:

```json
{"path": "/abs/path.webp", "tags": ["dog", "beach"], "meta": {"…": "…"}}
{"path": "/abs/path.webp", "error": "could not read"}
```

An error result is an answer, not a failure — it costs that file its tags and
nothing else.

**Emit each result as soon as it is ready. Never buffer stdin to EOF.** LightView
keeps a bounded number of requests in flight and releases a slot only when a
result comes back, so a plugin that waits for EOF deadlocks any job larger than
that window. `LIGHTVIEW_JOB_TOTAL` in the environment carries the expected
request count, for a plugin that wants to size a progress bar.

A plugin is judged to have stopped answering if it moves 128 requests past one
without answering it, or if it goes quiet for a long time *after* having answered
something. A first run that spends ten minutes downloading a model is not a
stall — no clock runs until the first result.

### The manifest

```json
{
  "name": "my-tagger",
  "display_name": "My Tagger",
  "version": "1.0.0",
  "api_version": 1,
  "description": "…",
  "execution": { "type": "cli", "command": "python3", "args": ["{plugin_dir}/tagger.py"] },
  "tag_prefix": "mine",
  "input": { "max_edge": 512, "video_frames": 5 }
}
```

`api_version` must be `1`. `{plugin_dir}` expands to the installed directory.

`tag_prefix` is the namespace the tags land in — `plugin.mine` — and that bucket is
**replaced wholesale** on each run, which is what makes re-running under a newer
version a re-tag rather than a union with what the old model thought.

`version` is the skip predicate. A file already carrying this plugin's tags at
this version **or higher** is skipped, so a retrained model ships as a version
bump and the next run re-tags the gallery.

**`max_edge` is a cost, and 512 is the free one.** LightView serves the smallest
cached thumbnail tier at least `max_edge` across — 128, 512, 1280 or 2560 —
rounding up, never down, because a model handed a smaller image than it trained
on has lost information it cannot recover. The tier the background worker warms
is 512; a plugin declaring more pays one full thumbnail generation per image, on
whatever machine runs the job. Models downsize internally, so 512 loses nothing
for most taggers.

**A plugin never sees a video.** LightView samples `video_frames` stills across a
clip (default 5, capped at 16), sends them as ordinary requests, and merges the
answers: a union of the tag sets, except that `rating:` is one choice rather than
a set and gets a fresh argmax across the frames.

The smallest plugin that conforms to all of this is the verification fixture in
[`.claude/skills/verify/example-auto-tagger/`](.claude/skills/verify/example-auto-tagger/):
dependency-free `python3`, about sixty lines.

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
