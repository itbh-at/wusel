#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Run the FUSE tests in the Linux container (they need /dev/fuse). This is the
# automated counterpart to `podman-shell.sh` — it runs `cargo test -p wusel-fuse`
# non-interactively, so the real mount end-to-end is exercised in CI/locally.
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

echo ">> Running FUSE tests in the container ..."
podman run --rm \
    --device /dev/fuse \
    --cap-add SYS_ADMIN \
    --security-opt label=disable \
    -v "$WORK":/work:Z \
    -e MISE_TRUSTED_CONFIG_PATHS=/work \
    "$IMAGE" \
    bash -lc "cd /work && mise exec -- cargo test -p wusel-fuse"
