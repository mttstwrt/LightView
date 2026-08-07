# AGENTS.md

## Development Commands
- **Full App (Dev):** `cargo tauri dev`
- **Frontend Dev:** `npm run dev`
- **Build (Production):** `npm run tauri build`
- **Rust Checks:** `cargo check` (from `src-tauri/`)
- **Rust Tests:** `cargo test` (use `-- --exact` for single tests)
- **Benchmarks:** `cargo bench --bench <name>` (from `src-tauri/`) or `npm run bench` (frontend)

## Quality & Verification
- **Rust Linting:** `cargo clippy --all-targets --all-features` (clean of errors; ~60 style warnings remain)
- **Frontend Types:** `npx tsc --noEmit`
- **Note:** There is no `npm run lint` script.
- **`cargo fmt --check` currently FAILS** on ~70 files — the tree has never been rustfmt-formatted. Do not run `cargo fmt` inside an unrelated change, and avoid `cargo clippy --fix` (its let-chain rewrites need a reformat you can't scope). See `docs/wiki/build-and-verify.md`.
- **Build prerequisites:** `cargo check` fails in a build script without GTK/WebKitGTK and `libheif >= 1.21` (Ubuntu 24.04 ships 1.17 — needs a source build), and the `lightview` binary additionally needs `dist/` to exist (`npm run build`). Same doc.

## Deeper Reference
`docs/wiki/` — subsystem maps and cross-module invariants. Start at `docs/wiki/README.md`; read `docs/wiki/invariants.md` before changing the cache, thumbnail pipeline, or remote API.

## Core Architecture
- **Boundary:** Frontend calls Rust via `src-solidjs/lib/ipc.ts` (canonical IPC).
- **Protocol:** Media/thumbnails are served via `lightview://` (custom URI protocol).
- **State:** Global state is in `src-tauri/src/lib.rs` (`AppState`) via `tauri::State`.
- **Lifecycle:** `commands/gallery.rs::open_gallery` manages provider registration, DB connection, and FS watching.

## Critical Implementation Notes
- **Cargo Features:** Default features include `gpu` and `custom-protocol`.
- **Linux Stability:** `main.rs` sets `GDK_BACKEND=x11` and `WEBKIT_DISABLE_DMABUF_RENDERER=1` for WebKit stability.
- **Performance:** `[profile.dev.package."*"] opt-level = 2` is intentional for image/DB workloads. Do not change.
