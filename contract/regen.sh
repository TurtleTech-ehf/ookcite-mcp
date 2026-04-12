#!/usr/bin/env bash
# Regenerate the encrypted OpenAPI contract spec from ttech-cite.
#
# Usage:
#   ./contract/regen.sh
#
# Requires:
#   - OOKCITE_CONTRACT_KEY in env, or pass entry at turtletech/ookcite-contract-key
#   - ttech-cite repo checked out next to this one (or set CITE_DIR)
#
# The plaintext spec is generated in ttech-cite/docs/openapi.json (private repo),
# then encrypted to contract/openapi.json.age (public repo). The plaintext must
# never be committed to this repo -- .gitignore enforces this.

set -euo pipefail

cd "$(dirname "$0")/.."

CITE_DIR="${CITE_DIR:-../ttech-cite}"
if [ ! -d "$CITE_DIR" ]; then
    echo "error: ttech-cite not found at $CITE_DIR (set CITE_DIR env var)" >&2
    exit 1
fi

KEY="${OOKCITE_CONTRACT_KEY:-}"
if [ -z "$KEY" ]; then
    if command -v pass >/dev/null 2>&1; then
        KEY="$(pass show turtletech/ookcite-contract-key 2>/dev/null)" || true
    fi
fi
if [ -z "$KEY" ]; then
    echo "error: OOKCITE_CONTRACT_KEY not set and not in pass at turtletech/ookcite-contract-key" >&2
    exit 1
fi

echo "Regenerating plaintext spec in ttech-cite..."
(cd "$CITE_DIR" && cargo test --bin ttech-cite -- export_openapi_spec --ignored >/dev/null 2>&1)

SPEC="$CITE_DIR/docs/openapi.json"
if [ ! -f "$SPEC" ]; then
    echo "error: spec not generated at $SPEC" >&2
    exit 1
fi

echo "Encrypting to contract/openapi.json.age..."
# Pipe the passphrase to rage via a file descriptor trick using a helper.
# rage doesn't accept passphrase on stdin, so we use a tiny Rust helper via
# cargo-run-bin pattern -- or, when the encryption key crate is available,
# just call it through an inline Cargo invocation.

# Use the encryption helper binary compiled into this crate.
cargo run --quiet --bin contract-encrypt -- "$SPEC" contract/openapi.json.age <<< "$KEY"

echo "Done. contract/openapi.json.age is $(wc -c < contract/openapi.json.age) bytes."
