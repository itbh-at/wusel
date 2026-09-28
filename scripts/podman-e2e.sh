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
# A major tag, not `latest`: an unpinned upstream image turns "green yesterday,
# red today" into a mystery with no code change behind it. A major tag still
# picks up Nextcloud's patch releases — which is what a server E2E should track —
# while a surprise major upgrade never lands unannounced.
#
# CI discovers the maintained majors from Docker Hub and runs all of them; this
# is the single-server local counterpart, so it names one and lets you point at
# another without editing the file:
#   NC_IMAGE=nextcloud:33-apache mise run e2e-local
NC_IMAGE="${NC_IMAGE:-nextcloud:34-apache}"

# TLS in front, the way a real server is reached: a Caddy reverse proxy with its
# own internal CA terminates HTTPS and offers HTTP/2 by ALPN, so the run goes
# through the TLS stack, a custom `ca_cert`, and HTTP/2 — none of which a plain
# http:// URL can exercise. On by default; E2E_TLS=0 talks to Nextcloud
# directly over plain HTTP (what CI does).
E2E_TLS="${E2E_TLS:-1}"
# E2E_HTTP1=1 (with TLS): the same run with `[tls] http1_only`, the baseline to
# hold HTTP/2's timings against.
E2E_HTTP1="${E2E_HTTP1:-0}"
PROXY="wusel-e2e-proxy"
PROXY_IMAGE="${PROXY_IMAGE:-docker.io/library/caddy:2-alpine}"

# Locate the repo as the VM sees it (direct share, or an rsync mirror as the
# fallback) — sets WORK and MIRRORED. Shared logic: scripts/podman-lib.sh.
# shellcheck source=scripts/podman-lib.sh
. "$(dirname "$0")/podman-lib.sh"
resolve_work

# Unconditional rebuild — podman layer-caches, so this is ~free, and it keeps the
# image from drifting behind a mise.toml toolchain bump (see podman-test.sh).
podman build -t "$IMAGE" -f "$REPO/Containerfile" "$REPO"

cleanup() {
    echo ">> cleaning up the Nextcloud container and network ..."
    podman rm -f "$NC" "$PROXY" >/dev/null 2>&1 || true
    podman network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup # drop leftovers from an aborted previous run

echo ">> creating network $NET and starting $NC ($NC_IMAGE, SQLite) ..."
podman network create "$NET" >/dev/null
# NEXTCLOUD_ADMIN_PASSWORD is a throw-away credential for a container that is
# created and destroyed by this very script — it never outlives the run.
podman run -d --name "$NC" --network "$NET" \
    -e SQLITE_DATABASE=nextcloud \
    -e NEXTCLOUD_ADMIN_USER=admin \
    -e NEXTCLOUD_ADMIN_PASSWORD=adminpass \
    -e "NEXTCLOUD_TRUSTED_DOMAINS=$NC $PROXY localhost 127.0.0.1" \
    "$NC_IMAGE" >/dev/null

# A Team/Group folder for the marking gate. It has to be made with `occ`, which
# only exists inside the Nextcloud container — the test container reaches the
# server over HTTP alone — so it happens here and the result is handed to the
# inner script as an environment variable.
#
# Best-effort on purpose: `app:install` reaches out to the app store, and an
# E2E must not turn red because that was unreachable or has no build for this
# Nextcloud. The inner gate skips when the name is empty, and says so.
GROUPFOLDER=""
echo ">> waiting for Nextcloud, then setting up a Team folder ..."
for _ in $(seq 1 60); do
    if podman exec -u www-data "$NC" php occ status 2>/dev/null | grep -q "installed: true"; then
        break
    fi
    sleep 5
done
if podman exec -u www-data "$NC" php occ app:install groupfolders >/dev/null 2>&1 ||
   podman exec -u www-data "$NC" php occ app:enable groupfolders >/dev/null 2>&1; then
    gf_id="$(podman exec -u www-data "$NC" php occ groupfolders:create "Team Folder" 2>/dev/null | tr -d '\r\n ')"
    if [ -n "$gf_id" ]; then
        # `admin` is in the `admin` group, so granting that group is what makes
        # the folder appear in the account the test logs in as. The permission
        # list is required: `groupfolders:group` with none grants read-only, and
        # the gate below creates a subdirectory inside the folder to prove the
        # marking does not bleed onto it — which a read-only mount refuses (403).
        podman exec -u www-data "$NC" php occ groupfolders:group "$gf_id" admin \
            read write delete share >/dev/null 2>&1 || true
        GROUPFOLDER="Team Folder"
        echo ">> Team folder created (id $gf_id)"
    fi
fi
[ -n "$GROUPFOLDER" ] || echo ">> groupfolders unavailable — the marking gate will skip"

NC_URL="http://$NC"
NC_CA=""
if [ "$E2E_TLS" = 1 ]; then
    echo ">> starting $PROXY ($PROXY_IMAGE): HTTPS + HTTP/2 in front of $NC ..."
    # Nextcloud sees plain HTTP from the proxy; tell it the outside is HTTPS so
    # the URLs it generates match what the client used.
    podman exec -u www-data "$NC" php occ config:system:set overwriteprotocol --value=https >/dev/null
    podman run -d --name "$PROXY" --network "$NET" "$PROXY_IMAGE" \
        caddy reverse-proxy --from "https://$PROXY" --to "$NC:80" \
        --internal-certs --access-log >/dev/null
    # The proxy's own root CA, handed to the test as `[tls] ca_cert`. It lands
    # in target-linux/ (ignored by git), which the test container sees as /work.
    mkdir -p "$WORK/target-linux"
    for _ in $(seq 1 30); do
        podman exec "$PROXY" cat /data/caddy/pki/authorities/local/root.crt \
            > "$WORK/target-linux/e2e-proxy-ca.pem" 2>/dev/null && break
        sleep 1
    done
    [ -s "$WORK/target-linux/e2e-proxy-ca.pem" ] || { echo "!! no CA from $PROXY" >&2; exit 1; }
    NC_URL="https://$PROXY"
    NC_CA="/work/target-linux/e2e-proxy-ca.pem"
fi

# The dev container reaches Nextcloud (or the proxy in front of it) by service
# name on the shared network. The e2e script (and the wusel daemon it starts)
# all run inside this container, so $NC_URL is the URL both for curl and for the
# mount's credentials.
echo ">> building wusel and running the E2E test in the FUSE container ..."
podman run --rm \
    --network "$NET" \
    --device /dev/fuse \
    --cap-add SYS_ADMIN \
    --cap-add NET_ADMIN \
    --security-opt label=disable \
    -v "$WORK":/work:Z \
    -e MISE_TRUSTED_CONFIG_PATHS=/work \
    -e "NC_URL=$NC_URL" \
    -e "NC_CA=$NC_CA" \
    -e "NC_HTTP1=$E2E_HTTP1" \
    -e "GROUPFOLDER=$GROUPFOLDER" \
    -e WUSEL=/work/target-linux/debug/wusel \
    -e "RUST_LOG=${RUST_LOG:-wusel=info,wusel_core=info,wusel_fuse=info}" \
    "$IMAGE" \
    bash -lc '
        set -euo pipefail
        cd /work
        mise run build-fuse
        exec bash scripts/e2e-nextcloud.sh
    '

# Gate 5: hydrating a whole file must cost ONE GET, not one per 8 MiB chunk. The
# inner test pinned hydra.bin (a 64 MiB file it never range-reads), so every
# `GET .../hydra.bin` in Nextcloud's own access log belongs to that hydration.
# The apache image logs each request to stdout, so this is a direct measurement,
# not an inference from our logs. Runs only if the E2E above passed (`set -e`).
GETS="$(podman logs "$NC" 2>&1 \
    | grep -c 'GET /remote.php/dav/files/admin/hydra.bin' || true)"
echo ">> hydration of hydra.bin cost $GETS GET request(s) (expected 1)"
[ "$GETS" = "1" ] || {
    echo "!! GATE 5 FAIL: hydration cost $GETS GET(s), expected 1" >&2
    exit 1
}

# Gate 6 (TLS mode): the client really speaks HTTP/2 where the server offers it.
# Measured at the proxy, which logs every request's protocol and User-Agent — not
# inferred from our own logs. Every WebDAV request must be HTTP/2; the
# notify_push client (capability lookup, WebSocket) stays on HTTP/1.1 by design
# and is not counted.
if [ "$E2E_TLS" = 1 ] && [ "$E2E_HTTP1" != 1 ]; then
    LOG="$(podman logs "$PROXY" 2>&1 | grep '"User-Agent":\["wusel"\]' \
        | grep '"uri":"/remote.php/dav/' || true)"
    H2="$(printf '%s\n' "$LOG" | grep -c '"proto":"HTTP/2.0"' || true)"
    H1="$(printf '%s\n' "$LOG" | grep -c '"proto":"HTTP/1' || true)"
    echo ">> wusel WebDAV requests through the proxy: $H2 over HTTP/2, $H1 over HTTP/1.x"
    if [ "$H2" -eq 0 ] || [ "$H1" -ne 0 ]; then
        echo "!! GATE 6 FAIL: expected every WebDAV request over HTTP/2" >&2
        podman logs "$PROXY" 2>&1 | grep '"User-Agent":\["wusel"\]' \
            | grep '"proto":"HTTP/1' | head -5 >&2
        exit 1
    fi
fi
