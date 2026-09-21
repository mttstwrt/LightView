# syntax=docker/dockerfile:1
#
# LightView, serving a gallery over the LAN: `lightview --serve /gallery`.
#
# **The image carries no graphical stack.** It used to build a Tauri library for
# a process with no window, pulling in webkit2gtk-4.1, GTK, libayatana and
# librsvg — around 150 MB of shared objects nothing called. There is one binary
# now and it is an HTTP server, so the runtime stage links `libheif` and shells
# out to `ffmpeg`, and that is the whole list.
#
# Base: Arch. `libheif-rs` needs libheif >= 1.21 and Debian/Ubuntu stable ship
# 1.17, which forces a source build on a Debian-family *host*. The image
# sidesteps that entirely, which is most of the reason it exists.

# ---- build stage ----------------------------------------------------------
FROM archlinux:latest AS build

# Downloaded packages live in a cache mount, so they never enter the layer —
# which also makes `pacman -Scc` pointless here (it would just wipe the mount).
RUN --mount=type=cache,target=/var/cache/pacman/pkg,sharing=locked \
    pacman -Syu --noconfirm --needed \
      rust \
      nodejs npm \
      libheif \
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
# dist/ into the binary and will not compile without it. Only npm's download
# cache is mounted; node_modules/ and dist/ stay real files in the layer.
RUN --mount=type=cache,target=/root/.npm \
    npm ci && npm run build

# Cargo's registry, git checkouts and target dir are cache mounts, so an
# unchanged dependency tree is not recompiled on a rebuild. A cache mount is
# NOT part of the image: target/release/ vanishes when this step ends, so the
# binary must be copied somewhere real *inside* this same RUN.
RUN --mount=type=cache,target=/root/.cargo/registry,sharing=locked \
    --mount=type=cache,target=/root/.cargo/git,sharing=locked \
    --mount=type=cache,target=/app/src-rust/target \
    cargo build --release --manifest-path src-rust/Cargo.toml \
 && cp src-rust/target/release/lightview /usr/local/bin/

# ---- runtime stage --------------------------------------------------------
FROM archlinux:latest AS runtime

# libheif is linked, not shelled out to; ffmpeg is a subprocess, for video
# thumbnails and frame extraction. Without ffmpeg clips fall back to a
# placeholder rather than failing, so it is a soft dependency — but a gallery
# with videos wants it.
RUN --mount=type=cache,target=/var/cache/pacman/pkg,sharing=locked \
    pacman -Syu --noconfirm --needed \
      libheif \
      ffmpeg \
      ca-certificates

# The SPA is compiled into the binary, so the runtime image carries no dist/ —
# there is nothing left that can fall out of step with the executable.
COPY --from=build /usr/local/bin/lightview /usr/local/bin/lightview

# One directory for all three XDG roots — cache/, data/ and config/ under it.
# `data/` holds the TLS certificate and the device pairings, `config/` holds
# server.toml, and `cache/` holds the derived thumbnails. Mount it to keep the
# certificate (and therefore every device's trust in it) across rebuilds.
#
# **The cache is not inside the gallery.** A gallery directory on a share is
# the one tree a person greps, rsyncs and backs up, and a SQLite database in it
# is the thing that breaks all three.
EXPOSE 8443
ENV RUST_LOG=info
ENTRYPOINT ["/usr/local/bin/lightview"]
CMD ["--serve", "/gallery", "--port", "8443", "--data-dir", "/state"]
