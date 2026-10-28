#!/usr/bin/env bash
# scripts/i18n-verify.sh — wrapper for i18n-verify.tcl.
#
# Exists because the translators' brief tells them to run a checker, and
# a bare `.tcl` path invites "call it with tclsh" guesses that fail in a
# shell without the dev environment. This resolves the dev shell itself.
#
# args: <tsv> [--part N] [keys-file]
#
#   Default: the TSV must cover every key in the key list.
#   --part N: the TSV is ONE part of a chunked job; it is checked
#             against keys-part-<N>.txt only, and "extra" is judged
#             against the full list (a part must not invent keys).
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TSV="${1:?usage: i18n-verify.sh <tsv> [--part N] [keys-file]}"
shift

PART=""
KEYS=""
while [ $# -gt 0 ]; do
    case "$1" in
        --part) PART="${2:?--part needs a number}"; shift 2 ;;
        *) KEYS="$1"; shift ;;
    esac
done

if [ -n "$PART" ]; then
    KEYS="${KEYS:-/home/user/i18n-work/keys-part-$PART.txt}"
    FULL=/home/user/i18n-work/keys.txt
else
    KEYS="${KEYS:-/home/user/i18n-work/keys.txt}"
    FULL="$KEYS"
fi

run() {
    if command -v direnv >/dev/null 2>&1; then
        direnv exec "$ROOT" tclsh "$ROOT/scripts/i18n-verify.tcl" "$@"
    else
        tclsh "$ROOT/scripts/i18n-verify.tcl" "$@"
    fi
}
run "$TSV" "$KEYS" "$FULL" "$ROOT/crates/veles-core/locales/en.yml"