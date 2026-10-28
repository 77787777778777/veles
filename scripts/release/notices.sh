#!/usr/bin/env bash
# Regenerate THIRD_PARTY_LICENSES.md from Cargo.lock with cargo-about.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
nix shell nixpkgs#cargo-about -c cargo about generate --locked \
    -c about.toml scripts/release/notices.hbs -o THIRD_PARTY_LICENSES.md
echo "notices: $(grep -c '^## ' THIRD_PARTY_LICENSES.md) licenses written to THIRD_PARTY_LICENSES.md"
