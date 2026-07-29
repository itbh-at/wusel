#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Builds the dev image (if needed) and compiles wusel including FUSE in the
# Linux container.
set -euo pipefail

export PATH="/opt/podman/bin:$PATH"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="wusel-dev"

# Locate the repo as the VM sees it (direct share, or an rsync mirror as the
# fallback) — sets WORK and MIRRORED. Shared logic: scripts/podman-lib.sh.
# shellcheck source=scripts/podman-lib.sh
. "$(dirname "$0")/podman-lib.sh"
resolve_work

echo ">> Building image $IMAGE (layers are cached) ..."
podman build -t "$IMAGE" -f "$REPO/Containerfile" "$REPO"

echo ">> Compiling wusel --features fuse (Linux) ..."
podman run --rm \
    -v "$WORK":/work:Z \
    -e MISE_TRUSTED_CONFIG_PATHS=/work \
    "$IMAGE" \
    bash -lc "cd /work && mise run build-fuse"

if [ "$MIRRORED" = 1 ]; then
    # Reclaim the disk: the mirror lives on /private/tmp — the macOS boot disk —
    # and its build tree (all deps) is gigabytes, filling that disk over repeated
    # builds. The mirror's job is only to *verify* the Linux build, so drop the
    # cache; trade-off: the next mirrored fuse-build recompiles from scratch.
    FREED="$(du -sh "$WORK/target-linux" 2>/dev/null | cut -f1 || true)"
    rm -rf "$WORK/target-linux"
    echo ">> Build OK. Removed the mirror's build tree (${FREED:-0} reclaimed)."
else
    # Building in place: the cache lives with the repo (not on the boot disk),
    # so keep it — rebuilds stay incremental.
    echo ">> Build OK. Binary: $WORK/target-linux/debug/wusel"
fi
