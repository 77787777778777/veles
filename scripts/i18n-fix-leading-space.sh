#!/usr/bin/env bash
# i18n-fix-leading-space.sh — repair rows whose key lost its leading spaces.
#
# args: <locale> <part>
#
# WHY THIS EXISTS
#
# A number of keys carry LEADING SPACES, and the spaces are part of the
# key, not formatting: they are the indentation the string is rendered
# with (`  (fallback: %1$s:%2$s)` is a continuation line under another
# row; `  (none)` likewise). en.msg carries them, the key list carries
# them, and `::vi18n::mc` is called with them.
#
# A translator who drops them produces a row whose key matches NOTHING.
# The verifier reports it as "missing" plus an "extra" whose text looks
# identical on screen — which is exactly how this was found (mr part 06
# and yo part 00, one row each): two strings that differ only in leading
# whitespace are indistinguishable in a terminal listing but never match
# at runtime. The cost is a translation that reaches nobody, and the UI
# silently falling back to English.
#
# The fix is mechanical and safe: if a row's key matches no key in the
# part's list, but `  <key>` matches one, rewrite the key to the exact
# spelling from the list, and add the same leading whitespace to the
# VALUE (the translation is rendered in the same position, so it needs
# the same indent).
set -uo pipefail
LOC="${1:?usage: i18n-fix-leading-space.sh <locale> <part>}"
PART="${2:?usage: i18n-fix-leading-space.sh <locale> <part>}"
WORK=/home/user/i18n-work
KEYS="$WORK/keys-part-$PART.txt"
TSV="$WORK/$LOC-part-$PART.tsv"

[ -f "$KEYS" ] || { echo "no key list: $KEYS" >&2; exit 1; }
[ -s "$TSV" ]  || { echo "no TSV: $TSV" >&2; exit 1; }

FIXED="$(mktemp)"
CHANGES=0
while IFS= read -r line; do
    # Only rows with exactly one tab matter; anything else is corrupt
    # and is the build guard's business, not this script's.
    case "$line" in
        *$'\t'*$'\t'*) printf '%s\n' "$line" >> "$FIXED" ; continue ;;
    esac
    key="${line%%$'\t'*}"
    [ "$key" = "$line" ] && { printf '%s\n' "$line" >> "$FIXED" ; continue ; }
    val="${line#*$'\t'}"
    # Exact match? Nothing to do.
    if grep -qxF "$key" "$KEYS"; then
        printf '%s\n' "$line" >> "$FIXED"
        continue
    fi
    # Try adding leading spaces until a key in the list matches.
    hit=""
    for pad in '  ' ' '; do
        if grep -qxF "$pad$key" "$KEYS"; then hit="$pad"; break; fi
    done
    if [ -n "$hit" ]; then
        # Pad the value with the same prefix ONLY when it does not
        # already carry it — a translation that kept its own indent must
        # not gain a second one.
        case "$val" in
            "$hit"*) printf '%s\t%s\n' "$hit$key" "$val" >> "$FIXED" ;;
            *)       printf '%s\t%s\n' "$hit$key" "$hit$val" >> "$FIXED" ;;
        esac
        echo "  padded key with $(printf %q "$hit"): $key" >&2
        CHANGES=$((CHANGES + 1))
    else
        printf '%s\n' "$line" >> "$FIXED"
    fi
done < "$TSV"

if [ "$CHANGES" -gt 0 ]; then
    cp "$TSV" "$TSV.leadspace-bak"
    mv "$FIXED" "$TSV"
    echo "$LOC part $PART: $CHANGES row(s) repaired"
else
    rm -f "$FIXED"
    echo "$LOC part $PART: nothing to repair"
fi