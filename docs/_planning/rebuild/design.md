# LightView rebuild — plan

**Status:** awaiting approval. No code until it is approved (principle 1).

**This document is self-sufficient by design.** It is written to be executed by a
session with no memory of the conversation that produced it, so every operative
decision is restated here rather than referenced. [`../../refactor.md`](../../refactor.md)
holds the *argument* for these decisions and the inventory of the system being
replaced; it is worth reading for context but nothing in it is required to
execute this plan.

**One document, not the `requirements.md` + `design.md` pair** principle 1
describes. Requirements are section 1 below. The split exists to keep a plan
reviewable; for a change this size the two halves reference each other on nearly
every line, and separating them would mean reading both to understand either.

**Reviewed, three rounds.** Two independent cold reads and an adversarial
self-review, then a **five-angle cold review** — performance, security,
simplicity, factual accuracy against the codebase, and failure modes — each
carried out with no memory of the conversation that produced this plan. Findings are folded into the text rather than appended, so the reasoning sits
where the decision is. Six reversed a settled decision or turned on a fact about
the deployment; the owner decided each, and section 7 carries them as rows.


**On completion:** fold what is durable into `docs/`, delete
`docs/_planning/rebuild/`, and delete `docs/refactor.md` — its inventory
describes a system that will no longer exist.

---

## 1. Requirements

What the rebuilt system must do. Everything else in this document serves these.

### Functional

1. **`lightview <dir>`** — open a gallery for local use. Full selection-scoped
   filesystem operations (copy, move, clipboard, open-with, trash).
2. **`lightview --serve <dir>`** — serve a gallery over the LAN. Remote clients
   get metadata writes, upload, and move-to-trash. **No filesystem access
   beyond that**, and no way to enable it.
3. **`lightview tag <dir> --plugin <name>`** — run a plugin over a gallery from
   the machine that can afford to, writing tags back. Replaces both the
   `lightview-worker` binary and the `--remote` mode an earlier draft designed to
   replace it. **This is confirmed to work because the desktop can mount the
   gallery the server serves** — see section 3.1 for what that fact deletes.
4. **Installable as an ordinary system package**, invoked as `lightview` from
   anywhere on `PATH`. A requirement rather than a nicety, and incompatible with
   how the current build resolves its state directory — see section 3.3.
5. **One grid view: justified.** Aspect-preserving rows. No square grid, no map.
6. **Storage that suits both usage patterns** — a folder of two hundred images
   processed once and never reopened, and a stable library of many thousands.
7. **Duplicate detection with durable "these are not duplicates"** that is
   visible, nameable, and not per-pair clutter.
8. **Plugin execution** for auto-tagging: a plugin runs over a gallery — from the
   viewer, or from `lightview tag` on any machine that mounts it — and writes
   tags back.

   **Grouping outputs a person confirms and names — face clustering and the
   like — is deliberately *not* in this rebuild.** Its durable half is: naming a
   group writes `set::<name>` on every member, which section 3.9 delivers in
   full and which is reachable today from a selection. What is deferred is the
   channel by which a *plugin* proposes a grouping. See "Grouping is deferred"
   in section 3.10 for why, and for what it costs to add later.

### Non-functional

9. **Fewer concepts.** The measure is how many times the word *except* is needed
   to describe the system truthfully. The system being replaced needs it about
   thirty-five times. **Section 2's ledger is the target: thirteen that are
   properties of the problem plus four this plan introduces knowingly —
   seventeen** — that list is the definition, not an estimate standing beside it.
   An earlier draft named a number here and then listed a different number
   there, which is how a falsifiable target quietly stops being falsifiable.
   Every survivor must be a property of the problem rather than of the history.
10. **Smaller.** 26,939 lines of Rust and 19,760 of TypeScript become roughly
    **17,000 and 14,500** — derived from section 4's port table, not asserted:
    ~7,960 Rust ported plus ~8,850 written fresh; ~5,300 TypeScript of concrete
    deletions from the baseline, with the ~6,600-line grid and viewer keep list
    off-limits. An earlier draft said 18,500 and 13,000, which the table could
    not support in either direction. **If the finished tree is not smaller,
    something was added that nobody asked for.**
11. **The photos and their companion files are the only durable data.**
    Everything else must be reconstructable from them.

### Explicit non-requirements

- Backwards compatibility with the current cache database. There is none, and
  no migration code exists to provide it.
- Preserving `not_duplicates` verdicts. They are abandoned; re-marking a handful
  of duplicates is cheaper than owning translation code that runs once.
- A native desktop window. Measured, the WebKitGTK window was slower than a
  browser on the same machine.
- Any intermediate state being runnable. The first working thing is the whole
  system.

---

## 2. The five questions (principle 1)

### Placement

The rebuild keeps the existing dependency direction, which is the part of the
current architecture that was right. Three layers, and nothing points upward:

```
  pure libraries   filter · sort · autocomplete · geocode · companion · util
        ↑          (take a connection or a struct; know nothing above them)
  services         cache · pipeline · plugin · tagging
                   media · gallery · tags · files · duplicates · trash
        ↑          (take state or pieces of it; no HTTP, no IPC types)
  adapter          server (routes + one command table) + cli
```

The single largest structural change is that the top layer collapses from **two
adapters to one**. Today `commands/` (Tauri) and `http_server/api.rs` (HTTP) are
parallel entry points kept in step by a `*_impl` naming convention; with no
Tauri there is one dispatch and the convention disappears.

**But the collapse is worth ~940 lines, not the 10,357 the port table implied.**
An earlier draft mapped all of `commands/` (6,557) and `http_server/` (3,800)
onto `server/`, the adapter. Two reviewers independently measured what is
actually adapter code: 98 `*_impl` call sites of about four lines each, plus 617
lines of dispatch in `api.rs`. The rest of `commands/` — `media.rs` 1,308,
`gallery.rs` 1,276, `tags.rs` 723, `plugins.rs` 567, `duplicates.rs` 419,
`files.rs` 249 — is **domain logic**: batch thumbnail orchestration, gallery open
and fs-watch, tag writes across a selection, plugin lifecycle. Land that in the
adapter and the plan commits the exact failure this section says it must not.
So the second row of the diagram names those six as services, they get their own
row in the port table, and `server/` is only what dispatches to them.

The failure this plan must not commit: a lower layer learning about a higher
one. Specifically — `cache/` must not know what a route is, and the pure
libraries must not gain a dependency on `AppState`. They have none today
(verified: zero references across 3,594 lines) and that is why they port
unchanged.

### Contract

Five contracts change. Two are durable and need care; three are local.

| Contract | Other side | Change | Risk |
|---|---|---|---|
| Companion file `<media>.lightview.json` | other LightView installs, `grep`, the user | **Location unchanged** — per directory, `<dir>/.lightview/companions/<name>.lightview.json` (`companion/reader.rs:46-62`); an earlier draft's storage diagram drew them root-level, which would have orphaned every companion outside the top directory on first open. **`tags.set: []` added as a sibling of `tags.user`; `tags.auto` removed.** Every field of the tag and meta structs gains `#[serde(default)]`, which is what makes an old sidecar parse. Schema version and `migrate()` hook stay. | Low, *given the serde attributes* — without them an old file fails to parse |
| `.lightview/trash/` layout | the user's own filesystem | **Replaced.** `<epoch_ms>_<seq>/<gallery-relative path>` instead of `<epoch_ms>_<seq>/` + `meta.json` — same directory name; the path inside it replaces the metadata file | Low — old entries are not read; purge them before switching or leave them inert |
| `cache.db` | nothing but this process | **Replaced**, moved out of the gallery, and deletable on a version mismatch | None — fully derived |
| `/api/invoke` + routes | the SPA | **Replaced** by one command table with trust levels | None — both sides ship together |
| Plugin NDJSON protocol | plugins on disk | `api_version: 1` only; input quantized to tier edges. **No new result kinds.** | Low — bundled plugins are rewritten in the same change, and the shape is unchanged |

**The companion file is the only thing here that cannot be regenerated.** Treat
any change to it as the largest commitment in the plan.

### Cost in concepts

The plan is overwhelmingly subtractive. What it *adds*:

- **One namespace** (`set`) in the tag vocabulary — but it replaces a table, a
  sweep exception, and a pairwise data model, so the net is negative.
- **One trust level distinction** (`Device` / `Owner`) — but it replaces an
  78-command list, a 46-arm allowlist, and the implicit relationship between
  them.
- **One CLI verb** (`lightview tag`) — but it deletes a binary, a cargo feature,
  a config file, a release artifact, a job broker, a credential store and a
  certificate-pinning protocol.
Nothing else is added. Every other change removes. An earlier draft also added
a `groups` plugin result kind; it is deferred, for the reasons in section 3.10.

**Cases still needing the word *except*** after this plan — thirteen, each a
property of the problem rather than of the history:

1. Four decoders inside one decode function (formats genuinely differ)
2. `fit_rgba` as a second entry shape, for video frames at a chosen timestamp
3. Only `jm`/`jh` are byte-budgeted (the unbounded tier is small)
4. The HEIC transcode cache sits in front of one decoder
5. `rating:x` is a tag, `rating>=x` is a comparison; `::` is a namespace, `:` is not
6. Grid cells keyed by path, pruned surgically
7. Two single-flight slots; the drain re-arms, the warm slot deliberately does not
8. `warping` must not be cleared by `markSettled()`
9. Speculation shares the one bounded pool, so it is gated on "nothing outstanding"
10. Self-signed TLS behind NAT needs its SANs named by hand
11. `.safe-panel` sets all four paddings and overrides `p-*`
12. Two files sharing a `set::` tag are never offered as a duplicate pair —
    **including when they genuinely are duplicates.** Two identical scans inside
    a 200-page comic will not be found. Accepted: the alternative is storing
    pairwise verdicts again.
13. A gallery mounted at different paths on two machines gets two derived
    caches. Today the cache lives *inside* the gallery and is shared by every
    machine that mounts it; keying by hash-of-canonical-root gives that up. It
    is the price of getting the blob out of the photo folder, and it is a real
    regression for a NAS mount browsed locally as well as served.

**Three entries an earlier draft carried here are gone, because none was a
property of the problem.** A ledger that quietly accumulates history-shaped
entries measures nothing:

- **`dist/` must exist before any `cargo` command** is a property of choosing
  `rust-embed` with a required folder. `.gitignore:8` excludes `dist/`, and git
  **cannot re-include a file whose parent directory is excluded** — so the
  negation alone does not work. The line becomes `dist/*` followed by
  `!dist/.gitkeep`, and that file is committed. Then `cargo check` works on a
  fresh clone forever. A build without `npm run build` then serves a 404 at `/` — a loud
  runtime failure instead of a confusing build failure. Two lines for one
  permanent exception. Step 0 had already found the trick and used it only as
  scaffolding.
- **Companion reads fall back to the alongside location; writes do not.** No
  build in this repository's history ever wrote an alongside sidecar, so the
  fallback is defensive code for a condition this application cannot produce.
  Section 3.7 states what deleting it costs.
- **An offline-capable web client has caches that can lie.** Nothing in section 1
  asks for offline operation, and a phone that cannot reach the server cannot
  display a photo anyway. Section 3.12 deletes the service worker; the browser's
  own HTTP cache, with the `ETag` revalidation section 3.5 already specifies,
  does the thumbnail caching correctly and with no code.

**Four the plan introduces and an earlier draft did not ledger.** They are
listed rather than argued away, because an unlisted exception is an undisclosed
cost:

14. `cache/` is written fresh *except* `coalescer.rs`, which is ported — the port
    table says so itself.
15. Plugin input is a cached tier *except* video frames, which are a timestamp
    rather than an edge.
16. The geocoder is not a plugin *except* that it writes into
    `tags.plugins["location"]`, because the bucket's replace-wholesale lifetime is
    the one it wants.
17. Confinement is universal *except* that it is lexical for requests answered
    from the database and canonicalizing for requests that open a file — section
    3.2 explains why, and the cost of collapsing them is a `realpath` walk per
    grid cell.

Two an earlier draft would have added were removed instead by design changes
above: `tag_counts` sitting outside the path-keyed sweep, and display preferences
being per-client *except* locally (both in section 3.3).

Anything beyond this list that a plan step introduces is a regression against
requirement 9 and needs to be argued for explicitly.

### Alternatives

| Considered | Why it lost |
|---|---|
| **Iced** for the desktop UI | `--serve` needs a web client regardless, so this means maintaining two complete UIs forever — the opposite of requirement 9 |
| **`tao`/`wry` shell** around the loopback URL | Keeps one frontend and re-adds a native window; still available later if a browser tab proves unacceptable, but the window is not currently wanted |
| **Incremental refactor**, nine shippable steps | At ~60% rewrite each step negotiates with the shape it replaces; requirement for runnable intermediates was explicitly waived |
| **New repository** with a salvage list | Loses history for no benefit; the same result is achievable on a branch |
| **`sets.json`** as a durable set store | Violates requirement 11 — a set is expressible as tags in files that already exist |
| **Keeping `not_duplicates`** and adding sets alongside | Two answers to one question; the pairwise table is what the complaint was about |
| **Migrating the cache schema** | The database is fully derived, so deleting and rebuilding is strictly simpler and the migration code would be permanent |
| **Compatibility shim** so the old SPA drives the new backend | Doubles the API surface for the duration; the dark period was accepted instead |
| **`lightview tag` fetching the server's warmed `j` tier over `/thumb/j/`** instead of decoding originals over the mount | Genuinely cheaper in bytes — ~800 MB instead of ~160 GB for 20k images, and no second derived cache on the desktop — for the price of one device pairing, which already exists. It lost because it is two byte sources (stills from the server, video frames from the mount), the parameterization section 3.10 just deleted; because 160 GB on gigabit is ~24 minutes, once; and because the second cache only matters when the ceiling is reached. Reopen it with a number if a first run proves slow |

### Assumptions

Named as assumptions because they are not measured. If one is wrong, the
consequence is stated.

1. **A browser renders the grid at least as well as WebKitGTK did.** Basis: the
   user reports the native window measurably underperformed a browser on the
   same machine. If wrong, the fallback is a `tao`/`wry` shell around the same
   loopback URL — the frontend does not change.
2. **The grid's scroll tuning still earns its keep without WebKitGTK.** The
   decode gate, staged tier upgrade and fling handling were tuned for
   main-thread decode. Deliberately **not** re-measured. If scrolling regresses,
   that is when to revisit — not before.
3. **A 512px `j` tier is sufficient input for the ML taggers.** They declare
   1024 today and will receive `jm` (1280); a 448-pixel model receives `j` (512).
   Models downsize internally. If a tagger degrades, raise its declared edge.
4. **dHash over aspect-preserving `j` bytes groups duplicates as well as over
   square-cropped `m` bytes.** The hash downsamples to 9×8 regardless. If
   grouping degrades, the threshold is already a parameter.
5. **A browser plays animated GIFs acceptably.** The sprite-sheet atlas existed
   solely for a WebKitGTK bug. If memory is a problem on iOS, the answer is
   `createImageBitmap` + explicit `close()`, not a resurrected atlas.
6. **All-pairs Hamming duplicate detection is fine at this library's scale.**
   Quadratic; fifty million comparisons at ten thousand images. It will not hold
   at a hundred thousand. Leave it until it hurts.

**Five more the plan was relying on without naming**, surfaced by cold review.
Each is now addressed in the section given, but they are listed here because an
unnamed assumption is itself the defect:

7. That the duplicate hasher can decode whatever codec the `j` tier stores. It
   cannot — section 3.9.
8. That the ThumbHash can be read out of a row containing a 30 KB blob without
   paying for the blob. It cannot — section 3.3.
9. That `cache.db`'s mtime tracks "opened". Under WAL it tracks "checkpointed" —
   section 3.3.
10. That the panels addressing *arbitrary* files (tag manager, duplicates, merge)
    are served by a backfill ordered **newest-first**. They are not — section 3.5.
11. That `devices.db` gets a read pool. Section 3.11 requires auth to read
    "through the read-only pool", but section 3.3 defines a pool only for
    `cache.db`'s thumbnail path. If `devices.db` is one connection behind a
    mutex, the serialization auth is supposed to avoid is moved rather than
    removed. **Decided: `devices.db` gets its own small read-only pool**, since
    auth runs on every thumbnail request.

---

## 3. Target architecture

### 3.1 One binary, three modes

```
lightview <dir>              serve <dir> on 127.0.0.1:<ephemeral>, open a browser at it
lightview --serve <dir>      serve <dir> on 0.0.0.0:<port> over TLS, with pairing
lightview tag <dir> --plugin <name> [--filter <expr>]
                             run a plugin over a gallery and write tags back
lightview pair               mint a one-time pairing code for this machine, and exit
lightview devices            list paired devices (id, name, last seen)
lightview devices revoke <id>   revoke one pairing
lightview password           set or clear the gallery password, reading it from
                             stdin — never from argv, where it lands in shell
                             history and `ps`
lightview cache              show the derived-cache directory and its size
lightview cache --prune      evict least-recently-opened galleries to the ceiling

  --serve takes --port <n> and --tls-san <addr>...; every mode takes
  --data-dir <path> (section 3.3). No other flags exist.
```

**`devices` exists because nothing else could.** An earlier draft stored every
pairing in `devices.db` and specified enrollment in detail, with no way to see or
undo one: the device-management UI lives in the part of `SettingsMenu` section
3.12 cuts, and section 7 settles that **nothing is `Owner` under `--serve`**, so a
web UI could not be the answer even if it survived. A lost phone would stay paired
forever — and since pairing is now per account rather than per gallery, to every
gallery the machine will ever serve. Administering the host from a shell is
already the plan's answer for the password and for `server.toml`; this joins that
list rather than reopening the trust model.

**`pair` takes no gallery argument.** Pairings live in the state directory and
are a property of the account serving (section 3.3), so there is nothing
per-gallery to name. The consequence, stated because it is a real widening: a phone paired
to this machine is paired to every gallery this machine serves, now or later.
That is consistent with all paired devices being equally trusted, and it is the
cost of removing the per-gallery cookie-name mint.

**`lightview tag` is why there is no `--remote` mode, and no worker binary.**

The problem it solves is real and unchanged: the server is an N100 that cannot
run the models, and the desktop has the GPU. An earlier draft solved it by
building a distributed job broker — a worker registry with TTL and liveness,
announce/claim/update/complete/fail, job pinning, two staleness clocks, a
`remote.toml` credential, a `remote-pair` verb, a trust-on-first-use certificate
pin, `--trust-new`, and a `?frame=` route on the media server whose *only*
justification was feeding that worker. All of it exists to move bytes and results
between two machines over HTTP.

**The desktop can mount the gallery.** So it doesn't need a protocol; it needs a
path. `lightview tag /mnt/nas/photos --plugin wd-tagger` opens the gallery the
way any other mode does, runs the plugin locally, and writes companions. The
server picks them up (see below). Nothing is claimed, nothing is heartbeated,
nothing is pinned, and nothing needs a credential — the filesystem already
answered the authentication question.

Two consequences worth stating rather than discovering:

- **Tagging is started from a shell, not from the phone** — unless the gallery
  is open in a local viewer, whose own plugin runner has progress and cancel. A
  shell is consistent with the password, pairing and `server.toml` already being
  administered that way (section 7). Unattended tagging of what a phone uploads
  is a systemd timer around the same verb (section 3.1b).
- **The bytes move, not the decodes.** The old worker fetched a small `?fit=`
  image over HTTP, so the N100 paid the decode. `lightview tag` reads full files
  over the mount and decodes on the desktop: ~160 GB for 20k eight-megabyte
  JPEGs, which on gigabit is about 24 minutes, once — and much less server CPU,
  the right trade when the server is the bottleneck. (This is exception 13 on
  section 2's ledger paying out: the cache left the gallery, so the desktop
  cannot read the server's tiers over the mount. Section 2's alternatives table
  prices fetching them over HTTP instead.) The desktop
  builds its own derived cache for that gallery on the first run, so subsequent
  runs read cached tiers locally; the cache-directory ceiling (section 3.3) is
  what stops that growing without bound.

**The server must notice companions written from another machine, and `inotify`
will not tell it.** Remote writes over NFS or SMB do not generate local
filesystem events — a property of the protocols, not a bug to work around. So
the idle worker **re-runs the companion index periodically**. An earlier draft
called that "no new code", and a reviewer who read `index_companions` found three
reasons it is not:

- **It holds the writer across the whole walk.** `index_companions` runs inside
  one transaction (`gallery.rs:356-484`) doing `walkdir` and a file read per
  changed companion, and its caller holds the mutex the whole time — the exact
  pattern section 3.3's writer rule forbids. Once per open that was a violation;
  every few minutes it would block every thumbnail the phone is asking for.
  **Split it**: a scan phase (walk, stat, read — no database handle) and a commit
  phase (one batched transaction, the lock taken only there).
- **Nothing tells anyone.** The autocomplete refresh lives in the caller, not the
  function; and there is no channel by which "tags changed" reaches a web client
  at all — the watcher `continue`s on companions, and the only tags-indexed event
  is a Tauri emit. After a run the phone's grid, filter and rating badges are
  stale until a manual reload. The sweep **refreshes autocomplete when it indexed
  anything, and emits the `resync` event section 3.11 specifies with `tags` as
  its domain.**
- **Cadence.** A full walk over a spun-down array every few minutes is what keeps
  the disks from ever sleeping. So `lightview tag` writes
  `<gallery>/.lightview/index-epoch` as its last act — a counter, disposable by
  construction, carrying no intent — and the idle cycle stats that one file and
  sweeps only when it changes. A full sweep on a long cadence (hourly) catches
  `rsync` and Samba drops that do not write it.

**One instance per role, still.** A machine that serves its own gallery and also
tags a remote one runs `--serve` and `lightview tag` as separate processes, which
is what the operating system is for.

**`tag` takes the gallery lock like every other mode**, so it refuses a gallery
that is open in a viewer on the same machine — printing the pid from
`instance.json` and that the viewer's own plugin runner does the same job with
progress and cancel. The alternative, running cacheless beside the viewer, would
mean a second process decoding full originals while the first holds the cache
that already has them.

**`tag` runs the index pass and nothing else.** It needs `tag_index` to evaluate
`--filter`, so it indexes; it does **not** run the GPS backfill or the geocode
pass, which are enrichment and belong to the gallery's steward — the `<dir>` or
`--serve` process. Without this rule the desktop's first cold run would re-geocode
every photo and rewrite every geotagged companion over the mount (the durable-side
gate in section 3.3 makes that a no-op, but this rule makes it not happen), and
every run would re-read the EXIF header of every photo that has no GPS, because a
`NULL` never becomes non-`NULL` for a file that has none.

**A re-run skips a file whose companion already carries `tags.plugins[<name>]` at
the manifest's current `version`.** A version bump therefore re-tags everything,
which is what a version bump means; the same run twice is a no-op.

**A mount that drops mid-run kills the run.** The decoder maps the source file
(`pipeline/thumbnailer.rs:128-132`), and a read fault on a hung mount arrives as
`SIGBUS`, not an `io::Error`. Resume (section 3.10) is the answer, and it is
stated here because `thumbnailer.rs` sits in the ported-not-to-be-reconsidered
table where nobody will look for it.

### 3.1b Packaging

The binary is `lightview`, from Cargo's package name — the `productName:
"Gallery"` mismatch that makes today's `.desktop` file point at a binary the
build does not produce is a Tauri bundling artifact and dies with Tauri.

**Two targets, and they are the two deployments that exist.**

| Target | Carries | Why |
|---|---|---|
| **Container image** (Arch base) | `--serve` on the Ubuntu server | The host distro is irrelevant, which is the point: the image carries current `libheif` and `ffmpeg`, sidestepping the Ubuntu 24.04 libheif-1.17 problem that forces a source build on a Debian-family *host*. |
| **Arch package** (PKGBUILD) | the local viewer **and** `lightview tag`, on one desktop | Idiomatic on Arch, current libheif and ffmpeg for free, CUDA and the plugin's own Python venv untouched, `lightview` on `PATH`, and the `.desktop` file works. |

**A package is one executable and three small files.** The SPA is embedded at
compile time, so there is nothing to install alongside it:

| Installed | Purpose |
|---|---|
| `/usr/bin/lightview` | the binary |
| `lightview.desktop` | `MimeType=inode/directory` and `Exec=lightview %f`, so a file manager offers "Open with LightView" on a folder, plus a `Desktop Action` for a folder's background |
| an icon | for the above |
| systemd **user** units (optional) | `lightview-serve@.service` for the one long-running mode, and `lightview-tag@.timer` + a `oneshot` service running `lightview tag %I --plugin <name> --filter "not has::plugin.<prefix>"` — the filter makes repeated firing idempotent, and this is how photos uploaded from a phone get tagged with nobody present |

**Runtime dependencies:** `ffmpeg` and `ffprobe` for video thumbnails and frame
extraction — without them clips fall back to a placeholder rather than failing —
and `xdg-utils` for the browser launch in local mode. `libheif` is linked, not
shelled out to, so it is a build and shared-library dependency rather than a
runtime binary. Building needs Node, because `dist/` must exist before any
`cargo` command.

**Nothing is installed into a shared writable location.** All state is per-user
under XDG (section 3.3), so the package installs read-only files and creates no
directories at install time — an ordinary package, with no post-install script.

#### Flatpak is not a target, and the reason is specific to this deployment

The XDG move above makes a Flatpak build *possible* — the sandbox's private home
at `~/.var/app/<app-id>/` is exactly the layout, where the exe-relative one would
have failed against a read-only `/app`. It is still the wrong choice here:

- **One desktop runs two roles.** The local viewer and `lightview tag` are both
  on the Arch machine. Tagging spawns a Python subprocess
  with its own venv and a CUDA stack, which is genuinely painful to sandbox — so
  a Flatpak viewer means a Flatpak *and* a native install of the same binary, on
  two update paths. That is precisely what deleting `lightview-worker` was for.
- **The containment buys little here.** Flatpak pays off most for applications
  that should not touch your files. This one's entire job is touching your
  files, so it needs `--filesystem=home` — keeping all the friction and giving
  up most of the benefit.
- **The document portal would break the cache key.** The "proper" Flatpak way to
  open a folder returns a remapped path under `/run/user/<uid>/doc/<id>/`, not
  the real one. `path_in_gallery` compares against a root canonicalized once at
  open, and `galleries/<sha256-of-canonical-root>/` would hash differently every
  session — every gallery re-thumbnailing on every launch. Static
  `--filesystem=home`, never portals, if this is ever revisited.
- **A Flatpak is invoked as `com.example.LightView`**, not `lightview`, which
  requirement 4 asks for. Solvable with an alias; worth knowing.

Flatpak's real value is distributing to other people. There is one known user,
and principle 2 says not to build for a hypothetical second. If that changes, the
XDG work is done and it becomes a manifest plus a filesystem permission — not a
redesign.

#### Two things this deletes

**Prebuilt generic Linux binaries have no consumer.** With a container image and
a from-source Arch package, nothing downloads a loose `lightview` tarball. That
release artifact and its CI path go, alongside the `lightview-worker` artifact
the single binary already removes.

**The container image sheds its graphical stack.** It builds the Tauri library
today and so pulls `webkit2gtk-4.1` transitively, for a process with no window;
after the rebuild there is no Tauri, no GTK, no WebKit and no `wgpu` in it at
all. The image should get materially smaller and faster to build. No target is
stated because no baseline was measured — if one is wanted, measure the current
image first rather than inventing a number to miss.

### 3.2 Trust is a property of the bind

One command table. Each entry carries the minimum trust it requires.

| Level | Reachable from | Covers |
|---|---|---|
| `Device` | any paired client | browse · sorted items · filter · autocomplete · media and thumbnail routes · tags, ratings, colour labels, notes, sets  · `record_view` · `set_default_filter` · `get_media_meta` · `regenerate_thumbnail` · **trash: move-to-trash, list, restore** · upload · duplicate detection and `get_merge_candidates`  · start/cancel a plugin run — `Device`, and it simply reports "no plugins installed" under `--serve`, where none are |
| `Owner` | **a loopback bind only** | copy · move · clipboard · open-with · open a gallery · list a directory (the picker) · install a plugin · **`purge_trash`** · **`merge_duplicates`**  |

**The client is told its own trust level, because otherwise it cannot obey the
next sentence.** Section 3.12 keeps components that offer copy, move, clipboard
and open-with, and this section says the frontend hides what the client cannot
do — but the store that answered that question is deleted with the desktop/web
split, and no replacement was named. Ported panels would offer `Owner` actions to
a phone and collect 403s. One `Device`-level command, `get_capabilities`,
returning `{ trust: "device" | "owner", upload: bool, clipboard: bool }` — the
clipboard entry because section 3.12 makes its availability a runtime question
rather than a compile-time one. The server enforces regardless; this exists only
so the UI does not lie.

Three of those placements were unstated in the first draft and are decisions,
not omissions. **`restore_trash` is `Device`** — it writes a file back to a path
the user already chose, which is the inverse of a delete the same client was
allowed to make. **`purge_trash` is `Owner`**, because permanent deletion is not
"move to trash" and requirement 2 says remote clients get move-to-trash.
**`merge_duplicates` is `Owner`**, because it rewrites a companion, stamps the
keeper's mtime on disk, and trashes the others; a remote client may *find*
duplicates and see the candidates, and may not resolve them. The frontend hides
what the client cannot do, and the server enforces it regardless.

**`open_with` names an index, not a program.** Today the command takes a
`command: String` and a `Vec<String>` of arguments straight off the wire and
spawns them (`commands/settings.rs:900-915`) — which is why every other finding
in this section escalates from "file access" to "code execution", and why the
loopback session exists at all. The user already configures `external_apps` as
`{label, command, args}`; make the command `open_with(app_index, path)` so the
server looks the entry up in its own configuration and substitutes a confined
path into the configured placeholder. The wire then carries an integer and a
gallery-relative path, and **no request can name a program**. This removes a
parameter rather than adding a check.

**`Owner` is granted by a loopback bind and by nothing else. There is no flag
that widens it.** This is the single security rule of the system and it must
survive every later change.

**And it is a property of the accepting *listener*, never of the peer address.**
A `0.0.0.0` bind includes `127.0.0.1`, so a rule written as "the peer is
loopback" hands `Owner` to any local process on a served host — including
anything a browser on that host can be made to issue. The trust level is
therefore decided when the listener is created and carried on the connection:
one listener is the loopback listener and its connections are `Owner`; every
other listener yields `Device` regardless of who dialled in. Stated here because
prose about "a loopback bind" reads ambiguously, and the ambiguous reading is
the exploitable one.

The bootstrap routes — `/healthz`, `/cert`, `/pair/redeem`, `/auth/launch`,
`/auth/password`, `/auth/status` — are unauthenticated by necessity, since there
would otherwise be no way past the auth layer the first time. They are a route
group, **not** a trust level: no *command* is ever reachable unauthenticated, and
naming them in the table would imply a third tier of command that does not exist.
(`/auth/launch` was missing from an earlier draft's list, which would have put
the token-redemption route behind the auth layer it exists to get you through.)

#### The loopback session

**This subsection is here because an earlier draft settled the design in section
7's decision table and never specified it anywhere else.** A one-line row is a
decision; section 3 is where an implementer reads *how*, and the security core of
the system was missing from it.

**The bind.** `lightview <dir>` binds a **random address in `127.0.0.0/8`** —
`127.<r>.<r>.<r>` on an ephemeral port — not `127.0.0.1`. The reason is specific
and it is the one thing here that cannot be fixed later:

> **Cookies are not port-scoped, and `SameSite` is site-scoped.** A cookie set by
> `127.0.0.1:54321` is a host-only cookie for `127.0.0.1` and is sent to **every
> other port on that host**. Any local service the user opens — a dev server, a
> notebook, a downloaded repository's `npm run dev` — receives the LightView
> session cookie in its request headers, and can replay it from a non-browser
> client where no `Origin` is expected. `HttpOnly` does not help; the *server*
> reads it. And `SameSite=Strict` blocks cross-*site* requests, but
> `127.0.0.1:3000` and `127.0.0.1:54321` are the **same site**, so a page served
> by any other local port can already issue credentialed POSTs.

The whole `/8` routes to `lo` on Linux and the entire range is a
"potentially trustworthy origin", so the secure context the async Clipboard API
needs is preserved. Binding a random address in it makes the session cookie's
host belong to **this process alone**: no other local service can receive it, and
no page on another local port is same-site with it. One line at bind, one line in
the launch URL.

**The token.** 32 random bytes, generated at startup and **rotated on every
redemption**: the moment one is exchanged for a cookie, a fresh one replaces it.
It is held in memory and mirrored into `<cache dir>/instance.json` (mode 0600,
beside the lock — section 3.3), which is how a second `lightview <dir>` on the
same folder finds a live URL to open, and how a user recovers from a browser
that never opened. There is no TTL: single-use plus rotation bounds exposure,
and the file is readable only by the account that already owns the photos. It is delivered in the launch URL — `http://127.<r>.<r>.<r>:<port>/?t=<token>`
— and **the URL is also printed to stdout, unconditionally**. Printing is not a
debugging affordance: on a headless host (an SSH session, a container) `xdg-open`
fails and the URL would otherwise be unknowable. It also means no
`--no-browser` flag is needed — the process always prints, always attempts the
launch, and reports if the launch failed. One fewer knob (principle 2), and
section 6's verification recipe stops depending on a flag that was never defined.
The cost, named rather than discovered: under a systemd user unit the URL lands
in the journal. A single-use token that rotates on redemption, on a
process-private loopback address, is an acceptable thing to have in a log; a password would not be, which
is why section 3.3 reads that from stdin instead.

**Redemption.** `POST /auth/launch` with the token exchanges it for the session
cookie, and rotates the token. The frontend calls
`history.replaceState` immediately so `?t=` does not persist in the address bar,
session restore, or history.

**The session cookie** is `HttpOnly`, `SameSite=Strict`, `Path=/`, and lives for
the process. It cannot carry `Secure` — loopback is plaintext — which is exactly
why the random-address bind above is doing the isolation work that `Secure` would
otherwise do.

**The `Origin` rule, stated precisely enough to implement.** On every
state-changing request: **reject when `Origin` is present and is not byte-equal
to this server's scheme + host + port. Allow `Origin` absent.** Two traps, both
of which the loose wording would have walked into — a *site*-level comparison, or
accepting `Sec-Fetch-Site: same-site`, both pass an attacker on another local
port; and *requiring* the header breaks the `curl` recipe, because non-browser
clients do not send it. Browsers always send `Origin` on a
cross-origin POST, so absence is safe. `Sec-Fetch-Site` is a useful second signal
and never a requirement.

Under `--serve` the same check uses `Sec-Fetch-Site: same-origin` instead, because
a `0.0.0.0` bind has no single origin to name — which is why CORS is `Any` today.

**Behaviour at the edges, decided rather than left to the implementer:**

| Case | Behaviour |
|---|---|
| Token never redeemed (the browser did not open) | The bind stays up and unauthenticated callers get 401. Run `lightview <dir>` again: it finds the lock held, reads the live URL from `instance.json`, opens it, and exits. **Never a loopback fallback** — see the rule below |
| Token redeemed twice | The second attempt is 401. Single-use means single-use |
| A second browser tab | Shares the cookie; no second token needed |
| Process restart | New token, new session, new address. A stale tab gets 401 |
| A 401 without `WWW-Authenticate` on loopback | **Not** the not-paired path. Section 3.12's `ipc.ts` contract redirects to pairing on that signal, and there is no pairing flow on a loopback bind — a stale tab would be sent to a page that cannot exist. Show "this session ended; restart LightView" |

**The hard rule, restated to cover the place it would actually be broken:** no
flag widens `Owner`, **and no failure path widens it either.** Every one of the
rows above is a case where the tempting shortcut is "just trust loopback this
once", and that shortcut is the whole vulnerability.

#### Path confinement is a type, not a call

**This is the most important correction in the plan, and it comes from a cold
review that took the previous wording at face value and then read the code it
described.** The earlier draft said "path confinement is universal, with no
exception. Every **route** that resolves a filesystem path canonicalizes it."
Both halves were wrong in a way that matters: the dangerous paths in this system
arrive as **command arguments**, not as route captures — and the ported command
layer's checks are lexical or absent.

Three confirmed instances, all reachable at `Device` trust:

- **`trash_files` uses `strip_prefix`, which preserves `..`.**
  `commands/trash.rs:120-127`. `Path::new("/g/photos/../../etc/passwd")
  .strip_prefix("/g/photos")` returns `Ok("../../etc/passwd")` — a check that
  passes. `move_file` (`trash.rs:85-89`, a rename with a copy-and-delete
  fallback) then deposits the host file **inside** `.lightview/trash/`, which is
  inside the gallery root and therefore served by the media route. That is
  arbitrary host-file exfiltration to a phone, plus destruction of anything
  writable. Today it sits behind the `remote.allow_delete` flag
  (`http_server/api.rs:575-581`); **this plan deletes that flag**, so it would
  ship promoted from opt-in to always-on.
- **Tag writes check nothing at all.** `commands/tags.rs:169-190` reaches
  `modify_companion` (`tags.rs:24-49`), which does `Path::new(path)` and never
  compares against the root. A device can create a companion JSON anywhere on
  the host with attacker-chosen contents, and the distinct error strings make it
  a filesystem-existence oracle. Section 3.9 rewrites exactly these functions,
  which is what makes this the moment to fix it rather than a separate task.
- **`enqueue_job` joins a device-supplied plugin name onto the plugin
  directory.** `plugin/runner.rs:68-71`. `Path::join` with an **absolute**
  argument discards the base entirely, and `..` traverses. The `manifest.name ==
  name` check afterwards is not a guard, because whoever writes the manifest
  writes its name. This breaks section 3.10's stated invariant that an instance
  only runs manifests installed under its own `plugins/`.

**So confinement is two newtypes, and the type system decides which check ran.**
`RelPath` is a wire path whose every component has been verified to be
`Component::Normal` — no `..`, no root, no prefix — and is what the database is
keyed on. `GalleryPath` is an absolute path that has been canonicalized and
compared against the root captured at open, and **it is the only type any
function that opens a file will accept**. `GalleryPath::from(&RelPath)` is the
single place the canonicalize happens. A handler that answers from the database
holds a `RelPath` and never pays for a syscall; a handler that is about to touch
the filesystem cannot compile without a `GalleryPath`, and cannot obtain one
without the check. There is no unchecked `String` for anyone to forget the check
on, and — the part one type with two constructors would not give — there is no
way to open a file with the cheap check by mistake. Returning **404, not 403**, because a 403
confirms the existence of files the caller has no business knowing about.

This is fewer concepts, not more: the scattered `path_in_gallery` calls collapse
into the extractor, and "sources confined always" becomes a property the
compiler enforces instead of a sentence in a doc comment. `path_in_gallery`
itself (`http_server/routes.rs:632-642`) is **correct today** — it canonicalizes
per request and uses `Path::starts_with`, which is component-wise, so
`/g/photos-secret` does not match `/g/photos`. It is the model for the
constructor, not a thing to replace.

**Destinations are the deliberate exception, and the only one.** A copy or move
*destination* was never confined and never can be — that is the entire content
of the `Owner` level. Sources confined always; destinations confined never; one
trust level deciding who may name a destination.

**But canonicalizing is not free, and it must not sit on the thumbnail hot
path.** `tokio::fs::canonicalize` is a blocking-pool hop plus a `realpath` walk —
one `lstat` per component, which on a NAS mount is round trips rather than
syscalls — and the current thumb route pays it before the cache lookup, on every
cell of every scroll (`routes.rs:473` → `:640`). The existing comment
(`routes.rs:472`) states the actual requirement precisely: the check exists for
the **generate-on-miss** branch, which decodes an arbitrary file. That is
what the two types above are for: `RelPath` (lexical, no syscall) for a request
answered from the database, `GalleryPath` (canonicalizing) in the branch that is
about to open a real file — generate-on-miss, the media route, `?fit=`, and every
filesystem command. Nothing reaches the filesystem without a canonicalized check;
nothing pays for one to read a cached blob.

#### Uploads

**The earlier draft said uploads "confirm the *resolved* destination is inside
the root before writing a byte". The code it was describing is lexical and says
so in its own doc comment** (`http_server/uploads.rs:217-238`: *"Not symlink-safe
on its own"*), and it checks only the **directory**, before `create_dir_all`
(`routes.rs:939`, `:948`) — the final destination is never checked at all. Point
`Uploads/` at a bigger disk with a symlink, which is an ordinary thing to do, and
every upload lands outside the root while the check returns true. Nothing outside
the root is indexed or served, so it fails **silently**.



What uploads actually enforce, in order:

1. Reduce the filename to a basename, rejecting traversal. Verified sound:
   `sanitize_component` (`uploads.rs:135-154`) fails closed on every divergence a
   reviewer could construct between the extension read from the raw name and the
   sanitized one.
2. Require the extension to resolve to a known media type. Verified sound: the
   allowlist (`companion/schema.rs:102-112`) admits no `.json`, `.svg` or
   `.html`, so a device cannot land a companion file, anything inside
   `.lightview/`, or a script-bearing type served from the gallery's own origin.
3. **Canonicalize the destination directory after `create_dir_all` and compare** —
   the `GalleryPath` constructor above, not a second mechanism.
4. **Create the final file with `RENAME_NOREPLACE`.** The dedupe loop checks
   `!candidate.exists()` and then renames unconditionally
   (`uploads.rs:189-211`, `routes.rs:1009`); between those two steps, two phones
   uploading `IMG_0001.jpg` do clobber each other, which is the exact thing the
   loop's comment claims it prevents.

**And uploads need bounds, which they have never had.** `field.bytes()`
(`routes.rs:905`) holds each part in RAM in full, under a 512 MiB per-request cap
(`server.rs:132`), with unbounded parts per request and unbounded concurrent
requests — a paired phone can OOM the NAS with a handful of parallel POSTs.
Stream each part to the temp file instead, cap the part count, and refuse when
free space is below a margin.

### 3.3 Storage

**Durable — the photos and their companion files. Nothing else.**

```
<gallery>/
  2026/january/photo.jpeg                       the media
  2026/january/.lightview/companions/photo.jpeg.lightview.json
                                                its companion — per directory, beside the media
  .lightview/
    settings.toml                               default filter + trash retention
    trash/<epoch_ms>_<seq>/2026/january/photo.jpeg      a trashed file, path = provenance
    trash/<epoch_ms>_<seq>/2026/january/photo.jpeg.lightview.json
```

`settings.toml` is durable and belongs in this list. It holds exactly two keys.
The **default filter** is user intent — an earlier draft put it in the derived
database, where a format bump would have deleted it — and is written by a
`Device` command, because under `--serve` the phone is the only UI there is.
**Trash retention** is per-gallery for the reason given below, and is set only
by editing the file: it is the one key in the system that deletes data, so no
command writes it at any trust level.

Kilobytes to low megabytes. Safe to copy, sync, or read with `grep`. Delete
everything else and reopen, and nothing is lost but time.

**Machine-local state follows the XDG base directories**, and the split that
requirement 4 forces is the same one safety already forced:

```
$XDG_CACHE_HOME/lightview/        (~/.cache/lightview)
  galleries/<sha256-of-canonical-root>/cache.db    DERIVED — disposable, budgeted

$XDG_DATA_HOME/lightview/         (~/.local/share/lightview)
  tls/                            private key and certificate
  devices.db                      every pairing
  
  recent.json                     recently opened galleries, for the opener
  plugins/<name>/                 installed plugin code

$XDG_CONFIG_HOME/lightview/       (~/.config/lightview)
  server.toml                     serve configuration
```

**The current build cannot be packaged**, and this is why the plan says so
explicitly rather than inheriting the mechanism. `util::paths::data_dir()`
resolves to `<exe_dir>/data/`, documented as deliberate — *"so a portable
install carries its plugins, TLS material, and recent-gallery list with it."*
Install that binary to `/usr/bin/lightview` and its state directory becomes
`/usr/bin/data/`: root-owned, unwritable, broken on first run.

**Three directories where the first draft had one, and that is fewer things to
explain, not more.** Each is a standard location with an established meaning, so
"which of these can I safely delete?" is answered by the path rather than by a
paragraph. A user emptying `~/.cache`, or systemd-tmpfiles sweeping it, becomes
safe *by construction* — which is exactly the property the earlier one-directory
version needed a warning to achieve.

`--data-dir <path>` overrides all three with `<path>/{cache,data,config}`, for
containers and for tests. The Docker image sets it, or sets the `XDG_*`
variables; either way one line in the compose file replaces the volume mount the
exe-relative layout needed.

**Portable install is given up**, and it was a real capability deliberately
built: a copied directory carried its own plugins and certificates. It is
directly incompatible with requirement 4, and neither deployment this system
has — a container and a desktop — is a USB stick.

**Pairings are therefore per *user account*, not per machine.** Two users on one
host serving galleries hold separate device lists, which is right. In a
container it is moot: one account runs everything, so `lightview pair` and
`lightview --serve` share a state directory by construction.

**Two budgets, two names, because they are not the same mechanism** :

| Name | Scope | Enforced |
|---|---|---|
| **tier budget** | `jm` and `jh` rows inside one gallery's `cache.db` | automatically, on both write paths (section 3.5) |
| **cache-directory ceiling** | total bytes under `galleries/`, across galleries | only by `lightview cache --prune`, LRU on the cache file's mtime |

The ceiling applies to the cache directory only — which under XDG is now
enforced by the path rather than by this sentence. The tier budget is derived
from free disk and overridden by `LIGHTVIEW_TIER_BUDGET_MB`, never by
configuration; `server.toml`'s key is the ceiling.

**Three corrections an earlier draft needed here, each of which was a real
failure rather than a wording problem:**

- **The ceiling is enforced at gallery open, not only by a command.** "Enforced
  only by `lightview cache --prune`" means requirement 6's first case — two
  hundred images processed once and never reopened — leaves a permanent cache
  that nothing reclaims, in a directory this plan advertises as safe *because* it
  is budgeted. A hundred such folders is a hundred orphan caches. Open is the
  right moment: it is one `read_dir` and a sort, and it is exactly when a stale
  cache is provably unused. `--prune` stays as the manual override.
- **The LRU key is an explicit `<gallery dir>/last_opened` file, not `cache.db`'s
  mtime.** Under `journal_mode=WAL` writes land in `cache.db-wal` and the main
  file's mtime moves only on checkpoint — so an mtime key measures *least recently
  written*, and a fully-warmed gallery you open daily and never write to looks
  colder than the throwaway folder you touched once. It would evict exactly the
  wrong thing.
- **`--prune` takes each gallery's `flock` non-blocking and skips what it cannot
  get.** Unlinking a `cache.db` that a running process holds open is completely
  silent on Linux: that process keeps writing to the unlinked inode and the work
  is discarded at exit. Pruning to reclaim space would throw away the thumbnails
  a running `--serve` generated all afternoon.

**And the tier budget's floor must respect free disk.** The current computation
is `share.clamp(floor, ceiling)` (`commands/media.rs:1049-1067`), which *raises* a
near-zero share up to the 512 MiB floor; with the 1.25× hysteresis
(`cache/thumbnails.rs:355`) each bounded tier can reach 640 MiB before the first
eviction. That is a guaranteed ~1.25 GiB of bounded tiers plus an unbounded `j`,
per gallery — and this plan **moves** that from the gallery's own filesystem, a
NAS with terabytes, to `~/.cache` on the OS disk. Use `min(floor, free / 4)` so a
nearly-full disk is not handed 512 MiB per tier by definition.

Naming what a full disk looks like, because it is loud in the log and invisible
in the UI: the tier write fails, `get_or_generate` logs a warning and returns a
miss (`thumb_serve.rs:136-139`), the route answers 404 (`routes.rs:497`), the
the browser caches nothing because the response is not `ok`, and the grid
re-requests and re-decodes the same file on every scroll pass forever.

Consequences, all of them currently missing:

1. The photo folder stops accumulating a multi-hundred-megabyte opaque blob.
2. Every derived cache is in one place, so it can have **one total-size ceiling
   with least-recently-opened eviction across galleries**, reachable from
   `lightview cache`.
3. Paths in the database become **gallery-relative**, since the root is no
   longer implied by the file's location. `rebase_root` and `infer_old_root`
   are not written.

**Accepted cost, two of them.** Keying by a hash of the canonical root means
moving a gallery re-thumbnails it — and it also means a gallery mounted at
different paths on two machines no longer shares one cache. Today the cache
lives inside the gallery, so a NAS mount is thumbnailed once and read by
everything; after this it is thumbnailed per machine that opens it locally. The
serve-plus-browse flow is unaffected, since a browser holds no cache of its own. An id file in `.lightview/` would avoid that for two lines,
and is deliberately not in the design — an id is not something the user needs,
and requirement 11 earns its power by having no exceptions. Add it later if
moving large galleries turns out to be a habit.

**A read-only gallery improves:** derived data goes to the local data dir, so
only the durable half degrades rather than nothing working.

#### One process per gallery, and what enforces it

`cache.db` has one writer behind an in-process `tokio::Mutex`. That assumption
holds only while there is one process, so it is enforced rather than assumed:
**an advisory `flock` on `$XDG_CACHE_HOME/lightview/galleries/<hash>/lock`**,
taken when the gallery is opened and held for the life of the process.

**The lock is on the derived cache directory, not on the gallery.** The gallery
tolerates any number of readers; `cache.db` is the thing that does not. Keying
the lock the same way the cache is keyed also means the two can never disagree
about what "the same gallery" is.

**`flock(LOCK_EX | LOCK_NB)` specifically, because it has no stale state to
recover.** The kernel releases it when the file descriptor closes — on exit, on
`SIGKILL`, on a container being torn down. A pidfile, or a lock-by-file-existence,
would survive a crash and brick the gallery until someone deleted a file they
have never heard of. Say `flock` in the plan rather than "an advisory lock",
because the difference *is* the answer to "what happens after a crash", and note
that a leftover lock **file** is not a leftover lock.

**A second launch opens the first one's window rather than refusing.** The
package ships a `.desktop` file with `Exec=lightview %f` (section 3.1b), so
double-clicking a folder twice is an ordinary user action, and "refused: already
running" is a bad answer to it — especially since the port is ephemeral, so the
second process cannot even tell the user where the first one is. The lock holder
keeps `<cache dir>/instance.json` current — pid and the live launch URL, rewritten
on every token rotation (section 3.2); a second `lightview <dir>` reads it, opens
a browser there, and exits 0. No signal, no socket: one file the holder keeps
fresh, readable only by the account that owns the photos.

Two consequences, both stated because they are real limits rather than bugs:

- **`lightview --serve <dir>` and `lightview <dir>` on the same directory are
  mutually exclusive.** Serving a gallery and browsing it locally at the same
  time means pointing a browser at the served URL, which needs no second
  process. This is a narrowing against today, where the failure is silent
  instead.
- **The lock is per machine and per account, so it does not coordinate two
  machines mounting the same NAS share** — nor `--serve` on the NAS plus a local
  `lightview <dir>` on a desktop over the mount, which is the two-role deployment
  section 3.1 contemplates. Each machine has its own derived cache (exception 13
  on section 2's ledger — and the reason section 3.1's bytes cross the LAN) and therefore its own private lock, so both processes are
  equally certain they are the only writer. What they share is `.lightview/` —
  the companions and the trash — and concurrent metadata writes resolve
  last-writer-wins through the atomic rename in section 3.7: no corruption, no
  merge, and **no visibility**. Worth stating plainly because it is a regression
  in *detectability*: today the shared in-gallery `cache.db` at least made the
  collision loud. Accepted, because the alternative is a lock file inside
  `.lightview/`, which puts machine coordination into the durable tree that
  requirement 11 says holds photos and intent only.
- **One `server.toml` holds one port**, so two concurrent `--serve` processes on
  one account cannot both start — which contradicts section 3.1's "every gallery
  this machine serves, now or later" unless `--port` overrides the file. It does.

#### Configuration is a file; commands are actions

`server.toml` in the config directory, read at startup and on change: bind
address, port, TLS SANs, password hash, inactivity window, upload enable and
scheme, the cache-directory ceiling.

**Trash retention is deliberately not in that list, and putting it there would
delete files.** It lives today in `gallery_meta` *inside the gallery*
(`commands/trash.rs:36`), so it travels with the folder, and `auto_purge`
`remove_dir_all`s everything past the window on **every gallery open**
(`gallery.rs:1021`). Move it to a per-machine config file and the NAS gallery
served by the container at, say, 365 days is opened once on the desktop — whose
`server.toml` does not exist, so it uses the 30-day default — and eleven months
of the *shared* `.lightview/trash/` is permanently deleted, with no warning. It
therefore goes in **`.lightview/settings.toml`**, next to the default filter.
Section 3.3 already makes the argument for that file — the default filter is user
intent and must survive a format bump — and it applies with more force to the one
setting in the system that destroys data.

**There is no remote-delete flag.** Today one exists and gates the whole trash
group plus merge; requirement 2 makes move-to-trash something a remote client
*gets*, not something it might get, so a flag could only narrow `Device` into a
third trust state — an *except* that is not on section 2's list. Two consequences
worth naming rather than discovering: **`purge_trash` and `merge_duplicates`
become unreachable remotely**, where today they are reachable with the flag on,
and a gallery can no longer be served with deletion turned off. Not commands. A headless
deployment configures itself by editing a file, which is what a headless
deployment expects.

Pairing stays a command (`lightview pair` prints a PIN) because minting a code
is an action, not a setting. **So does the password**, for a sharper reason: the
stored value is an argon2id PHC string, and "configuration is a file" cannot mean
asking a person to hand-compute a hash. `lightview password` reads the passphrase
from stdin, hashes it, and writes the field; `lightview password --clear`
removes it. The hash format is argon2id with the crate's default parameters,
unchanged from today.

**The password gates the `--serve` bind only.** A loopback session is already
authenticated by the launch token, and someone with local shell access has the
photos regardless. This is also a contract change worth naming: the password
moves from per-gallery to per-machine along with the pairings.

Display preferences live in the browser's `clientPrefs`, for **every** client,
the local one included. An earlier draft put a local gallery's in
`.lightview/settings.toml` and a remote one's in local storage — but a file
inside the gallery is per-*gallery*, not per-client, so two desktops mounting one
share would fight over thumbnail size, which is the exact thing a per-client
preference exists to prevent. One mechanism, and no local-versus-remote branch
survives into the frontend.

#### The database

Four thumbnail tables plus the index — and **no `tag_counts`**. That table was
keyed `(namespace, tag)`, so it was not path-keyed and sat outside the sweep this
section says has no exceptions; it needed two maintenance paths of its own
(`cache/counts.rs:1-9`), and rebuilding it per apply batch scaled with the
library rather than the batch. Autocomplete already holds every tag in memory
with a count, so it is populated from one
`SELECT namespace, tag, COUNT(*) FROM tag_index GROUP BY 1, 2` at refresh — an
aggregate over an indexed table, at the moments the engine already refreshes. **No migration list, no
`SCHEMA_VERSION` derivation, no idempotency tests.** One `format_version`
integer in `gallery_meta`; if it does not match the build's, delete the file and
re-index.

| Table | Holds |
|---|---|
| `media_meta` | relative path (PK), media type, size, mtime, `date_taken`, `date_added`, `last_viewed`, `last_rated`, rating, width, height, duration, `gps_lat`, `gps_lon`, `color_label`, **`thumbhash`** |
| `tag_index` | `(path, namespace, tag)` — rebuilt from companions |
| `index_state` | `(mtime_nanos, size)` per companion path, so re-indexing skips unchanged files. **Nanoseconds and size, not whole seconds** — today's gate truncates to seconds (`gallery.rs:434-440`) and compares for equality, which was tolerable for one pass at open and is not once the sweep runs concurrently with an hours-long stream of writes from another machine: a companion read at `T.2` and rewritten at `T.6` has the same second, is skipped forever, and both caches confidently disagree with the durable file in opposite directions. NFSv3+ and SMB2 carry sub-second mtimes |
| `thumbs_js` | 128px fit — panel thumbnails; unbounded, rows ~4 KB |
| `thumbs_j` | 512px fit — also carries the `phash` column |
| `thumbs_jm` | 1280px fit — LRU byte-budgeted |
| `thumbs_jh` | 2560px fit — LRU byte-budgeted |
| `gallery_meta` | key/value: `format_version`, location tagger version |

**The ThumbHash lives on `media_meta`, not on the tier — and that placement is
the difference between a gallery that opens instantly and one that does not.**
An earlier draft put it on `thumbs_j` and had section 3.6 reach it with a
`LEFT JOIN`, "purely to inline the ~25-byte ThumbHash". In the current schema
`thumbhash` was added by a later `ALTER TABLE` (`cache/db.rs:156`), so it sits
*after* the 20–40 KB thumbnail blob in record order. SQLite spills a blob that
size to overflow pages, and reaching a column past it walks that row's overflow
chain — so the join touches essentially every byte of the thumbnail table to
produce 25 bytes per row. `get_sorted_items` is the most frequent expensive query
in the system: gallery open, every sort change, every filter apply, every
filesystem change. On a 20k library `thumbs_j` is several hundred megabytes
against a 256 MB `mmap_size`.

It is path-keyed, it is already swept with the media row, and moving it deletes
the join, the alias-qualification trap section 3.6 has to warn about, and one
line of `path_keyed_tables()` reasoning. **`phash` stays on `thumbs_j`**: it is
read once per duplicate scan rather than per gallery open, so the cost argument
does not apply to it.

Migrations are forbidden after this ships, so a schema mistake here is not a
patch later — it is a `format_version` bump that re-thumbnails every library.

**`date_added` and `last_viewed` are mirrored into `meta.core`, or requirement 11
is false.** Both are sort fields (`sort/sorter.rs:26-27`); neither exists in the
companion (`companion/schema.rs:159-167`); `record_view` writes only the database
(`sorter.rs:215-225`) and `date_added` is set to *now* at insert
(`gallery.rs:60`). So "delete everything else and reopen, and nothing is lost but
time" is **not true today** — three operations this plan blesses destroy them
permanently and silently: a `format_version` bump, `lightview cache --prune`, and
the scan-prune below. After any of them, "Date added" collapses to a single
instant for the whole library and "Last viewed" is empty. Mirror both into
`meta.core`, and rebuild them at index time — **with the companion winning
whenever the field is present, and the database value written into the companion
only when it is absent.** The direction has to be stated: mirrored the other way,
the first machine to open a gallery with a fresh cache would stamp *its* `now`
onto every file's `date_added` in the durable tree — routinely the desktop running
`lightview tag` — which is precisely the loss the mirroring exists to prevent. Two fields on a struct that is already gaining `#[serde(default)]`
throughout.

**A scan that errored is not a prune authority.** `populate_media_meta` deletes
every path-keyed row for any path missing from the scan result
(`gallery.rs:88-116`), and `list_dir_recursive` swallows walkdir errors and
returns `Ok` regardless (`provider/local.rs:46-49`). Two ordinary events
therefore wipe the cache: **a NAS not yet mounted at boot** — the mountpoint
exists and is empty, so the scan returns zero entries and everything is pruned,
including the two unrecoverable columns above — and any transient `EIO`
mid-walk, which prunes partially with no log line. `provider/local.rs` sits in
the "ported near-verbatim, not to be reconsidered" table, so this ships unless
the plan says otherwise. Two guards: **propagate walkdir errors** instead of
`continue`, and **refuse the prune when a previously-populated gallery scans to
zero**, loudly. (One item to check rather than assume: the walk's
`filter_entry(|e| !e.file_name().starts_with('.'))` is evaluated on the root
too, so a gallery at `~/.photos` may scan to zero for a third reason. Worth a
five-line test.)

**Indexes are part of the schema, not an optimization to add afterwards.**
Section 3.6's rule — a field is filterable only if it is indexed — makes them
load-bearing, so the plan names them rather than leaving them to be discovered
by a slow gallery: `tag_index` on `(namespace, tag)`, which serves the filter's
`EXISTS` subqueries, autocomplete's refresh and section 3.9's `set::` scan, and
on `(path)` for the path-keyed sweep; `media_meta` on every column the grammar
can compare (`date_taken`, `date_added`, `last_viewed`, `rating`, `color_label`,
`media_type`, `width`, `height`, `size`); `thumbs_jm` and `thumbs_jh` on
`accessed_at`, which the eviction window function orders by.

**Deleting the derived cache causes one re-geocode pass, which rewrites every
geotagged sidecar** — a derived wipe triggering durable writes. An earlier draft
blamed the location-tagger version stamp in `gallery_meta` and left it at that.
**That is the wrong cause, and it matters because the obvious remedy — move the
stamp somewhere durable — fixes nothing.** The actual "already geocoded?" test is
a `NOT EXISTS` over `tag_index` (`gallery.rs:240-246`), and `tag_index` is
derived; section 3.8 requires the geocode pass to run *before* the companion
index pass, so on any fresh or wiped cache the table is empty and every geotagged
file is re-tagged whatever the stamp says.

**Gate the skip on the durable side**: read the candidate's
`tags.plugins["location"].version` from the companion on a cold cache. The
alternative — run geocode *after* the index pass and re-index only the paths it
wrote — also works and is a larger reordering. Either way the pass becomes a
no-op on a rebuilt cache, which is what makes "nothing is lost but time" true
rather than "nothing is lost but time, and every sidecar's mtime".

**Every path-keyed table is swept together.** Keep a single
`path_keyed_tables()` source of truth and a test asserting that removing a
media row clears every one of them — the failure it prevents is a
multi-megabyte blob keyed to a path nothing can reach again. There is no
`not_duplicates`, so there is no table sitting outside that sweep.

PRAGMAs, carried over because they were measured: `journal_mode=WAL`,
**`synchronous=NORMAL`**, `temp_store=MEMORY`, `mmap_size=268435456` (per
connection), `cache_size` 64 MB on the writer and 8 MB per read-only pool
connection. `synchronous=NORMAL` (`cache/db.rs:499`) was missing from an earlier
draft's list; without it the writer inherits SQLite's default `FULL` and fsyncs
the WAL on every commit, which on a batched index pass over a large library is a
large startup regression that nobody would trace back to an omission from a list
marked authoritative.

Connection strategy, carried over: one writer behind a `tokio::Mutex` (because
`rusqlite::Connection` is `Send` but not `Sync`), and a read-only pool of 2–6
connections from `available_parallelism` for the thumbnail serve path.
`devices.db` gets the same shape in miniature — one writer, a pool of two — so
that authentication, which runs on every thumbnail request, never queues behind
a pairing write. More than
one statement means a transaction — SQLite autocommits per statement, so a
variable-length write loop pays a WAL commit each time.

**The writer is held for statements only — never across filesystem I/O, an image
decode or encode, a subprocess, or a loop whose length scales with the library.**
The rule is stated this widely because the expensive holds being ported are
mostly *not* decodes, encodes or subprocesses:

| Site | Held across |
|---|---|
| `pipeline/idle.rs:196-200` → `duplicates.rs:110-146` | **64 image decodes** per acquisition — and it is ported, as "the idle backfill also computes perceptual hashes" |
| `commands/gallery.rs:977-1010` | the GPS backfill, the location backfill (a rayon sidecar read-modify-**write** over every geotagged file, `:271-284`), `index_companions` reading every companion off disk (`:446`), and a checkpoint |
| `commands/duplicates.rs:63-65` | the entire all-pairs Hamming loop, on an async worker with no `spawn_blocking` |
| `commands/plugins.rs:514-521` | `rebuild_tag_counts` + `query_all_tag_counts` + `autocomplete.refresh()`, **per apply batch of 32** |

Every one of these blocks `generate_and_store_tier`, which needs the writer to
store (`commands/media.rs:1246`) — so the grid cannot warm a single thumbnail
while any of them runs. Opening a 20k gallery holds the writer through the whole
index pass; a tagging job over it does a full `tag_index` aggregate plus an
autocomplete rebuild roughly 625 times under the lock, which is what section 8
lists as wanting "a measurement on a large library". The arithmetic does not need
a measurement.

Three consequences follow and are part of the design, not tuning: the open-time
index pass reads companions and writes sidecars **outside** the lock and takes it
only to commit batched statements; `find_duplicates` loads `(path, phash)` under
the lock, releases it, and runs the loop on `spawn_blocking`; and the tagging job
maintains counts incrementally, rebuilding once at job completion.

### 3.4 Trash

`.lightview/trash/<epoch_ms>_<seq>/<gallery-relative path>`. The first segment
is the deletion time plus a sequence number, and is the uniqueness key;
everything after it is the original path. There is no metadata file.

- **Purge** is `read_dir`, parse the numeric name, compare against the retention
  window, `remove_dir_all`. No file reads.
- **Restore** moves the media back to `<root>/<relative path>`, refusing if
  something already occupies it, `create_dir_all` for a vanished parent, then
  prunes empty directories back up to the timestamp directory. **The companion
  goes to the current write location** — `<its directory>/.lightview/companions/<name>.lightview.json` — not alongside the media where the trash entry keeps
  it. A naive path-mirroring restore drops it beside the photo, where the read
  fallback in section 3.7 still finds it, so it *appears* to work and the next
  metadata write forks a second sidecar. The trash round-trip test must assert
  the companion's destination, not just the media's.
- **One delete is one directory**, which makes undoing an operation a natural
  unit. **Keep the `_<seq>` uniqueness suffix** on the timestamp segment
  (`<epoch_ms>_<seq>/`, `commands/trash.rs:99-115`): an earlier draft dropped it,
  and two deletes landing in the same millisecond would then merge into one
  directory, silently breaking the invariant this bullet states.
- The companion sits alongside the media inside the trash, uniformly, whichever
  location it came from. **Media first, companion second** (`trash.rs:133-142`).
  Reversed, a crash between the two moves leaves the photo in the gallery with
  its ratings and tags in the trash — state the order, because either order looks
  arbitrary until you name the failure.

#### The entry id is not a path, and that is a security requirement

**An earlier draft made the client-visible entry id `<epoch_ms>/<relative path>`
— a string containing slashes — and a cold review found that this creates an
arbitrary-file-move primitive at `Device` trust.** The reasoning is worth keeping
in full, because the deleted check is the thing that would have stopped it and
the plan is what deletes it.

`valid_entry_id` (`trash.rs:70-75`) accepts digits and underscores only, and its
comment names this exact attack: *"Anything else (separators, `..`) is rejected
so a remote client can't escape the trash dir."* A slash-bearing id cannot pass
it, so the new layout **forces its removal**. The code it guards normalizes
nothing: `trash.rs:262` is `root.join(id)`, and `Path::join` with an absolute
argument discards the base entirely; `resolve_original` (`trash.rs:93-99`) pushes
`..` components verbatim. `restore_trash` is `Device`. The chain:

1. Upload `x.jpg` whose bytes are a plugin manifest — it passes the extension
   allowlist, which checks the name, not the content.
2. Move it to trash.
3. `restore_trash` with `<ts>/../../../../tmp/evil/manifest.json` — the file is
   renamed there.
4. `enqueue_job(plugin_name = "/tmp/evil")` — see section 3.10 — and the server
   executes the manifest's command.

Even without step 4 it is arbitrary write-anywhere as the server user:
`~/.config/autostart/`, `~/.bashrc`, a systemd user unit.

**So the id stays opaque and the destination is rebuilt from validated parts.**
`list_trash` returns `{id, relative_path, file_name, deleted_at, size}` where
**`id` is the directory name only** (`<epoch_ms>_<seq>`, digits and underscores,
validated as today) and `relative_path` is a separate field. `restore_trash`
takes both, and **every component of `relative_path` must be
`Component::Normal`** before any join — the `GalleryPath` constructor from
section 3.2, not a second mechanism. Confine against the **trash root**, not the
gallery root: `.lightview/trash/` is *inside* the gallery, so a gallery-root
check passes a path that has escaped the trash.

This also deletes work: section 3.12's per-segment percent-encoding rule for
trash ids goes away, because an id with no slashes needs no encoding.

Two things a bare path mirror could not do, and why the timestamp segment
exists: mtime cannot carry the deletion time (a rename preserves it, and
preserving it is the point — restoring a file with a rewritten mtime is silent
data loss), and relative paths are not unique over time (trash, restore, edit,
trash again).

`.lightview` is skipped by the media scan, the companion indexer and the
fs-watcher. Keep all three.

### 3.5 Thumbnails — one pipeline

**Four tiers, one family**, all aspect-preserving, all WebP.

| Tier | Segment | Longest edge | Bounded | Also carries |
|---|---|---|---|---|
| `js` | `js` | 128 | no | panel thumbnails |
| `j` | `j` | 512 | no | `phash`, GIF handling, the grid's base rung |
| `jm` | `jm` | 1280 | **LRU** | the viewer's progressive underlay |
| `jh` | `jh` | 2560 | **LRU** | high zoom |

**`js` exists because an earlier draft assigned panel thumbnails to `j` and
nobody did the arithmetic.** `TagManagerPanel` renders up to 120 thumbnails
(`TagManagerPanel.tsx:54`) in an 88px grid (`:432`), today from a 128px tier
(`:438`). At 512px that is ~7.9 MB of decoded bitmaps becoming **~84 MB** —
roughly 34× the pixels the screen can show, on the exact axis `lib/runtime.ts:80-90`
documents as the one iOS kills tabs on. `DuplicatesPanel` and `MergeDialog` do
the same thing, so there are three real consumers and the second-implementation
test is satisfied rather than argued around.

It also fixes a scheduling mismatch: those three panels address *arbitrary*,
typically old files, while the idle backfill warms newest-first — so each panel
open could fire up to 120 simultaneous generate-on-miss requests at precisely the
part of the library the backfill reaches last. `js` rows are ~4 KB, so the tier
is unbounded and the backfill warms it alongside `j` for a few megabytes per
gallery.

**This reverses "three tiers, one family" from section 7, deliberately.** Three
was never the requirement — *one pipeline* was, and four edges through one
`generate_for_path_fit` is still one pipeline, one family, one encoder. A tier is
a cached edge; the ladder having four rungs costs a table and a row in
`path_keyed_tables()`, not a concept.

**One render path, and this is the invariant that keeps it one:**

```
decode_image(path, edge)  →  fit_dims + resize_rgba  →  encode WebP
  dispatch on format           one implementation        one encoder
```

`decode_image` dispatches on source type — JPEG with scale-on-decode, HEIC via
`libheif`, video via `ffmpeg`, everything else via the `image` crate — and
converges on RGBA immediately. That is dispatch, not duplication.

**Every cached thumbnail is `generate_for_path_fit(path, edge)` at one of four
edges.** Anything that makes this untrue — a tier derived from a larger tier
instead of decoded, a second encoder, a GPU fast path — is a new failure mode
and must be argued for, not slipped in as an optimization.

**Refused in advance: deriving `jm` from a cached `jh`.** The Micro-from-Standard
fast path it would imitate earned its branch by saving a 16× decode on the rung
the grid hammered hardest. `jm` from `jh` saves 4×, on an LRU-bounded tier that
is rarely cold, in exchange for a second way a thumbnail can be wrong.

**A tier is a cached edge.** The serve path, the batch warm, the plugin input
path and the `?fit=` route all call the same function; the difference is only
which edge they ask for and whether the result is stored. Put the
cache-and-coalesce wrapper around that one function so every caller inherits
coalescing.

**Serving.** `GET /thumb/{tier}/{*rel}` reads through the read-only pool; on a
miss it elects one generator while others wait on a `Notify`, bounded to three
attempts. **The waiter enrols in the wake queue before re-checking the cache** —
reversing those two steps loses a notify that races with the generator's
release. A cancelled generator wakes its waiters having produced nothing; the
woken waiter re-checks, finds the slot free, and becomes the generator. ETag
revalidation on the response, so a phone returning after `max-age` expiry
refreshes its grid for a few hundred bytes per thumbnail.

**Disk budget** on `jm` and `jh`: 10% of free disk, floored at 512 MiB, capped
at 8 GiB, per tier, overridable with `LIGHTVIEW_TIER_BUDGET_MB`. Byte-budgeted
with a window function accumulating warmest-first, keyed on an `accessed_at`
column, evicting only past **1.25×** the budget and then trimming all the way
back. Freshly written rows seed `accessed_at` to now — leaving it at the column
default marks every new row maximally cold, so the next pass deletes exactly
what was just generated. Access marks buffer in memory because the read path
holds a read-only connection, and are drained **immediately before** an eviction
pass; reversing that order evicts what the user is looking at. Both write paths
enforce the budget, not just the batch one.

**Idle backfill** warms `j` when no user-driven thumbnail request has landed in
60 s, newest-first (the order the default date-descending sort presents),
re-checking between work units. It also computes perceptual hashes.

**Video**, carried over intact because it is knowledge rather than code:
`ffmpeg` does the downscale in its filter graph at exact pixel dimensions, so a
4K clip crosses the pipe at ~1.8 MB instead of ~33 MB. Those dimensions come
from the probe **with display-matrix rotation applied** — phone clips are
landscape on disk and portrait on screen, and a mismatch here is what makes
`.MOV` thumbnails come back sideways. Every invocation is timeout-bounded. The
same probe lifts the container's ISO 6709 location tag (phones spell the key
three ways) into the GPS columns, and a probe with no location must not clear a
stored one.

**The idle backfill's "is anyone looking" signal must be rebuilt.** Today it is
`fs_change_tx.receiver_count() > 0` — *no web client is subscribed* — because the
desktop user was a separate kind of client detected by a thumbnail-activity
timestamp. After this rebuild **the local user is an SSE subscriber**, so that
counter is true whenever anyone has the gallery open in a browser and the
backfill would never run at all — taking perceptual hashing, and therefore
duplicate detection, with it. **The activity timestamp is the only signal.** The
subscriber count no longer means anything and must not be consulted; the
sentence above is the authoritative statement of `is_idle`.

**Placeholders must not write source dimensions.** A `0×0` write both fills the
`width IS NULL` gap that guards the column and hands the grid a degenerate
aspect ratio.

### 3.5b Serving media

`server/` is written fresh, so the media route is written from nothing and these
three behaviours must be carried deliberately. Each is currently load-bearing and
none is recoverable from the tier pipeline.

**Range requests.** `Accept-Ranges: bytes` and real `206 Partial Content`
responses. Without them `<video>` scrubbing does not work in any browser, and the
ported viewer assumes it. Stream the range rather than buffering it — the current
implementation notes that buffering allocated `len - N` bytes per seek.

**HEIC is transcoded on the serve path, not only in the decoder.** No browser
renders HEIC, so a full-resolution request for a `.heic` original returns JPEG,
through the same bounded transcode cache keyed on `(path, mtime)`. This is
distinct from `decode_image`'s HEIC branch, which produces thumbnails. Wiring
only the latter ships a build where HEIC thumbnails work and the viewer is blank
— a failure that survives to production because the grid looks correct.

**`?fit=<edge>`** returns an aspect-preserving WebP resize of a still, through
the same `generate_for_path_fit` the tiers use and the same coalescer. It applies
to `jpg`/`jpeg`/`png`/`webp` only; GIF and video fall back to the whole file.

**It stays as a grid source at mid and high detail, with its eligibility gates
intact.** An earlier draft removed served-original cells in a single bullet —
"the grid uses tiers only" — which would also have deleted
`ORIGINAL_SRC_TOLERANCE`, the 256px quantization bucket, the resolution gate and
the never-warm rule (`JustifiedGrid.tsx:135-150,271-276,396-402`), the last of
which carries a recorded measurement saying that warming those cells *made things
worse*. Two costs nobody had priced: a cell at mid detail would decode ~1750px
instead of ~768px against a file that records "four times the memory per cell"
for one rung, and six rows of speculative precache would begin **generating and
storing** `jh` for cells that previously produced nothing at all. Keeping the
route costs no new machinery — it and the coalescer exist anyway.

**`?frame=` is deleted along with `--remote`.** It existed for exactly one
consumer — a remote plugin host that had no `ffmpeg` and could not pull whole
videos across the LAN — and `lightview tag` runs beside the files with `ffmpeg`
already listed as a runtime dependency (section 3.1b). Video frame extraction
happens in `plugin/input.rs`, locally, as it always did for a local run. Step 7's
acceptance still requires a clip in the test gallery: the failure being guarded
against was never the route, it was video being silently skipped.

#### The routes, named

The whole surface is small enough to write down:

| Route | Trust | Notes |
|---|---|---|
| `POST /api/invoke` | per command | the one command table |
| `GET /api/events` | `Device` | SSE, one channel, typed events |
| `GET /thumb/{tier}/{*rel}` | `Device` | `js` · `j` · `jm` · `jh` |
| `GET /media/{*rel}` | `Device` | Range/206, HEIC transcode, `?fit=` |
| `POST /api/upload` | `Device` | section 3.5c |
| `GET /api/dirs?path=` | **`Owner`** | the directory picker: returns `{name, path}` for subdirectories only, never media, never file contents |
| *(every route above)* | — | **503 until the initial scan has completed and the watcher is armed** — the readiness gate section 3.5c depends on |
| `GET /healthz` · `GET /cert` | bootstrap | unauthenticated |
| `POST /pair/redeem` · `POST /auth/launch` · `POST /auth/password` · `GET /auth/status` | bootstrap | unauthenticated |

**The command table is a deliverable of step 4, not prose here** — but its shape
is fixed: `(name, argument struct, minimum trust)`, one row per command, and the
trust level is a field rather than a lookup in a second list. That is the whole
replacement for today's 78-command registration plus a 46-arm allowlist kept in
step by hand.

Enumerating today's frontend calls turned up commands that section 3.2's
categories do not account for, and each needs a decision rather than a discovery
during step 6: `get_recent_galleries` / `remove_recent_gallery` (`Owner`; the
store is named in section 3.3), `generate_pairing_code` and `revoke_remote_device`
/ `delete_remote_device` (**not commands at all** — they become the
`lightview devices` verbs, since nothing is `Owner` under `--serve`),
`get_server_capabilities` (replaced by `get_capabilities`, section 3.2),
`rebuild_thumbnails` and `precache_thumbnails` (`Device`; the Thumbnails pane
section 3.12 keeps calls both, and "rebuild" now means four tiers rather than
seven), `get_all_thumbnail_tiers` (`Device`; `InfoPanel` is in the ported list and
calls it), and `list_plugins` (`Device`; both `AutoTagPanel` and `ContextMenu`
call it). `close_gallery` and `reindex_gallery` have no live caller and are
dropped — stated so their absence is a decision.

**Paths on the wire are gallery-relative**, matching the database. Two
exceptions, both `Owner`: a copy or move *destination* is absolute by necessity,
and a plugin's temp file path is absolute by protocol. Carry the encoding rule
with it — **percent-encode each path segment independently and leave `/`
literal**, because axum's router decodes captures but rejects paths containing
raw encoded slashes. A single `encodeURIComponent` over the whole path 404s every
file in a subdirectory.

### 3.5c Ingest — how a new file becomes a grid cell

**This section exists because nobody had traced it.** A cold review followed one
photo from a phone's upload to a cell in another client's grid and found that the
middle of the path — the part between "the file lands" and "the client is told" —
was described in the plan by a single sentence: *"once a file lands, the ordinary
fs-watcher ingests it."* The watcher is named three times in this document and
never specified, and the code it refers to splits in a way that guarantees the
policy is lost: `util/fs_watch.rs` is **ported**, and it is a 60-line transport
whose own doc comment says *"deliberately only a transport… the caller decides
what a burst of events means"*. The 190 lines that decide are in
`commands/gallery.rs:674-862`, and `commands/` is in the written-fresh column. So
all of it would be re-derived from nothing.

Section 3.5b takes a page to say the media route is written fresh and these
behaviours must be carried deliberately. The watcher deserves the same page.

**The watcher's policy, carried deliberately:**

- **A quiet-period debounce, not a throttle.** `POLL_MS` 300, `DEBOUNCE_MS` 500,
  and the timer is **reset by every event** (`gallery.rs:714-715`, `:802`). A
  phone uploading two hundred photos back to back produces no database rows and
  no SSE until 500 ms after the last one lands. That is correct and deliberate,
  and it is invisible unless written down.
- **Three skip filters, in this order**: the `settings.toml` hot-reload branch
  must run **before** the `.lightview` skip (`:751-784`), or changing a display
  preference stops reaching the running process; then `.lightview` itself; then
  the media-extension filter (`:787-793`), which is the only reason the upload's
  own `.lv-upload-*.tmp` file is invisible to the watcher.
- **Only `Create` and `Modify(Name(To))` count as additions.** `Modify(Data)` is
  ignored entirely (`:795-813`).
- **The insert is `INSERT OR IGNORE` of five columns** (`:661-665`).
- **Armed on the canonical root.** Today the database and the watcher both use
  the user-supplied path (`provider/local.rs:38`, `gallery.rs:1055`), so they
  agree by accident. Section 3.3 makes database paths relative to the
  **canonical** root (`gallery.rs:953-961`); leave the watcher on the
  user-supplied one and every event fails `strip_prefix`, which looks exactly
  like "not in this gallery". Concretely: `lightview --serve ~/photos` where
  `~/photos` is a symlink to `/mnt/nas/photos` uploads fine, thumbnails fine, and
  **the grid never learns the file exists** until a restart. A strip failure is a
  `log::warn`, never a silent `continue`.
- **`notify`'s own errors are surfaced.** `fs_watch.rs:53-57` is `if let Ok(event)`,
  which drops them on the floor — and inotify watch-limit exhaustion and queue
  overflow arrive exactly there. On a large gallery against a default
  `fs.inotify.max_user_watches` the watcher goes **partially deaf with no log
  line**: some subtrees stop ingesting and nothing says so. The same channel
  carries the unmount signal, so a NAS dropping out mid-run is equally silent.
  **On a root that disappears, exit non-zero with a message naming it.** A
  serving process can do nothing useful without its gallery, an unmount under it
  is an operator event, and a systemd unit restarts it when the mount returns;
  re-arming in place would be a second lifecycle to get right.
- **Armed before the readiness gate opens.** `gallery_gate` 503s every route
  until the gallery is set (`http_server/middleware.rs:84-98`), which happens
  inside `open_gallery_impl` — *before* the caller starts the watcher
  (`bin/lightview-headless.rs:156`). A file arriving in that window is in neither
  the completed scan nor the watcher. The window is milliseconds today, but a fresh implementation without one turns the window into the entire initial
  scan. The gate is a row in section 3.5b's route table for that reason.

**The upload writer, carried deliberately.** Section 3.2 covers confinement;
these four are the rest of it, and every one is load-bearing:

- **Temp file in the destination directory, then rename** (`routes.rs:988-1020`).
  Without it the watcher fires `Create` on an empty file and the `INSERT OR
  IGNORE` records `file_size = 0` and `date_taken = <upload time>` — and because
  it is `INSERT OR IGNORE`, **nothing ever corrects them**. The thumbnail
  self-heals, because it is generated on demand later; the metadata does not. Size
  sort, `size>=10mb` and date sort are permanently wrong for that file.
- **Stamp the mtime on the temp file *before* the rename** (`:983-1011`), or the
  indexer records the upload time as the capture time and every uploaded photo
  sorts as "today" forever.
- **Collision dedupe**, and **`RENAME_NOREPLACE`** so it cannot lose its race
  (section 3.2).
- **Remove the temp file on *every* error path.** Today `?` propagates out of the
  write (`routes.rs:995`) and only a failed *rename* triggers cleanup (`:1016`),
  so an `ENOSPC` or a dropped connection leaves `.lv-upload-<nanos>-<rand>.tmp`
  behind permanently. It has no media extension, so neither the scan nor the
  watcher will ever see it: invisible litter accumulating in the one tree this
  plan tells the user is safe to `grep`, `rsync` and back up.

**A path is immutable within a gallery session.** This is the cheap resolution to
a three-layer staleness problem: nothing updates a row whose file was replaced
(`INSERT OR IGNORE`, and `Modify(Data)` is not watched), the tier lookup is keyed
on path with no mtime predicate (`cache/thumbnails.rs:198-211`,
`thumb_serve.rs:65-96`), and the ETag is a hash of the *cached thumbnail bytes*
(`routes.rs:204`), which have not changed — so a phone revalidates, gets a 304,
and re-stamps its freshness window for up to thirty days. Section 3.5's sentence
about ETag revalidation refreshing a phone's grid is true for a **new** file and
false for a **changed** one, and it is stated as though it were general. Dedupe
guarantees a new upload is a new path; replacing a file in place on the host is
outside what this design supports, and saying so is cheaper than putting `mtime`
in every tier lookup.

**A newly ingested file gets its companion indexed in the same breath.**
`insert_media_meta_row` writes five columns and never touches `tag_index`; the
watcher `continue`s on companion files (`:781-784`); `index_companions` runs only
in the post-open background task. So a batch of photos arriving *with* their
sidecars over `rsync` or Samba — the NAS case, which is this plan's headline
deployment — appears in the grid with no tags, no rating and no colour label
until the process restarts, and any edit made in that state overwrites a
companion the index never read. One line in the add branch fixes it:
`read_companion` + `reindex_tags_for_file` + `set_index_state`, exactly as
`restore_trash_impl` already does (`commands/trash.rs:319-325`).

**The client half.** The SSE event racing the thumbnail is benign — the event
carries paths, the client refetches, and the thumbnail generates on demand; the
new cell paints from nothing and reflows when the image loads. The **reconnect**
is not benign. Section 3.11 says a reconnecting phone should re-fetch state
rather than replay history, which is a requirement with no owner:
`App.tsx:496-502` registers three listeners and no `onopen`, and `EventSource`
reconnects silently, so every event during the gap is gone. On a phone that
happens constantly — screen lock, Wi-Fi to LTE, backgrounding — and with the item
list persisted in IndexedDB the client can sit on a confidently wrong grid
indefinitely. `App.tsx` is in the rewritten column, so the omission carries
forward by default unless it is assigned here: **`onopen` re-fetches boot state**,
named alongside the two `ipc.ts` behaviours section 3.12 already pins down.

### 3.6 Query — filter, sort, group, autocomplete

Ported essentially unchanged. The grammar is the contract; keep it exactly.

```
vacation                    bare word — any namespace
user::vacation              namespaced ("::" so single colons stay inside tag values)
plugin.face::person:alice   plugin namespace
set::kellys-comic           set membership (new)
Japan  Kyoto                place names, written as tags by the geocoder
NOT auto::indoor            negation; AND / OR / parentheses
rating>=4                   rating comparison — but rating:general is a TAG
type:video                  media type
has::user  has::set  has:geo   namespace non-empty / has coordinates
color:red  color:none       colour label, or its absence (IS NULL, binds nothing)
date>=2024-01-01  date=2024 dates: taken / added / viewed; a year expands to its range
width>=1920  size>=10mb     pixel dimensions, file size (b/kb/mb/gb)
```

Removed: the `GeoBbox` term (its only caller was the map view) and the `auto`
namespace. Added: `set`.

(The only writer of `auto` was the duplicate merge's union — section 4.)

**Add quoted strings to the tokenizer.** Today a tag containing a space cannot
be named by any query — it fails outright, and autocomplete offers such tags, so
clicking a suggestion returns a 500. The tokenizer already handles one
context-sensitive character (`(` groups only when it starts a token, which is
what keeps `hatsune_miku_(vocaloid)` intact), so a quote state belongs in the
same loop. Two frontend spots move with it: the current-token regex in the
filter bar would split inside a quoted phrase, and inserting a suggestion
containing whitespace must wrap it in quotes.

**Compilation.** `parse_filter` produces an AST; the evaluator walks it into a
`WHERE` fragment plus bound parameters against `media_meta`. Tag terms compile
to correlated `EXISTS` subqueries over `tag_index`; everything else is a column
comparison. **A field is filterable only if it is indexed** — that is the
constraint the language is shaped by, and the reason `rating` and `color_label`
are mirrored from the companion into columns.

**Sorting** is a single-table select from `media_meta`, which now carries the
ThumbHash (section 3.3) so the grid paints every cell blurry before any thumbnail
request goes out. There is no join, and section 3.3 says why the column lives there. The ported
code qualifies every column anyway (`sort/sorter.rs:68-72`); keep the habit.

**The filter compiles into the sort statement.** One command,
`get_items { sort, order, sub_sort, filter, group_by }`, and the `WHERE` fragment
goes straight into it. An earlier draft kept the current two-step shape, in which
`apply_filter` returns a `Vec<String>` of paths that the *client* hands straight
back to `get_sorted_items` to be re-expanded with `json_each`
(`commands/filter.rs:45`, `stores/filterStore.ts:58-59`, `sorter.rs:121`) — the
filter result crosses the network twice, and `filteredPaths` has no other
consumer anywhere in the frontend. At 20k matches that is roughly a megabyte of
path strings up and a multi-megabyte item payload down, per debounced keystroke,
to a phone. Compiling it in deletes `apply_filter`, the path list, the JSON
parameter and the `json_each` expansion. The current split exists so that
"changing the sort does not re-run the filter" (`commands/filter.rs:5-6`) — it
buys one indexed SQL scan at the price of two network transfers of the whole
result set.

**Filesystem changes send what changed, not everything.** `App.tsx:466-472`
re-fetches the entire sorted list on any `fs-changed` event carrying an addition
— and passes `undefined` for the filter, silently dropping the active filter as
it goes. One phone upload costs every connected client a full-library payload.
Send the added and removed paths and splice client-side; refetch only when the
client cannot place an insertion.

**Grouping** is a separate in-memory pass over the already-sorted list, emitting
`{label, start_index, count}` where the group key changes. Compare a
`(year, month, day)` triple, not the rendered label — formatting is the
expensive half, and a hundred thousand items hold a few hundred distinct
periods. Key and label must stay one-to-one per granularity, and both spell an
unrepresentable timestamp "Unknown date".

**Autocomplete** holds every unique tag in memory (~300 KB at 5,000 tags) with
its lowercase form precomputed at refresh — folding case per query means
thousands of allocations to answer one keystroke with a value that has not
changed since the last write. Four-tier score: exact, prefix, substring,
subsequence. Deduplicate across namespaces, summing counts. **The subsequence
tier compares character counts, not byte counts** — comparing against
`needle.len()` silently kills fuzzy matching for every non-ASCII query.

### 3.7 Companion files

The record of intent, and the only durable data. Format unchanged:

```
{ schema_version, file, file_hash, media_type, created, modified,
  tags: { user: [...],
          set:  [...],                        // NEW — sibling of user, user-owned
          plugins: { "<name>": { version, tags: [...], ...extras } } },
  meta: { core: { rating, date_rated, color_label, notes, media, location },
          plugins: { "<name>": {...} } } }
```

**`tags.set` is a sibling of `tags.user`, not a plugin bucket.** A plugin bucket
is versioned and replaced wholesale on a re-run, which is right for geocoded
place names and exactly wrong for a set: a set is user-owned and must survive
re-tagging.

**Unknown keys are preserved on write, not dropped.** `#[serde(default)]` makes
an old sidecar *parse*; it does not stop the next write from erasing what the
struct no longer models. Removing the `auto` field means the first rating change
or plugin run silently deletes a user's `auto` tags from the one file that
cannot be regenerated. So `TagCollection` and `MetaCollection` each carry a
`#[serde(flatten)] extra: Map<String, Value>` and round-trip it untouched. The
decision in section 4 — that `auto` tags are dropped — is a decision about the
*index*, and this is what keeps it from quietly becoming a decision about the
*file*.

**Every field of the tag and meta structs takes `#[serde(default)]`.** None of
them has it today. That attribute — not the schema version — is what makes an
old sidecar without `set`, and a new one without `auto`, parse rather than fail.
The claim that "an old file reads correctly in the new build" is true *because
of* this line and false without it.

`user` is never overwritten by anything but the user. A plugin writes only under
its own key, so a re-run replaces that plugin's output and touches nothing else.
`rating` and `color_label` are mirrored into `media_meta` columns by every path
that sets them, and rebuilt from the companion at index time.

**Writes are atomic *and durable*, which are two different claims and the code
today makes only the first.** Serialize to a uniquely-named temp file **in the
target directory** (same filesystem, therefore an atomic rename), then
`write_all` → **`sync_all` on the file** → rename → **`sync_all` on the parent
directory**. A reader sees the old file or the new one, never a truncated one —
that part `companion/writer.rs:59-62` already gets right. What it does not do is
fsync anything, so on ext4 the rename can be durable while the data is not, and
the NAS leg is weaker still. This document calls the companion *"the only thing
here that cannot be regenerated"* and *"the largest commitment in the plan"*, and
then rested it on a durability guarantee the code does not provide.

**Over a network mount the `sync_all` is not a durability nicety; it is where
the error is.** `std::fs::write` (`writer.rs:59`) is create + `write_all`, and
the `close()` happens in `Drop`, which cannot report — and NFS defers `ENOSPC`,
`EDQUOT`, `ESTALE` and `EIO` to flush-at-close. So the write returns `Ok`, the
rename succeeds, and the companion on the NAS is truncated. The index sweep then
swallows it twice more (`gallery.rs:445-447` is two nested `if let Ok`), never
stamps `index_state`, and re-reads and re-fails that file on every pass forever,
indistinguishable from an unchanged one. `sync_all` surfaces the error; **a
companion that fails to parse in the sweep is logged at `warn`**, not skipped.

**Remove the temp file on every error path.** Today it is removed only when the
*rename* fails (`writer.rs:62-66`), so an `ENOSPC` inside the write leaks
`.lightview-tmp-<uuid>.json` into the durable tree — the same shape as the upload
temp leak in section 3.5c, in a different directory.

**Read-modify-write is one operation under `flock`.** Both writers today do a
whole-file read → mutate → serialize with no lock (`tags.rs:24-51`,
`plugins.rs:72-101`), so a rating set from the phone is silently gone if the
desktop's plugin run read that companion a moment earlier — and over NFS "a
moment" is the attribute cache's `acregmin`, three seconds by default. The
losing write is not the older one; it is whichever reader lost the race. So
`write_companion(path, |c| …)` takes `flock(LOCK_EX)` on the target for the
whole read-mutate-write-rename, which works locally and over NFSv4 and degrades
to today's behaviour where locks are unsupported. `modify_companion` becomes that
one function. Section 3.3's "last-writer-wins, no corruption, no merge" is then
true per *operation*, which is the claim that was meant.

The writer stamps `modified`, not the caller.

**One companion location, read and write** — `.lightview/companions/`. The
`companion_location` setting is deleted; it had a UI control and never reached a
write, since every path called `CompanionLocation::default()`
(`companion/reader.rs:32`, `commands/tags.rs:697`).

**The alongside read fallback goes with it**, which is a change from an earlier
draft that kept it "because it costs nothing". It costs one entry on a permanent
exception ledger. `LightviewFolder` is the `#[default]` and has been since
`reader.rs` entered the tree, so **no build in this repository's history ever
wrote an alongside sidecar**: the fallback is defensive code for a condition this
application cannot produce, which principle 2 says not to write. What deleting it
costs, stated plainly: a `photo.jpg.lightview.json` placed by hand or by another
tool is ignored. It is not destroyed — writes go to `.lightview/companions/`, so
a stray file simply sits there — and nothing this project shipped produced one.

**The alongside *form* still exists**, because section 3.4 stores the companion
next to the media inside a trash entry. `companion_path(media, Alongside)`
survives as a path constructor. What is deleted is the two-location *resolution*
in the read path, and with it the both-locations loops in the copy, move and
trash code (`commands/files.rs:221`, `commands/trash.rs:41-43`).

`schema_version` is stamped on write, checked on read, and `migrate()` is called
unconditionally on every parse. It is the identity function today. **This is the
one migration hook that stays**, because a sidecar is a wire format holding data
that cannot be regenerated.

### 3.8 Geocoding

Ported unchanged. GeoNames *cities1000* embedded via `reverse_geocoder`
(~144,000 places, 7.9 MB), lazily built behind a `OnceLock` so a gallery with no
geotagged media never pays for it. Nearest-neighbour in a k-d tree, with two
ceilings that stop confident nonsense: past **25 km** the city tag is dropped
(country and region are still right at that range); past **100 km** nothing is
emitted at all.

Names go into the companion under `tags.plugins["location"]`, **spaces joined
with underscores**, so `Japan` works as a bare filter word for the same reason
`vacation` does and `grep -rl Kyoto` finds it without LightView. Only whitespace
is normalised — accents, apostrophes and hyphens tokenize fine.

It reuses the plugin-bucket container without being a plugin, and that stays
correct next to the new `set` namespace: a location bucket is versioned and
replaced wholesale on a re-geocode, while a set tag is user-owned and must
survive re-runs. Different lifetimes, different homes.

Runs in the background task after the GPS backfill and **before** the companion
index pass, so the sidecars it writes are picked up in the same sweep. A version
stamp in `gallery_meta` forces exactly one re-resolve pass when the emitted tags
would change, and is stamped only if every write succeeded.

Accuracy, measured against twelve landmarks: country 12/12, region 11/12, city
4/11. A dense metropolis is subdivided into wards with their own centroids, so
the nearest entry to a landmark is routinely a neighbour. **Country and region
are dependable; the city tag is "the nearest named place".**

### 3.9 Sets — one concept for three features

**Set membership is a tag.** A `set` namespace alongside `user` and
`plugin.<name>`, one tag per member:

```
set::vacation-burst-3      a burst that is not forty duplicates
set::kellys-comic          a work that exists as several images
set::alice                 a face cluster, once a person has named it
```

**The tag-write commands take a namespace parameter.** Every one of them is
user-hardcoded today — add, remove, the batch forms, rename, merge, delete, list
— so "the tag-write commands apply unchanged" was wrong. Each gains a
`namespace` argument accepting `user` or `set` and nothing else; a plugin
namespace is never writable this way, since a plugin bucket is replaced
wholesale by its own run. One parameter on an existing family, rather than a
parallel family, because the operations are identical and only the destination
differs.

That gives sets their whole surface for free: create is a batch add over a
selection, rename is `rename`, merge two clusters is `merge`, delete is
`delete`, and the tag manager lists both namespaces instead of one.

That is the entire data model. No new file, no new table, no new wire format, no
new filter syntax. The tag index, autocomplete, grouping and the tag-write
commands all apply unchanged, and it is reconstructable from
companions because it *is* companion content.

**"Not a duplicate" is not stored.** It is derived: two files sharing any
`set::` tag are never offered as a duplicate pair. Forty burst frames cost forty tag rows instead of 780 pairwise ones, and
the user sees a name rather than a list of negations.

**Where that check goes, stated precisely, because an earlier draft called it
"one `EXISTS` clause in the finder" and the finder is not SQL.**
`find_duplicates` (`cache/duplicates.rs:152`) loads every `(path, phash)` row
into memory, runs all-pairs Hamming in Rust, and today loads the whole
`not_duplicates` table separately to suppress pairs by hash lookup
(`duplicates.rs:181`). The replacement keeps that shape rather than inventing a
SQL one: a single `SELECT path, tag FROM tag_index WHERE namespace = 'set'`
loaded once before the loop.

**Index-based, not path-keyed**, and the code being replaced says why
(`duplicates.rs:143-152`): probing a set keyed by paths *"meant building an owned
`(String, String)` for every near-match just to ask whether it had been
dismissed — two allocations and a string comparison per candidate… Indices make
it two integers and no allocation."* So: interned set ids in a
`Vec<SmallVec<[u32; 2]>>` indexed by the same position map the loop already
builds (`duplicates.rs:174-179`), and suppression is an intersection of two tiny
sorted lists.

That comment matters more here than it did for `not_duplicates`, because of a
reversal worth stating: dismissed pairs were rare, so the old check almost never
fired. Set co-membership is the **common** case — a forty-frame burst is 780
near-matches, every one of them suppressed. A check that used to be cold is now
the inner loop of the one quadratic algorithm in the tree.

**Order comes from the gallery's own sort.** A comic strip's pages are
`page01.jpg`, `page02.jpg`. Storing an ordinal per member would be a second
thing to keep in step with the filename, for a case the filename answers. Add
one only when a set genuinely needs an order its filenames do not carry.

**One kind, no `source` field.** A confirmed variants group does not persist —
the merge trashes the extras, so one file survives and there is no set left. An
unconfirmed one is a candidate, recomputed from hashes on demand. What remains
is one relation: these belong together. A plugin wanting attribution already has
`meta.plugins[<name>]`.

**A merge unions `set::` tags onto the keeper**, like user tags. Without it,
merging a set member silently drops that member's set. A keeper ending up in two
sets is fine — suppression is pairwise co-membership, so two sets do not become
one through it.

**Sets are cheap and fluid, deliberately.** Renaming one rewrites every member's
sidecar; trashing a member shrinks it silently. That is accepted — a set is not
a durable object with an identity, it is a name several files agree on.

**Duplicate detection**: a 64-bit dHash computed from the cached `j` thumbnail
(already decoded, already in the database, so hashing a gallery costs no source
decodes), stored in a `phash` column on `thumbs_j`. All-pairs Hamming comparison;
threshold is a parameter for precision, not for cost.

> **The ported hasher is codec-specific, and `j` is WebP. Ported unchanged, this
> destroys the library.** `cache/duplicates.rs:136-141` matches `"rgba"` and
> `"jpeg"` and falls through to `_ => None`, then stores `hash.unwrap_or(0)`.
> Today it reads the *Standard* tier, which is JPEG — only the fit tiers are WebP
> (`cache/thumbnails.rs:146-151`). Point it at `thumbs_j` and **every row stores
> the sentinel `0`**. `find_duplicates` selects `WHERE phash IS NOT NULL`
> (`duplicates.rs:155`), so every row qualifies; `hamming(0, 0)` is `0`, which is
> under any threshold; the all-pairs loop (`:214-222`) unions **the entire
> library into one duplicate group**. There is no error, no log line, and no
> failing test — `find_duplicates` returns `Ok`. `DuplicatesPanel` renders it and
> `merge_duplicates` trashes the non-keepers.
>
> Assumption 4 in section 2 discussed only the *geometry* change (square crop to
> aspect-preserving) and never noticed the codec change riding along with it.
>
> **Two fixes, both required.** Decode with `thumbnailer::decode_thumb_bytes_to_rgba`
> (`pipeline/thumbnailer.rs:472`, whose own doc comment says the `image` crate
> sniffs the codec so JPEG and WebP both work) and feed `dhash_rgba`. And **stop
> conflating "could not hash" with "hashed to zero"** — a genuinely flat image
> legitimately hashes to 0 — so store `NULL` on failure and keep the
> `IS NOT NULL` filter meaningful. Section 6 carries the test this needs: a
> `j`-tier row yields a non-sentinel hash, and two unrelated photos do not group.

**Merge**, unchanged: fold several copies' metadata onto one survivor and trash
the rest. User tags union (editable), plugin tags union (automatic), rating /
colour / notes / location as per-field picks, file mtime stamped with
`filetime`, embedded EXIF GPS promotable into the keeper's companion. **Image
bytes are never rewritten** — there is no EXIF write path, and GPS is the one
exception precisely because it can be captured without touching them. The dialog
resolves conflicts; the backend applies a fully-resolved plan.

### 3.10 Plugins and tagging

**The protocol.** The host spawns the plugin as a subprocess and speaks
streaming NDJSON over stdin/stdout:

- request `{"action":"tag","path":"/abs/path.webp"}`
- result `{"path":"...","tags":[...],"meta":{...}}` or `{"path":"...","error":"..."}`
- **exactly one result per request**, including an error result for anything the
  plugin cannot process
- **plugins must consume requests as they arrive and emit each result as soon as
  it is ready** — never buffer stdin to EOF. A plugin that waits for EOF
  deadlocks any job larger than the download window by construction.
- `LIGHTVIEW_JOB_TOTAL` in the environment carries the expected request count,
  replacing the read-to-EOF sizing pattern.

**`api_version: 1` only.** Version 0 predates the streaming contract and is
refused everywhere — not refused in one place and allowed in another. A version
newer than the host is refused too.

**The host decides input, and a plugin never sees a video.** `plugin/input.rs`
scales a still to the requested edge, splits a clip into `input.video_frames`
samples (default 5, clamped to 16) sent as ordinary still requests, and merges
the results: a **union** of the per-frame tag sets, plus a **redone argmax** for
`rating:`, because a rating is one choice rather than a set. A single-part item
passes through untouched. All executors share this machinery, so a plugin cannot
behave differently depending on where it ran.

**Input is quantized up to a cached tier edge.** A plugin declares the longest
edge it wants; the host serves the smallest tier at least that big — ≤128 → `js`,
≤512 → `j`, ≤1280 → `jm`, ≤2560 → `jh`, above that decode from source. **Round up, never
down**: a model handed a smaller image than it trained on has lost information it
cannot recover. The plugin host reads the same `/thumb/<tier>/<path>` URLs a
browser does. Video frames are the irreducible exception — a frame at a timestamp
is not a tier.

**The payoff is conditional, and the condition is not automatic.** "A job over a
warmed gallery does no decoding at all" holds only for the tier the idle worker
warms, which is `j`. The three bundled ML taggers currently declare
`max_edge: 1024`, which rounds up to `jm` — so every image in a job would trigger
a full `jm` generation *on the server*, which is the exact cost this change
exists to remove, on the machine least able to pay it. **Rewrite the bundled
taggers to declare 512**; models downsize internally, so the loss is nil.

State the general rule where a plugin author will read it: **a plugin declaring
an edge above the warmed tier pays one generation per image.** If a tagger
genuinely needs `jm`, the answer is to warm `jm` for that gallery, not to absorb
the decode silently.

**Delete the stubs.** `ExecutionConfig::Wasm`, advisory `capabilities`
(`ReadImage`, `NetworkAccess` — there is no sandbox), and `ui.context_menu_items`
promise things that do not exist. A stub that errors is worse than an honest
absence.

**Grouping is deferred, and the result kind is deleted rather than stubbed.**

An earlier draft added a `groups` result kind — a plugin proposes groupings, the
user names one, the name becomes `set::<name>` on every member — and claimed it
needed "no new storage at all". That claim does not survive the motivating case.
Wiring a face-clustering plugin to a confirmation screen needs four things this
plan does not have:

1. **A way to get proposals off a remote host.** Tags travel back through
   `apply_plugin_tags`; groups have no equivalent command. With `lightview tag`
   the model runs on the desktop and the panel is served by the NAS, so proposals
   are produced in the wrong process with nothing to carry them — the collapse in
   section 3.1 makes this *more* true, not less.
2. **A terminal-line contract.** `PluginResult.path` is required, so a
   standalone `{"groups": …}` line does not deserialize. Worse, the download
   window releases a permit only when a result matches a request — a plugin that
   withholds output until it has seen every face **deadlocks past 64 images**,
   which is precisely the failure this codebase shipped for a year. A clustering
   plugin must emit a per-image acknowledgement to keep permits recycling and
   then a terminal line, and that is a contract, not an implementation detail.
3. **Durable proposals.** "Losing them on restart costs one re-run" is true of a
   tagger and false of clustering, where a re-run is hours of GPU across the
   library. They need a `plugin_groups` table in the derived cache, path-keyed,
   in the sweep list.
4. **Regions, and a channel home.** `{id, label, paths}` points at whole photos,
   so a group shot with five people lands in five clusters with nothing
   distinguishing which face is which — the feature does not work for the case
   it exists for. And cluster ids are not stable across runs, so without a
   channel carrying already-confirmed names back to the plugin, a second pass
   re-proposes everyone.

**Why deferring is cheap.** Each of those is additive against decisions this plan
already makes: a command is one row in the command table; a table is one line in
`path_keyed_tables()`, covered automatically by its test, behind a
`format_version` bump that deletes and rebuilds for free; `path` becoming
`Option` plus a `groups` field is backwards compatible and gated by
`api_version: 2`, which exists for exactly this; `region` and `known` are
additive JSON with `serde(default)`.

**The one decision that had to be made early is made:** a confirmed group name is
a `set::` tag in the companion (section 3.9). That is the load-bearing choice, it
is independent of how a grouping gets proposed, and sets need nothing from the
clustering case — naming, many members, and one photo in several sets all work
identically for a burst. Regions are proposal-time scaffolding, discarded on
confirmation; if face boxes are ever wanted durably, `meta.plugins["face"]`
already exists to hold them.

Build it when a plugin exists that needs it — a result kind nothing receives is
the video-tagging bug's shape, and a protocol with zero implementations is what
principle 2 exists to refuse. Grouping by selection — what a burst or a comic
actually needs — works from day one through the tag commands in section 3.9.

**Findings are deferred for the same reason**, and more comfortably: the
`choice`/`confirm`/`label` shapes, a `pending::` filter term and two extra tables
are an elaborate design for plugins that do not exist. Nothing here forecloses
them.

**One executor, in process, and no queue between machines.** A plugin run is a
local operation: read bytes, feed the subprocess, write companions, report
progress. It is driven from the UI on a loopback bind and from `lightview tag`
on the command line, and those are the same code path with a different progress
sink.

An earlier draft made every run go through a job *queue* so that a local run and
a remote one shared a code path — an executor "parameterized on a byte source
(HTTP fetch vs local read) and a result sink". With `--remote` gone there is one
byte source and one sink, so the parameterization has one implementation and
principle 2 says to write the concrete thing. What goes with it: the worker
registry, announce/claim/update/complete/fail, job pinning, the requeue-versus-
fail distinction between two staleness clocks, and the fire-and-forget terminal
call whose loss would re-run an entire job.

What stays is everything about running *one* plugin well, because that was never
the distributed part: `plan_parts`, `InputPolicy`, `PartTracker`, `MergedItem`,
and the staleness rules below — a wedged subprocess is still a wedged subprocess
whether or not a second machine is involved.

**Constants, carried over because they were arrived at by failure:**

| Constant | Value | Why |
|---|---|---|
| files-on-disk window | 64 | bounds the temp directory the plugin reads from |
| `STALE_AFTER_RESULTS` | 128 | abandon a request once the plugin answered this many *others* — a count, not a clock, so a slow CPU tagger never sheds images |
| `IDLE_RECLAIM` | 5 min | clears a job's tail, where no further results arrive to drive the count; only once the plugin has answered something, so a first-run model download is never mistaken for a wedge |
| `NO_RESULT_STALL` | 20 min | outer backstop; refreshed **only by results that matched** |
| apply batch | 32 | results per `apply_plugin_tags` |
| `MAX_LOCAL_PENDING` | 32 | in-process executor's pending window, with the compile-time invariant `MAX_VIDEO_FRAMES * 2 <= MAX_LOCAL_PENDING` — a clip must never fill the window by itself |
| no-progress | 30 min | a run that stops **progressing** is failed and reported. Never retried: retrying hands the same wedge to the same plugin |

**Requests are keyed on the temp file *name*, not the full path.** A plugin that
canonicalizes its input under a symlinked `TMPDIR` echoes back a different
string for the same file; keying on the name makes directory-level rewriting
harmless. Log an unmatched result rather than dropping it silently.

**A run is resumable because writes are per-file and idempotent.** Companions are
written as results arrive (`commands/plugins.rs:490-559`), so an interrupted run —
`Ctrl-C`, a crash, a closed laptop — leaves the files it finished finished. Re-run it with the same `--filter` and the already-tagged files are skipped
(section 3.1 gives the predicate). That is the
whole recovery story now, and it replaces a page about requeueing, claim
expiry and partially applied batches.

**A live subprocess is not a progressing one.** A tagger's first run legitimately
produces nothing for minutes while it loads a model, so the no-progress clock
counts from the last *matched result*, not from process liveness, and a run that
stops progressing is failed and reported — never retried.

**The server never receives or executes code.** A job carries a plugin *name*;
an instance only runs manifests installed under its own state directory's
`plugins/`.

**That invariant is broken today by one line, and `enqueue_job` is `Device`.**
`find_plugin` does `plugin_dir.join(name)` (`plugin/runner.rs:68-71`) — and
`Path::join` with an **absolute** argument discards the base entirely, while `..`
traverses out of it. The `manifest.name == name` check afterwards is not a guard,
because whoever writes the manifest writes its name field. Combined with the
trash-restore primitive section 3.4 closes, this was a full path from a paired
phone to code execution on the server. **Reject any `name` that is not a single
`Component::Normal` before the join** — or better, delete the join fast path
entirely, since the fallback branch (`runner.rs:76-85`) already finds a plugin by
scanning installed manifests. Deleting it makes the invariant structural: only an
actual child of the install root can ever be selected.

Worth carrying forward as sound, so it is not "fixed" away: job *targets* are
already confined. `tagging/mod.rs:650-673` intersects device-supplied paths with
rows that exist in `media_meta`, so `enqueue_job` cannot point a plugin at an
arbitrary host file. Keep the intersection.

**Move the ML taggers out of this repository.** Keep `plugins/example-auto-tagger`
— dependency-free `python3`, and what the verification recipe drives. The three
ML taggers are personal tools on their own release cadence, pinned to model
repositories and CUDA stacks the gallery knows nothing about. They share a
virtualenv that is not in the repo: each manifest runs
`{plugin_dir}/../.venv/bin/python`, a sibling in the *install* root, so they must
be installed into a common parent and something must build that venv from their
`requirements.txt` files.

### 3.11 Remote access

TLS is always on for a non-loopback bind, because browsers gate the async
Clipboard API and friends behind a secure context. Self-signed ECDSA, persisted,
regenerated when the LAN IP changes or expiry nears. The certificate carries
`basicConstraints CA:TRUE` and `keyCertSign` alongside its serverAuth EKU — not
because it signs anything, but because iOS and macOS only offer the full-trust
toggle for CA certificates, and a click-through exception is per-origin and
short-lived.

**It must also carry X.509 `nameConstraints`, and today it does not**
(`http_server/tls.rs:194-200` sets `is_ca` and `keyCertSign` with SANs only). The
consequence of the trade this plan is deliberately making has never been written
down: once that certificate is enabled under iOS *Certificate Trust Settings*,
the phone trusts **any** server certificate that key signs — for a bank as
readily as for the gallery — and the key is a file on a home NAS that any backup,
snapshot or `rsync` of the data directory carries off. Permitted subtrees =
exactly the SANs the certificate already names; excluded = everything else. Two
lines next to `is_ca`, `rcgen` supports it directly, Apple and Chromium both
enforce it, and the iOS install flow is unchanged. The trade stays; its blast
radius stops being "the internet". `GET /cert` serves the PEM unauthenticated; it leaks nothing, since
every handshake hands out the same certificate, and it must be reachable
*before* the browser trusts the connection enough to pair.

**SANs behind NAT or Docker.** Interface detection sees only the interface this
process routes through — inside a container that is the bridge address, never
the host address clients dial. Name the reachable address explicitly with
`--tls-san` or `LIGHTVIEW_TLS_SAN`. Getting this wrong fails quietly: desktop
browsers survive on a click-through exception that iOS drops readily.

**Pairing.** A device holds a cookie `lv_device=<device_id>.<secret>` — a fixed name, because two `--serve` processes on one account share pairings by design, and two *accounts* on one host is not a deployment that exists. (An earlier draft kept a per-install suffix from the days of per-gallery cookie mints; nothing needs it now.) The loopback session cookie is `lv_session`, and needs no suffix either: its host is a process-unique address. The server
stores a **SHA-256** hash of the secret — deliberately not argon2: the secret is
32 random bytes, so a slow hash buys nothing against that search space, and
verification runs on *every thumbnail request*. Comparison is length-checked and
constant-time. Enrollment is a short-lived, single-use row: a 6-digit PIN typed
by hand or a 32-byte hex token in a QR code.

**An earlier draft said the PIN is safe "because of the 10-minute TTL and
single-use redemption, not because six digits are hard to guess". Single use
bounds a successful guess, not the number of attempts,** and there is no rate
limit, attempt counter or lockout anywhere in the redemption path
(`http_server/auth_routes.rs:43-88` → `devices.rs:222-241`, a bare
`SELECT … WHERE code = ?1`). A million codes over a 600-second window is about
1,700 guesses a second to exhaust, trivially parallel on a LAN, against a server
whose ordinary job is hundreds of thumbnail requests per scroll — and the
distinct 404/409/410 responses confirm progress. Two things make the prize
larger than one gallery: pairings are now per **account**, so a guessed PIN is a
permanent credential for every gallery this machine serves; and the argon2id
password never fires, because a freshly redeemed device gets `last_auth_at = now`
(`devices.rs:250-258`) and the challenge only triggers past the inactivity window
(`middleware.rs:190-196`, six hours by default).

**Fail closed on the pairing row, not per IP:** an `attempts` column, incremented
on every failed redemption while a code is outstanding, and **every outstanding
pairing row is deleted at ten failures**. The human runs `lightview pair` again.
One column and one `UPDATE`; no configuration, no timing state, no per-client
bookkeeping — and it makes the sentence above true. `POST /auth/password`
(`auth_routes.rs:99-131`) gets the same treatment.

Pairings live in the **state directory**, not the gallery, because they are a
property of this account serving. That removes the per-gallery cookie-name mint that
existed because cookies are scoped by host and not by port.

**Nothing else attaches over the network.** An earlier draft gave `--remote` its
own credential file, a trust-on-first-use certificate pin, a `remote-pair` verb
and a `--trust-new` escape hatch — roughly a page of security design whose whole
job was authenticating one machine to another. Section 3.1 deletes the mode, so
all of it goes with it, and the TLS story shrinks to what browsers need.

**Auth is on the hot path.** It runs on every thumbnail request, so it must not
take the writer lock and must not write unconditionally. Read through `devices.db`'s own
read-only pool (section 3.3); rate-limit any `last_seen` touch.

**Change notification.** The fs-watcher and the plugin executor publish to **one**
broadcast channel, relayed as SSE on `/api/events` with a typed event kind per
message. Late subscribers see only events from subscription onward — a
reconnecting phone should re-fetch state, not replay history.

They are two channels today for one reason: the fs channel's subscriber count
doubled as the "is anyone watching?" signal for the idle worker, and tagging
traffic must not make the server think a user is present. Section 3.5 abolishes
that signal, so the reason is gone and the second channel with it. One channel,
one stream, one concept fewer.

**But the two channels also carry different lag contracts, and merging them
naively collapses both into the expensive one.** On `RecvError::Lagged` the fs
relay emits an empty payload that the client reads as "refetch everything"
(`routes.rs:761-765`), while the tagging relay simply `continue`s, safe because
every tagging event is a full snapshot the client re-syncs anyway (`:793-795`).
With one channel the receiver cannot tell which domain it missed, so the safe
answer is the fs answer — and tagging is by far the heavier producer, broadcasting
a snapshot per apply batch plus periodic status. Lag would become common
exactly where it was rare, and each occurrence would cost every connected client a
full-library payload.

So: **one channel, typed lag recovery.** On `Lagged`, emit a single `resync` event
naming the domains that may have been missed, and the client re-fetches only
those. And **throttle job-progress broadcasts to at most one per second** — a
progress bar does not need 32-file granularity, and it is the only high-rate
producer on the shared channel.

**Uploads** are the one write channel from a device. Once a file lands, the
ordinary fs-watcher ingests it — uploads have no separate indexing path.

### 3.12 Frontend

One SolidJS bundle, one runtime. `lib/ipc.ts` remains the only module that talks
to the backend, but it no longer branches on transport: everything is
`POST /api/invoke` plus the media/thumb routes. Delete `isTauri()`,
`safeListen`, the dual-default capabilities store, and the Tauri dependencies.

Two things `ipc.ts` must keep absorbing so they never leak to callers: a `401`
carrying `WWW-Authenticate: LV-Password` raises a challenge, waits for the modal
and retries — **concurrent 401s share one pending promise**, or a grid firing
twenty requests produces twenty stacked modals; and a `401` *without* that header
means the cookie is missing or revoked, which emits a not-paired event and
redirects to pairing.

**Kept, ported near-verbatim** — this is measured, tuned code and section 6 says
it is not to be reconsidered: `JustifiedGrid`, `MediaViewer`, `VideoPlayer`,
`InfoPanel`, `ThumbnailCell`, `SelectionBar`, `ContextMenu`, `ScrollBar`,
`TopBar`, `FilterBar`, `SortMenu`, `CommandMenu`, `TagManagerPanel`,
`DuplicatesPanel`, `MergeDialog`, `TrashPanel`, `AutoTagPanel`, `UploadSheet`,
`PairApp`, `PasswordModal`, `ConnectionBanner`, and every `lib/` primitive:
`scrollDynamics`, `loadPriority`, `thumbSwap`, `thumbProgress`,
`galleryControls`, `wheelScroll`, `thumbRegeneration`, `justifiedLayout`,
`urlVersions`, `pathIndex`, `thumbQueue`, `fetchLoop`, `cellSources`,
`loadedUrls`, `scrollHost`, `viewerCache`, `thumbhashPlaceholder`.

**Three components in the keep list need real work, and calling them
"near-verbatim" was wrong.**

- **`DuplicatesPanel`** calls `markNotDuplicates`, which no longer exists. Its
  replacement gesture is *name a set*: a text field with autocomplete over
  existing `set::` tags, writing a batch add across the group. The rest of the
  panel — detection, grouping, thresholds, the merge entry point — is unchanged.
- **`AutoTagPanel`** loses its desktop/web branch (one runtime now) **and its
  worker roster and job list**, which described a distributed queue that section
  3.1 deletes. What remains is the per-plugin run entry and a progress display
  for the in-process executor — and on a `--serve` bind, where plugins are not
  installed and the models cannot run anyway, it renders nothing. It gains
  nothing either: plugin-proposed grouping is deferred, so there is no proposal
  section.
- **`TrashPanel`** keeps rendering an opaque entry id. Section 3.4 explains at
  length why the id must **not** become a path: `list_trash` returns
  `{id, relative_path, file_name, deleted_at, size}` where `id` is still
  `<epoch_ms>_<seq>` (digits and underscores, validated as today) and the
  original location travels in its own field. The panel change is therefore
  smaller than an earlier draft claimed — it reads `relative_path` where it read
  `original_path`, and passes both fields back to `restore_trash`.

**`lib/ipc.ts` is written fresh, not ported.** Every one of its ~84 call
wrappers targets a command name and argument shape that section 3.2 replaces.
The *components* calling it port near-verbatim; the module underneath them does
not. Note also that `MediaViewer` imports `invoke` from `@tauri-apps/api/core`
directly today, so "`ipc.ts` is the only module that talks to the backend" is a
goal of this rebuild rather than a description of what is being ported.

**The stores are the layer this plan kept forgetting, and they are where the new
API actually lands.** Seven of the nine import `lib/ipc`, which is written fresh
because every command name and argument shape changes — so "the components port
near-verbatim" is true only because the stores absorb that change. Four of the
nine were accounted for above and five were named nowhere, so all nine are named
here:

| Store | Lines | Disposition |
|---|---|---|
| `capabilitiesStore` | 49 | **deleted** — its entire job was the dual desktop/web default |
| `pluginStore` · `taggingStore` · `thumbnailProgressStore` | 58 · 159 · 37 | **merged** into one activity store |
| `galleryStore` | 261 | **rewritten** — the item list, the selection and the SSE wiring; one broadcast channel and gallery-relative paths both land here |
| `settingsStore` | 191 | **rewritten** — `companion_location` and enabled-views go, and the default filter moves from the database to `settings.toml` |
| `filterStore` · `viewerStore` · `uploadStore` | 87 · 90 · 35 | **ported**, following `ipc.ts` for call shapes only |

**Four more files belonged to no list at all.** `index.tsx` (37) is the entry
point and is **rewritten**: it mounts the app, routes the pairing view, and is
where section 3.2's launch-token redemption has to run before anything else.
`components/topbar/icons.tsx` (145) is **ported minus the orphans** — deleting
`ViewSwitcher`, `TitleBar`, `MapView` and `GalleryGrid` strands their icons.
`lib/haptics.ts` (17) and `components/shared/ConfirmButton.tsx` (38) are
**ported unchanged**.

**Three components in the keep list call the Tauri file dialog** — the gallery
opener in `App.tsx`, the copy/move destination in `ContextMenu.tsx`, and the
plugin install path in `AutoTagPanel.tsx`. A browser cannot return a filesystem
path; the File System Access API yields a handle, not a path, and only in
Chromium. **The replacement is an `Owner`-only directory-listing endpoint behind
a small picker component**, which needs no new dependency and works headless.

That does not contradict the settled decision that local mode is
selection-scoped. That decision is about what a *remote* client may reach and
about not putting a bypass in `path_in_gallery` — a directory listing is
`Owner`, loopback-only, and returns directory names rather than media, which the
local user can already enumerate with any file manager. `rfd` was the
alternative and loses: a new dependency that links GTK or a desktop portal, on a
process that may have no display.

**Deleted:** `GalleryGrid`, `MapView`, `ViewSwitcher`, `gridLayout`, `GifCanvas`,
`DebugOverlay`, `Sparkline`, `DevtoolsApp`, `perfMonitor`, `metricRows`,
`devtools.html`, `WindowResizeGrips`, `TitleBar`, and most of `SettingsMenu`
(1,333 lines → roughly 300: Display, Thumbnails, Default Filter).

**`App.tsx` is rewritten, not ported.** It has no entry in either list because it
is neither: it hosts every panel and imports both `@tauri-apps/api/window` and
the dialog plugin. The panel wiring, the scroll host, the keyboard handling and
the scrollbar indicator builders port; the window controls and the dialog calls
go.

**The rest of `lib/`, decided rather than left out.** Ported: `mediaExts`,
`mediaPlayback`, `openAtBottom`, `clientPrefs`, `touch`, `viewerTransition`,
`wheel`, `version`, `types`.
Rewritten: `runtime` and `memoryPressure`.

`runtime` needs care rather than deletion. It is named above only as the home of
`isTauri`/`safeListen`, but it also defines **`isMobile()` as `isWeb() && width <
640`**. Delete `isWeb()` and that silently becomes "narrow window", so a desktop
browser at a narrow width takes the mobile path — and the mobile default sizes
cells for two columns. Redefine it deliberately: viewport width plus `hasTouch()`,
which is a capability rather than a guess.

`memoryPressure` **is already most of the way there and must not be rewritten
from scratch.** An earlier draft described a defect that has since been fixed in
the tree: `lib/memoryPressure.ts:62-66` already branches to a single
`navigator.deviceMemory` read on the web, the empty catch is gone in favour of a
log-once flag (`:53-57`, `:84-91`), and the module comment states the old bug in
the past tense (`:17-22`). The backend command genuinely is absent from the
allowlist, so that half was right — but the prescribed remedy is already written.
The remaining work is deleting the desktop poll branch, not re-deriving 111 lines
of working, commented code.

**Grid invariants that must survive the port**, each of which fails silently:

- **Cells are keyed by path, never by index.** An index-keyed list rewrites every
  slot's `src` as the window shifts, which is per-row scroll flicker.
- **Prune per-path state surgically, never wholesale.** The URL-assignment effect
  has already run for that update; wiping surviving cells blanks the grid until
  the next scroll.
- **A cell's URL, its rung, and its in-flight swap are one unit.** Drop the URL
  but keep the rung and the cell is permanently un-evictable; drop the rung but
  leave the swap running and an abandoned image keeps fetching with nothing to
  commit to.
- **Two single-flight slots, not one.** The visible drain re-arms on completion;
  the warm slot deliberately does not, or the background crawl becomes
  continuous on the one bounded pool. Both reset in `finally`.
- **`warping` must not be cleared by `markSettled()`.** `scrollend` fires after
  every programmatic scroll, so mid-scrub it reopens the gate for one frame in
  every sixty — enough to assign a whole window of cells sixty times a second.
- **Speculation is never free.** It lands on the same bounded pool as visible
  cells, so gate it behind "nothing the viewport is waiting on is outstanding",
  and use a smaller batch than the visible drain — nothing preempts a batch once
  issued, so the batch size *is* the worst-case delay.
- **Served-original cells stay, with their gates.** `?fit=` remains a grid
  source at mid and high detail; the 256px quantization bucket, the resolution
  gate and the never-warm rule all port (section 3.5b says why).

**The file clipboard is ported but its precondition is gone.** The X11 backend
owns the selection on a background thread for the life of the process, and its
own comment notes that a Wayland session without XWayland fails at
`Clipboard::new()` — "fine in practice" only because the host forced
`GDK_BACKEND=x11` for WebKit. That variable is deleted with WebKit, and the host
may now have no display at all. Keep the module, make the failure explicit
rather than a panic, and let the frontend hide the action when the backend
reports it unavailable. It is an `Owner` command, so it is never offered
remotely regardless.

**The service worker is deleted, and with it the second cache layer.** An earlier
draft kept Cache Storage for thumbnails (2000 entries FIFO, 1-hour revalidation,
a 30-day hard ceiling), the sorted item list in IndexedDB with its own ceiling,
`networkFirstShell` and its "serve the cached shell only when `navigator.onLine`
is false" carve-out, a recovery page whose *Reset connection* button unregisters
the worker, and the rule that the worker's version must be bumped in the same
change or a paired phone serves the old shell forever.

That is an exception on section 2's permanent ledger, a deploy-time coupling, a
recovery flow, and a second expiry policy — all to provide offline browsing that
nothing in section 1 asks for, for a client that cannot render a single
full-resolution photo without the server. Meanwhile section 3.5 already specifies
`ETag` revalidation so a phone returning after `max-age` expiry refreshes its grid
for a few hundred bytes per thumbnail: the browser's own HTTP cache does this
job, correctly, with no code and no ceiling to get wrong.

Deleted: `public/sw.js`, `lib/swControl.ts`, `lib/bootSnapshot.ts` and the
recovery page. What breaks, named rather than discovered: opening the gallery
while the server is unreachable shows a connection error instead of a stale grid,
and first paint on a phone waits for the item list rather than restoring it from
IndexedDB. Both are honest behaviour for a client whose content lives on the
server. `ConnectionBanner` stays and now carries the whole story.

The in-memory decoded-image cache in the viewer is unaffected — it is a different
mechanism with a different lifetime, and section 3.12's `memoryPressure` note is
what bounds it.

**Mobile defaults.** `thumbnail_size` is a cell size, not a column count, so the
200px that gives a desktop six columns gives a 390px phone one — the most
expensive possible layout. Derive a mobile default from the viewport's **short**
edge (so rotating does not change the answer) sized for two columns, and cap the
render scale so a 3× DPR phone does not push every cell to the top of the
ladder.

**Full-screen panels carry `.safe-panel`** — four `env(safe-area-inset-*)`
paddings on the panel root. Without it a close button renders under the status
bar on a notched phone: visible, tappable-looking, and completely dead. Padding
on the root specifically, so the panel still paints edge to edge.

---

## 4. The port table

**Anything in this table is copied, not reconsidered.** A rebuild's failure mode
is drifting into re-litigating decisions that were already settled correctly.
Reopening one of these needs a written reason, not a feeling that it could be
nicer.

### Ported near-verbatim

| From | Lines | Why it is not to be rewritten |
|---|---|---|
| `filter/` (ast, parser, evaluator) | 1,263 | the query language is the contract; zero `AppState` references |
| `sort/` (sorter, grouper) | 664 | correct, decoupled; the alias-qualification trap is already handled |
| `autocomplete/engine.rs` | 279 | the case-folding and character-count fixes are both bugs already found |
| `geocode/` (mod, countries) | 568 | the 25 km / 100 km ceilings are measured judgement |
| `companion/` (schema, reader, writer, migration) | 587 | the durable wire format; atomic write-and-rename |
| `provider/local.rs`, `util/` | 233 | scan, whole-file reads, data dir, fs-watch wrapper |
| `file_clipboard/` | 230 | self-contained per-platform selection ownership; see section 3.12 for the precondition that changes. (198 of those 230 are the Linux path; `macos.rs` and `windows.rs` are 32 lines that have never been built.) |
| `plugin/input.rs` | 1,034 | `PartTracker` + staleness rules — a year-old silent hang already fixed |
| `plugin/{runner,manifest,install}.rs` | 833 | subprocess/NDJSON, venv-relative interpreter rewriting |
| `plugin/mod.rs` | 209 | `PLUGIN_API_VERSION`, `check_api_version`, `RequestDelivery`, `scan_plugins` — the version gate section 3.10 relies on. **`default_dir()` is the one thing rewritten in it**, since plugins move to the XDG data directory (section 3.3) |
| `pipeline/video.rs` | 810 | ffmpeg rotation, exact-dimension downscale, timeouts, ISO 6709 |
| `pipeline/{exif,heic_cache}.rs` | 273 | EXIF extraction; a 12-entry transcode LRU keyed on (path, mtime) |
| `pipeline/thumbnailer.rs` — `decode_image`, `fit_dims`, `generate_for_path_fit`, `fit_rgba`, `resize_rgba`, `compute_thumbhash`, WebP encode | ~600 of 1,149 | the one render path |
| `thumb_serve::get_or_generate` **and `cache/coalescer.rs`** | ~200 | enrol-before-recheck ordering, three-attempt bound — and the mechanism that matters more: **the generator slot is an RAII guard.** A dropped request future, which the grid's virtual scrolling causes constantly, must release the slot. The previous explicit-release design leaked the key and presented as "the server stops responding until restart". `cache/` is otherwise written fresh; this file is the exception. |
| `cache/duplicates.rs` dHash | 319 | the hash and the Hamming comparison |
| Frontend: `JustifiedGrid`, `MediaViewer`, `ThumbnailCell`, `ScrollBar`, `ContextMenu`, all `lib/` primitives | ~6,000 | measured, tuned, and untestable by `tsc` |

### Written fresh

| What | Replacing | Why |
|---|---|---|
| `cache/` | 2,044 lines | four tier tables not seven, no `tag_counts`, no migrations, relative paths, new location. (The directory is 2,552 lines; `duplicates.rs` 319 and `coalescer.rs` 86 are ported and `gif_atlas.rs` 103 is deleted, so those 508 are not what this row replaces — an earlier draft's 2,516 double-counted them.) |
| `server/` (routes + one command table) | the ~940 adapter lines of `commands/` + `http_server/` — 98 `*_impl` wrappers and `api.rs`'s 617-line dispatch — plus middleware, TLS, pairing, uploads | one adapter; the `*_impl` convention has nothing left to keep in step |
| `services/{media,gallery,tags,files,duplicates,trash}` | the domain half of `commands/` (~4,500) | **rewritten against the new cache and trust model, not ported** — but placed as services, because putting them in the adapter is the layering failure section 2 forbids. `tags` gains the `namespace` parameter and the `GalleryPath` argument; `gallery` loses Tauri lifecycle and gains the flock; `trash` is the section 3.4 layout |
| `AppState` | `lib.rs` 479 | half its fields are Tauri, GPU, or dual-transport artifacts |
| `tagging/` | `tagging/` 1,446 + worker bin 1,678 | **one in-process executor, no queue** — the registry, the claim protocol and the two staleness clocks all go with `--remote` (section 3.1) |
| `cli` | `main.rs` 429 + headless 434 | three modes, one binary |
| Sets in the duplicate finder | `not_duplicates` table | derived from co-membership |
| Trash | `commands/trash.rs` 563 | path-mirrored layout, no metadata file |
| `lib/ipc.ts` | 969 | every wrapper targets a command name and shape that changes |
| `lib/runtime.ts`, `lib/memoryPressure.ts` | 231 | one runtime; `isMobile()` and the pressure signal both need redefining |
| `pipeline/idle.rs` | 206 | the backfill survives but its idleness signal is replaced outright — its module doc and `idle.rs:69` both define idle as "`fs_change_tx` has no SSE subscribers **AND** no recent thumbnail activity", and section 3.5 deletes the first half. Porting the file ports the check that makes the worker never run |
| `stores/galleryStore.ts`, `stores/settingsStore.ts` | 452 | see section 3.12 — the SSE wiring, gallery-relative paths, and settings that no longer exist |
| A directory-picker component + its `Owner` endpoint | `tauri-plugin-dialog` | a browser cannot return a filesystem path |

### One correction to this table

An earlier draft asserted that **nothing writes the `auto` tag namespace**. That
is false, and it is corrected here rather than quietly: the duplicate merge
unions `tags.auto` across every copy onto the survivor and writes it, with a
test asserting exactly that. It propagates rather than originates — no command
*creates* an auto tag — so the conclusion survives, but the reasoning offered
for it did not.

Two consequences the implementer needs, which the false claim would have hidden:
that merge test must be deleted along with the union, and **an old companion
carrying `tags.auto` will have those tags silently dropped from the index**,
because the tag-enumeration function will no longer emit them. Decided: drop
them. Folding them into `user::` would silently promote machine output to user
intent, which is the one boundary the companion format exists to keep.

This is recorded because the port table is the part of this plan an implementer
is told not to reconsider. A false fact there is more expensive than anywhere
else in the document.

### Deleted outright, with nothing replacing them

`pipeline/gpu_pipeline.rs` (450) and the `wgpu`/`pollster` deps and `gpu`
feature — unreachable once the square grid goes, since its only call site is
reachable only from a command that is not in the allowlist and whose only caller
is `GalleryGrid`. `gif_serve.rs` (187), `cache/gif_atlas.rs` (103) and
`GifCanvas.tsx` (171) — a workaround for a WebKitGTK animation bug that leaves
with the engine. `views.rs` (164) and the enabled-views setting. `commands/geo.rs`
(268). The `/thumbhash` route, protocol arm and PNG cache — the blob is inlined
into the items payload and decoded client-side; nothing ever fetched the route.
`RenderConfig`. The `hardware/` probes for **filesystem and reflink**, which are
logged and displayed and drive nothing (there is no reflink copy path); keep core
count and RAM.

**Not `storage_type` — an earlier draft listed it here and that was false.** It
is the *sole* input to `thumbnail_threads()` (`hardware/mod.rs:72-80`: NVMe →
`cores.min(12)`, SSD → `(cores/2).clamp(2,8)`, HDD → 2, Network →
`(cores/4).clamp(1,4)`), which `lib.rs:350` consumes to build the rayon
`thumb_pool` every thumbnail in the system runs on (`lib.rs:378-382`). Deleting
it silently replaces an I/O-class-aware 2–12 threads with whatever the
implementer invents — on the N100 server that section 3.5's idle backfill exists
for, and on the NAS mount where "Network → 4" is the whole point. Keep the probe,
or replace the policy deliberately and say so; do not delete it as dead weight.
`decisions/` and the whole convention. Most of `docs/refactor.md`'s subject
matter.

---

## 5. Order of construction

Not a shipping sequence — nothing ships until it all does. A dependency order,
with what each step must produce.

| # | Step | Produces | Done when |
|---|---|---|---|
| 0 | **Branch and clear** | the old tree deleted in the same commit that adds the first new file, **plus `dist/*` + `!dist/.gitkeep` in `.gitignore` and that file committed** (the
bare `dist/` exclusion cannot be negated) — permanently, not as scaffolding: it is what deletes the "`dist/` must exist before any `cargo` command" exception (section 2) | `cargo check` on an empty skeleton, from a clone that has never run `npm` |
| 1 | **Pure modules** | `filter/`, `sort/`, `autocomplete/`, `geocode/`, `companion/`, `util/`, `provider/`, `file_clipboard/` moved across; `auto` removed and `set` added in `TagNamespace` **and its TypeScript mirror**, `#[serde(default)]` on the companion structs, quoted strings in the tokenizer | the ported tests pass **after their `auto::` cases are rewritten to `set::` and
their `GeoBbox` cases are deleted** — the term has live code and tests
(`filter/ast.rs:60`, `evaluator.rs:110`, `parser.rs:224,265,298`, and tests at
`parser.rs:761,775,782,787,808`) that will not compile once it goes — the enum is serialized both directions, so this is a wire change, not only a parser change — plus new tests for quoting, `set::`, and an old sidecar parsing without `set` |
| 2 | **`cache/`** | four tier tables, relative paths, `format_version`, the named indexes, the `flock` on the cache directory, the path-keyed sweep and its test | a fresh open indexes a gallery; a version bump deletes and rebuilds |
| 3 | **Pipeline** | one render path, four tiers, the coalescer, the byte budget, the idle worker | tier bytes appear for a test gallery at all four edges |
| 4 | **Server + command table** | routes, the two trust levels, path confinement, TLS, pairing, the launch-token session, SSE, upload | `curl` exercises every route; an unauthenticated call is 401; an `Owner` command on a non-loopback bind is 403; **and on a loopback bind, redeeming a launch token and then calling the directory-listing endpoint succeeds** |
| 5 | **CLI** | `<dir>`, `--serve`, `pair`, `devices`, `password`, `cache` (`tag` lands in step 7 with the executor it drives) | `lightview <dir>` prints its URL and opens a browser; `--serve` binds and pairs |
| 6 | **Frontend** | the ported SPA against the new API | the grid fills in headless Chromium |
| 7 | **Plugins + tagging** | one in-process executor, driven from the UI and from `lightview tag`; the periodic companion re-index in the idle worker | the example tagger completes a run from the UI and the same run from `lightview tag <dir> --plugin <name>` — **with a clip in the test gallery**, so a video's companion gains a merged tag entry rather than being silently skipped; and a companion written into the gallery by another process is picked up without a restart |
| 8 | **Docs** | `docs/` rewritten; `_planning/rebuild/`, `refactor.md` and `todo.md` deleted; `.claude/skills/verify/SKILL.md` rewritten against the new CLI | every page describes what exists, and the verify recipe runs |

---

## 6. Verification

There is no frontend test harness and there will not be one in this change. The
verification that exists is worth more than it was, because after this the
browser is the *only* runtime.

**Rust tests to carry or write:**

- the path-keyed sweep test — **update it, do not delete it**. It currently
  covers **eleven** tables, not four: `path_keyed_tables()` chains four non-tier
  tables with `ThumbTier::ALL`, which is seven (`cache/db.rs:23-27`,
  `cache/thumbnails.rs:111-119`). It also asserts `not_duplicates` behaviour
  (`db.rs:843,873-880`), which goes with the table
- **the duplicate hasher decodes what the `j` tier actually stores** — a `j` row
  yields a non-sentinel hash, and two unrelated photos do not group. Section 3.9
  explains why this is the cheapest test in the list to be missing
- filter parse/compile round-trips, including quoted strings and `set::`
- the sort alias qualification (a bare column name must not compile)
- companion round-trip, atomic write, and the read fallback
- geocode ceilings at 25 km and 100 km
- **the duplicate finder's new "never offer a pair that shares a `set::` tag"
  clause** — the one piece of genuinely new logic sitting where a user's
  judgement is stored, cheap to test and expensive to get wrong quietly
- trash: round-trip a nested path, refuse a restore onto an occupied
  destination, purge by age from the directory name alone
- the tier budget: hysteresis at 1.25×, warm seeding, drain-before-evict

**Both binds must be exercised, and the loopback one is the least-reviewed
surface in this document.** Section 6's browser recipe pairs a device and drives
`--serve`, which is `Device` throughout — so on its own it verifies that `Owner`
is *refused* and never that it works. Add a loopback pass: start `lightview
<dir>` (with no display the browser launch fails harmlessly), read the launch
URL from stdout, redeem its token,
and call a directory listing, a copy into a temp destination, and `purge_trash`.
Without it the launch-token flow, the picker endpoint and the whole `Owner` half
of the trust table ship unverified.

**The `verify` skill drives the old CLI and will be wrong.**
`.claude/skills/verify/SKILL.md` is written against `lightview-headless serve`,
`lightview-headless pair`, `cargo tauri dev` and the per-gallery cookie name.
None of those survive. It is the repository's own recipe for exercising the
stack, so leaving it stale means the first person to reach for it — plausibly a
future session — follows instructions that cannot work. Rewrite it in step 8.

**End-to-end, no display required.** Build the SPA (`npm run build` — `dist/`
is embedded at compile time, so the Rust build fails without it), start the
server on a throwaway gallery, and drive it with `curl`: pair, fetch a thumbnail
at each tier, watch the SSE stream while copying a file in, confirm an
unauthenticated call is 401. Then drive the real SPA in headless Chromium at
`/opt/pw-browsers/chromium` with the device cookie injected, and assert the grid
fills. This is the only way to exercise tier selection, eviction and decode
timing.

**The acceptance test for the whole change** is requirement 10: the finished tree
must be smaller. The baseline is 26,939 lines of Rust and 19,760 of TypeScript,
counted as: every `.rs` file under `src-tauri/src/` including inline
`#[cfg(test)]` modules and excluding `benches/`; every `.ts` and `.tsx` under
`src-solidjs/` excluding `.css`. Count the same way at the end, or the one
falsifiable global criterion is not falsifiable.

`benches/cache_db.rs` and `benches/thumbnailer.rs` reference the old schema and
will fail `cargo clippy --all-targets` from step 2 onward. Rewrite or delete
them in the step that breaks them; do not leave the prescribed lint command
failing.

---

## 7. Decisions already taken

Settled during review. A fresh session should treat these as given and reopen
one only with a written reason.

| Decision | Consequence if reversed |
|---|---|
| Browser-only; no native window | two UIs, or WebKitGTK back |
| Local mode is **selection-scoped**, not filesystem navigation | `path_in_gallery` needs a bypass — the one check between the server and the host filesystem |
| Trust is derived from the bind; no flag widens `Owner` | the security model becomes configuration |
| One grid (justified); no map, no canvas, no virtual folders | the tier collapse unwinds |
| Four tiers, one family, all WebP — `js` 128 · `j` 512 · `jm` 1280 · `jh` 2560 | four parallel generators come back. (An earlier draft said three; section 3.5 explains why panel thumbnails needed the fourth rung) |
| Scroll tuning is **not** re-measured after WebKitGTK leaves | speculative work on the part users feel most |
| Durable = photos + companions only | `sets.json`, `gallery.json`, and an exemption for trash |
| No migration code, anywhere, except the companion `migrate()` hook | permanent code that runs once |
| Sets are tags; `not_duplicates` is deleted | a table, a sweep exception, and 780 rows per burst |
| Sets are cheap and fluid — renaming rewrites members, trashing shrinks silently | a durable set object with an identity |
| Plugin input rounds **up** to a tier edge | a decode per image per job |
| One binary; `<dir>`, `--serve`, and the `tag` verb; one instance per role | a second binary and a release-skew story |
| A dark period is accepted; no compatibility shim | double the API surface for the duration |
| Rebuild in **this** repository, one branch | history lost for no benefit |
| `decisions/` deleted; reasoning lives in subsystem pages | eleven new records owed by this change alone |
| `AGENTS.md` is the one guidance file; `CLAUDE.md` symlinks to it | the drift that produced two different principle numberings |
| **Every bind authenticates, loopback included**; a single-use token in the launch URL, rotated on redemption, and an `Origin` check | `open_with` reachable by any local process or any web page the user visits |
| **`tags.set` is a sibling of `tags.user`**, with `#[serde(default)]` on every field | old sidecars fail to parse, or sets get erased by plugin re-runs |
| **`purge_trash` and `merge_duplicates` are `Owner`; `restore_trash` is `Device`** | remote clients get permanent deletion, which requirement 2 forbids |
| **The bundled taggers declare 512, not 1024** | every tagging job generates `jm` per image on the server |
| **Plugin-proposed grouping is deferred and its result kind deleted, not stubbed** | a protocol a plugin can emit into with nothing receiving it — the video-tagging failure shape, and a plugin point with zero implementations |
| **The directory picker is an `Owner` listing endpoint**, not `rfd` | a new GTK/portal dependency on a possibly-headless process |
| **`auto` tags in old sidecars are dropped from the index, and preserved in the file** by a flattened extras map | the next write erases durable data the struct no longer models |
| **`auto` tags in old sidecars are dropped, not folded into `user::`** | machine output silently promoted to user intent |
| **Machine-local state follows XDG**, with a `--data-dir` override | the exe-relative state directory, which makes `/usr/bin` installation impossible |
| **Portable install is given up** | a capability deliberately built, incompatible with being packaged |
| **A loopback client holds a process-lifetime session, not a device row**; token redeemed at `/auth/launch`, single use, rotated on redemption | ambient authority on `127.0.0.1`, or a pairing flow where none is wanted |
| **`Origin` on loopback, `Sec-Fetch-Site: same-origin` on `--serve`** | a `0.0.0.0` bind has no fixed origin to name, which is why CORS is `Any` today |
| **Under `--serve`, nothing is `Owner`**; the host is administered by CLI and `server.toml` | a web UI that can move files on the server |
| **One process per gallery**, enforced by an advisory lock on the cache directory | two writers on one `cache.db` behind an in-process mutex |
| **The password is a CLI verb reading stdin**, argon2id, `--serve` only | hand-editing a hash into TOML |
| **Tag-write commands take a `namespace` of `user` or `set`** | a parallel command family for an identical operation |
| **One broadcast channel, not two** | a second channel whose only justification was the abolished subscriber-count signal |
| **Path confinement is a `GalleryPath` newtype**, lexical for DB-answered requests and canonicalizing before any file is opened | the ported command layer's checks, which are lexical (`trash_files`) or absent (tag writes) |
| **The trash entry id stays opaque**; the original location travels in its own field | `restore_trash` at `Device` becomes an arbitrary-file-move primitive |
| **The loopback bind is a random `127.x.x.x`**, not `127.0.0.1` | the session cookie reaches every other local port, where `SameSite` is same-site and does not help |
| **`open_with` takes an index into configured apps**, never a program name | `Owner` means arbitrary code execution, which is what makes every other finding an RCE |
| **`?fit=` stays as a grid source** with its gates and the never-warm rule | tuning with a recorded measurement behind it deleted in one bullet |
| **The service worker is deleted**; `ETag` on the thumbnail route does the caching | an offline mode nothing asked for, an exception on the ledger, and a version-bump rule that strands phones |
| **`settings.toml` holds exactly the default filter (a `Device` command) and trash retention (edited by hand)**; display preferences are `clientPrefs` everywhere | a per-gallery file described as per-client, and a default filter nobody on a `--serve` bind could set |
| **`devices.db` has its own read-only pool** | auth queuing behind a pairing write on every thumbnail request |
| **Two path types, `RelPath` and `GalleryPath`**; only the second can open a file | one type with two constructors, which cannot tell the compiler which check ran |
| **The launch token rotates on redemption and lives in `instance.json`**; no TTL | a re-mint channel nobody specified, or a token that expires before a cold browser starts |
| **`tag` refuses a gallery that is open locally**, and runs the index pass only — never enrichment | the desktop's cold run rewriting every geotagged companion over the mount |
| **On mirrored companion fields the companion wins** when present | the first fresh cache stamping its `now` onto every file's `date_added` |
| **Companion `index_state` keys on `(mtime_nanos, size)`** | a concurrent sweep skipping a rewritten file forever |
| **Companion read-modify-write is one operation under `flock`** | a phone's rating silently lost to a plugin run that read the file three seconds earlier |
| **Companions stay per directory**, beside the media | every companion outside the top directory orphaned on first open |
| **`--remote` collapses into `lightview tag <dir> --plugin <name>`**, because the desktop can mount the gallery | a distributed job broker, a second credential store, a certificate pin, and a route (`?frame=`) with one consumer |
| **The idle worker re-runs the companion index periodically** | `inotify` does not fire for NFS or SMB writes, so tags written from another machine are invisible until restart |
| **The PIN fails closed after ten attempts** | a million-code space with no rate limit, for a credential that is now per account |
| **The launch URL is always printed to stdout**; no `--no-browser` flag | a headless local mode with no way to learn its own URL, and a verification recipe depending on an undefined flag |
| **The ThumbHash moves to `media_meta`** | the items query walks the thumbnail table's overflow pages to extract 25 bytes a row |
| **The filter compiles into the sort statement** | the filter result crosses the network twice, with the client as courier |
| **The duplicate hasher decodes RGBA, and failure stores `NULL`** | every row hashes to the sentinel `0` and the whole library becomes one duplicate group |
| **`date_added` and `last_viewed` are mirrored into `meta.core`** | requirement 11 is false, and three blessed operations destroy them silently |
| **A scan that errored is not a prune authority** | an unmounted NAS at boot wipes every path-keyed row |
| **Trash retention lives in `.lightview/settings.toml`**, not `server.toml` | a desktop's default retention deletes a served gallery's trash |
| **`tag_counts` is deleted**; autocomplete aggregates at refresh | a table outside the sweep the plan says has no exceptions |
| **The cache-directory *ceiling* is enforced at gallery open and by `lightview cache --prune`** — a different mechanism from the per-tier budget; the LRU key is an explicit `last_opened` file, never `cache.db`'s mtime | unbounded growth in `~/.cache`, and an LRU that evicts the warmest gallery |

---

## 8. Deliberately not in this plan

Named so their absence reads as a decision rather than an oversight.

- **The map view, the infinite canvas, the virtual folder view.** The first is
  deleted; the other two were designed and unbuilt, and are incompatible with
  having one view. They leave the roadmap.
- **Plugin-proposed grouping** (a `groups` result kind, face clustering). See
  section 3.10 — deferred until a plugin exists that would use it, and deleted
  rather than left as a stub. Naming a group *from a selection* ships, because
  that is just a batch tag write.
- **Plugin findings** (`choice`/`confirm`/`label`, `pending::`). Deferred; no
  plugin needs them.
- **A decode-worker pool / canvas cells.** Would give up the browser's own image
  cache (measured: returning to visited positions costs +2 MB rather than
  +55 MB) in exchange for explicit `ImageBitmap.close()`. Two rationales on two
  platforms, neither measured here. Revisit only with a number.
- **A BK-tree for duplicate detection.** All-pairs is fine at this scale.
- **Per-device scopes.** All paired devices are equally trusted.
- **EXIF writing.** No path exists and acquiring one means rewriting image bytes.
- **A portable install.** Requirement 4 replaces it; see section 3.3.

**Open items carried forward** — real, and none of them blocking:

- Move the ML taggers to their own repository (section 3.10).
- Whether auth's per-request read is measurable on a real phone at real queue
  depth. Section 3.3 gives `devices.db` its own pool, so it no longer touches the
  writer; get the number before tuning further.

---
