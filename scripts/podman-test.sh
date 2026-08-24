#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Run the FUSE tests in the Linux container (they need /dev/fuse). This is the
# automated counterpart to `podman-shell.sh` — it runs `cargo test -p wusel-fuse`
# non-interactively, so the real mount end-to-end is exercised in CI/locally.
#
# Extra arguments are forwarded to `cargo test`, so the interesting invocations
# stay inside the script instead of being rebuilt by hand:
#
#   mise run fuse-test                                  # the whole suite
#   mise run fuse-test -- --test interrupt_probe -- --nocapture
#
# That matters: a hand-rebuilt `podman run` skips everything else this script
# does — the mirror bookkeeping above all — and a build context left behind in
# the VM after a failure is measured in tens of gigabytes.
#
# Note: if the repo is not visible in the podman VM (no direct share), we work
# on a mirror under /private/tmp (rsync); the original tree is left untouched.
set -euo pipefail

export PATH="/opt/podman/bin:$PATH"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="wusel-dev"

# Locate the repo as the VM sees it (direct share, or an rsync mirror as the
# fallback) — sets WORK and MIRRORED. Shared logic: scripts/podman-lib.sh.
# shellcheck source=scripts/podman-lib.sh
. "$(dirname "$0")/podman-lib.sh"
resolve_work

# Build unconditionally (not `podman image exists ||`): podman caches the layers,
# so a no-op rebuild is ~free, while a stale image after a mise.toml bump would
# silently test the old toolchain. Same in the other podman-*.sh scripts.
podman build -t "$IMAGE" -f "$REPO/Containerfile" "$REPO"

# With no filter, the whole Linux-only gate runs: the FUSE mount tests plus the
# network-home proof, which needs a mount namespace and therefore this container
# too. A filter means somebody is chasing one test, so only that runs.
# Clippy belongs here too, not in the native `clippy` task: that one excludes
# wusel-fuse, because the crate does not build off Linux. So the frontend was
# the one crate nothing ever linted, and a misplaced `cfg` attribute sat in its
# crate root for as long as it took somebody to run clippy here by hand.
if [ "$#" -eq 0 ]; then
    EXTRA='&& mise exec -- cargo clippy -p wusel-fuse --all-targets -- -D warnings && ./scripts/check-network-home.sh'
else
    EXTRA=''
fi

echo ">> Running the Linux-only tests in the container ..."
podman run --rm \
    --device /dev/fuse \
    --cap-add SYS_ADMIN \
    --security-opt label=disable \
    -v "$WORK":/work:Z \
    -e MISE_TRUSTED_CONFIG_PATHS=/work \
    "$IMAGE" \
    bash -lc "cd /work && mise exec -- cargo test -p wusel-fuse \"\$@\" $EXTRA" fuse-test "$@"
