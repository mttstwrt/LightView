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

# Downloaded packages live in a cache mount, so they never enter the layer —
# which also makes `pacman -Scc` pointless here (it would just wipe the mount).
# The runtime stage mounts the same cache and installs an overlapping set
# (webkit2gtk-4.1 alone is ~150 MB), so sharing one is worth the `locked`
# serialization on a cold cache.
RUN --mount=type=cache,target=/var/cache/pacman/pkg,sharing=locked \
    pacman -Syu --noconfirm --needed \
      rust \
      nodejs npm \
      webkit2gtk-4.1 \
      libheif \
      libayatana-appindicator \
      librsvg \
      pkgconf \
      base-devel

WORKDIR /app
COPY . .

# Optional short commit id baked into the SPA's build stamp (Settings → About).
# .git is dockerignored, so pass it explicitly to get a real commit:
#   GIT_SHA=$(git rev-parse --short HEAD) docker compose build
# Left empty, the build-time stamp alone still tells rebuilds apart.
ARG GIT_SHA=""
ENV VITE_GIT_SHA=$GIT_SHA

# Frontend SPA → dist/. This must run *before* cargo: the Rust build embeds
# dist/ into the binary (http_server::web_assets) and fails to compile without
# it. Only npm's download cache is mounted; node_modules/ and dist/ stay real
# files in the layer.
RUN --mount=type=cache,target=/root/.npm \
    npm ci && npm run build

# Headless server binary. Drop the default `gpu` feature (no GPU in a
# container); keep `custom-protocol` so the Tauri lib builds in release mode.
#
# Cargo's registry/git checkouts and the target dir are cache mounts, so an
# unchanged dependency tree isn't recompiled on a rebuild (CARGO_HOME is root's
# default, /root/.cargo). Note that a cache mount is NOT part of the image:
# target/release/ vanishes when this step ends, so the binary must be copied
# somewhere real *inside* this same RUN or the runtime stage can't COPY it.
RUN --mount=type=cache,target=/root/.cargo/registry,sharing=locked \
    --mount=type=cache,target=/root/.cargo/git,sharing=locked \
    --mount=type=cache,target=/app/src-tauri/target \
    cargo build --release \
      --manifest-path src-tauri/Cargo.toml \
      --no-default-features --features custom-protocol \
      --bin lightview-headless \
 && cp src-tauri/target/release/lightview-headless /usr/local/bin/

# ---- runtime stage --------------------------------------------------------
FROM archlinux:latest AS runtime

# Shared libs the binary links against at load time (webkit2gtk pulled in by
# Tauri, libheif for HEIC/HEIF, the appindicator/rsvg deps of webkit).
# ffmpeg is invoked as a subprocess for video thumbnails + metadata probing.
RUN --mount=type=cache,target=/var/cache/pacman/pkg,sharing=locked \
    pacman -Syu --noconfirm --needed \
      webkit2gtk-4.1 \
      libheif \
      libayatana-appindicator \
      librsvg \
      ffmpeg \
      ca-certificates

WORKDIR /opt/lightview
# The SPA is compiled into the binary, so the runtime image carries no dist/ —
# there is nothing left that can fall out of step with the executable.
# Staged to /usr/local/bin by the build stage — target/ is a cache mount and so
# doesn't exist in that stage's filesystem.
COPY --from=build /usr/local/bin/lightview-headless ./lightview-headless

# `data/` (self-signed TLS cert, server-side plugins, recent.json) is resolved
# relative to the binary → /opt/lightview/data. Mount it to persist the cert.
EXPOSE 8787
ENV RUST_LOG=info
ENTRYPOINT ["/opt/lightview/lightview-headless"]
CMD ["serve", "/gallery", "--port", "8787"]
