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
ffmpeg -y -v error -f lavfi -i "testsrc=size=320x240:duration=2:rate=10" -c:v libx264 -pix_fmt yuv420p "$G/clip.mp4" 2>/dev/null
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

CAPS=$(inv get_capabilities)
echo "$CAPS" | jq -e '.trust == "owner"' >/dev/null && ok "capabilities report owner" || bad "caps: $CAPS"

# The Owner half: the picker.
DIRS=$(inv list_dirs)
echo "$DIRS" | jq -e '[.entries[].name] | index("2026")' >/dev/null && ok "list_dirs defaults to the gallery root" || bad "dirs: $DIRS"
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

# A second launch opens the first one's window instead of refusing.
SECOND=$("$BIN" "$G" --data-dir "$D" 2>/dev/null | head -1)
[ "${SECOND%%/?t=*}" = "$BASE" ] && ok "a second launch printed the running URL" || bad "second launch: $SECOND"

kill $OPEN_PID 2>/dev/null; wait $OPEN_PID 2>/dev/null

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

echo
echo "== $pass passed, $fail failed =="
[ "$fail" = "0" ] || { echo "--- open.err ---"; tail -20 "$WORK/open.err"; echo "--- serve.err ---"; tail -20 "$WORK/serve.err"; }
rm -rf "$WORK"
exit "$fail"
