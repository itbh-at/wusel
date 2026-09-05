#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Produce the documentation/marketing screencast (asciicast v2) of wusel against
# a REAL Nextcloud. Sibling of scripts/e2e-nextcloud.sh, but with a different
# job: not to assert, but to *show*. It seeds a small, neutral, nice-looking set
# of files, mounts, and hands off to scripts/demo-record.py, which drives a fixed
# sequence of real commands and lays their real output into a calm cast.
#
# Runs inside the FUSE dev container (see scripts/podman-demo.sh), which supplies
# a Nextcloud at $NC_URL and a wusel binary at $WUSEL.
#
# Env (all have container defaults):
#   NC_URL=http://localhost:8080  NC_USER=admin  NC_PASS=adminpass
#   WUSEL=./target-linux/debug/wusel   CAST=/work/wusel-demo.cast
set -euo pipefail

NC_URL="${NC_URL:-http://localhost:8080}"
NC_USER="${NC_USER:-admin}"
NC_PASS="${NC_PASS:-adminpass}"
WUSEL="${WUSEL:-./target-linux/debug/wusel}"
CAST="${CAST:-/work/documentation/modules/ROOT/attachments/wusel-demo.cast}"

DAV="$NC_URL/remote.php/dav/files/$NC_USER"
OCS_H=(-H "OCS-APIRequest: true")

# Quiet by default: a screencast should not carry the engine's INFO stream. This
# reaches every wusel invocation the recording makes (pin/pins/status), not just
# the daemon, so no stray log line lands between a command and its output.
export RUST_LOG="${RUST_LOG:-warn}"

# A wusel on PATH, so the daemon and every client call (`pin`, `pins`, `status`)
# resolve it the same way regardless of the demo's HOME.
install -m 0755 "$WUSEL" /usr/local/bin/wusel

# A presentable home: `df` prints the mount's real path, so a plain ~/Wusel (the
# product's default mountpoint, not a mktemp path) is what keeps the recording
# clean AND matches what a real install shows — no invented ~/Nextcloud.
DEMO_HOME="/home/you"
MNT="$DEMO_HOME/Wusel"
mkdir -p "$MNT"
# XDG dirs (and the seed/curl scratch) live in a temp root that the trap removes.
# XDG is exported so the background daemon and the foreground client calls share
# one account and state DB.
TMPROOT="$(mktemp -d)"
export XDG_CONFIG_HOME="$TMPROOT/config"
export XDG_STATE_HOME="$TMPROOT/state"
export XDG_CACHE_HOME="$TMPROOT/cache"

WUSEL_PID=""
fail() { echo "!! DEMO FAIL: $*" >&2; exit 1; }
ok()   { echo ">> ok: $*"; }

cleanup() {
    fusermount3 -u "$MNT" 2>/dev/null || true
    if [ -n "$WUSEL_PID" ]; then
        kill "$WUSEL_PID" 2>/dev/null || true
        for _ in $(seq 1 50); do
            kill -0 "$WUSEL_PID" 2>/dev/null || break
            sleep 0.1
        done
        kill -9 "$WUSEL_PID" 2>/dev/null || true
        wait "$WUSEL_PID" 2>/dev/null || true
    fi
    rm -rf "$TMPROOT" "$DEMO_HOME"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Same credential hygiene as the E2E: creds in a 0600 curl config, not in argv.
esc() { local s=$1; s=${s//\\/\\\\}; s=${s//\"/\\\"}; printf '%s' "$s"; }
write_curl_rc() { printf 'user = "%s:%s"\n' "$(esc "$2")" "$(esc "$3")" > "$1"; chmod 600 "$1"; }

ADMIN_RC="$TMPROOT/curl-admin.rc"
APP_RC="$TMPROOT/curl-app.rc"
write_curl_rc "$ADMIN_RC" "$NC_USER" "$NC_PASS"
ADMIN=(--config "$ADMIN_RC")

# --- 1. Wait for Nextcloud --------------------------------------------------
echo ">> waiting for Nextcloud at $NC_URL ..."
for i in $(seq 1 60); do
  curl -fsS "$NC_URL/status.php" 2>/dev/null | grep -q '"installed":true' && break
  sleep 5
  [ "$i" = 60 ] && fail "Nextcloud did not become ready"
done
ok "Nextcloud is up"

# --- 2. App password + a real quota so `df` has real numbers -----------------
APP_PASS="$(curl -fsS "${ADMIN[@]}" "${OCS_H[@]}" \
  "$NC_URL/ocs/v2.php/core/getapppassword?format=json" \
  | sed -n 's/.*"apppassword":"\([^"]*\)".*/\1/p')"
[ -n "$APP_PASS" ] || fail "could not obtain an app password"
write_curl_rc "$APP_RC" "$NC_USER" "$APP_PASS"
AUTH=(--config "$APP_RC")

curl -fsS "${ADMIN[@]}" "${OCS_H[@]}" -X PUT \
  -d 'key=quota' -d 'value=10 GB' \
  "$NC_URL/ocs/v2.php/cloud/users/$NC_USER" >/dev/null \
  || fail "could not set a storage quota"
ok "storage quota set to 10 GB"

mkdir -p "$XDG_CONFIG_HOME/wusel"
printf '{"server":"%s","loginName":"%s","appPassword":"%s","in_keyring":false}\n' \
  "$(esc "$NC_URL")" "$(esc "$NC_USER")" "$(esc "$APP_PASS")" \
  > "$XDG_CONFIG_HOME/wusel/credentials.json"
chmod 600 "$XDG_CONFIG_HOME/wusel/credentials.json"
# Pin the mount point in the config so `wusel status` derives the SAME mountpoint
# as the daemon — the diagnostics socket is keyed on that path, so a mismatch
# would make status report "no mount running" over a mount that is right there.
cat > "$XDG_CONFIG_HOME/wusel/config.toml" <<EOF
[mount]
point = "$MNT"
EOF
ok "credentials + config written"

# --- 3. Seed a small, neutral, presentable tree (before mounting) -----------
# Neutral names only: nothing here should look private when the cast is public.
WORK="$TMPROOT/seed"
mkdir -p "$WORK"
mkcol() { curl -fsS "${AUTH[@]}" -X MKCOL "$DAV/$1" >/dev/null 2>&1 || true; }
put()   { curl -fsS "${AUTH[@]}" -T "$2" "$DAV/$1" >/dev/null; }

# Clear Nextcloud's default skeleton first (Manual.pdf, a Documents/ and Photos/
# with sample files, Templates/, …). Left in, it would clutter the listing and,
# worse, merge into the folders we pin — the reason an earlier take reported
# "7 files" for a 3-file Documents/. Delete every top-level entry, then seed.
echo ">> clearing the default skeleton ..."
props="$(curl -fsS "${AUTH[@]}" -X PROPFIND -H 'Depth: 1' "$DAV/" 2>/dev/null || true)"
printf '%s' "$props" \
  | grep -oiE '<[a-z0-9]*:?href>[^<]+</[a-z0-9]*:?href>' \
  | sed -E 's#</?[a-z0-9]*:?href>##gi' \
  | while read -r href; do
      # Skip the account root itself; delete each child (dir hrefs end in '/').
      case "$href" in
        */files/"$NC_USER"/|*/files/"$NC_USER") continue ;;
      esac
      curl -fsS "${AUTH[@]}" -X DELETE "$NC_URL$href" >/dev/null 2>&1 || true
    done || true   # tolerate an empty root (grep finds nothing) under pipefail
ok "skeleton cleared"

mkcol Documents
mkcol Photos
mkcol Projects
mkcol notes

cat > "$WORK/welcome.txt" <<'EOF'
Welcome!

Your whole Nextcloud is now a normal folder on this machine.
Browse it, open files, drag things in and out -- it all lands
on the server. Files download only when you open them, and you
can pin the ones you want kept offline.
EOF
put notes/welcome.txt "$WORK/welcome.txt"

cat > "$WORK/README.md" <<'EOF'
# My files
Documents, photos and projects, synced with Nextcloud.
EOF
put README.md "$WORK/README.md"

# A few files with believable sizes, so `ls -lh` reads like a real home.
mk() { head -c "$2" /dev/urandom > "$WORK/blob"; put "$1" "$WORK/blob"; }
mk Documents/report.pdf   2359296   # ~2.3M
mk Documents/budget.ods     46080   # ~45K
mk Documents/contract.pdf  786432   # ~768K
mk Photos/holiday.jpg     1887436   # ~1.8M
mk Photos/sunset.jpg      2516582   # ~2.4M
mk Projects/archive.zip   8912896   # ~8.5M
ok "seeded a presentable tree on the server"

# --- 4. Mount ---------------------------------------------------------------
echo ">> mounting at $MNT ..."
# The daemon runs for real; RUST_LOG=warn (exported above) keeps its INFO stream
# out of the podman log. HOME is the presentable home so the mount, and every
# client call the recording makes, agree on ~/Wusel.
HOME="$DEMO_HOME" "$WUSEL" mount "$MNT" &
WUSEL_PID=$!
for i in $(seq 1 30); do
  [ -e "$MNT/README.md" ] && break
  sleep 1
  [ "$i" = 30 ] && fail "mount never showed the seeded files"
done
ok "mounted; the tree is visible"

# --- 5. Record --------------------------------------------------------------
echo ">> recording the cast to $CAST ..."
DEMO_HOME="$DEMO_HOME" DEMO_MOUNT="Wusel" DEMO_CAST="$CAST" \
  python3 scripts/demo-record.py
ok "cast written to $CAST"
