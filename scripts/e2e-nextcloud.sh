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

# How long to wait for an asynchronous write-back to reach the server. wusel
# uploads on close, out of band; `sync` does NOT force that to complete, so the
# assertions below poll instead of sleeping a fixed amount — any constant short
# enough to be worth writing is one a loaded runner will outlast eventually.
UPLOAD_TIMEOUT_S=60

# One temp root for everything this run creates — mount point, XDG dirs, curl
# config files, scratch files. Everything is inside it, so the trap can remove
# all of it, and nothing lands on a predictable (symlink-attackable) /tmp path.
TMPROOT="$(mktemp -d)"
MNT="$TMPROOT/mnt"
CFG_HOME="$TMPROOT/home"
WORK="$TMPROOT/work"
mkdir -p "$MNT" "$WORK"
export XDG_CONFIG_HOME="$CFG_HOME/config"
export XDG_STATE_HOME="$CFG_HOME/state"
export XDG_CACHE_HOME="$CFG_HOME/cache"

WUSEL_PID=""
fail() { echo "!! E2E FAIL: $*" >&2; exit 1; }
ok()   { echo ">> ok: $*"; }

cleanup() {
    fusermount3 -u "$MNT" 2>/dev/null || true
    if [ -n "$WUSEL_PID" ]; then
        kill "$WUSEL_PID" 2>/dev/null || true
        # SIGTERM does not reach a process blocked in a FUSE request — exactly
        # the state a FAILING test leaves wusel in — so a bare `wait` here would
        # block forever and the job would burn until the runner timeout. Give it
        # a bounded grace period, then SIGKILL, and only then reap.
        for _ in $(seq 1 50); do
            kill -0 "$WUSEL_PID" 2>/dev/null || break
            sleep 0.1
        done
        kill -9 "$WUSEL_PID" 2>/dev/null || true
        wait "$WUSEL_PID" 2>/dev/null || true
    fi
    rm -rf "$TMPROOT"
}
# Installed BEFORE anything is created or backgrounded: a Ctrl-C in the window
# between launching the daemon and arming the trap would otherwise orphan both
# the daemon and its mount. A signal with no handler skips the EXIT trap, hence
# the explicit INT/TERM handlers that turn the signal into an ordinary exit.
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Escape backslash and double quote — the two characters that are special both
# inside a JSON string and inside a quoted value of a curl config file. Order
# matters (backslash first), and because every backslash ends up doubled, no
# unintended escape sequence (\t, \r, …) can form out of the input.
# Without this a password containing " or \ writes a syntactically broken
# credentials.json, and the parse error points nowhere near the actual cause.
esc() {
    local s=$1
    s=${s//\\/\\\\}
    s=${s//\"/\\\"}
    printf '%s' "$s"
}

# Credentials go into curl config files rather than `-u user:pass`, because argv
# is world-readable through /proc/<pid>/cmdline for as long as curl runs. The
# value has to be *quoted*: in an unquoted value curl strips a trailing
# " #comment", so a password containing a space followed by # would be silently
# truncated and authentication would fail for no visible reason. The files live
# in the 0700 temp root and die with it.
write_curl_rc() { # $1=file  $2=user  $3=password
    printf 'user = "%s:%s"\n' "$(esc "$2")" "$(esc "$3")" > "$1"
    chmod 600 "$1"
}

ADMIN_RC="$TMPROOT/curl-admin.rc"
APP_RC="$TMPROOT/curl-app.rc"
write_curl_rc "$ADMIN_RC" "$NC_USER" "$NC_PASS"
ADMIN=(--config "$ADMIN_RC")

# --- 1. Wait for Nextcloud to finish installing ----------------------------
echo ">> waiting for Nextcloud at $NC_URL ..."
for i in $(seq 1 60); do
  if curl -fsS "$NC_URL/status.php" 2>/dev/null | grep -q '"installed":true'; then break; fi
  sleep 5
  [ "$i" = 60 ] && fail "Nextcloud did not become ready"
done
ok "Nextcloud is up"

# --- 2. App password + credentials (no browser Login Flow in CI) -----------
APP_PASS="$(curl -fsS "${ADMIN[@]}" "${OCS_H[@]}" \
  "$NC_URL/ocs/v2.php/core/getapppassword?format=json" \
  | sed -n 's/.*"apppassword":"\([^"]*\)".*/\1/p')"
[ -n "$APP_PASS" ] || fail "could not obtain an app password"
write_curl_rc "$APP_RC" "$NC_USER" "$APP_PASS"
AUTH=(--config "$APP_RC")

mkdir -p "$XDG_CONFIG_HOME/wusel"
printf '{"server":"%s","loginName":"%s","appPassword":"%s","in_keyring":false}\n' \
  "$(esc "$NC_URL")" "$(esc "$NC_USER")" "$(esc "$APP_PASS")" \
  > "$XDG_CONFIG_HOME/wusel/credentials.json"
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
printf 'line1: base\nline2: base\nline3: base\n' > "$WORK/base.txt"
curl -fsS "${AUTH[@]}" -T "$WORK/base.txt" "$DAV/merge.txt" >/dev/null
ok "seeded merge.txt on the server"

# --- 4. Mount ---------------------------------------------------------------
echo ">> mounting at $MNT ..."
RUST_LOG="${RUST_LOG:-wusel=info,wusel_core=info,wusel_fuse=info}" \
  "$WUSEL" mount "$MNT" &
WUSEL_PID=$!
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
srv=""
for _ in $(seq 1 "$UPLOAD_TIMEOUT_S"); do
    srv="$(curl -fsS "${AUTH[@]}" "$DAV/created.txt" 2>/dev/null || true)"
    if [ "$srv" = "hello from the mount" ]; then break; fi
    sleep 1
done
[ "$srv" = "hello from the mount" ] || fail "new file did not upload: '$srv'"
ok "write-back upload works"

# --- 7. Pin keeps a file offline -------------------------------------------
"$WUSEL" pin merge.txt
"$WUSEL" pins | grep -q 'merge.txt' || fail "pin not listed"
ok "pin/pins works"

# --- 8. THE 3-way merge on a real 412 conflict -----------------------------
# base (cached by wusel from step 5) = line1/2/3 base.
# remote edit: change line1 on the server, out of band (a "second client").
printf 'line1: REMOTE\nline2: base\nline3: base\n' > "$WORK/remote.txt"
curl -fsS "${AUTH[@]}" -T "$WORK/remote.txt" "$DAV/merge.txt" >/dev/null
# local edit: change line3 in the mount. Write IN PLACE (dd conv=notrunc, no
# O_TRUNC) to model an editor's read-modify-write: a plain `> file` truncates to
# zero first, which wusel treats — by design — as a wholesale overwrite with no
# merge base (and flushes the empty state, spuriously conflicting). In-place, the
# base cached by the read above survives. On close, wusel uploads with its stale
# If-Match -> 412 -> 3-way text merge (base vs local vs remote), non-overlapping.
# (dd conv=notrunc never shrinks the file; the edited line is longer, so fine.)
printf 'line1: base\nline2: base\nline3: LOCAL\n' \
  | dd of="$MNT/merge.txt" conv=notrunc status=none
sync
# Poll for the merged result rather than sleeping: the upload, the 412 and the
# merge round trip all happen after close(), on wusel's own schedule.
merged=""
for _ in $(seq 1 "$UPLOAD_TIMEOUT_S"); do
    merged="$(curl -fsS "${AUTH[@]}" "$DAV/merge.txt" 2>/dev/null || true)"
    if printf '%s' "$merged" | grep -q 'line1: REMOTE' \
       && printf '%s' "$merged" | grep -q 'line3: LOCAL'; then
        break
    fi
    sleep 1
done
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
