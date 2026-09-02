#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Builds the Antora docs.
#
#   ./build.sh          # official build: calls antora once
#   ./build.sh watch    # live mode: rebuild on file change (event-driven)
#
# `antora` and `watchexec` are pinned in mise.toml. In a mise-activated shell
# they are on PATH; otherwise prefix with `mise exec --`.
set -euo pipefail
cd "$(dirname "$0")"

# The lunr search extension is a library, not a command: antora `require`s it
# while building. mise gives every npm package its own prefix, so it is not a
# sibling of antora's own node_modules and plain module resolution misses it —
# NODE_PATH is what makes `require: '@antora/lunr-extension'` in the playbooks
# resolve. mise gives every npm package its own prefix; the module lives *some*
# levels below it, but not always at the same subpath — mise 2027.x changed the
# npm layout, and a hardcoded `$prefix/lib/node_modules` (correct on 2026.x)
# then pointed at nothing, so antora failed with "Cannot find module". Find the
# module wherever mise actually put it and set NODE_PATH to the directory that
# holds its `@antora/` scope, so this survives the next layout change too.
if ! lunr_prefix="$(mise where "npm:@antora/lunr-extension" 2>/dev/null)" || [ -z "$lunr_prefix" ]; then
  echo "build.sh: @antora/lunr-extension is missing — run: mise install 'npm:@antora/lunr-extension'" >&2
  exit 1
fi
lunr_module="$(find "$lunr_prefix" -type d -path '*/@antora/lunr-extension' -print 2>/dev/null | head -1)"
if [ -z "$lunr_module" ]; then
  echo "build.sh: @antora/lunr-extension installed at $lunr_prefix but its module dir was not found" >&2
  exit 1
fi
# NODE_PATH must hold the dir *containing* the @antora scope, i.e. two up.
NODE_PATH="$(cd "$lunr_module/../.." && pwd)"
export NODE_PATH

# Diagrams are pre-rendered committed SVGs (see diagrams/ and `mise run
# docs-diagrams`), so this build needs nothing beyond antora — no server.
#
# That independence is worth keeping, but it means the build would happily embed
# an SVG that no longer matches its source. check-diagrams.sh closes that hole
# using hashes alone, so it costs the build nothing and needs no d2.

case "${1:-build}" in
  watch)
    # A stale diagram only warns here: live editing must not be blocked by a
    # picture the author may be in the middle of changing.
    ./check-diagrams.sh || echo "build.sh: continuing anyway (watch mode)" >&2

    # Live preview: initial build, then serve over HTTP with the default browser
    # opened and auto-reload, while rebuilding on source change (event-driven).
    antora antora-playbook.yml

    # browser-sync serves build/site, opens the default browser, and reloads the
    # page whenever the generated site changes.
    browser-sync start --server build/site --startPath / \
      --files 'build/site/**' --no-ui --no-notify &
    bs_pid=$!
    trap 'kill "$bs_pid" 2>/dev/null' EXIT INT TERM

    # Rebuild only on change. Ignore the output dir, otherwise antora's own
    # writes (build/site contains css/js/svg) would retrigger in a loop.
    watchexec --debounce 500ms --ignore 'build/**' \
      --exts adoc,yml,hbs,css,js,svg -- antora antora-playbook.yml
    ;;
  build)
    # An official build refuses to ship pictures that no longer match their
    # source. This is also what makes the Pages workflow catch it.
    ./check-diagrams.sh
    antora antora-playbook.yml
    ;;
  site)
    # The published, multi-version site: the default branch as "latest" plus each
    # release tag as its number (antora-playbook-site.yml). Needs the repo's full
    # history and tags — the Pages workflow fetches them. Same diagram guard.
    ./check-diagrams.sh
    antora antora-playbook-site.yml
    ;;
  *)
    echo "Usage: $0 [build|watch|site]" >&2
    exit 1
    ;;
esac
