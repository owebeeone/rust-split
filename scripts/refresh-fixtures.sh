#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
corpus_root="${1:-/Users/owebeeone/limbo/glial-dev}"
fixture_root="$repo_root/tests/fixtures/real"

rm -rf "$fixture_root/gwz-core" "$fixture_root/gwz-cli"
mkdir -p "$fixture_root"

cd "$corpus_root"
find gwz-core gwz-cli \
  -path '*/target' -prune -o \
  -path '*/.git' -prune -o \
  -name '*.rs' -print |
  while IFS= read -r rel; do
    mkdir -p "$fixture_root/$(dirname "$rel")"
    cp "$rel" "$fixture_root/$rel"
  done
