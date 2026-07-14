---
name: verify
description: Build, launch, and drive the headless server to verify backend/web changes end-to-end without a display.
---

# Verifying LightView changes (headless surface)

The full recipe (build, throwaway gallery, serve, PIN pairing, cookie, SSE,
worker/tagging loop) lives in CLAUDE.md under "Headless test server" — start
there. Gotchas learned the hard way:

- `/thumb/{tier}/{*path}`: tier is one of `s`/`m`/`l`/`p` (not `grid`), and
  `path` is the **absolute** media path with the leading `/` stripped —
  `https://localhost:PORT/thumb/m/tmp/.../gallery/sub/clip.mp4`. Same
  convention for `/media` and `/thumbhash`. Auth cookie required.
- Companion files: `<dir>/.lightview/companions/<name>.lightview.json`
  **per directory** (each subfolder has its own tree), or alongside as
  `<name>.lightview.json`. Minimum valid JSON needs `schema_version: 1`,
  `file`, `file_hash`, `media_type` (`"image"`/`"video"`/`"gif"`),
  `created`/`modified` (RFC3339), `tags: {user, auto, plugins}`, `meta`.
- Companion indexing + rating backfill run in a background task after the
  grid data is ready — `sleep 3` after server start before asserting on
  tags/ratings via `apply_filter` / `get_media_meta` on `/api/invoke`.
- Startup log lines need `RUST_LOG=info` in the environment.
- Kill the server with `pkill -f 'lightview-headles[s] serve'` — the bracket
  keeps pkill from matching (and killing) your own shell command line, which
  otherwise dies with exit 144. The bracket does NOT save you if another part
  of the same compound command contains the literal string (e.g. a later
  `lightview-headless serve` restart) — run the pkill as its own Bash call.
- `apply_filter` matches user tags as bare terms (`{"query":"my-tag"}`);
  `tag:my-tag` and `user:my-tag` silently return `[]`. Namespaced forms in
  the grammar: `type:image`, `has::plugin.<name>`.
- Test video for ffmpeg paths:
  `ffmpeg -f lavfi -i testsrc=duration=1:size=128x128:rate=10 -pix_fmt yuv420p clip.mp4`
