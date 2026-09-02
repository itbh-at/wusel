#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Build a Wusel RPM — from source, the way a build service (COPR, OBS, `mock`)
# would: an SRPM carrying the source tree plus a `cargo vendor` tarball, then
# `rpmbuild --rebuild` on that SRPM with no further network access. This
# script is the entry point for a local/CI build, but it exercises the exact
# same two steps a submission to COPR/OBS goes through, so a spec regression
# shows up here first.
#
# Builds from `HEAD`, via `git archive` — not the working tree. Commit first
# if you are iterating on something not yet committed; this mirrors what an
# actual release tag would contain, which is the point of testing this way.
#
# Run this ON Fedora, with rpm-build, gcc, make, pkgconf, nautilus-devel,
# glib2-devel, fuse3-devel, AND rust >= 1.85 / cargo installed via `dnf`
# specifically — not just on PATH. The `cargo vendor` step below happily uses
# mise's pinned toolchain if present, but `rpmbuild --rebuild` runs its own
# %build in a buildroot that resolves wusel.spec's BuildRequires only against
# installed RPM packages, the same as a real COPR/OBS/`mock` build — mise is
# invisible there no matter what is on this shell's PATH. Skip the `dnf
# install` and this script still gets partway (the SRPM builds fine), then
# fails at the rebuild step with "cargo is needed" / "rust >= 1.85 is needed"
# even though `cargo --version` works right above it. To build from a macOS
# host without a Fedora machine, use scripts/podman-rpm.sh, which runs this
# inside a Fedora container that already has the dnf packages.
#
# Options / env:
#   --check-version    validate the version only (see EXPECT_VERSION) and exit
#   EXPECT_VERSION=X   fail unless the packaged version is X. The release
#                      workflow passes the git tag here: the tag decides what the
#                      release page is called, the manifest decides what goes
#                      into the .rpm, and nothing else forces those to agree —
#                      so tagging v0.2.0 without bumping Cargo.toml would happily
#                      publish "v0.2.0" carrying wusel-0.1.0-*.rpm.
set -euo pipefail

CHECK_ONLY=0
case "${1:-}" in
    --check-version) CHECK_ONLY=1 ;;
    "") ;;
    *) echo "!! unknown argument: $1" >&2; exit 2 ;;
esac

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO"

# Pinned toolchain via mise when available, else fall back to the system cargo.
if command -v mise >/dev/null 2>&1; then RUN=(mise exec --); else RUN=(); fi

# Ask cargo for the version instead of grepping the manifest: `grep -m1
# '^version = '` picks whatever `version =` comes first in the file, which is the
# right one only by accident of table ordering — a `[workspace.dependencies.foo]`
# section moved above `[workspace.package]` would silently package that crate's
# version. cargo pkgid prints `…#<version>` or `…#<name>@<version>`; stripping
# through the last `#`/`@` handles both spellings.
PKGID="$("${RUN[@]}" cargo pkgid -p wusel)"
VERSION="${PKGID##*[#@]}"
case "$VERSION" in
    [0-9]*.[0-9]*) ;;
    *) echo "!! could not read the wusel version (cargo pkgid: $PKGID)" >&2; exit 1 ;;
esac

# The tag/manifest guard. Both spellings are accepted so the caller can pass the
# raw tag ("v0.1.0") or the bare version ("0.1.0").
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
    echo ">>       (Source0 is 'git archive HEAD' — see the header comment)." >&2
fi

echo ">> Building Wusel $VERSION RPM (from source) ..."

RPMTOP="$(mktemp -d)"
VENDOR_STAGE="$(mktemp -d)"
trap 'rm -rf "$RPMTOP" "$VENDOR_STAGE"' EXIT
mkdir -p "$RPMTOP"/{SOURCES,SPECS,BUILD,RPMS,SRPMS}

echo ">> Archiving the source tree (Source0) ..."
git archive --format=tar.gz --prefix="wusel-$VERSION/" \
    -o "$RPMTOP/SOURCES/wusel-$VERSION.tar.gz" HEAD

# The one point in this whole build that touches the network: `mock`/COPR/OBS
# never run this script, only the two rpmbuild calls below, against the
# tarball this produces.
echo ">> Vendoring dependencies (Source1) ..."
"${RUN[@]}" cargo vendor "$VENDOR_STAGE/vendor" --locked >/dev/null
tar -C "$VENDOR_STAGE" -czf "$RPMTOP/SOURCES/wusel-$VERSION-vendor.tar.gz" vendor

cp packaging/rpm/wusel.spec "$RPMTOP/SPECS/"

echo ">> Building the SRPM ..."
rpmbuild --define "_topdir $RPMTOP" \
         --define "wusel_version $VERSION" \
         -bs "$RPMTOP/SPECS/wusel.spec"
SRPM="$(find "$RPMTOP/SRPMS" -name '*.src.rpm')"
[ -n "$SRPM" ] || { echo "!! no SRPM produced"; exit 1; }

echo ">> Rebuilding the SRPM — the same operation a build service performs ..."
# `wusel_version` is an rpmbuild macro, not something the SRPM carries — the
# spec text inside it still reads `%{?wusel_version}` verbatim, so it needs
# defining again here exactly as for -bs above, or it falls back to 0.0.0 and
# looks for the wrong Source0 filename.
rpmbuild --define "_topdir $RPMTOP" \
         --define "wusel_version $VERSION" \
         --rebuild "$SRPM"

mkdir -p "$REPO/dist"
find "$RPMTOP/RPMS" -name '*.rpm' -exec cp {} "$REPO/dist/" \;
echo ">> Done. RPM(s):"
ls -1 "$REPO/dist"/*.rpm
