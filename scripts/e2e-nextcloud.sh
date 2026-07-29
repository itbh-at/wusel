#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# End-to-end test against a REAL Nextcloud (not the mock). Expects a Nextcloud
# reachable at $NC_URL with admin creds, and a wusel binary built with the fuse
# feature at $WUSEL. Mounts, then exercises read/hydration, write/upload, pin,
# and — the headline — the opt-in 3-way text merge on a genuine 412 conflict.
#
# Env (all have CI defaults):
#   NC_URL=http://localhost:8080  NC_USER=admin  NC_PASS=adminpass
#   WUSEL=./target/release/wusel
set -euo pipefail

NC_URL="${NC_URL:-http://localhost:8080}"
NC_USER="${NC_USER:-admin}"
NC_PASS="${NC_PASS:-adminpass}"
WUSEL="${WUSEL:-./target/release/wusel}"

DAV="$NC_URL/remote.php/dav/files/$NC_USER"
OCS_H=(-H "OCS-APIRequest: true")
MNT="$(mktemp -d)"
CFG_HOME="$(mktemp -d)"
export XDG_CONFIG_HOME="$CFG_HOME/config"
export XDG_STATE_HOME="$CFG_HOME/state"
export XDG_CACHE_HOME="$CFG_HOME/cache"
fail() { echo "!! E2E FAIL: $*" >&2; exit 1; }
ok()   { echo ">> ok: $*"; }

# --- 1. Wait for Nextcloud to finish installing ----------------------------
echo ">> waiting for Nextcloud at $NC_URL ..."
for i in $(seq 1 60); do
  if curl -fsS "$NC_URL/status.php" 2>/dev/null | grep -q '"installed":true'; then break; fi
  sleep 5
  [ "$i" = 60 ] && fail "Nextcloud did not become ready"
done
ok "Nextcloud is up"

# --- 2. App password + credentials (no browser Login Flow in CI) -----------
APP_PASS="$(curl -fsS -u "$NC_USER:$NC_PASS" "${OCS_H[@]}" \
  "$NC_URL/ocs/v2.php/core/getapppassword?format=json" \
  | sed -n 's/.*"apppassword":"\([^"]*\)".*/\1/p')"
[ -n "$APP_PASS" ] || fail "could not obtain an app password"
AUTH=(-u "$NC_USER:$APP_PASS")

mkdir -p "$XDG_CONFIG_HOME/wusel"
printf '{"server":"%s","loginName":"%s","appPassword":"%s","in_keyring":false}\n' \
  "$NC_URL" "$NC_USER" "$APP_PASS" > "$XDG_CONFIG_HOME/wusel/credentials.json"
chmod 600 "$XDG_CONFIG_HOME/wusel/credentials.json"
# Enable the 3-way merge; keep the TTL high so wusel keeps its cached ETag and a
# stale-If-Match upload genuinely conflicts (that is what we want to test).
cat > "$XDG_CONFIG_HOME/wusel/config.toml" <<EOF
[sync]
text_merge = true
revalidate_secs = 3600
EOF
ok "credentials + config written"

# --- 3. Seed a base file on the server -------------------------------------
printf 'line1: base\nline2: base\nline3: base\n' > /tmp/base.txt
curl -fsS "${AUTH[@]}" -T /tmp/base.txt "$DAV/merge.txt" >/dev/null
curl -fsS "${AUTH[@]}" -X MKCOL "$DAV/e2e" >/dev/null 2>&1 || true
ok "seeded merge.txt on the server"

# --- 4. Mount ---------------------------------------------------------------
echo ">> mounting at $MNT ..."
RUST_LOG="${RUST_LOG:-wusel=info,wusel_core=info,wusel_fuse=info}" \
  "$WUSEL" mount "$MNT" &
WUSEL_PID=$!
cleanup() {
  fusermount3 -u "$MNT" 2>/dev/null || true
  kill "$WUSEL_PID" 2>/dev/null || true
  wait "$WUSEL_PID" 2>/dev/null || true
}
trap cleanup EXIT
for i in $(seq 1 30); do
  [ -e "$MNT/merge.txt" ] && break
  sleep 1
  [ "$i" = 30 ] && fail "mount never showed merge.txt"
done
ok "mounted; merge.txt visible"

# --- 5. Read / hydration (also caches the base + its ETag) ------------------
got="$(cat "$MNT/merge.txt")"
[ "$got" = "$(printf 'line1: base\nline2: base\nline3: base')" ] || fail "read mismatch:
$got"
ok "read/hydration returns the server content"

# --- 6. Write / upload a new file ------------------------------------------
printf 'hello from the mount\n' > "$MNT/created.txt"
sync
sleep 2
srv="$(curl -fsS "${AUTH[@]}" "$DAV/created.txt")"
[ "$srv" = "hello from the mount" ] || fail "new file did not upload: '$srv'"
ok "write-back upload works"

# --- 7. Pin keeps a file offline -------------------------------------------
"$WUSEL" pin merge.txt
"$WUSEL" pins | grep -q 'merge.txt' || fail "pin not listed"
ok "pin/pins works"

# --- 8. THE 3-way merge on a real 412 conflict -----------------------------
# base (cached by wusel from step 5) = line1/2/3 base.
# remote edit: change line1 on the server, out of band (a "second client").
printf 'line1: REMOTE\nline2: base\nline3: base\n' > /tmp/remote.txt
curl -fsS "${AUTH[@]}" -T /tmp/remote.txt "$DAV/merge.txt" >/dev/null
# local edit: change line3 in the mount. On close, wusel uploads with its stale
# If-Match -> 412 -> 3-way text merge (base vs local vs remote), non-overlapping.
printf 'line1: base\nline2: base\nline3: LOCAL\n' > "$MNT/merge.txt"
sync
sleep 3
merged="$(curl -fsS "${AUTH[@]}" "$DAV/merge.txt")"
echo "--- merged server content ---"; printf '%s\n' "$merged"; echo "-----------------------------"
echo "$merged" | grep -q 'line1: REMOTE' || fail "merge lost the remote edit"
echo "$merged" | grep -q 'line3: LOCAL'  || fail "merge lost the local edit"
# A clean 3-way merge must NOT fall back to a conflicted copy.
if curl -fsS "${AUTH[@]}" -X PROPFIND -H 'Depth: 1' "$DAV/" \
     | grep -qi 'conflicted copy'; then
  fail "expected a clean merge but a conflicted copy was created"
fi
ok "3-way text merge combined both non-overlapping edits, no conflict copy"

echo ">> E2E PASSED"
