#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Local counterpart to the GitHub E2E workflow: boot a real Nextcloud and the
# FUSE dev container in one podman network, build wusel, and run the same
# scripts/e2e-nextcloud.sh against it. This is how the end-to-end test — mount,
# write-back, 3-way merge — is iterated without a GitHub round-trip.
#
#   mise run e2e-local            # info logs
#   RUST_LOG=wusel_core=debug mise run e2e-local   # deep cache/merge tracing
#
# Note: if the repo is not visible in the podman VM (no direct share), we work on
# a mirror under /private/tmp (rsync, via podman-lib.sh); the original tree is
# left untouched, and edits to the test only take effect after re-running this.
set -euo pipefail

export PATH="/opt/podman/bin:$PATH"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="wusel-dev"
NET="wusel-e2e-net"
NC="wusel-e2e-nc" # also the Host header -> must be a trusted domain (below)

# Locate the repo as the VM sees it (direct share, or an rsync mirror as the
# fallback) — sets WORK and MIRRORED. Shared logic: scripts/podman-lib.sh.
. "$(dirname "$0")/podman-lib.sh"
resolve_work

podman image exists "$IMAGE" || podman build -t "$IMAGE" -f "$REPO/Containerfile" "$REPO"

cleanup() {
    echo ">> cleaning up the Nextcloud container and network ..."
    podman rm -f "$NC" >/dev/null 2>&1 || true
    podman network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup # drop leftovers from an aborted previous run

echo ">> creating network $NET and starting $NC (nextcloud:latest, SQLite) ..."
podman network create "$NET" >/dev/null
podman run -d --name "$NC" --network "$NET" \
    -e SQLITE_DATABASE=nextcloud \
    -e NEXTCLOUD_ADMIN_USER=admin \
    -e NEXTCLOUD_ADMIN_PASSWORD=adminpass \
    -e "NEXTCLOUD_TRUSTED_DOMAINS=$NC localhost 127.0.0.1" \
    nextcloud:latest >/dev/null

# The dev container reaches Nextcloud by service name on the shared network. The
# e2e script (and the wusel daemon it starts) all run inside this container, so
# http://$NC is the URL both for curl and for the mount's credentials.
echo ">> building wusel and running the E2E test in the FUSE container ..."
podman run --rm \
    --network "$NET" \
    --device /dev/fuse \
    --cap-add SYS_ADMIN \
    --security-opt label=disable \
    -v "$WORK":/work:Z \
    -e MISE_TRUSTED_CONFIG_PATHS=/work \
    -e "NC_URL=http://$NC" \
    -e WUSEL=/work/target-linux/debug/wusel \
    -e "RUST_LOG=${RUST_LOG:-wusel=info,wusel_core=info,wusel_fuse=info}" \
    "$IMAGE" \
    bash -lc '
        set -euo pipefail
        cd /work
        mise exec -- cargo build -p wusel --features fuse
        exec bash scripts/e2e-nextcloud.sh
    '
