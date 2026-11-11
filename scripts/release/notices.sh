#!/usr/bin/env bash
# Regenerate THIRD_PARTY_LICENSES.md from Cargo.lock with cargo-about.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
nix shell nixpkgs#cargo-about -c cargo about generate --locked \
    -c about.toml scripts/release/notices.hbs -o THIRD_PARTY_LICENSES.md
# The Linux packages also ship tcl-lsp (flake.nix tclLsp, DEV-6): its crates
# come from its own Cargo.lock, in the source at the revision flake.lock pins.
tcl_lsp_src=$(nix eval --raw ".#tcl-lsp.src")
nix shell nixpkgs#cargo-about -c cargo about generate --locked \
    --manifest-path "$tcl_lsp_src/Cargo.toml" \
    -c about.toml scripts/release/notices-tcl-lsp.hbs >> THIRD_PARTY_LICENSES.md
grep -q '^# tcl-lsp$' THIRD_PARTY_LICENSES.md || { echo "notices: the tcl-lsp section is missing" >&2; exit 1; }
# Some crates' license files have CRLF line ends; the tracked file is LF
# (.gitattributes), and the bundles copy the working tree's.
sed -i 's/\r$//' THIRD_PARTY_LICENSES.md
echo "notices: $(grep -c '^## ' THIRD_PARTY_LICENSES.md) licenses written to THIRD_PARTY_LICENSES.md"
