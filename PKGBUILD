# Maintainer: LightView
#
# The desktop half of the two deployments: the local viewer and `lightview tag`,
# on one machine. Arch because it tracks a current libheif and ffmpeg, leaves
# CUDA and a plugin's own Python venv alone, puts `lightview` on PATH, and makes
# the .desktop file work.
#
#   makepkg -si
#
# **A package is one executable and two small files.** The SPA is compiled into
# the binary, so there is nothing to install alongside it and nothing that can
# fall out of step with it.
#
# Nothing is installed into a shared writable location: all state is per-user
# under XDG, so this creates no directories at install time and needs no
# post-install script.

pkgname=lightview
pkgver=0.1.0
pkgrel=1
pkgdesc="A fast, plugin-extensible media gallery"
arch=('x86_64')
url="https://github.com/mttstwrt/LightView"
license=('GPL-3.0-or-later')

# libheif is linked, not shelled out to. ffmpeg and ffprobe are subprocesses,
# for video thumbnails and frame extraction — without them clips fall back to a
# placeholder rather than failing. xdg-utils opens the browser in local mode.
depends=('libheif' 'ffmpeg' 'xdg-utils')

# Node is a build dependency because `dist/` must exist before any cargo
# command: the SPA is embedded into the library, not read from disk at runtime.
makedepends=('rust' 'nodejs' 'npm')

# aws-lc-sys (pulled in by axum-server's tls-rustls feature) compiles AWS-LC's
# C/assembly crypto sources through its own build script, which picks up
# makepkg's injected CFLAGS/LDFLAGS. Under this system's default LTO option
# that adds -flto=auto, producing LTO-bytecode objects that rust-lld (no GCC-LTO
# plugin) cannot resolve symbols from at final link — every AWS-LC symbol comes
# up undefined. Opt out for this package rather than hand-stripping CFLAGS.
options=('!lto')

source=()

build() {
  cd "$startdir"
  npm ci
  npm run build
  cargo build --release --manifest-path src-rust/Cargo.toml --locked
}

check() {
  cd "$startdir"
  cargo test --release --manifest-path src-rust/Cargo.toml --locked
}

package() {
  cd "$startdir"
  install -Dm755 src-rust/target/release/lightview "$pkgdir/usr/bin/lightview"
  install -Dm644 lightview.desktop "$pkgdir/usr/share/applications/lightview.desktop"
  install -Dm644 src-solidjs/public/icons/icon-512.png \
    "$pkgdir/usr/share/icons/hicolor/512x512/apps/lightview.png"
}
