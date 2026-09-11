# Field report — five findings from the first real run

The first build outside this repository's own harness, on a real library, on
Arch, launched from a file manager's context menu. Five findings, in the order
they were reported.

## 1. The copy/move destination picker has no pinned locations

`DirectoryPicker` opens at the gallery root and moves one level at a time
through `parent`. Reaching `~/Pictures/Archive` from a gallery at `/mnt/photos`
is four clicks up and three down, with no way to jump. Every native file dialog
has a sidebar of places; this has none.

**Requirement.** The picker offers the handful of directories a person actually
files things into, reachable in one click, on a process that may have no
desktop session to ask.

## 2. A local session never ends

`lightview <dir>` binds loopback, opens a browser, and serves forever. Closing
the window leaves the process holding the gallery lock, its watcher, its idle
worker and its thread pool, with nothing left to serve. "Open with LightView"
from a file manager is the case that makes this obvious: the process is started
by a click nobody associates with a lifetime.

**Requirement.** A local session ends when its last window closes, without
asking the user to find and kill it, and without ending a `--serve` deployment
when a phone locks its screen.

## 3. `makepkg` fails to link

A manual `cargo build --release` succeeds; the same build under `makepkg` fails
at the final link with every AWS-LC symbol undefined. `aws-lc-sys` — pulled in
by `axum-server`'s `tls-rustls` feature — compiles its C and assembly through
its own build script, which inherits makepkg's `CFLAGS`. The default `lto`
option adds `-flto=auto`, producing bytecode objects that `rust-lld`, with no
GCC LTO plugin, cannot resolve.

**Requirement.** `makepkg -si` builds the package.

## 4. Almost nothing has a date

Most files sort to the end under the default date sort and show no date in the
info panel. Only a few — the ones straight off a camera — are placed correctly.

**Requirement.** Every file has a date to sort and group by, the panel says
where that date came from, and a file that arrives while the server is running
gets one without a restart.

## 5. The justified grid leaves short rows

Rows that should fill the width sometimes stop after a few images and wrap,
leaving a ragged right edge mid-scroll rather than only at the very end.

**Requirement.** A row short of the container width is the exception a reader
can predict, not something that happens partway down a scroll.

## Out of scope

- Replacing `date_taken` with a writable field, or letting a user correct a
  date. Nothing reported asks for it and it is a durable-format commitment.
- A typed-path entry in the picker. Pins were what was asked for.
- Any new configuration. None of the five is a preference.
