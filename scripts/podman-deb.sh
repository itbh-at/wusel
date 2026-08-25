#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Build the Wusel .deb in a Debian container (for a macOS host, or a Fedora
# one). Builds the Debian image if needed, runs packaging/deb/build-deb.sh
# inside it, and leaves the .deb in ./dist/.
#
# On a Debian/Ubuntu host you do not need this — just run
# packaging/deb/build-deb.sh.
set -euo pipefail

export PATH="/opt/podman/bin:$PATH"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="wusel-deb"

# Locate the repo as the VM sees it (direct share, or an rsync mirror as the
# fallback) — sets WORK and MIRRORED. Shared logic: scripts/podman-lib.sh.
# shellcheck source=scripts/podman-lib.sh
. "$(dirname "$0")/podman-lib.sh"
resolve_work

# Unconditional rebuild — podman layer-caches, so this is ~free, and it keeps the
# image from drifting behind a mise.toml toolchain bump (see podman-test.sh).
podman build -t "$IMAGE" -f "$REPO/packaging/deb/Containerfile" "$REPO"

echo ">> Building the .deb in the container ..."
podman run --rm \
    -v "$WORK":/work:Z \
    -e MISE_TRUSTED_CONFIG_PATHS=/work \
    "$IMAGE" \
    bash -lc "cd /work && ./packaging/deb/build-deb.sh"

# When building on an rsync mirror, the .deb landed in the mirror's dist/ —
# copy it back so it ends up in the repo's ./dist regardless.
if [ "$MIRRORED" = 1 ]; then
    mkdir -p "$REPO/dist"
    cp "$WORK"/dist/*.deb "$REPO/dist/" 2>/dev/null || true
fi

echo ">> .deb(s) in $REPO/dist:"
ls -1 "$REPO"/dist/*.deb 2>/dev/null || echo "  (none — check the build output above)"
