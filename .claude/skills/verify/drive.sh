#!/usr/bin/env bash
# Drive the real binary end to end, with no display and no browser.
set -uo pipefail

BIN=${BIN:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)/src-rust/target/debug/lightview"}
WORK=$(mktemp -d)
G="$WORK/gallery"
D="$WORK/state"
mkdir -p "$G/2026/january"

pass=0; fail=0
ok()   { echo "  ok   $*"; pass=$((pass+1)); }
bad()  { echo "  FAIL $*"; fail=$((fail+1)); }
check(){ if [ "$2" = "$3" ]; then ok "$1 ($2)"; else bad "$1: expected $3, got $2"; fi }

echo "== fixtures =="
ffmpeg -y -v error -f lavfi -i testsrc=size=800x600:duration=1 -frames:v 1 "$G/2026/january/wide.png" 2>/dev/null
ffmpeg -y -v error -f lavfi -i testsrc=size=480x640:duration=1 -frames:v 1 "$G/tall.png" 2>/dev/null
# Dated deliberately, and far from a year boundary: the stored value is a wall
# clock, so the day it reads back as depends on the host's zone, but the year
# does not -- and the file's own mtime is today, so a 2019 here can only have
# come from the container.
ffmpeg -y -v error -f lavfi -i "testsrc=size=320x240:duration=2:rate=10" -c:v libx264 -pix_fmt yuv420p \
  -metadata creation_time="2019-07-14T09:22:33.000000Z" "$G/clip.mp4" 2>/dev/null
ls "$G"/2026/january/wide.png "$G"/tall.png "$G"/clip.mp4 >/dev/null && ok "gallery built" || bad "gallery"

echo "== help and argument errors =="
"$BIN" --help >/dev/null 2>&1 && ok "--help" || bad "--help"
"$BIN" --srve /tmp >/dev/null 2>&1; check "a mistyped flag exits 2" "$?" "2"
"$BIN" /does/not/exist --data-dir "$D" >/dev/null 2>&1; check "a missing gallery exits 1" "$?" "1"

echo "== host administration =="
PIN=$("$BIN" pair --data-dir "$D" 2>/dev/null)
if [[ "$PIN" =~ ^[0-9]{6}$ ]]; then ok "pair prints a 6-digit PIN"; else bad "pair printed: $PIN"; fi
"$BIN" devices --data-dir "$D" 2>/dev/null | grep -q "no paired devices" && ok "devices lists none yet" || bad "devices"
echo "hunter2" | "$BIN" password --data-dir "$D" >/dev/null 2>&1 && ok "password set" || bad "password"
grep -q 'password_hash = "\$argon2id' "$D/config/server.toml" && ok "argon2id hash in server.toml" || bad "no hash"
"$BIN" password --clear --data-dir "$D" >/dev/null 2>&1 && ok "password cleared" || bad "password --clear"
"$BIN" cache --data-dir "$D" 2>/dev/null | grep -q "galleries" && ok "cache prints its directory" || bad "cache"

echo "== lightview <dir> : loopback, Owner =="
"$BIN" "$G" --data-dir "$D" > "$WORK/open.log" 2>"$WORK/open.err" &
OPEN_PID=$!
for _ in $(seq 1 50); do [ -s "$WORK/open.log" ] && break; sleep 0.2; done
URL=$(head -1 "$WORK/open.log")
BASE=${URL%%/?t=*}
TOKEN=${URL##*t=}

if [[ "$BASE" =~ ^http://127\.[0-9]+\.[0-9]+\.[0-9]+:[0-9]+$ ]]; then
  ok "bound a random 127.x.x.x: $BASE"
else
  bad "unexpected launch URL: $URL"
fi
[ "$BASE" != "http://127.0.0.1" ] && ok "not 127.0.0.1" || bad "bound the shared loopback address"

J="$WORK/jar"
code() { curl -s -o "$WORK/body" -w '%{http_code}' "$@"; }

check "healthz" "$(code "$BASE/healthz")" "200"
check "an unauthenticated command is 401" \
  "$(code -X POST -H 'content-type: application/json' -d '{"command":"get_items"}' "$BASE/api/invoke")" "401"
grep -q "session has ended" "$WORK/body" && ok "401 body is the dead end, not a pairing redirect" || bad "401 body: $(cat "$WORK/body")"

check "a loopback thumb is 401 before redemption" "$(code "$BASE/thumb/j/tall.png")" "401"
check "redeeming the launch token" \
  "$(code -c "$J" -X POST -H 'content-type: application/json' -H "origin: $BASE" -d "{\"token\":\"$TOKEN\"}" "$BASE/auth/launch")" "200"
check "the same token a second time is 401" \
  "$(code -X POST -H 'content-type: application/json' -H "origin: $BASE" -d "{\"token\":\"$TOKEN\"}" "$BASE/auth/launch")" "401"

inv() {
  local args=${2-}; [ -z "$args" ] && args='{}'
  curl -s -b "$J" -H 'content-type: application/json' -H "origin: $BASE" \
    -d "{\"command\":\"$1\",\"args\":$args}" "$BASE/api/invoke"
}

ITEMS=$(inv get_items)
echo "$ITEMS" | jq -e '.items | length == 3' >/dev/null && ok "get_items returned all three files" || bad "items: $(echo "$ITEMS" | head -c 200)"
echo "$ITEMS" | jq -e '[.items[].path] | index("2026/january/wide.png")' >/dev/null && ok "nested paths are gallery-relative" || bad "paths"

# The clip has never been thumbnailed at this point -- nothing has requested a
# tier for it -- so a duration here can only have come from the index. It is
# what the grid's short-video autoplay reads, and it was null for every video
# ever indexed until the container was probed when the file was indexed.
# Enrichment is spawned, not awaited, so the URL can be printed before the
# clip has been probed. Waiting for it is the honest shape of the check.
for _ in $(seq 1 25); do
  DUR=$(inv get_items | jq -r '.items[] | select(.path == "clip.mp4") | .duration')
  [ "$DUR" != "null" ] && [ -n "$DUR" ] && break
  sleep 0.2
done
awk -v d="$DUR" 'BEGIN { exit !(d > 1) }' 2>/dev/null \
  && ok "a video carries its duration before anything thumbnails it ($DUR s)" \
  || bad "clip duration: $DUR"
inv get_items | jq -e '.items[] | select(.path == "2026/january/wide.png") | .duration == null' >/dev/null \
  && ok "an image has no duration" || bad "an image reported a duration"

# Same argument as the duration check above: nothing has asked for a tier for
# these files yet, so dimensions here can only have come from the header read at
# index time. Without them the grid lays a file out as a square and repacks
# every row below it when the thumbnail arrives to correct the guess.
DIMS=$(inv get_items | jq -c '[.items[] | select(.path == "tall.png") | {width, height}]')
check "an image carries its shape before anything thumbnails it" "$DIMS" '[{"width":480,"height":640}]'
UNSHAPED=$(inv get_items | jq '[.items[] | select(.media_type != "video") | select(.width == null)] | length')
check "no image is left without a shape" "$UNSHAPED" "0"

CLIP_DATE=$(inv get_items | jq -r '.items[] | select(.path == "clip.mp4") | .date')
CLIP_YEAR=$(date -u -d "@$CLIP_DATE" +%Y 2>/dev/null)
check "a video sorts by the date in its container, not its mtime" "$CLIP_YEAR" "2019"

CAPS=$(inv get_capabilities)
echo "$CAPS" | jq -e '.trust == "owner"' >/dev/null && ok "capabilities report owner" || bad "caps: $CAPS"

# The Owner half: the picker.
DIRS=$(inv list_dirs)
echo "$DIRS" | jq -e '[.entries[].name] | index("2026")' >/dev/null && ok "list_dirs defaults to the gallery root" || bad "dirs: $DIRS"

# The picker's sidebar. Every entry must be a directory that exists, or the
# shortcut is a dead end -- which is the whole reason the server builds this
# list rather than the client guessing at conventional names.
echo "$DIRS" | jq -e '.places[0].label == "Gallery"' >/dev/null \
  && ok "the sidebar leads with the gallery" || bad "places: $(echo "$DIRS" | jq -c .places)"
MISSING=0
for pl in $(echo "$DIRS" | jq -r '.places[].path'); do
  [ -d "$pl" ] || { MISSING=1; echo "    not a directory: $pl"; }
done
[ "$MISSING" = "0" ] && ok "every pinned place exists" || bad "the sidebar offers a dead end"
echo "$DIRS" | jq -e '.parent != null' >/dev/null && ok "the listing carries its parent, so the picker walks up" || bad "no parent: $DIRS"

for tier in js j jm jh; do
  ct=$(curl -s -b "$J" -o "$WORK/t.webp" -w '%{content_type}' "$BASE/thumb/$tier/tall.png")
  if [ "$ct" = "image/webp" ] && head -c4 "$WORK/t.webp" | grep -q RIFF; then
    ok "tier $tier serves WebP ($(stat -c%s "$WORK/t.webp") bytes)"
  else
    bad "tier $tier: content-type $ct"
  fi
done

ETAG=$(curl -s -b "$J" -D - -o /dev/null "$BASE/thumb/j/tall.png" | grep -i '^etag:' | tr -d '\r' | cut -d' ' -f2)
check "a matching ETag is 304" "$(code -b "$J" -H "if-none-match: $ETAG" "$BASE/thumb/j/tall.png")" "304"

check "a range request is 206" "$(code -b "$J" -H 'range: bytes=0-9' "$BASE/media/tall.png")" "206"
# `--path-as-is` is load-bearing: without it curl collapses the `..` segments
# itself and sends `GET /etc/passwd`, which never reaches the media route at
# all — so the check passed for years while testing nothing. Both spellings are
# here because they fail differently: the raw one is rejected by `RelPath`, the
# encoded one by axum before routing.
check "a raw traversal is 404" \
  "$(code -b "$J" --path-as-is "$BASE/media/../../../../etc/passwd")" "404"
check "an encoded traversal is 404" \
  "$(code -b "$J" "$BASE/media/..%2f..%2f..%2f..%2fetc%2fpasswd")" "404"
grep -q "root:" "$WORK/body" && bad "a traversal returned /etc/passwd" || ok "no host file came back"

# The SPA is embedded in the binary, so `/` serves the real app.
check "the SPA is served at /" "$(code -b "$J" "$BASE/")" "200"
grep -q '<div id="root">' "$WORK/body" && ok "/ is the built SPA, not a placeholder" || bad "/ served: $(head -c 120 "$WORK/body")"
check "an unknown route falls back to the SPA" "$(code -b "$J" "$BASE/pair")" "200"

# The video decoder, with ffmpeg present.
curl -s -b "$J" -o "$WORK/clip.webp" "$BASE/thumb/j/clip.mp4"
if head -c4 "$WORK/clip.webp" | grep -q RIFF && [ "$(stat -c%s "$WORK/clip.webp")" -gt 1000 ]; then
  ok "a clip thumbnails to a real frame ($(stat -c%s "$WORK/clip.webp") bytes)"
else
  bad "clip thumbnail: $(stat -c%s "$WORK/clip.webp" 2>/dev/null) bytes"
fi

# A tag write, all the way to the companion file and back into the index.
inv add_tags '{"paths":["tall.png"],"tags":["vacation"],"namespace":"user"}' >/dev/null
inv add_tags '{"paths":["tall.png"],"tags":["burst-3"],"namespace":"set"}' >/dev/null
COMPANION="$G/.lightview/companions/tall.png.lightview.json"
if [ -f "$COMPANION" ]; then
  jq -e '.tags.user == ["vacation"] and .tags.set == ["burst-3"]' "$COMPANION" >/dev/null \
    && ok "tags landed in the companion under both namespaces" || bad "companion: $(cat "$COMPANION")"
else
  bad "no companion written at $COMPANION"
fi
echo "$(inv get_items '{"filter":"set::burst-3"}')" | jq -e '.items | length == 1' >/dev/null \
  && ok "a set:: filter runs against the index" || bad "set filter"
echo "$(inv get_items '{"filter":"user::vacation"}')" | jq -e '.items | length == 1' >/dev/null \
  && ok "a user:: filter runs against the index" || bad "user filter"

# A plugin namespace must not be writable through the tag commands.
check "a plugin namespace is refused" \
  "$(code -b "$J" -H 'content-type: application/json' -H "origin: $BASE" \
     -d '{"command":"add_tags","args":{"paths":["tall.png"],"tags":["x"],"namespace":"plugin.wd"}}' \
     "$BASE/api/invoke")" "400"

# The watcher: a file copied in appears without a restart.
cp "$G/tall.png" "$G/2026/dropped.png"
for _ in $(seq 1 40); do
  n=$(inv get_items | jq '.items | length')
  [ "$n" = "4" ] && break
  sleep 0.25
done
check "the watcher ingested a new file" "$(inv get_items | jq '.items | length')" "4"

echo "== the Custom order =="
# Arranging is refused until the open-time companion sweep has run; the checks
# above waited on enrichment, but wait for this one honestly too.
for _ in $(seq 1 25); do
  LOCK=$(inv lock_set '{"name":"comic","paths":["clip.mp4","tall.png"]}')
  echo "$LOCK" | jq -e '.changed' >/dev/null 2>&1 && break
  sleep 0.2
done
echo "$LOCK" | jq -e '.changed == 2' >/dev/null && ok "lock_set locked two files" || bad "lock_set: $LOCK"
CUSTOM=$(inv get_items '{"sort":"custom"}')
check "a block keeps the order it was given" \
  "$(echo "$CUSTOM" | jq -c '[.items[] | select(.block == "comic") | .path]')" '["clip.mp4","tall.png"]'
check "a block is contiguous under Custom" \
  "$(echo "$CUSTOM" | jq '.items | map(.path) | index("tall.png") - index("clip.mp4")')" "1"
check "Custom has no group headers" \
  "$(inv get_items '{"sort":"custom","group_by":{"type":"time_period","granularity":"month"}}' | jq -c .groups)" "[]"
check "no other sort names a block" "$(inv get_items | jq '[.items[] | select(.block != null)] | length')" "0"

inv place '{"path":"2026/january/wide.png","after":"tall.png","before":null}' >/dev/null
check "a placed file lands right behind its anchor" \
  "$(inv get_items '{"sort":"custom"}' | jq '.items | map(.path) | index("2026/january/wide.png") - index("tall.png")')" "1"
jq -e '.meta.order.set == "comic" and (.meta.order.key | type) == "string" and (.meta.order.pos | type) == "string"' \
  "$G/.lightview/companions/tall.png.lightview.json" >/dev/null \
  && ok "the block lives in its members' sidecars" || bad "tall.png sidecar: $(jq -c .meta "$G/.lightview/companions/tall.png.lightview.json")"
jq -e '.meta.order.set == null and (.meta.order.key | type) == "string"' \
  "$G/2026/january/.lightview/companions/wide.png.lightview.json" >/dev/null \
  && ok "a placement lives in the placed file's sidecar" || bad "wide.png sidecar"
check "a file already in a block cannot join another" \
  "$(code -b "$J" -H 'content-type: application/json' -H "origin: $BASE" \
     -d '{"command":"lock_set","args":{"name":"zine","paths":["tall.png"]}}' "$BASE/api/invoke")" "500"
grep -q 'already in the ordered set' "$WORK/body" && ok "the refusal names the block" || bad "refusal: $(cat "$WORK/body")"
ARRANGED=$(inv get_items '{"sort":"custom"}' | jq -c '[.items[].path]')

# Another machine's arrangement: a sidecar edited behind the server's back must
# reach every open window as one event, and a view must not.
curl -s -N -b "$J" -H "origin: $BASE" "$BASE/api/events" > "$WORK/events" &
EVENTS_PID=$!
sleep 0.5
DROPPED="$G/2026/.lightview/companions/dropped.png.lightview.json"
inv record_view '{"path":"2026/dropped.png"}' >/dev/null
sleep 1
grep -q 'order-changed' "$WORK/events" && bad "a view announced a reorder" || ok "a view is not a reorder"
jq '.meta.order = {"key":"0"}' "$DROPPED" > "$WORK/edited.json" && mv "$WORK/edited.json" "$DROPPED"
for _ in $(seq 1 20); do grep -q 'order-changed' "$WORK/events" && break; sleep 0.25; done
grep -q 'order-changed' "$WORK/events" && ok "a sidecar edited elsewhere arrives as order-changed" || bad "no order-changed: $(tail -c 300 "$WORK/events")"
kill $EVENTS_PID 2>/dev/null; wait $EVENTS_PID 2>/dev/null
check "the hand-edited placement took effect" \
  "$(inv get_items '{"sort":"custom"}' | jq -r '.items[0].path')" "2026/dropped.png"
ARRANGED=$(inv get_items '{"sort":"custom"}' | jq -c '[.items[].path]')

# A second launch opens the first one's window instead of refusing.
SECOND=$("$BIN" "$G" --data-dir "$D" 2>/dev/null | head -1)
[ "${SECOND%%/?t=*}" = "$BASE" ] && ok "a second launch printed the running URL" || bad "second launch: $SECOND"

kill $OPEN_PID 2>/dev/null; wait $OPEN_PID 2>/dev/null

echo "== the Custom order survives a lost cache =="
# The cache is derived; the arrangement is not. Delete every cache this data
# dir holds, reopen, and the order must come back from the sidecars alone.
rm -rf "$D/cache"
"$BIN" "$G" --data-dir "$D" > "$WORK/reopen.log" 2>"$WORK/reopen.err" &
REOPEN_PID=$!
for _ in $(seq 1 50); do [ -s "$WORK/reopen.log" ] && break; sleep 0.2; done
URL=$(head -1 "$WORK/reopen.log"); BASE=${URL%%/?t=*}; TOKEN=${URL##*t=}
J="$WORK/jar2"
code -c "$J" -X POST -H 'content-type: application/json' -H "origin: $BASE" -d "{\"token\":\"$TOKEN\"}" "$BASE/auth/launch" >/dev/null
for _ in $(seq 1 50); do
  AGAIN=$(inv get_items '{"sort":"custom"}' | jq -c '[.items[].path]')
  [ "$AGAIN" = "$ARRANGED" ] && break
  sleep 0.2
done
check "a rebuilt cache restores the arrangement" "$AGAIN" "$ARRANGED"
kill $REOPEN_PID 2>/dev/null; wait $REOPEN_PID 2>/dev/null

echo "== lightview --serve : LAN, Device, TLS =="
"$BIN" --serve "$G" --port 18443 --data-dir "$D" > "$WORK/serve.log" 2>"$WORK/serve.err" &
SERVE_PID=$!
for _ in $(seq 1 60); do curl -sk https://127.0.0.1:18443/healthz >/dev/null 2>&1 && break; sleep 0.25; done

check "TLS healthz" "$(curl -sk -o /dev/null -w '%{http_code}' https://127.0.0.1:18443/healthz)" "200"
curl -sk https://127.0.0.1:18443/cert | grep -q "BEGIN CERTIFICATE" && ok "/cert serves a PEM" || bad "/cert"
curl -sk https://127.0.0.1:18443/auth/status | jq -e '.trust == "device" and .pairing == true' >/dev/null \
  && ok "auth/status reports a device bind with pairing" || bad "auth/status"

SPIN=$("$BIN" pair --data-dir "$D" 2>/dev/null)
PJ="$WORK/phone"
check "pairing over TLS" \
  "$(curl -sk -c "$PJ" -o /dev/null -w '%{http_code}' -H 'content-type: application/json' \
     -d "{\"code\":\"$SPIN\",\"name\":\"phone\"}" https://127.0.0.1:18443/pair/redeem)" "200"

sinv() {
  local args=${2-}; [ -z "$args" ] && args='{}'
  curl -sk -b "$PJ" -H 'content-type: application/json' -H 'sec-fetch-site: same-origin' \
    -d "{\"command\":\"$1\",\"args\":$args}" https://127.0.0.1:18443/api/invoke
}
sinv get_items | jq -e '.items | length >= 3' >/dev/null && ok "a paired phone sees the grid" || bad "served items"
sinv get_capabilities | jq -e '.trust == "device" and .clipboard == false' >/dev/null \
  && ok "a phone is told it has no clipboard" || bad "served caps"

for cmd in list_dirs copy_files purge_trash merge_duplicates; do
  st=$(curl -sk -b "$PJ" -o /dev/null -w '%{http_code}' -H 'content-type: application/json' \
       -H 'sec-fetch-site: same-origin' -d "{\"command\":\"$cmd\",\"args\":{\"path\":\"/tmp\"}}" \
       https://127.0.0.1:18443/api/invoke)
  check "$cmd is refused on a served bind" "$st" "403"
done

check "a cross-site POST is refused" \
  "$(curl -sk -b "$PJ" -o /dev/null -w '%{http_code}' -H 'content-type: application/json' \
     -H 'sec-fetch-site: cross-site' -d '{"command":"get_items"}' https://127.0.0.1:18443/api/invoke)" "403"

"$BIN" devices --data-dir "$D" 2>/dev/null | grep -q phone && ok "devices lists the phone" || bad "devices list"
DEV=$("$BIN" devices --data-dir "$D" 2>/dev/null | head -1 | cut -f1)
"$BIN" devices revoke "$DEV" --data-dir "$D" >/dev/null 2>&1 && ok "devices revoke" || bad "revoke"
check "a revoked device is 401" \
  "$(curl -sk -b "$PJ" -o /dev/null -w '%{http_code}' -H 'content-type: application/json' \
     -H 'sec-fetch-site: same-origin' -d '{"command":"get_items"}' https://127.0.0.1:18443/api/invoke)" "401"

kill $SERVE_PID 2>/dev/null; wait $SERVE_PID 2>/dev/null


# ---------------------------------------------------------------------------
echo "== plugins and tagging =="

# Install the bundled example into this run's state directory. That is the
# whole install procedure: a directory whose name matches its manifest's.
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
# `--data-dir <root>` maps the three XDG roots to `<root>/{cache,data,config}`,
# so the install root is `<root>/data/plugins`.
mkdir -p "$D/data/plugins"
cp -r "$REPO/plugins/example-auto-tagger" "$D/data/plugins/"

# A gallery with a clip in it, so a video's companion has to gain a *merged*
# tag entry rather than being silently skipped — the failure this codebase
# shipped for a year.
T="$WORK/tagged"
mkdir -p "$T"
ffmpeg -y -v error -f lavfi -i testsrc=size=320x240:duration=1 -frames:v 1 "$T/still.png" 2>/dev/null
ffmpeg -y -v error -f lavfi -i "testsrc=size=320x240:duration=3:rate=10" -c:v libx264 -pix_fmt yuv420p "$T/clip.mp4" 2>/dev/null

"$BIN" tag "$T" --plugin example-auto-tagger --data-dir "$D" > "$WORK/tag.log" 2>"$WORK/tag.err"
TAG_RC=$?
check "lightview tag exits 0" "$TAG_RC" "0"
grep -q "tagged 2" "$WORK/tag.log" && ok "tag reported both files" || bad "tag said: $(cat "$WORK/tag.log")"

STILL="$T/.lightview/companions/still.png.lightview.json"
CLIP="$T/.lightview/companions/clip.mp4.lightview.json"
jq -e '.tags.plugins.example.tags | index("example")' "$STILL" >/dev/null \
  && ok "the still's companion carries the plugin bucket" || bad "still: $(cat "$STILL" 2>&1 | head -c 200)"
jq -e '.tags.plugins.example.tags | index("example")' "$CLIP" >/dev/null \
  && ok "the clip's companion carries a merged tag entry" || bad "clip: $(cat "$CLIP" 2>&1 | head -c 200)"
jq -e '.tags.plugins.example.version == "1.0.0"' "$CLIP" >/dev/null \
  && ok "the bucket records the version the skip predicate reads" || bad "no version on the clip"

# A second run is a no-op at the same version — the resumability story, and
# what makes re-running after an upload cheap.
"$BIN" tag "$T" --plugin example-auto-tagger --data-dir "$D" > "$WORK/tag2.log" 2>/dev/null
grep -q "tagged 0, skipped 2" "$WORK/tag2.log" \
  && ok "a second run at the same version skips everything" || bad "rerun said: $(cat "$WORK/tag2.log")"

# A version bump re-tags. This is the whole reason the predicate is
# "version or higher" rather than "has this plugin's bucket".
sed -i 's/"version": "1.0.0"/"version": "2.0.0"/' "$D/data/plugins/example-auto-tagger/manifest.json"
"$BIN" tag "$T" --plugin example-auto-tagger --data-dir "$D" > "$WORK/tag3.log" 2>/dev/null
grep -q "tagged 2" "$WORK/tag3.log" \
  && ok "a manifest version bump re-tags the gallery" || bad "bumped run said: $(cat "$WORK/tag3.log")"
jq -e '.tags.plugins.example.version == "2.0.0"' "$CLIP" >/dev/null \
  && ok "the bucket now records the new version" || bad "version did not move"

# A filter scopes the run.
sed -i 's/"version": "2.0.0"/"version": "3.0.0"/' "$D/data/plugins/example-auto-tagger/manifest.json"
"$BIN" tag "$T" --plugin example-auto-tagger --filter 'type:video' --data-dir "$D" > "$WORK/tag4.log" 2>/dev/null
grep -q "tagged 1" "$WORK/tag4.log" \
  && ok "--filter scopes the run to what it matches" || bad "filtered run said: $(cat "$WORK/tag4.log")"

# A name that is a path selects nothing, because there is no join to abuse.
"$BIN" tag "$T" --plugin /tmp/evil --data-dir "$D" >/dev/null 2>&1
check "a plugin name that is a path is refused" "$?" "1"
"$BIN" tag "$T" --plugin ../example-auto-tagger --data-dir "$D" >/dev/null 2>&1
check "a plugin name with .. is refused" "$?" "1"

# ---------------------------------------------------------------------------
# The session's own lifetime
# ---------------------------------------------------------------------------
echo "== session lifetime =="

# A local process that no browser ever reached must not exit. The watchdog is
# armed by the first window, and without that rule it races the browser it was
# just started for -- which on a slow desktop is the common case, not the edge.
LONE="$WORK/lone"; mkdir -p "$LONE/gallery" "$LONE/state"
cp "$T/tall.png" "$LONE/gallery/" 2>/dev/null || true
"$BIN" "$LONE/gallery" --data-dir "$LONE/state" > "$WORK/lone.out" 2> "$WORK/lone.err" &
LONE_PID=$!
sleep 20
if kill -0 "$LONE_PID" 2>/dev/null; then
  ok "a local session with no window yet stays up"
else
  bad "the process exited before any browser reached it"
fi
kill "$LONE_PID" 2>/dev/null || true
wait "$LONE_PID" 2>/dev/null || true

echo
echo "== $pass passed, $fail failed =="
[ "$fail" = "0" ] || { echo "--- open.err ---"; tail -20 "$WORK/open.err"; echo "--- serve.err ---"; tail -20 "$WORK/serve.err"; }
rm -rf "$WORK"
exit "$fail"
