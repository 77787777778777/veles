#!/usr/bin/env bash
# Export the public release snapshot: the tracked tree at <commit>, minus
# the paths in public-exclude.txt, refusing when a private identifier
# (public-forbidden.txt) or a credential-shaped string is left in it.
# <dest> is the public repository's checkout: its tree is REPLACED by the
# snapshot (its .git is kept), ready for one commit per release.
#
# Usage: scripts/release/export-public.sh <commit> <dest-dir>
#        scripts/release/export-public.sh --check <commit>   # report only
set -euo pipefail
shopt -s globstar dotglob nullglob
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
check=0
if [ "${1:-}" = --check ]; then check=1; shift; fi
commit=${1:?usage: export-public.sh [--check] <commit> [<dest-dir>]}
dest=${2:-}
[ "$check" = 1 ] || [ -n "$dest" ] || { echo "export-public: no destination" >&2; exit 2; }
# The forbidden list is read from the working tree, never from <commit>:
# it is private, and an export of an old commit must meet today's list.
forbidden="$here/public-forbidden.txt"
[ -s "$forbidden" ] || { echo "export-public: $forbidden is missing" >&2; exit 2; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
git -C "$root" archive --format=tar "$commit" | tar -x -C "$tmp"
while IFS= read -r pat; do
    case "$pat" in ''|'#'*) continue ;; esac
    (cd "$tmp" && for p in $pat; do rm -rf -- "$p"; done)
done < "$here/public-exclude.txt"

fail=0
# Private identifiers, by file and line.
if hits=$(grep -rniIE -f <(grep -vE '^\s*(#|$)' "$forbidden") "$tmp" 2>/dev/null); then
    echo "export-public: private identifiers left in the snapshot:" >&2
    printf '%s\n' "$hits" | sed "s|^$tmp/||" | head -100 >&2
    echo "  ($(printf '%s\n' "$hits" | wc -l) lines in $(printf '%s\n' "$hits" | cut -d: -f1 | sort -u | wc -l) files)" >&2
    fail=1
fi
# Credential shapes. Test fixtures use obviously fake values; anything
# that looks live is a stop, not a warning.
cred='(-----BEGIN [A-Z ]*PRIVATE KEY-----)|(AGE-SECRET-KEY-1[0-9A-Z]{20,})|(gh[pousr]_[A-Za-z0-9]{36})|(AKIA[0-9A-Z]{16})|(xox[baprs]-[A-Za-z0-9-]{20,})|([0-9]{8,10}:AA[A-Za-z0-9_-]{33})'
if hits=$(grep -rnIE "$cred" "$tmp" 2>/dev/null | grep -vE 'canary|fixture|example|fake|test|0123456789abcdef|ABCDEFGHIJ' ); then
    echo "export-public: credential-shaped strings:" >&2
    printf '%s\n' "$hits" | sed "s|^$tmp/||" | cut -c1-160 >&2
    fail=1
fi
find "$tmp" -type f \( -name '*.p12' -o -name '*.jks' -o -name '*.keystore' -o -name '.env' -o -name 'secrets.env' \) \
    -printf 'export-public: secret-bearing file: %P\n' | grep . >&2 && fail=1
[ "$fail" = 0 ] || exit 1

echo "export-public: $(find "$tmp" -type f | wc -l) files at $(git -C "$root" rev-parse --short "$commit"), clean"
[ "$check" = 1 ] && exit 0
mkdir -p "$dest"
# Replace the destination's tree, keep its history.
find "$dest" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf -- {} +
cp -a "$tmp"/. "$dest"/
echo "export-public: written to $dest"
