#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Prove that a home directory on a network filesystem moves the state database
# to local storage.
#
# Mounting real NFS or CIFS in a test would need a server, a kernel module and
# privileges nobody wants in a test suite. But the detection reads
# /proc/self/mounts, and inside a private mount namespace /proc can be replaced
# by a directory that answers that question differently. The kernel is not being
# fooled — nothing is actually mounted — the *detection* is being handed the
# input a machine with an NFS home would hand it, which is what is under test.
#
# A single file cannot be bind-mounted over procfs (the kernel refuses), so the
# whole of /proc is replaced. `self` is a magic symlink in the real /proc; here
# it is an ordinary directory, which resolves just as well for the two files the
# detection reads.
#
# Run inside the Linux container (Linux-only: it needs mount namespaces).
#   mise run fuse-shell   ->   ./scripts/check-network-home.sh
set -euo pipefail

cd "$(dirname "$0")/.."

if [ "$(uname -s)" != "Linux" ]; then
    echo "!! Linux only (mount namespaces); run it in the container." >&2
    exit 1
fi

HOME_DIR=/tmp/network-home
STATE_DIR="$HOME_DIR/.local/state/wusel"
mkdir -p "$STATE_DIR"

# A mount table in the kernel's own format, claiming the home is NFS. `/` and
# /var/tmp stay local, so there is somewhere for the database to go.
FAKE=/tmp/fake-proc
rm -rf "$FAKE"
mkdir -p "$FAKE/self"
cat >"$FAKE/self/mounts" <<EOF
/dev/vda1 / ext4 rw,relatime 0 0
proc /proc proc rw,nosuid,nodev,noexec 0 0
/dev/vda1 /var/tmp ext4 rw,relatime 0 0
fileserver:/export/home $HOME_DIR nfs4 rw,relatime,vers=4.2 0 0
EOF
# The uid is read from here, and a relocated database needs one to build its
# per-user directory with. Same format as the kernel's.
cp /proc/self/status "$FAKE/self/status"

echo ">> Building the test binary (outside the namespace, where the network is) ..."
BIN=$(mise exec -- cargo test -p wusel-core --test db_location --no-run --message-format=json 2>/dev/null |
    sed -n 's/.*"executable":"\([^"]*db_location[^"]*\)".*/\1/p' | tail -1)
[ -n "$BIN" ] || {
    echo "!! could not find the compiled test binary" >&2
    exit 1
}

echo ">> Baseline: an ordinary local home must NOT be relocated ..."
XDG_STATE_HOME="/tmp/local-home/.local/state" "$BIN" --nocapture

echo ">> Now with /proc/self/mounts saying the home is on NFS ..."
# root in the container already has what a mount namespace needs; anywhere else
# --map-root-user borrows it for the namespace alone. Either way the bind lands
# on /proc/self/mounts inside this namespace only, and nothing outside sees it.
if [ "$(id -u)" = 0 ]; then NS=(--mount); else NS=(--mount --map-root-user); fi
unshare "${NS[@]}" bash -c "
    set -euo pipefail
    mount --bind '$FAKE' /proc
    grep -q nfs4 /proc/self/mounts || { echo '!! the doctored table did not take' >&2; exit 1; }
    XDG_STATE_HOME='$HOME_DIR/.local/state' '$BIN' --nocapture
"

rm -rf "$FAKE"
echo ">> The database moves off a network home, and stays put on a local one."
