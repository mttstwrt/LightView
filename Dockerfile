# syntax=docker/dockerfile:1
#
# LightView headless server — containerizes the `lightview-headless serve`
# binary (the same Rust backend + axum HTTP server the desktop app uses, minus
# the Tauri window). GPU thumbnailing is dropped: it's useless in a container,
# and cutting `wgpu` shrinks the build. See README "Headless server".
#
# Base: Arch. `libheif-sys` (via libheif-rs 2.7) needs libheif >= 1.21, which
# Debian/Ubuntu stable don't ship yet; Arch tracks the current release (matching
# a typical dev host). webkit2gtk-4.1 is pulled in transitively by the Tauri lib.

# ---- build stage ----------------------------------------------------------
FROM archlinux:latest AS build

RUN pacman -Syu --noconfirm --needed \
      rust \
      nodejs npm \
      webkit2gtk-4.1 \
      libheif \
      libayatana-appindicator \
      librsvg \
      pkgconf \
      base-devel \
 && pacman -Scc --noconfirm

WORKDIR /app
COPY . .

# Frontend SPA → dist/ (served by the headless binary).
RUN npm ci && npm run build

# Headless server binary. Drop the default `gpu` feature (no GPU in a
# container); keep `custom-protocol` so the Tauri lib builds in release mode.
RUN cargo build --release \
      --manifest-path src-tauri/Cargo.toml \
      --no-default-features --features custom-protocol \
      --bin lightview-headless

# ---- runtime stage --------------------------------------------------------
FROM archlinux:latest AS runtime

# Shared libs the binary links against at load time (webkit2gtk pulled in by
# Tauri, libheif for HEIC/HEIF, the appindicator/rsvg deps of webkit).
# ffmpeg is invoked as a subprocess for video thumbnails + metadata probing.
RUN pacman -Syu --noconfirm --needed \
      webkit2gtk-4.1 \
      libheif \
      libayatana-appindicator \
      librsvg \
      ffmpeg \
      ca-certificates \
 && pacman -Scc --noconfirm

WORKDIR /opt/lightview
COPY --from=build /app/dist ./dist
COPY --from=build /app/src-tauri/target/release/lightview-headless ./lightview-headless

# `data/` (self-signed TLS cert, server-side plugins, recent.json) is resolved
# relative to the binary → /opt/lightview/data. Mount it to persist the cert.
EXPOSE 8787
ENV RUST_LOG=info
ENTRYPOINT ["/opt/lightview/lightview-headless"]
CMD ["serve", "/gallery", "--port", "8787"]
