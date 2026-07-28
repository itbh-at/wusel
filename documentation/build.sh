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

# Diagrams are pre-rendered committed SVGs (see diagrams/ and `mise run
# docs-diagrams`), so this build needs nothing beyond antora — no server.

case "${1:-build}" in
  watch)
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
    antora antora-playbook.yml
    ;;
  *)
    echo "Usage: $0 [build|watch]" >&2
    exit 1
    ;;
esac
