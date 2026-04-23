#!/usr/bin/env bash
set -euo pipefail
version="$1"
perl -0pi -e "s/^version = \"[^\"]+\"/version = \"\${version}\"/m" Cargo.toml
perl -0pi -e "s/\"version\": \"[^\"]+\"/\"version\": \"\${version}\"/" npm/package.json
