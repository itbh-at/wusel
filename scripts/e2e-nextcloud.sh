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

# 3G link emulation for the responsiveness checks (steps 9 and 10). We shape
# BOTH directions: downloads (a read stalls on these — step 9) and uploads (a
# write/flush stalls on these — step 10). Egress is the root qdisc; ingress
# cannot be rate-limited directly, so it is redirected to an intermediate `ifb`
# device and shaped there. Profile: a few Mbit/s at mobile latency, so one large
# transfer takes many seconds — long enough that an operation stuck on it visibly
# freezes an unrelated `stat`. Needs NET_ADMIN and iproute2.
#
# `tc` and `ip` need NET_ADMIN. In the dev container the script is root and has
# it; on a CI runner it is an ordinary user with passwordless sudo. Resolving it
# once here keeps the shaping helpers identical in both places.
if [ "$(id -u)" -eq 0 ]; then
    PRIV=""
elif command -v sudo >/dev/null 2>&1; then
    PRIV="sudo"
else
    fail "link shaping needs NET_ADMIN: run as root or provide sudo"
fi

# The interface carrying the traffic to Nextcloud. `eth0` in the dev container,
# but a CI runner may name it anything, so ask the routing table and keep the
# old default only as a fallback.
NET_IF="${NET_IF:-$(ip route show default 2>/dev/null | awk '{print $5; exit}')}"
NET_IF="${NET_IF:-eth0}"

# Whether this machine will let us shape the link at all. The throttled steps
# were written for the dev container, which is privileged and has a plain
# interface; a CI runner is neither, and its default qdisc refuses to be
# displaced ("Exclusivity flag on, cannot modify"). Rather than guess at another
# machine's kernel, probe once: add a qdisc and take it away again.
SHAPING=1
probe_shaping() {
    # Both halves, because they fail for different reasons and only one of them
    # is obvious. The root qdisc usually goes on fine; the *ingress* one is the
    # one that collides — a machine already running containers tends to carry a
    # `clsact` qdisc there, and ingress and clsact are mutually exclusive, which
    # the kernel reports as "Exclusivity flag on, cannot modify". Probing only
    # the root qdisc says yes and then the run dies at the second step.
    if ! $PRIV tc qdisc add dev "$NET_IF" root netem delay 1ms >/dev/null 2>&1; then
        SHAPING=0
        return
    fi
    if ! $PRIV tc qdisc add dev "$NET_IF" handle ffff: ingress >/dev/null 2>&1; then
        SHAPING=0
    fi
    $PRIV tc qdisc del dev "$NET_IF" ingress >/dev/null 2>&1 || true
    $PRIV tc qdisc del dev "$NET_IF" root >/dev/null 2>&1 || true
}

# Announce a skipped step as loudly as a passing one. A test that quietly does
# not run is worse than one that fails: this suite already spent a month looking
# green while the gate skipped it entirely.
skip() { echo "!! SKIPPED: $*  (no link shaping on this host)"; SKIPPED=$((SKIPPED + 1)); }
SKIPPED=0
NET_3G_RATE="${NET_3G_RATE:-3mbit}"
NET_3G_DELAY="${NET_3G_DELAY:-150ms}"
net_3g_on() {
    # Egress (upload) shaping: the interface's root qdisc.
    $PRIV tc qdisc add dev "$NET_IF" root netem delay "$NET_3G_DELAY" rate "$NET_3G_RATE"
    # Ingress (download) shaping: redirect to an ifb device and shape that.
    # The module is built in the dev container's kernel but not loaded on a bare
    # runner, where `ip link add ... type ifb` would fail without it.
    $PRIV modprobe ifb 2>/dev/null || true
    $PRIV ip link add ifb0 type ifb 2>/dev/null || true
    $PRIV ip link set ifb0 up
    $PRIV tc qdisc add dev "$NET_IF" handle ffff: ingress
    $PRIV tc filter add dev "$NET_IF" parent ffff: protocol ip u32 match u32 0 0 \
        action mirred egress redirect dev ifb0
    $PRIV tc qdisc add dev ifb0 root netem delay "$NET_3G_DELAY" rate "$NET_3G_RATE"
}
net_3g_off() {
    $PRIV tc qdisc del dev "$NET_IF" root 2>/dev/null || true
    $PRIV tc qdisc del dev "$NET_IF" ingress 2>/dev/null || true
    $PRIV tc qdisc del dev ifb0 root 2>/dev/null || true
    $PRIV ip link del ifb0 2>/dev/null || true
}

# Latency-only shaping for the concurrency check (step 12). Deliberately NOT
# bandwidth-limited: two downloads sharing one throttled pipe finish in the same
# wall time whether or not they overlap, which would hide parallelism. High
# latency with ample bandwidth instead lets concurrent transfers overlap their
# round-trips, so parallel is measurably faster than sequential — but only if the
# engine really runs them at once (multi-threaded runtime + dispatch threads).
NET_LATENCY="${NET_LATENCY:-300ms}"
net_latency_on() {
    $PRIV tc qdisc add dev "$NET_IF" root netem delay "$NET_LATENCY"
}
net_latency_off() {
    $PRIV tc qdisc del dev "$NET_IF" root 2>/dev/null || true
}

cleanup() {
    net_3g_off  # never leave the link shaped, even if step 9 aborts mid-way
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

# --- 0. Can this host shape the link? --------------------------------------
probe_shaping
if [ "$SHAPING" = 1 ]; then
    echo ">> link shaping available on $NET_IF — the throttled steps will run"
else
    echo ">> link shaping NOT available on $NET_IF — the throttled steps will be skipped"
fi

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
[mount]
dispatch_threads = 4
EOF
ok "credentials + config written"

# --- 3. Seed files on the server (BEFORE mounting) -------------------------
printf 'line1: base\nline2: base\nline3: base\n' > "$WORK/base.txt"
curl -fsS "${AUTH[@]}" -T "$WORK/base.txt" "$DAV/merge.txt" >/dev/null
ok "seeded merge.txt on the server"

# The responsiveness check (step 9) needs a large online-only file and a small
# cached one. They MUST be seeded before the mount: this image has no notify_push
# ("relying on TTL revalidation" in the log) and the config pins the directory
# TTL at 3600s for the merge test, so a file uploaded out of band AFTER the mount
# never enters the cached listing and stays invisible. Seeded here, both are in
# the mount's initial PROPFIND. big.bin is never read before step 9, so it stays
# online-only (uncached) — exactly the transfer the check puts under load.
echo ">> seeding big.bin (256 MiB) + small.txt for the responsiveness check ..."
dd if=/dev/urandom of="$WORK/big.bin" bs=1M count=256 status=none
curl -fsS "${AUTH[@]}" -T "$WORK/big.bin" "$DAV/big.bin" >/dev/null
printf 'small\n' > "$WORK/small.txt"
curl -fsS "${AUTH[@]}" -T "$WORK/small.txt" "$DAV/small.txt" >/dev/null
ok "seeded big.bin + small.txt on the server"

# hydra.bin: a large file used only to measure hydration cost (step 12). It is
# pinned — which forces a whole-file download — and never range-read, so the
# GET count against it (checked on the host in podman-e2e.sh) is exactly the
# hydration's: one streamed GET now, versus one per 8 MiB chunk before Etappe 5.
# 64 MiB = 8 chunk-GETs the old way. Seeded before the mount, like the others.
echo ">> seeding hydra.bin (64 MiB) for the hydration-cost measurement ..."
dd if=/dev/urandom of="$WORK/hydra.bin" bs=1M count=64 status=none
curl -fsS "${AUTH[@]}" -T "$WORK/hydra.bin" "$DAV/hydra.bin" >/dev/null
ok "seeded hydra.bin on the server"

# Four equal, online-only files for the concurrency check (step 12): par1/par2
# are read in parallel, seq1/seq2 back to back. Each is read exactly once (never
# before step 12), so no cache-clear is needed. Seeded before the mount so they
# are in the initial listing.
echo ">> seeding par1/par2/seq1/seq2 (32 MiB each) for the concurrency check ..."
for f in par1 par2 seq1 seq2; do
    dd if=/dev/urandom of="$WORK/$f.bin" bs=1M count=32 status=none
    curl -fsS "${AUTH[@]}" -T "$WORK/$f.bin" "$DAV/$f.bin" >/dev/null
done
ok "seeded the concurrency-check files on the server"

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

# --- 6b. Large file through the mount (chunked upload NG) -------------------
# A file larger than one 4 MiB chunk, copied INTO the mount, must chunk-upload
# (MKCOL + PUT parts + MOVE assemble) and reassemble byte-for-byte on a *real*
# Nextcloud. This is the path a real "cp big file" takes, and the one the mock
# cannot vouch for: the mock concatenates the parts, whereas Nextcloud assembles
# them and enforces OC-Total-Length. Not covered before, and exactly what a
# tester reported failing.
echo ">> copying a 12 MiB file through the mount (chunked upload NG) ..."
dd if=/dev/urandom of="$WORK/big-up.bin" bs=1M count=12 status=none
want="$(sha256sum "$WORK/big-up.bin" | cut -d' ' -f1)"
cp "$WORK/big-up.bin" "$MNT/big-up.bin"
sync
got=""
for _ in $(seq 1 "$UPLOAD_TIMEOUT_S"); do
    if curl -fsS "${AUTH[@]}" "$DAV/big-up.bin" -o "$WORK/big-up.down" 2>/dev/null; then
        got="$(sha256sum "$WORK/big-up.down" | cut -d' ' -f1)"
        [ "$got" = "$want" ] && break
    fi
    sleep 1
done
[ "$got" = "$want" ] || fail "large file did not chunk-upload intact: want $want got ${got:-<none>}"
ok "chunked upload through the mount works"

# --- 6c. Overwrite a large file in place (the "compress to ZIP" pattern) ----
# A file manager building a ZIP creates the target and then rewrites it; each
# rewrite of a >4 MiB file is a chunked upload with `If-Match: <etag>`. If that
# conditional MOVE misbehaves the engine reads it as a conflict and parks a
# "conflicted copy" — which is exactly the flood of `(conflicted copy …).zip`
# files a tester saw. Overwrite big-up.bin in place, and assert both that the
# new content lands AND that no conflicted copy was created.
echo ">> overwriting the 12 MiB file in place (no conflict copies expected) ..."
dd if=/dev/urandom of="$WORK/big-up2.bin" bs=1M count=12 status=none
want2="$(sha256sum "$WORK/big-up2.bin" | cut -d' ' -f1)"
cp "$WORK/big-up2.bin" "$MNT/big-up.bin"
sync
got2=""
for _ in $(seq 1 "$UPLOAD_TIMEOUT_S"); do
    if curl -fsS "${AUTH[@]}" "$DAV/big-up.bin" -o "$WORK/big-up2.down" 2>/dev/null; then
        got2="$(sha256sum "$WORK/big-up2.down" | cut -d' ' -f1)"
        [ "$got2" = "$want2" ] && break
    fi
    sleep 1
done
[ "$got2" = "$want2" ] || fail "overwrite of a large file did not land: want $want2 got ${got2:-<none>}"
# No conflicted copy may have appeared in the directory listing.
copies="$(curl -fsS "${AUTH[@]}" -X PROPFIND -H 'Depth: 1' "$DAV/" 2>/dev/null \
    | grep -c 'conflicted copy' || true)"
[ "$copies" = 0 ] || fail "overwriting a large file spawned $copies conflicted copies"
ok "in-place overwrite of a large file works (no conflict copies)"

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

# --- 9. Responsiveness under a running transfer (throttled 3G link) --------
# The property under test: an unrelated, purely local operation must be served
# while a large read is in flight. With a single dispatch thread it is not —
# `stat` queues behind the transfer, which is what makes a file manager freeze.
# On a fast localhost link each 8 MiB range GET returns in ~50 ms, so the freeze
# is invisible; we throttle the download to a 3G link so one range GET blocks the
# dispatch thread for several seconds and the freeze becomes measurable. big.bin
# and small.txt were seeded before the mount (step 3), so both are already in the
# cached listing here — no out-of-band upload to wait on.
ls "$MNT" >/dev/null                       # both nodes are known from the listing
cat "$MNT/small.txt" >/dev/null            # cache the small one (local content)
if [ "$SHAPING" = 1 ]; then
net_3g_on
cat "$MNT/big.bin" > /dev/null &           # the (now slow) transfer under test
READER_PID=$!
sleep 2                                     # let it enter a chunked range GET
# Probe every 1.1 s — longer than the 1 s attribute TTL — so each stat forces a
# fresh getattr onto the dispatch thread rather than being served from the
# kernel's attribute cache. The reader stays blocked on the throttled link for
# the whole window (256 MiB at 3 Mbit/s far outlasts the probes), so on a single
# dispatch thread every probe queues behind it and times out.
STALLS=0
for _ in $(seq 1 8); do
    kill -0 "$READER_PID" 2>/dev/null || break   # transfer finished; stop probing
    timeout 2 stat "$MNT/small.txt" >/dev/null 2>&1 || STALLS=$((STALLS + 1))
    sleep 1.1
done
kill "$READER_PID" 2>/dev/null || true     # we bounded the read; no need to finish
wait "$READER_PID" 2>/dev/null || true
net_3g_off
[ "$STALLS" -eq 0 ] || fail "$STALLS of the probes stalled >2s while a read was in flight"
ok "the mount stays responsive during a large transfer (3G link)"
else
    skip "step 9 - responsiveness during a throttled download"
fi

# --- 10. Ordering and responsiveness under a running upload ----------------
# Two writes to the same file in quick succession must reach the server in
# order: an event-driven write path must queue per inode, not race. On the
# sequential write path this holds trivially; it is the guard that keeps the
# event-driven rewrite (Etappe 4) honest.
printf 'first\n' > "$MNT/order.txt"
printf 'second\n' > "$MNT/order.txt"
sync
final=""
for _ in $(seq 1 "$UPLOAD_TIMEOUT_S"); do
    final="$(curl -fsS "${AUTH[@]}" "$DAV/order.txt" 2>/dev/null || true)"
    [ "$final" = "second" ] && break
    sleep 1
done
[ "$final" = "second" ] || fail "write ordering violated, server has '$final'"
ok "writes to one file keep their order"

# A large upload must not stall unrelated operations either. Throttled to 3G so
# the upload's flush holds the write path for many seconds; a stat on an
# unrelated, already-cached file (small.txt, cached in step 9) must still return
# promptly. Probe every 1.1 s (> the 1 s attribute TTL) so each stat forces a
# fresh, dispatch-bound getattr.
if [ "$SHAPING" = 1 ]; then
dd if=/dev/urandom of="$WORK/up.bin" bs=1M count=64 status=none
net_3g_on
cp "$WORK/up.bin" "$MNT/up.bin" &          # blocks in close() on the throttled flush
UPLOADER_PID=$!
sleep 3                                     # let the writes finish and the flush begin
STALLS=0
for _ in $(seq 1 8); do
    kill -0 "$UPLOADER_PID" 2>/dev/null || break   # upload finished; stop probing
    timeout 2 stat "$MNT/small.txt" >/dev/null 2>&1 || STALLS=$((STALLS + 1))
    sleep 1.1
done
net_3g_off                                  # unthrottle so the upload can finish
wait "$UPLOADER_PID" 2>/dev/null || true
[ "$STALLS" -eq 0 ] || fail "$STALLS probes stalled >2s while an upload was in flight"
ok "the mount stays responsive during a large upload (3G link)"
else
    skip "step 10b - responsiveness during a throttled upload"
fi

# --- 11. Hydration cost: pinning a whole file is ONE GET -------------------
# Pin hydra.bin (never range-read), which forces a whole-file download. Since
# Etappe 5 that is a single streamed GET instead of one per 8 MiB chunk. The GET
# count against hydra.bin is asserted on the host (podman-e2e.sh reads the
# Nextcloud access log); here we just trigger the hydration and confirm the file
# is now served locally.
"$WUSEL" pin hydra.bin
"$WUSEL" pins | grep -q 'hydra.bin' || fail "hydra.bin not listed as pinned"
[ "$(stat -c %s "$MNT/hydra.bin")" = "$((64 * 1024 * 1024))" ] || fail "pinned hydra.bin has the wrong size"
ok "pinned hydra.bin (hydration cost checked on the host)"

# --- 12. Two transfers actually overlap (informational) --------------------
# The hard, deterministic proof that reads run concurrently lives in the mock
# test `two_reads_run_in_parallel` (fixed per-GET delay, no throughput effects).
# This real-server timing is kept for visibility but is NOT a pass/fail gate: a
# `netem delay` link throttles per-connection throughput (delayed ACKs shrink the
# window), so two parallel transfers largely *share* the pipe and the wall-clock
# speedup is small and noisy — and a from-0 sequential `cat` also kicks off a
# background whole-file hydration that competes. We log the numbers (ms) and warn
# if parallel was not faster, without failing the run.
ls "$MNT" >/dev/null
if [ "$SHAPING" = 1 ]; then
net_latency_on
t0=$(date +%s%3N)
cat "$MNT/par1.bin" >/dev/null &
P1=$!
cat "$MNT/par2.bin" >/dev/null &
P2=$!
wait "$P1" "$P2"
t_par=$(($(date +%s%3N) - t0))
t0=$(date +%s%3N)
cat "$MNT/seq1.bin" >/dev/null
cat "$MNT/seq2.bin" >/dev/null
t_seq=$(($(date +%s%3N) - t0))
net_latency_off
echo ">> parallel ${t_par}ms vs sequential ${t_seq}ms (informational; see the mock test for the gate)"
if [ "$t_par" -lt "$t_seq" ]; then
    ok "concurrent transfers overlapped (${t_par}ms < ${t_seq}ms)"
else
    skip "step 12 - overlap of concurrent transfers under added latency"
fi
else
    echo ">> note: parallel was not faster this run (${t_par}ms vs ${t_seq}ms) — expected on a throughput-throttled link; the deterministic proof is the mock test"
fi

if [ "$SKIPPED" -gt 0 ]; then
    echo ">> E2E PASSED — but $SKIPPED step(s) were SKIPPED for want of link shaping."
    echo ">> Those checks only run where NET_ADMIN and a shapeable interface exist:"
    echo ">>   mise run e2e-local   (podman, privileged)"
else
    echo ">> E2E PASSED"
fi
