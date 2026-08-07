#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Verifies that the committed diagram SVGs still match their D2 sources.
#
#   ./check-diagrams.sh          # verify (exits non-zero when stale)
#   ./check-diagrams.sh --write  # record the current sources as rendered
#
# Why this exists. The docs embed pre-rendered SVGs and the build deliberately
# does not depend on d2 — the Pages workflow installs antora and nothing else.
# That is a good property and it has one sharp edge: nothing noticed when the
# SVGs fell behind their sources. A `mise run docs-diagrams` that fails partway
# leaves some diagrams re-rendered and the rest stale, the docs still build
# green, and the site ships pictures that no longer match the page around them.
#
# So the render records what it rendered from, and this script compares hashes.
# It needs sha256 and nothing else, which is why it can run in a job that has no
# d2 — the whole point.
#
# The manifest covers *every* .d2, partials included: `_steps.d2` is imported by
# nine diagrams, so a change to it invalidates all of them while leaving their
# own sources untouched.

set -euo pipefail
cd "$(dirname "$0")"

MANIFEST="diagrams/rendered.sha256"
IMAGES="modules/ROOT/images"

if command -v sha256sum >/dev/null 2>&1; then
  hash_files() { sha256sum "$@"; }
elif command -v shasum >/dev/null 2>&1; then
  hash_files() { shasum -a 256 "$@"; }
else
  echo "check-diagrams: neither sha256sum nor shasum found" >&2
  exit 1
fi

current=$(mktemp)
trap 'rm -f "$current"' EXIT
# Sorted by path so the manifest is stable and its diffs are readable.
hash_files diagrams/*.d2 | sort -k2 >"$current"

if [ "${1:-}" = "--write" ]; then
  cp "$current" "$MANIFEST"
  echo "check-diagrams: recorded $(wc -l <"$MANIFEST" | tr -d ' ') sources in $MANIFEST"
  exit 0
fi

fail=0

if [ ! -f "$MANIFEST" ]; then
  echo "check-diagrams: $MANIFEST is missing — run 'mise run docs-diagrams'" >&2
  exit 1
fi

if ! diff -q "$MANIFEST" "$current" >/dev/null 2>&1; then
  echo "check-diagrams: these diagram sources changed since the SVGs were rendered:" >&2
  # Both sides of the diff, so an added or removed source is named too.
  diff "$MANIFEST" "$current" | awk '/^[<>]/ {print $3}' | sort -u | sed 's/^/  /' >&2
  fail=1
fi

# A source with no SVG at all — the case a hash comparison alone would miss,
# because the manifest would happily agree with itself.
for f in diagrams/*.d2; do
  base=$(basename "$f" .d2)
  case "$base" in _*) continue ;; esac   # a partial, not a diagram of its own
  if [ ! -f "$IMAGES/$base.svg" ]; then
    echo "check-diagrams: $IMAGES/$base.svg is missing (source: $f)" >&2
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  echo "" >&2
  echo "Run 'mise run docs-diagrams' and commit the regenerated SVGs." >&2
  exit 1
fi
