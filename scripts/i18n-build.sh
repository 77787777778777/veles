#!/usr/bin/env bash
# scripts/i18n-build.sh — build the catalogs for one locale from its parts.
#
# args: <locale-id> [workdir]
#
# A translator's work arrives as up to five part files
# (<locale>-part-NN.tsv). This merges them, writes BOTH catalogs —
# the desktop's msgcat catalog (veles-desktop/msgs/<id>.msg) and the
# engine's runtime catalog (crates/veles-core/locales/<id>.yml) —
# from the same rows, then PROVES each one loads.
#
# The two catalogs are generated from one input on purpose: the whole
# point of the English-as-key convention is that a translator works once
# and both surfaces speak the same language. A second hand-maintained
# list is how the desktop and the CLI ended up disagreeing about `de_DE`.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOC="${1:?usage: i18n-build.sh <locale> [workdir]}"
WORK="${2:-/home/user/i18n-work}"

parts=()
for p in "$WORK/$LOC"-part-*.tsv; do
    [ -s "$p" ] || continue
    case "$p" in *.invalid.*|*.leadspace-bak|*.bak) continue ;; esac
    parts+=("$p")
done
if [ ${#parts[@]} -eq 0 ]; then
    echo "$LOC: no part files" >&2
    exit 1
fi

# The SYMBOLIC fixup file is a part too, and it merges LAST so its rows
# win. This script used to glob `-part-*.tsv` only, so the output of the
# symbolic pass never reached the catalog: a locale could have
# `agents_header` translated to Tamil in `ta-symbolic.tsv` and still ship
# the literal string `agents_header`, because the part file's
# copied-through row was the only one read. Found by asking why sk's
# catalog kept an untranslated key while its fixup file had the
# translation. The driver's `merge_parts` already ordered it this way;
# the builder is what disagreed.
SYMFILE=""
if [ -s "$WORK/$LOC-symbolic.tsv" ]; then
    SYMFILE="$WORK/$LOC-symbolic.tsv"
fi

MERGED="$(mktemp)"
# The merge used to be `cat "$p" >> "$MERGED"`, which welds the parts
# together whenever one does not END in a newline: a translator writes
# the last row of a part and the file simply stops, so the last row of
# part N and the first row of part N+1 become ONE line. The desktop
# shipped the result: ru.msg carried the cron-help entry with
# `...границе хода.Send}` welded onto its value and `Send`'s own
# translation silently dropped. Found by counting, not reading — the
# verifier checks a part at a time, and only the MERGE loses rows.
: > "$MERGED"
for p in "${parts[@]}"; do
    cat "$p" >> "$MERGED"
    # Terminate the part. `tail -c 1` through command substitution is
    # empty exactly when the last byte is a newline, which is the test.
    if [ -n "$(tail -c 1 "$p")" ]; then
        printf '\n' >> "$MERGED"
        echo "$LOC: note — $(basename "$p") lacked a trailing newline (terminated)" >&2
    fi
done

# The symbolic fixup merges LAST, so its rows win awk's last-write-wins.
# That is the whole point of the file: it exists to OVERRIDE rows whose
# part-file translation came through as the copied identifier.
if [ -n "$SYMFILE" ]; then
    cat "$SYMFILE" >> "$MERGED"
    if [ -n "$(tail -c 1 "$SYMFILE")" ]; then
        printf '\n' >> "$MERGED"
        echo "$LOC: note — $(basename "$SYMFILE") lacked a trailing newline (terminated)" >&2
    fi
fi

# Review corrections merge after THAT: `<locale>-fix-*.tsv`, rows that a
# check of the SHIPPED catalog found wrong (scripts/i18n-catalog-check.tcl
# — the 2026-09-25 review's name-translated symbolic rows are the first
# of them). Last, so a rebuild from the part files cannot quietly bring
# back what a review fixed. Sorted, so a later fix file wins over an
# earlier one.
for fx in $(ls "$WORK/$LOC"-fix-*.tsv 2>/dev/null | sort); do
    [ -s "$fx" ] || continue
    cat "$fx" >> "$MERGED"
    [ -n "$(tail -c 1 "$fx")" ] && printf '\n' >> "$MERGED"
done

# And the check, so a future weld is LOUD rather than one key short.
# A welded row splits into three tab-separated fields (it ate a field
# boundary), and no honest row can: the BRIEF forbids tabs inside a
# translation. Two fields is the contract, anything else is refused.
if awk -F'\t' 'NF != 2 { printf "%s: corrupt row (%d fields): %s\n", \
        ENVIRON["LOC"], NF, substr($0, 1, 100) > "/dev/stderr"; bad = 1 }
    END { exit bad }' "$MERGED"; then :; else
    echo "$LOC: refusing to build — the merge is corrupt (see above)" >&2
    rm -f "$MERGED"
    exit 1
fi

before=$(grep -c . "$MERGED" || true)
# Dedupe by key, last occurrence wins.
awk -F'\t' '{ v[$1] = $2 } END { for (k in v) print k "\t" v[k] }' "$MERGED" | LC_ALL=C sort > "$MERGED.u"
mv "$MERGED.u" "$MERGED"
after=$(grep -c . "$MERGED" || true)

echo "$LOC: ${#parts[@]} part file(s), $before rows -> $after unique keys"

EXIST_MSG="$ROOT/veles-desktop/msgs/$LOC.msg"
EXIST_YML="$ROOT/crates/veles-core/locales/$LOC.yml"
PRES_MSG=""
PRES_YML=""
[ -f "$EXIST_MSG" ] && PRES_MSG="$EXIST_MSG"
[ -f "$EXIST_YML" ] && PRES_YML="$EXIST_YML"

# The engine-reachable key set, derived from the Rust tree (see
# i18n-tsv2yml.sh's header for why the filter exists). Computed ONCE per
# run into a cache under the work dir: the walk is fast (0.3s) but this
# script runs 47 times in a rebuild loop, and it must not depend on a
# temp file that a parallel locale could clobber. Regenerated when any
# .rs file is newer than the cache, so an edit during a rebuild is
# picked up rather than silently filtered against a stale list.
ENGKEYS="${WORK}/engine-keys.txt"
_needs=0
if [ ! -s "$ENGKEYS" ]; then
    _needs=1
elif [ -n "$(find "$ROOT/crates" -name '*.rs' -newer "$ENGKEYS" -print -quit 2>/dev/null)" ]; then
    _needs=1
fi
if [ "$_needs" = "1" ]; then
    if command -v direnv >/dev/null 2>&1; then
        direnv exec "$ROOT" tclsh "$ROOT/scripts/i18n-keys.tcl" "$ROOT" --engine-keys \
            2>/dev/null | sort -u > "$ENGKEYS.tmp"
    else
        tclsh "$ROOT/scripts/i18n-keys.tcl" "$ROOT" --engine-keys \
            2>/dev/null | sort -u > "$ENGKEYS.tmp"
    fi
    # A collapsed set would silently strip real translations, so refuse
    # rather than write a catalog built on a broken extraction.
    if [ "$(wc -l < "$ENGKEYS.tmp")" -lt 200 ]; then
        echo "$LOC: engine-key extraction collapsed ($(wc -l < "$ENGKEYS.tmp") keys) — refusing" >&2
        rm -f "$ENGKEYS.tmp"
        exit 1
    fi
    mv "$ENGKEYS.tmp" "$ENGKEYS"
fi

bash "$ROOT/scripts/i18n-tsv2msg.sh" "$MERGED" "$LOC"
# The engine catalog gets the engine-reachable key filter; the desktop
# catalog above does not (it reads .msg, and i18ntest.tcl pins its
# completeness). The key list is derived from the Rust tree, so it
# tracks the source rather than a hand-kept list.
bash "$ROOT/scripts/i18n-tsv2yml.sh" "$MERGED" "$LOC" "$PRES_YML" "$ENGKEYS"

rm -f "$MERGED"
echo "$LOC: catalogs written"