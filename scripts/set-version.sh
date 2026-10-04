#!/usr/bin/env bash
set -euo pipefail
version="$1"
VERSION="$version" perl -0pi -e 's/^version = "[^"]*"/version = "$ENV{VERSION}"/m' Cargo.toml
VERSION="$version" perl -0pi -e 's/"version": "[^"]*"/"version": "$ENV{VERSION}"/' npm/package.json
# The registry manifest carries the version twice: the server's own, and the
# npm package it points at. A stale manifest advertises a version the registry
# cannot resolve, so both move with every bump.
VERSION="$version" perl -0pi -e 's/"version": "[^"]*"/"version": "$ENV{VERSION}"/g' server.json
# The root manifest, the marketplace entry, and the plugin manifest each
# carry one version. A stale manifest advertises a plugin the repository
# does not contain, so each moves with every bump.
shopt -s nullglob
version_files=(plugin.json .*/marketplace.json plugins/*/.*/plugin.json)
if ((${#version_files[@]})); then
    VERSION="$version" perl -0pi -e 's/"version": "[^"]*"/"version": "$ENV{VERSION}"/' \
        "${version_files[@]}"
fi
