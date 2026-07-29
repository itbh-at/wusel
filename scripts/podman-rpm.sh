#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Build the Wusel RPM in a Fedora container (for a macOS host without a Fedora
# machine). Builds the Fedora image if needed, runs packaging/rpm/build-rpm.sh
# inside it, and leaves the .rpm in ./dist/.
#
# On a Fedora host you do not need this — just run packaging/rpm/build-rpm.sh.
set -euo pipefail

export PATH="/opt/podman/bin:$PATH"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="wusel-rpm"

# Locate the repo as the VM sees it (direct share, or an rsync mirror as the
# fallback) — sets WORK and MIRRORED. Shared logic: scripts/podman-lib.sh.
# shellcheck source=scripts/podman-lib.sh
. "$(dirname "$0")/podman-lib.sh"
resolve_work

# Unconditional rebuild — podman layer-caches, so this is ~free, and it keeps the
# image from drifting behind a mise.toml toolchain bump (see podman-test.sh).
podman build -t "$IMAGE" -f "$REPO/packaging/rpm/Containerfile" "$REPO"

echo ">> Building the RPM in the container ..."
podman run --rm \
    -v "$WORK":/work:Z \
    -e MISE_TRUSTED_CONFIG_PATHS=/work \
    "$IMAGE" \
    bash -lc "cd /work && ./packaging/rpm/build-rpm.sh"

# When building on an rsync mirror, the RPM landed in the mirror's dist/ — copy
# it back so it ends up in the repo's ./dist regardless.
if [ "$MIRRORED" = 1 ]; then
    mkdir -p "$REPO/dist"
    cp "$WORK"/dist/*.rpm "$REPO/dist/" 2>/dev/null || true
    # Reclaim the disk (same rationale as podman-build.sh): the mirror lives on
    # /private/tmp — the macOS boot disk — and its build tree is gigabytes, so
    # drop it; the next mirrored RPM build recompiles from scratch.
    FREED="$(du -sh "$WORK/target-rpm" 2>/dev/null | cut -f1 || true)"
    rm -rf "$WORK/target-rpm"
    echo ">> Removed the mirror's RPM build tree (${FREED:-0} reclaimed)."
fi

echo ">> RPM(s) in $REPO/dist:"
ls -1 "$REPO"/dist/*.rpm 2>/dev/null || echo "  (none — check the build output above)"
