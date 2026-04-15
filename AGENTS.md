# AGENTS.md

## Development Commands
- **Full App (Dev):** `cargo tauri dev`
- **Frontend Dev:** `npm run dev`
- **Build (Production):** `npm run tauri build`
- **Rust Checks:** `cargo check` (from `src-tauri/`)
- **Rust Tests:** `cargo test` (use `-- --exact` for single tests)
- **Benchmarks:** `cargo bench --bench <name>` (from `src-tauri/`) or `npm run bench` (frontend)

## Quality & Verification
- **Rust Linting:** `cargo fmt --check` and `cargo clippy --all-targets --all-features`
- **Frontend Types:** `npx tsc --noEmit`
- **Note:** There is no `npm run lint` script.

## Core Architecture
- **Boundary:** Frontend calls Rust via `src-solidjs/lib/ipc.ts` (canonical IPC).
- **Protocol:** Media/thumbnails are served via `lightview://` (custom URI protocol).
- **State:** Global state is in `src-tauri/src/lib.rs` (`AppState`) via `tauri::State`.
- **Lifecycle:** `commands/gallery.rs::open_gallery` manages provider registration, DB connection, and FS watching.

## Critical Implementation Notes
- **Cargo Features:** Default features include `gpu` and `custom-protocol`.
- **Linux Stability:** `main.rs` sets `GDK_BACKEND=x11` and `WEBKIT_DISABLE_DMABUF_RENDERER=1` for WebKit stability.
- **Performance:** `[profile.dev.package."*"] opt-level = 2` is intentional for image/DB workloads. Do not change.
