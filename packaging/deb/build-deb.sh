#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Build a Wusel .deb — from source, offline, mirroring build-rpm.sh: stage a
# source tree (git archive, not the working tree — commit first if you are
# iterating on something uncommitted), add this directory's debian/, vendor
# the Cargo dependencies (the one step that touches the network), and run
# dpkg-buildpackage. The result lands in ./dist/.
#
# Run this ON Debian/Ubuntu (needs: build-essential, debhelper, cargo,
# rustc >= 1.85, libnautilus-extension-dev, libglib2.0-dev, libfuse3-dev,
# pkgconf — mise is used for cargo/rustc if present, else the system ones).
# To build from a macOS host, use scripts/podman-deb.sh, which runs this
# inside a Debian container.
#
# Options / env:
#   --check-version    validate the version only (see EXPECT_VERSION) and exit
#   EXPECT_VERSION=X   fail unless the packaged version is X — see build-rpm.sh
#                      for why this exists; same rationale, same contract.
set -euo pipefail

CHECK_ONLY=0
case "${1:-}" in
    --check-version) CHECK_ONLY=1 ;;
    "") ;;
    *) echo "!! unknown argument: $1" >&2; exit 2 ;;
esac

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO"

if command -v mise >/dev/null 2>&1; then RUN=(mise exec --); else RUN=(); fi

# See build-rpm.sh for why `cargo pkgid`, not a manifest grep.
PKGID="$("${RUN[@]}" cargo pkgid -p wusel)"
VERSION="${PKGID##*[#@]}"
case "$VERSION" in
    [0-9]*.[0-9]*) ;;
    *) echo "!! could not read the wusel version (cargo pkgid: $PKGID)" >&2; exit 1 ;;
esac

if [ -n "${EXPECT_VERSION:-}" ] && [ "${EXPECT_VERSION#v}" != "$VERSION" ]; then
    echo "!! version mismatch: expected ${EXPECT_VERSION#v} (from ${EXPECT_VERSION})," >&2
    echo "!! but the workspace manifest says $VERSION." >&2
    echo "!! Bump the version in Cargo.toml, or re-tag — do not publish these apart." >&2
    exit 1
fi

if [ "$CHECK_ONLY" = 1 ]; then
    echo ">> version OK: $VERSION"
    exit 0
fi

if [ -n "$(git status --porcelain)" ]; then
    echo ">> Note: uncommitted changes are present and will NOT be in this build" >&2
    echo ">>       (the source is 'git archive HEAD')." >&2
fi

echo ">> Building Wusel $VERSION .deb (from source) ..."

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
SRC="$STAGE/wusel-$VERSION"

echo ">> Archiving the source tree ..."
mkdir -p "$SRC"
git archive HEAD | tar -x -C "$SRC"
cp -a packaging/deb/debian "$SRC/debian"

# debian/changelog's version is static text, unlike the RPM spec's
# %{wusel_version} macro — keep it from drifting from the one source of
# truth (Cargo.toml) by rewriting just the version on the top entry. No
# "-1" revision suffix: this is source format "3.0 (native)" (see
# debian/source/format — there is no separate upstream to version against,
# this repository *is* upstream), and a native package's version may not
# carry a debian_revision at all. `dpkg-buildpackage -b` never caught this
# (it skips building the .dsc entirely) — `dpkg-source -b` does.
sed -i.bak "1s/^wusel ([^)]*)/wusel ($VERSION)/" "$SRC/debian/changelog"
rm -f "$SRC/debian/changelog.bak"

# The one point in this whole build that touches the network — see the same
# note in build-rpm.sh. dpkg-buildpackage itself runs fully offline from here.
echo ">> Vendoring dependencies ..."
"${RUN[@]}" cargo vendor "$SRC/vendor" --locked >/dev/null

echo ">> dpkg-buildpackage ..."
(cd "$SRC" && dpkg-buildpackage -b -us -uc --no-sign)

mkdir -p "$REPO/dist"
find "$STAGE" -maxdepth 1 -name '*.deb' -exec cp {} "$REPO/dist/" \;
echo ">> Done. .deb(s):"
ls -1 "$REPO/dist"/*.deb
