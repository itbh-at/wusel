#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Record the wusel screencast the same way the E2E runs: a real Nextcloud and
# the FUSE dev container on one podman network, wusel built inside, and
# scripts/demo-nextcloud.sh driving a real mount. The product of this run is a
# single asciicast file at the repo root: wusel-demo.cast.
#
#   mise run demo-cast
#
# Like the E2E, if the repo is not visible to the podman VM we work on an rsync
# mirror (podman-lib.sh) and copy the cast back out at the end.
set -euo pipefail

export PATH="/opt/podman/bin:$PATH"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="wusel-dev"
NET="wusel-demo-net"
NC="wusel-demo-nc"
NC_IMAGE="${NC_IMAGE:-nextcloud:34-apache}"
# The recording and its derived assets land at their published locations, so one
# `mise run demo-cast` keeps the interactive player (the .cast attachment) and
# the README's GIF in lockstep — no manual copy step to forget.
CAST_REL="documentation/modules/ROOT/attachments/wusel-demo.cast"
GIF_REL="documentation/modules/ROOT/images/wusel-demo.gif"

# Locate the repo as the VM sees it (direct share or rsync mirror) -> WORK.
# shellcheck source=scripts/podman-lib.sh
. "$(dirname "$0")/podman-lib.sh"
resolve_work

podman build -t "$IMAGE" -f "$REPO/Containerfile" "$REPO"

cleanup() {
    echo ">> cleaning up the Nextcloud container and network ..."
    podman rm -f "$NC" >/dev/null 2>&1 || true
    podman network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

echo ">> creating network $NET and starting $NC ($NC_IMAGE, SQLite) ..."
podman network create "$NET" >/dev/null
podman run -d --name "$NC" --network "$NET" \
    -e SQLITE_DATABASE=nextcloud \
    -e NEXTCLOUD_ADMIN_USER=admin \
    -e NEXTCLOUD_ADMIN_PASSWORD=adminpass \
    -e "NEXTCLOUD_TRUSTED_DOMAINS=$NC localhost 127.0.0.1" \
    "$NC_IMAGE" >/dev/null

echo ">> building wusel and recording the cast in the FUSE container ..."
podman run --rm \
    --network "$NET" \
    --device /dev/fuse \
    --cap-add SYS_ADMIN \
    --security-opt label=disable \
    -v "$WORK":/work:Z \
    -e MISE_TRUSTED_CONFIG_PATHS=/work \
    -e "NC_URL=http://$NC" \
    -e WUSEL=/work/target-linux/debug/wusel \
    -e "CAST=/work/$CAST_REL" \
    "$IMAGE" \
    bash -lc '
        set -euo pipefail
        cd /work
        # python3 is the cast generator (scripts/demo-record.py); it is a
        # build-time tool for the recording, not a product dependency, so it is
        # installed here rather than baked into the dev image.
        apt-get update -qq && apt-get install -y --no-install-recommends python3 >/dev/null
        mise run build-fuse
        exec bash scripts/demo-nextcloud.sh
    '

# On a mirrored run the cast was written into the mirror; bring it back to the
# real repo so the committed file is the one just recorded.
if [ "${MIRRORED:-0}" = 1 ]; then
    cp "$WORK/$CAST_REL" "$REPO/$CAST_REL"
fi

# Render the README's GIF from the same cast, so the two never drift. agg is the
# asciinema project's own renderer, pinned in mise.toml; it runs on the host (not
# in the container) because that is where the fonts are. The font list covers
# macOS (Menlo) and a typical Linux (DejaVu); --speed/--idle keep a 46 s take to
# a tighter loop, and --last-frame-duration holds the final screen before it
# repeats. Keep these flags in step with the player's feel.
echo ">> rendering the README GIF from the cast (agg) ..."
mise exec -- agg \
    --theme asciinema \
    --font-family "Menlo,DejaVu Sans Mono,monospace" \
    --font-size 20 \
    --speed 1.4 \
    --idle-time-limit 1 \
    --last-frame-duration 3 \
    "$REPO/$CAST_REL" "$REPO/$GIF_REL"

echo ">> done:"
echo ">>   cast: $REPO/$CAST_REL"
echo ">>   gif:  $REPO/$GIF_REL"
