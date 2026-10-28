#!/usr/bin/env bash
# scripts/i18n-symbolic-check.sh — catch symbolic keys copied verbatim.
#
# args: <merged-tsv> [<locale>]
#
# The catalog keys are English source text, EXCEPT for 16 symbolic ids
# (agents_none, choice_retry, doctor_title, …) extracted before that
# convention landed. An id copied straight through — `agents_none` ->
# `agents_none` — is not a translation: the app would render the
# IDENTIFIER in the UI, which is exactly the failure this whole round
# exists to remove.
#
# The check is not "the value differs from the key" in general (many
# English-as-key rows legitimately stay identical — "OK", "HTTP",
# "IRQ"), it is specific to the symbolic set, whose English text lives
# in en.yml.
#
# Prints the offenders, exit 1 when any exist.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TSV="${1:?usage: i18n-symbolic-check.sh <tsv> [locale]}"
LOC="${2:-?}"

# A process substitution is a PIPE (`/dev/fd/63`), and this script greps
# the file once per key. The first grep drains the pipe and every later
# one reads EOF - so with `<(…)` the loop found nothing, took the
# `[ -z "$row" ] && continue` branch every time, and printed
# "symbolic keys OK" for ANY input. That is how a vacuous pass looks:
# the driver called this with `<(merge_parts …)`, so it had been reporting
# success without reading a single row. Copy to a real file first.
if [ ! -f "$TSV" ]; then
    TMP_TSV="$(mktemp)"
    cat "$TSV" > "$TMP_TSV" || { echo "cannot read $TSV" >&2; exit 2; }
    TSV="$TMP_TSV"
    trap 'rm -f "$TMP_TSV"' EXIT
fi

# Derive the symbolic set from the key list builder (single source).
CTX="$(mktemp)"
bash "$ROOT/scripts/i18n-keylist.sh" "$CTX" /home/user/i18n-work/keys.txt >/dev/null 2>&1

bad=0
while IFS=$'\t' read -r key english; do
    [ "$key" = "$english" ] && continue     # not symbolic
    row=$(grep -P "^$(printf '%s' "$key" | sed 's/[.[\*^$()+?{}|]/\\&/g')\t" "$TSV" | head -1)
    [ -z "$row" ] && continue
    val="${row#*$'\t'}"
    if [ "$val" = "$key" ] || [ "$val" = "$english" ]; then
        echo "  UNTRANSLATED $LOC: $key  ->  $val"
        bad=$((bad + 1))
    fi
done < "$CTX"
rm -f "$CTX"

if [ "$bad" -gt 0 ]; then
    echo "$LOC: $bad symbolic key(s) not translated"
    exit 1
fi
echo "$LOC: symbolic keys OK"