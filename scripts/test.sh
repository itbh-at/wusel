#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# `mise run test` — the engine + CLI suite, with a throwaway keyring around it.
#
# Why a wrapper at all: `credentials.rs` has a fail-soft rule ("the keyring must
# never be the reason the tool does not work"), and half of that rule is only
# observable against a keyring that *does* work. The alternatives are both bad:
# marking such a test `#[ignore]` means it never runs, and skipping it when no
# Secret Service is around means a green run says nothing. So we do not ask the
# machine whether it has a keyring — we bring one.
#
#   dbus-run-session   a private session bus, gone when the tests are
#   gnome-keyring      an empty, unlocked keyring on that bus
#   XDG_DATA_HOME      its files land in a temp dir, not in ~/.local/share
#
# Two consequences worth having. The keyring tests give the same answer on every
# Linux machine, unlocked desktop or bare CI container. And no test can reach the
# developer's real login keyring even by accident — which is not hypothetical:
# these tests used to write a dummy secret over the `wusel` entry of the default
# account, because the account key is the same one the product uses.
#
# On macOS there is no wrapper: the keyring backend there is the stub (see
# keyring.rs), so nothing in the suite talks to a Secret Service.
#
# Extra arguments are forwarded to `cargo test`:
#
#   mise run test
#   mise run test -- credentials -- --nocapture
set -euo pipefail

CARGO_ARGS=(test --workspace --exclude wusel-fuse "$@")

if [ "$(uname -s)" != "Linux" ]; then
    exec cargo "${CARGO_ARGS[@]}"
fi

missing=""
for tool in dbus-run-session gnome-keyring-daemon; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
    # Fail rather than run without them. Continuing would drop the one test that
    # covers the real backend, and a suite that quietly tests less than it says
    # is worse than one that stops and tells you why.
    cat >&2 <<EOF
test.sh: missing:$missing

The Linux test suite runs against a throwaway keyring, so the credential tests
mean the same thing everywhere. Install the packages that provide them:

  Fedora        sudo dnf install gnome-keyring dbus-daemon
  Debian/Ubuntu sudo apt-get install gnome-keyring dbus-bin
EOF
    exit 1
fi

# Everything the keyring daemon writes goes here and dies with the run. mktemp
# keeps concurrent runs (CI matrix, two shells) from sharing a keyring.
KEYRING_HOME="$(mktemp -d "${TMPDIR:-/tmp}/wusel-test-keyring.XXXXXX")"
trap 'rm -rf "$KEYRING_HOME"' EXIT

# `--unlock` takes the password on stdin; the value is irrelevant, the keyring
# exists for the length of the bus session. `--components=secrets` starts the
# Secret Service and nothing else (no ssh-agent, no pkcs11).
# The single quotes are the point: `$@` must be expanded by the inner shell,
# from the arguments passed after the script name, not by this one.
# shellcheck disable=SC2016
XDG_DATA_HOME="$KEYRING_HOME" dbus-run-session -- bash -c '
    set -euo pipefail
    eval "$(printf "%s" wusel-test | gnome-keyring-daemon --unlock --components=secrets --daemonize)"
    export GNOME_KEYRING_CONTROL
    exec cargo "$@"
' test-keyring "${CARGO_ARGS[@]}"
