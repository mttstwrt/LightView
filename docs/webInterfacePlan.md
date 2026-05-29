# Web Interface — View-Only MVP Plan

## Goal

Let a remote browser on the LAN load the LightView SPA and browse the
**currently-open** gallery read-only: grid, sort, filter, map, full-res image
viewing, video playback with seeking, and metadata/tags display. Behind a
bearer token. No folder picking, editing, file operations, or plugins.

This is the smallest safe slice. Read-write parity and a server-side folder
browser are explicitly out of scope for the MVP.

## Why it's an MVP and not a rewrite

The media/auth/config foundation already exists:

- `src-tauri/src/http_server/` is a real axum server that streams
  `/media/{*path}` with Range support and HEIC transcoding (`routes.rs`).
- `config.rs` already exposes `bind: IpAddr` and `AuthMode::BearerToken`.
- `middleware.rs` already implements bearer-token auth (constant-time compare,
  `Authorization: Bearer` **and** `?token=` query support).
- `mod.rs` docstring anticipates this: *"When remote access lands, flipping
  `bind` to `0.0.0.0` and setting `AuthMode::BearerToken(_)` is the whole
  change."* — true for media; the rest below is the remaining work.

The WebGL renderer + `img.src` thumbnail loading work unchanged in any browser.
The BC7 atlas is a backend GPU optimization, not used for webview display, so
it is not a blocker.

## The four gaps this plan closes

1. Thumbnails are served via the `lightview://thumb/...` custom protocol, which
   a remote browser cannot resolve. → Add HTTP routes.
2. The whole control plane is Tauri `invoke()` (~60 commands). → Add a
   read-only HTTP API bridge.
3. The frontend leans on Tauri-only APIs (dialog, window, events). → Guard
   behind a runtime mode flag.
4. `/media/{*path}` serves any absolute path on the host. → Confine to the
   gallery root before binding non-loopback.

---

## Backend tasks

1. **Make `AppState` shareable into the server.** All fields are already `Arc`,
   so derive `Clone` on `AppState` (`lib.rs`). Extend `ServerState`
   (`http_server/server.rs`) to hold an `AppState` clone alongside `config`.

2. **Extract read-only command bodies into plain functions** of the form
   `async fn foo(state: &AppState, ...) -> Result<T, String>`, and have the
   existing `#[tauri::command]` wrapper call it (`tauri::State` derefs to
   `&AppState`, so desktop behavior is unchanged). Needed set:
   - `get_gallery_info`, `get_sorted_items`, `get_timeline_index`
   - `apply_filter`, `clear_filter` (inline SQL in `filter.rs` moves into the shared fn)
   - `get_media_meta`, `get_tags`, `get_thumbhashes`
   - `get_geo_points`, `get_geo_paths`
   - `autocomplete_tags`, `get_recent_tags`

3. **HTTP API bridge.** New axum route `POST /api/invoke` taking
   `{ command, args }`. `match` on `command` against the **read-only allowlist**
   above; deserialize `args`, dispatch, serialize result. The server-side
   allowlist is defense in depth — a write command name is rejected even if a
   client forges it.

4. **Shared thumbnail serving.** Lift `read_cached_thumbnail`,
   `serve_thumbnail_fast`, `serve_thumbnail_generate`, `serve_thumbhash` out of
   `main.rs` into a shared module (they already only need `&AppState`). Tauri
   protocol handler calls the shared fns (no behavior change). Add axum routes
   `GET /thumb/{tier}/{*path}` and `GET /thumbhash/{*path}` calling the same fns.

5. **Path confinement (security, mandatory for remote).** Before serving any
   path in `/media` and `/thumb`, canonicalize and verify it is under the open
   gallery root (`state.current_gallery`); reject otherwise.

6. **Static SPA serving.** Add tower-http `ServeDir` for the built `dist/` with
   an `index.html` SPA fallback so the browser loads the app at `/`.

7. **Auth + bind toggle.** A "Enable remote access" setting restarts the server
   with `bind: 0.0.0.0` and `AuthMode::BearerToken(<generated>)`. Token gates
   `/api`, `/media`, `/thumb` (via `?token=` so `img.src`/`<video>` work).
   Settings UI shows the URL + token (optionally a QR). Default stays
   loopback / no-auth.

## Frontend tasks

8. **Runtime mode detection.** Detect Tauri (`window.__TAURI_INTERNALS__`) vs
   browser once at startup.

9. **HTTP transport shim in `ipc.ts`.** Swap the inner `_rawInvoke` so browser
   mode does `fetch('/api/invoke', { command, args })` with the token; Tauri
   mode uses the real invoke. Every typed wrapper in `ipc.ts` stays untouched.

10. **URL builders for browser mode.** `thumbUrl` / `thumbhashUrl` switch from
    `lightview://` to HTTP base + `?token=`. `mediaUrl` already targets the HTTP
    server — just append the token.

11. **Guard Tauri-only UI behind mode.** In browser mode: hide folder pickers
    (`App.tsx`, `SettingsMenu`, `ContextMenu`), guard `getCurrentWindow()`,
    no-op `listen()` (events), and hide all edit affordances (rating stars,
    tag/notes inputs, color labels, file-op context menu, plugin actions,
    install/reindex/clear-cache).

12. **Token bootstrap.** Open via a one-time URL containing the token; store in
    `localStorage`; append to all requests.

## Known MVP limitations

- No-op'd `listen()` means a remote view won't auto-refresh if the desktop
  opens a different folder or while thumbnails are still generating. Acceptable
  for view-only; revisit with SSE/WebSocket in a later phase.

## Verification

- `cargo tauri dev` — desktop regression: protocol + media playback unchanged.
- Build frontend, hit the axum server from a second device's browser: grid
  loads thumbnails; sort/filter/map work; images and a video play with seeking;
  metadata shows.
- Forged write command (`POST /api/invoke {command:"trash_files"}`) is rejected.

## Implementation status (MVP built)

Backend:
- `AppState` derives `Clone`; `ServerState` carries an `AppState` handle.
- Read-only command bodies extracted to `*_impl(&AppState, ...)` fns; Tauri
  wrappers delegate to them.
- `POST /api/invoke` bridge with a server-side read-only allowlist
  (`http_server/api.rs`).
- Thumbnail serving lifted into `thumb_serve.rs`; axum `/thumb/{tier}/{*path}`
  and `/thumbhash/{*path}` routes added; the Tauri protocol handler reuses them.
- Path confinement (`path_in_gallery`) on `/media` and `/thumb`.
- SPA served via `ServeDir` + `index.html` fallback; auth scoped to data routes.
- `HttpConfig::remote(...)`; `enable_remote_access` / `disable_remote_access` /
  `get_remote_access_info` commands run a separate 0.0.0.0 + bearer-token server
  sharing `AppState` (loopback video server untouched).
- Auth middleware also accepts an `lv_token` cookie.

Frontend:
- `lib/runtime.ts`: `isTauri`/`isWeb`, `initWebAuth` (URL token → cookie),
  `safeListen` (no-ops in web).
- `ipc.ts`: transport switches between Tauri `invoke` and `POST /api/invoke`;
  `thumbUrl`/`thumbhashUrl`/`mediaUrl` emit same-origin paths in web mode.
- Web client auto-loads the desktop's open gallery (read-only) on mount.
- Tauri-only UI guarded in web mode: folder pickers, window controls, context
  menu, selection bar, InfoPanel editing, plus maintenance/plugins/dedup in the
  settings menu. Remote-access toggle added to the settings menu (desktop only).

Known limitation: events are no-op'd in web mode, so a remote view won't
auto-refresh if the desktop opens a different folder. Revisit with SSE later.

Not yet done: live runtime verification from a second device (build + type
checks pass; the GUI/browser flow has not been exercised here).
