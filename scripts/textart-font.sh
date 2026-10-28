#!/usr/bin/env bash
# textart-font.sh — rebuild crates/veles-textart/font/unifont.bin.z, the
# glyph table veles-textart draws IRC art with, from GNU Unifont's own
# precompiled .hex files (the source tarball nixpkgs' `unifont` builds
# from, so the version is the flake's).
#
#   direnv exec . scripts/textart-font.sh
#
# Unifont is dual-licensed: GPL-2.0-or-later with the font embedding
# exception, and SIL OFL 1.1. The OFL text is copied beside the table.
set -euo pipefail
cd "$(dirname "$0")/.."

tarball=$(nix build --inputs-from . nixpkgs#unifont.src --no-link --print-out-paths)
version=$(nix eval --inputs-from . --raw nixpkgs#unifont.version)
work=$(mktemp -d "${TMPDIR:-/dev/shm}/textart-font.XXXXXX")
trap 'rm -rf "$work"' EXIT

top="unifont-$version"
tar -xzf "$tarball" -C "$work" \
    "$top/font/precompiled/unifont-$version.hex" \
    "$top/font/precompiled/unifont_upper-$version.hex" \
    "$top/OFL-1.1.txt"

dest=crates/veles-textart/font
mkdir -p "$dest"
cargo run --quiet -p veles-textart --example pack_font -- \
    "$dest/unifont.bin.z" \
    "$work/$top/font/precompiled/unifont-$version.hex" \
    "$work/$top/font/precompiled/unifont_upper-$version.hex"
command cp -f "$work/$top/OFL-1.1.txt" "$dest/OFL-1.1.txt"
printf '%s\n' "$version" >| "$dest/VERSION"
echo "unifont $version -> $dest"
