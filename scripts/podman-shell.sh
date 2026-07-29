#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Interactive Linux shell WITH /dev/fuse — here you can test the mount:
#
#   mise exec -- cargo run -p wusel --features fuse -- mount /mnt/nc
#   # in a second shell: ls /mnt/nc
#
# Note: if the repo is not visible in the podman VM (no direct share), the shell
# works on a mirror under /private/tmp — changes to the original only become
# visible after running this script again (rsync).
set -euo pipefail

export PATH="/opt/podman/bin:$PATH"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="wusel-dev"

# Locate the repo as the VM sees it (direct share, or an rsync mirror as the
# fallback) — sets WORK and MIRRORED. Shared logic: scripts/podman-lib.sh.
# shellcheck source=scripts/podman-lib.sh
. "$(dirname "$0")/podman-lib.sh"
resolve_work

# Unconditional rebuild — podman layer-caches, so this is ~free, and it keeps the
# image from drifting behind a mise.toml toolchain bump (see podman-test.sh).
podman build -t "$IMAGE" -f "$REPO/Containerfile" "$REPO"

# Persistent home for credentials/state across `--rm` runs, so `login` is a
# one-time step. Mapped to the XDG dirs the daemon uses (see wusel_core::config).
PERSIST="/private/tmp/wusel-devhome"
mkdir -p "$PERSIST"

echo ">> Starting shell with /dev/fuse (suggested mount point: /mnt/nc) ..."
echo ">> Credentials/state persist in $PERSIST (login once)."
# FUSE in the container needs /dev/fuse + SYS_ADMIN. If mounting fails,
# --privileged can help as a test.
podman run --rm -it \
    --device /dev/fuse \
    --cap-add SYS_ADMIN \
    --security-opt label=disable \
    -v "$WORK":/work:Z \
    -v "$PERSIST":/persist:Z \
    -e MISE_TRUSTED_CONFIG_PATHS=/work \
    -e XDG_CONFIG_HOME=/persist/config \
    -e XDG_STATE_HOME=/persist/state \
    -e XDG_CACHE_HOME=/persist/cache \
    "$IMAGE" \
    bash -lc "mkdir -p /mnt/nc && cd /work && exec bash -l"
