#!/usr/bin/env bash
# scripts/test-export-public.sh — the public export names the commit it
# was made from.
#
# WHY. The public tree is an export of this one with commit ids of its
# own, so a published build reports a revision no build of this tree can
# share. 0.2.2 ran on four nodes, two built here and two from the
# published artifacts, and the fleet's consistency check could not tell
# that they were the same source. scripts/release/export-public.sh now
# writes the commit it exported into SNAPSHOT_OF at the root of the
# snapshot; the build stamps it (crates/veles-frontends/build.rs) and
# the check compares it (fleet_obs::consistency).
#
# This runs the real export of the tree about to be committed into a
# scratch directory and reads the marker back.
set -euo pipefail
cd "$(dirname "$0")/.."
work=$(mktemp -d "${TMPDIR:-/tmp}/veles-export-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }

rev=$(git stash create 2>/dev/null) || true
sha=$(git rev-parse "${rev:-HEAD}")
dest="$work/public"

bash scripts/release/export-public.sh "$sha" "$dest" >"$work/export.log" 2>&1 \
    || { cat "$work/export.log" >&2; fail "the export of $sha did not pass"; }
[ -f "$dest/SNAPSHOT_OF" ] || fail "the snapshot has no SNAPSHOT_OF"
[ "$(cat "$dest/SNAPSHOT_OF")" = "$sha" ] \
    || fail "SNAPSHOT_OF holds '$(head -c 80 "$dest/SNAPSHOT_OF")', not $sha"
[ "$(wc -l <"$dest/SNAPSHOT_OF")" = 1 ] || fail "SNAPSHOT_OF is not exactly one line"
# It is the full id: a build compares it with a full revision.
[[ $sha =~ ^[0-9a-f]{40}$ ]] || fail "not a full commit id: $sha"

# The next release's export replaces what the last one left.
echo "0000000000000000000000000000000000000000" >"$dest/SNAPSHOT_OF"
bash scripts/release/export-public.sh "$sha" "$dest" >/dev/null 2>&1
[ "$(cat "$dest/SNAPSHOT_OF")" = "$sha" ] || fail "a second export left the old marker"

# The export's first job still holds with the marker written.
[ ! -e "$dest/PENDING.md" ] || fail "a private note reached the snapshot"
[ -f "$dest/Cargo.toml" ] || fail "the snapshot has no Cargo.toml"

# --check reports and writes nothing.
bash scripts/release/export-public.sh --check "$sha" >/dev/null 2>&1 || fail "--check did not pass"

# A tree that is itself a snapshot is not exported again: its marker
# would be replaced by a commit id of the public tree, which names
# nothing a development build can share.
git init -q "$work/snap"
cp -a "$dest"/. "$work/snap"/
git -C "$work/snap" add -A
git -C "$work/snap" -c user.name=probe -c user.email=probe@example.invalid \
    -c commit.gpgsign=false commit -q -m "a snapshot"
if bash "$work/snap/scripts/release/export-public.sh" HEAD "$work/again" >"$work/again.log" 2>&1; then
    fail "a snapshot was exported again"
fi
grep -q "is itself a snapshot" "$work/again.log" \
    || { cat "$work/again.log" >&2; fail "the re-export failed for another reason"; }

echo "PASS: the public export names its source commit ($sha)"
