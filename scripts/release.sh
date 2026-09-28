#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Prepare a release: `mise run release 0.4.2`.
#
# On a fresh `release/<version>` branch off a clean `main`, this sets the
# version in Cargo.toml, brings Cargo.lock along, and opens the entry in all
# three changelogs — the RPM spec, debian/changelog and the documentation's
# changelog page — with version, date and author filled in. What is left by
# hand is the text of the release notes (a TODO line in each file), then commit,
# merge request and tag.
#
# Nobody types an entry header: the date comes from `date`, the name from git.
# A hand-written weekday went wrong once, and rpmbuild only warns about it; a
# release that bumped Cargo.toml but not Cargo.lock needed a follow-up fix.
set -euo pipefail

NEW="${1:-}"
# Plain MAJOR.MINOR.PATCH: the tag, the RPM version and the Debian version are
# all derived from it, and a pre-release suffix sorts differently in each.
if ! printf '%s\n' "$NEW" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    echo "usage: mise run release <MAJOR.MINOR.PATCH>" >&2
    exit 2
fi

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

SPEC=packaging/rpm/wusel.spec
DEB=packaging/deb/debian/changelog
DOC=documentation/modules/ROOT/pages/project/changelog.adoc

if [ "$(git branch --show-current)" != main ]; then
    echo "!! run this on main — it branches off release/$NEW itself" >&2
    exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
    echo "!! the working tree is not clean — commit or stash first" >&2
    exit 1
fi
if git rev-parse -q --verify "refs/heads/release/$NEW" >/dev/null \
    || git rev-parse -q --verify "refs/tags/v$NEW" >/dev/null; then
    echo "!! release/$NEW or v$NEW exists already" >&2
    exit 1
fi
if grep -q -- "- $NEW-1\$" "$SPEC" || grep -q "^wusel ($NEW)" "$DEB" \
    || grep -q "^== $NEW " "$DOC"; then
    echo "!! a changelog entry for $NEW exists already" >&2
    exit 1
fi

if command -v mise >/dev/null 2>&1; then RUN=(mise exec --); else RUN=(); fi
PKGID="$("${RUN[@]}" cargo pkgid -p wusel)"
OLD="${PKGID##*[#@]}"
# sort -V puts the higher version last; the new one has to be it, and differ.
if [ "$OLD" = "$NEW" ] || [ "$(printf '%s\n%s\n' "$OLD" "$NEW" | sort -V | tail -n1)" != "$NEW" ]; then
    echo "!! $NEW is not above the current version $OLD" >&2
    exit 1
fi

NAME="$(git config user.name)"
EMAIL="$(git config user.email)"
[ -n "$NAME" ] && [ -n "$EMAIL" ] \
    || { echo "!! git user.name / user.email are not set" >&2; exit 1; }

git switch -q -c "release/$NEW"

# The version lives once, in [workspace.package]; every crate inherits it. Only
# that table's line is touched — a dependency with the same version string
# elsewhere in the manifest must stay as it is.
awk -v old="$OLD" -v new="$NEW" '
/^\[/ { in_pkg = ($0 == "[workspace.package]") }
in_pkg && $0 == "version = \"" old "\"" { $0 = "version = \"" new "\""; hit = 1 }
{ print }
END { exit !hit }
' Cargo.toml >Cargo.toml.new || { rm -f Cargo.toml.new; echo "!! no version = \"$OLD\" in [workspace.package]" >&2; exit 1; }
mv Cargo.toml.new Cargo.toml
# Only the workspace's own crates: no dependency moves as a side effect.
"${RUN[@]}" cargo update --workspace --quiet

# One instant, three spellings. LC_ALL=C because both package formats want the
# English day and month names, whatever the user's locale.
NOW="$(date +%s)"
fmt() { LC_ALL=C date -d "@$NOW" "$1" 2>/dev/null || LC_ALL=C date -r "$NOW" "$1"; }
RPM_DATE="$(fmt '+%a %b %d %Y')"
DEB_DATE="$(fmt '+%a, %d %b %Y %H:%M:%S %z')"
ISO_DATE="$(fmt '+%Y-%m-%d')"

# awk rather than `sed -i`: portable between GNU and BSD, and the multi-line
# insertions stay readable.
insert() { # file, awk program
    awk -v v="$NEW" -v who="$NAME <$EMAIL>" -v rpm="$RPM_DATE" \
        -v deb="$DEB_DATE" -v iso="$ISO_DATE" "$2" "$1" >"$1.new"
    mv "$1.new" "$1"
}

insert "$SPEC" '
{ print }
/^%changelog/ && !done { printf "* %s %s - %s-1\n- TODO: describe the release\n\n", rpm, who, v; done = 1 }
'
insert "$DEB" '
NR == 1 { printf "wusel (%s) unstable; urgency=medium\n\n  * TODO: describe the release\n\n -- %s  %s\n\n", v, who, deb }
{ print }
'
insert "$DOC" '
/^== / && !done { printf "== %s — %s\n\n=== Changed\n\n* TODO: describe the release\n\n", v, iso; done = 1 }
{ print }
'

echo ">> on branch release/$NEW: version $OLD -> $NEW (Cargo.toml, Cargo.lock)"
echo ">> write the release notes over the TODO lines in:"
printf '   %s\n' "$SPEC" "$DEB" "$DOC"
echo ">> then: git commit -am 'chore(release): $NEW', merge request, and after the merge the tag v$NEW"
