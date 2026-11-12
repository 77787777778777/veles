#!/usr/bin/env bash
# scripts/release/check-public-tree.sh — run the flake's checks on the public
# snapshot of <commit> as a published build sees it: a git tree with a
# revision of its own and the SNAPSHOT_OF marker.
#
# WHY. The first 0.2.3 tag was gated with `nix flake check path:<export>`.
# A path has no revision, so the flake told the build
# `unreported-dirty-build`, and a build that does not know its source is not
# what the public repository builds: a test that compares two fingerprints
# passed there and failed in the public CI, on the tagged commit, with every
# release lane already green. Here the scratch export becomes a git
# repository with one commit, so the flake has a revision and the tests run
# with it and with the marker, as they do in the public CI.
#
# Run it before the tag, after the gate on the same commit:
#   NIX_FLAGS="--cores 4 --max-jobs 1" bash scripts/release/check-public-tree.sh <commit>
set -euo pipefail
cd "$(dirname "$0")/../.."
commit=${1:-HEAD}
sha=$(git rev-parse "$commit^{commit}")
work=$(mktemp -d "${TMPDIR:-/tmp}/veles-public-tree.XXXXXX")
trap 'rm -rf "$work"' EXIT

bash scripts/release/export-public.sh "$sha" "$work/tree"
git -C "$work/tree" init -q
git -C "$work/tree" add -A 2>/dev/null
git -C "$work/tree" -c user.name=snapshot -c user.email=snapshot@example.invalid \
    -c commit.gpgsign=false commit -q -m "snapshot of $sha"
rev=$(git -C "$work/tree" rev-parse HEAD)
url="git+file://$work/tree"
system=$(nix eval --impure --raw --expr builtins.currentSystem)

# What a published build is told about its source, before anything is built.
told_rev=$(nix eval --raw "$url#packages.$system.default.VELES_SOURCE_REVISION")
told_of=$(nix eval --raw "$url#packages.$system.default.VELES_SNAPSHOT_OF")
[ "$told_rev" = "$rev" ] || {
    echo "check-public-tree: the build is told revision '$told_rev'; the tree is $rev" >&2
    exit 1
}
[ "$told_of" = "$sha" ] || {
    echo "check-public-tree: the build is told it is a snapshot of '$told_of'; it was exported from $sha" >&2
    exit 1
}
echo "check-public-tree: $sha exported as $rev; running the flake's checks on it"
# shellcheck disable=SC2086
nix flake check --no-eval-cache --keep-going ${NIX_FLAGS:-} "$url"
echo "check-public-tree: the public tree of $sha passes as a published build"
