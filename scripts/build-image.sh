#!/usr/bin/env bash
# build-image.sh — build the VPS images on the laptop (no docker or podman
# needed: nix dockerTools writes the image tarballs itself).
#
#   scripts/build-image.sh            both: the gateway and the signer
#   scripts/build-image.sh gateway    one of them
#   scripts/build-image.sh signer
#
# Output, in result-images/ (gitignored — images never enter git):
#   veles-agent-<tag>.tar.gz  (+ .sha256, + .key)   packages.ociImage
#   veles-signer-<tag>.tar.gz (+ .sha256)           packages.ociSignerImage
# <tag> is <version>-<commit>, so the VPS can say which commit it runs.
# `.key` is the module build key of the gateway in the image: what every
# module on the VPS must carry before push-image.sh tags it :current.
#
# Nix builds from the git tree: commit first, or a dirty tree's tag says
# "-dirty" and the image is whatever was staged.
set -euo pipefail
cd "$(dirname "$0")/.."

which="${1:-all}"
case "$which" in
    all) targets="ociImage ociSignerImage" ;;
    gateway) targets="ociImage" ;;
    signer) targets="ociSignerImage" ;;
    *) echo "usage: $0 [all|gateway|signer]" >&2; exit 2 ;;
esac

# Every build and eval below reads ONE flake: the commit at the start.
# `.` is re-read by each nix call, so a commit landing mid-build named
# the file after a tree the image was not built from (2026-10-11: built
# from 3fe82b29, named dec6e0c, and push-image.sh found no such tag).
# A dirty tree still builds `.` (its tag says "-dirty").
if [ -z "$(git status --porcelain --untracked-files=no)" ]; then
    flake="git+file://$PWD?rev=$(git rev-parse HEAD)"
else
    flake="."
fi

mkdir -p result-images
for t in $targets; do
    echo "▸ nix build .#$t (the first build compiles veles, and is slow)"
    out=$(nix build "$flake#$t" --no-link --print-out-paths)
    name=$(nix eval --raw "$flake#$t.imageName")
    tag=$(nix eval --raw "$flake#$t.imageTag")
    file="result-images/$name-$tag.tar.gz"
    # buildImage's output IS a gzipped tarball: copied, not compressed again.
    install -m 0644 "$out" "$file"
    (cd result-images && sha256sum "$(basename "$file")" > "$(basename "$file").sha256")
    echo "  $file ($(du -h "$file" | cut -f1))"
    if [ "$t" = ociImage ]; then
        # The image carries `.#default` (velesPkg); its modules, built
        # with it, hold the gateway's key in `.veles_module_key`
        # (push-plugin.sh reads the same section).
        pkg=$(nix build "$flake#default" --no-link --print-out-paths)
        keybin=$(mktemp)
        objcopy -O binary --only-section=.veles_module_key \
            "$pkg/lib/libveles_module_irc.so" "$keybin"
        key=$(tr -dc '0-9a-f' <"$keybin" | head -c 64)
        rm -f "$keybin"
        [ ${#key} = 64 ] || { echo "no module key in $pkg/lib/libveles_module_irc.so" >&2; exit 1; }
        printf '%s\n' "$key" >"${file%.tar.gz}.key"
        echo "  module build key: $key"
    fi
done
echo "  next: scripts/push-image.sh   (loads them into their users' podman on the VPS)"
