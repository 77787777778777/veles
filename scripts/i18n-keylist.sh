#!/usr/bin/env bash
# scripts/i18n-keylist.sh — emit the translator's key list WITH context.
#
# The catalog keys are English source text, EXCEPT for a set of symbolic
# ids (agents_none, choice_retry, doctor_title, …) extracted before that
# convention landed. A translator reading "agents_none" alone cannot
# know what to write, and asking them to guess produced a part where the
# ids were copied verbatim — which the app would then render as
# "agents_none" in Russian.
#
# This emits, for each key:
#
#   <key><TAB><the English text that key renders>
#
# where the right-hand column is the key itself for English-as-key
# entries, and the real string from en.yml for a symbolic one. A
# translator's row is still `key<TAB>translation`; the context is what
# they READ, and the TSV they WRITE uses column 1 as the key.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:?usage: i18n-keylist.sh <output-file> [keys-file]}"
KEYS="${2:-/home/user/i18n-work/keys.txt}"

run_tcl() {
    if command -v direnv >/dev/null 2>&1; then
        direnv exec "$ROOT" tclsh "$@" 2>/dev/null || tclsh "$@"
    else
        tclsh "$@"
    fi
}

GEN="$(mktemp --suffix=.tcl)"
cat > "$GEN" <<'TCL'
# args: keysfile en-yml out
lassign $argv keysfile enyml out

# English text per symbolic key, read from the engine's en.yml.
set en {}
set fh [open $enyml r]
fconfigure $fh -encoding utf-8
set data [read $fh]
close $fh
foreach line [split $data \n] {
    if {![regexp {^([a-z0-9_]+):[ ]*"(.*)"[ ]*$} $line -> k v]} { continue }
    dict set en $k $v
}

set fh [open $keysfile r]
fconfigure $fh -encoding utf-8
set keys [split [read $fh] \n]
close $fh

set out_fh [open $out w]
fconfigure $out_fh -encoding utf-8
foreach k $keys {
    if {[string trim $k] eq ""} { continue }
    if {[dict exists $en $k]} {
        # A symbolic key: show the text it renders, NOT the id. The id
        # is what goes in column 1 of the translator's TSV.
        puts $out_fh "$k\t[dict get $en $k]"
    } else {
        puts $out_fh "$k\t$k"
    }
}
close $out_fh
TCL
run_tcl "$GEN" "$KEYS" "$ROOT/crates/veles-core/locales/en.yml" "$OUT" >/dev/null
rm -f "$GEN"
echo "$OUT: $(grep -c . "$OUT") keys (symbolic keys expanded to their English text)"