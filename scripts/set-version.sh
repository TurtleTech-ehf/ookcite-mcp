#!/usr/bin/env bash
set -euo pipefail
version="$1"
VERSION="$version" perl -0pi -e 's/^version = "[^"]*"/version = "$ENV{VERSION}"/m' Cargo.toml
VERSION="$version" perl -0pi -e 's/"version": "[^"]*"/"version": "$ENV{VERSION}"/' npm/package.json
# The registry manifest carries the version twice: the server's own, and the
# npm package it points at. A stale manifest advertises a version the registry
# cannot resolve, so both move with every bump.
VERSION="$version" perl -0pi -e 's/"version": "[^"]*"/"version": "$ENV{VERSION}"/g' server.json
