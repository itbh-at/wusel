#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Shared plumbing for the podman-*.sh scripts: figure out where the podman VM
# can see the repo. Sourced after REPO is set; call `resolve_work`, which sets
#
#   WORK      — bind-mount source for the container
#   MIRRORED  — 1 when WORK is an rsync mirror, 0 when it is the repo itself
#
# Resolution order:
#  0. Native Linux host: no podman VM at all — containers share the host
#     kernel and see the filesystem directly, so the repo is used as-is.
#  1. The VM sees $REPO as-is (repo under /Users, /private or /var/folders —
#     podman's standard macOS shares).
#  2. The repo lives on an extra disk under /Volumes/<disk> that is shared into
#     the VM at /var/mnt/<disk> (one-time setup, see development.adoc
#     "Building from an external disk"). Building in place there keeps the
#     build cache off the boot disk and incremental.
#  3. Fallback: mirror to /private/tmp/wusel-linux via rsync. Container-side
#     changes never reach the original tree, and podman-build.sh drops the
#     mirror's build cache afterwards to spare the boot disk.

# Does the podman VM see this path? (~0.5 s SSH probe. macOS only: on a native
# Linux host there is no machine, the probe always fails — which is why
# resolve_work short-circuits on Linux before ever calling this.)
vm_sees() {
    local quoted
    printf -v quoted '%q' "$1"
    podman machine ssh "test -e $quoted" >/dev/null 2>&1
}

# WORK and MIRRORED are this function's *output* — read by the sourcing
# podman-*.sh script, which shellcheck cannot see from here.
# shellcheck disable=SC2034
resolve_work() {
    if [ "$(uname -s)" = "Linux" ]; then
        WORK="$REPO" MIRRORED=0
        return
    fi
    if vm_sees "$REPO"; then
        WORK="$REPO" MIRRORED=0
        return
    fi
    case "$REPO" in
    /Volumes/*)
        local vm_path="/var/mnt/${REPO#/Volumes/}"
        if vm_sees "$vm_path"; then
            echo ">> Using the VM's disk share: $vm_path"
            WORK="$vm_path" MIRRORED=0
            return
        fi
        ;;
    esac
    WORK="/private/tmp/wusel-linux" MIRRORED=1
    echo ">> Repo not visible in the podman VM — mirroring to $WORK ..."
    mkdir -p "$WORK"
    # Exclude every build-output tree (macOS/Linux/RPM targets and dist) — they
    # are host-local caches, gigabytes each, and the mirror rebuilds its own.
    rsync -a --delete \
        --exclude target --exclude target-linux --exclude target-rpm \
        --exclude dist --exclude .git \
        "$REPO/" "$WORK/"
}
