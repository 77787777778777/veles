#!/usr/bin/env bash
# scripts/i18n-symbolic-fix.sh — emit the symbolic keys a locale still
# has untranslated, with their English text, as translator input.
#
# args: <locale> [workdir]
#
# A symbolic key (agents_none, choice_retry, doctor_title — 16 of them)
# is a catalog key that is NOT English prose: its real English string
# lives in crates/veles-core/locales/en.yml. A translator who is handed
# the bare id either guesses or copies it, and a copied id renders as
# the IDENTIFIER in the UI.
#
# This builds <workdir>/<locale>-symbolic.tsv containing
#   <key><TAB><English text>
# for every symbolic key the locale's parts have left untranslated (or
# copied through) — which IS the file the translator then fixes in place.
# Writing the translation back over the English column is idempotent: a
# rerun on a corrected file emits nothing.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOC="${1:?usage: i18n-symbolic-fix.sh <locale> [workdir]}"
WORK="${2:-/home/user/i18n-work}"

OUT="$WORK/$LOC-symbolic.tsv"
CTX="$(mktemp)"

# The symbolic set, derived from the key-list builder (single source).
bash "$ROOT/scripts/i18n-keylist.sh" "$CTX" "$WORK/keys.txt" >/dev/null 2>&1

# What the locale currently says, from its part files. `cat` a part
# that does not end in a newline and the next part's first row rides
# along — the weld bug that ate three keys in ru.msg. Terminating each
# part before the next one starts is the fix; the awk below then sees
# only whole rows.
CURRENT="$(mktemp)"
for _p in "$WORK/$LOC"-part-*.tsv; do
    [ -s "$_p" ] || continue
    cat "$_p"
    [ -n "$(tail -c 1 "$_p")" ] && printf '\n'
done | awk -F'\t' '{v[$1]=$2} END {for (k in v) print k"\t"v[k]}' | LC_ALL=C sort > "$CURRENT"

# The placeholders a string carries, in a comparable form.
ph() { grep -oE '%\{[A-Za-z0-9_]+\}|%[0-9]+\$[sd]|%[sd]' <<<"$1" | sort | tr '\n' ' '; }

: > "$OUT"
while IFS=$'\t' read -r key english; do
    [ "$key" = "$english" ] && continue          # not a symbolic key
    row=$(grep -F "$key"$'\t' "$CURRENT" | head -1)
    val="${row#*$'\t'}"
    # Nothing yet, copied through, or translated from the key's NAME
    # rather than its English text — the last shows up as placeholders
    # that do not match (`tasks_cancelled` → "Aufgaben abgebrochen", the
    # %{id} gone). The first version only caught the first two, so ~125
    # name-translated rows counted as done.
    if [ -z "$row" ] || [ "$val" = "$key" ] || [ "$val" = "$english" ] \
        || [ "$(ph "$val")" != "$(ph "$english")" ]; then
        printf '%s\t%s\n' "$key" "$english" >> "$OUT"
    fi
done < "$CTX"

rm -f "$CTX" "$CURRENT"
echo "$OUT: $(grep -c . "$OUT" || true) key(s) to translate"