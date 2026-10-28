#!/usr/bin/env bash
# scripts/i18n-tsv2yml.sh — build a Rust locale catalog from a TSV.
#
# args: <tsv> <locale-id> [<existing yml to preserve>] [engine-keys file]
#
# The engine's catalogs (crates/veles-core/locales/*.yml) are a flat
# `key: "value"` map under `_version: 1`, read at runtime by
# crates/veles-core/src/i18n.rs (docs/i18n-runtime.md). They use the SAME
# keys as the desktop catalogs (English text, or a symbolic id for the
# strings extracted before that convention landed), so one translator's
# TSV feeds both surfaces.
#
# The TSV carries ~975 keys because it also feeds the desktop's .msg
# catalog, while the engine can reach ~150. The trim was first made to
# cut compile memory, back when rust-i18n compiled every entry into the
# binary; measured 2026-09-28, before the catalogs moved to runtime
# files, all 47 of them cost +0.34 GB on veles-core's library compile and
# +0.60 GB on its test binary. That cost is gone now, and the trim stays
# because it is right on its own terms: 7x smaller catalogs (3.3 MB ->
# 448 KB), no unreachable entries, no translation lost.
#
# So when an engine-keys file is supplied, a row whose key is not in it
# is NOT written. The desktop side is untouched — it reads .msg, never
# .yml — and the TSV keeps every row, so nothing is lost from the
# translator's artifact: re-running this script with a fuller key set
# grows the catalog back, because the keys come from the source tree
# rather than from the catalog.
#
# The filter is deliberately keyed on "the key text appears as a Rust
# string literal", which is a SUPERSET of what lookup() can be handed
# (a key can also arrive through a variable: the help_entries table and
# tasks.rs's match arms both do exactly that). Over-inclusion costs
# bytes; under-inclusion would silently cost a translation, since a
# missing entry renders English with no error anywhere.
#
# YAML quoting: every scalar is written as a double-quoted string with
# backslashes and quotes escaped. YAML's plain style would break on a
# leading %, a colon followed by space, a hash, or a trailing space —
# all of which occur in this key set.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TSV="${1:?usage: i18n-tsv2yml.sh <tsv> <locale> [preserve-yml] [engine-keys]}"
LOC="${2:?usage: i18n-tsv2yml.sh <tsv> <locale> [preserve-yml]}"
PRESERVE="${3:-}"
ENGKEYS="${4:-}"
OUT="$ROOT/crates/veles-core/locales/$LOC.yml"

GEN="$(mktemp --suffix=.tcl)"
cat > "$GEN" <<'TCL'
# args: <tsv> <out> <preserve> <locale> <engine-keys> <en.yml>
lassign $argv tsv out preserve locale engkeys enyml

# ── edge whitespace is the SOURCE's, never the translator's ──────────
#
# A leading or trailing space in these strings is LAYOUT, not language:
# "  (none)" is an indented sub-line and " · next: %s" is a suffix that
# gets appended to other text. The 2026-09-21 round shipped ~128 rows
# that had lost (or grown) one, so suffixes welded onto the text before
# them. The value keeps its words and gets exactly the English source's
# edge spaces — en.yml's text for a symbolic key, the key itself
# otherwise. scripts/i18n-catalog-check.tcl checks the result.
proc load_en_symbolic {path} {
    set d [dict create]
    if {$path eq "" || ![file exists $path]} { return $d }
    set fh [open $path r]
    fconfigure $fh -encoding utf-8
    foreach line [split [read $fh] \n] {
        if {[regexp {^([A-Za-z0-9_]+): "((?:[^"\\]|\\.)*)"\s*$} $line -> k v]} {
            if {$k eq "_version"} { continue }
            dict set d $k [string map [list {\\} "\\" {\"} "\"" {\n} "\n" {\t} "\t"] $v]
        }
    }
    close $fh
    return $d
}
proc edge_normalize {src v} {
    if {[string trim $src " "] eq ""} { return $v }
    regexp {^( *)} $src -> lead
    regexp {( *)$} $src -> trail
    return "$lead[string trim $v { }]$trail"
}
set en_sym [load_en_symbolic $enyml]

proc yaml_str {s} {
    # Double-quoted YAML scalar: backslash and quote escaped, newlines
    # and tabs as escapes. Nothing else is legal raw inside quotes.
    set s [string map [list "\\" "\\\\" "\"" "\\\"" "\n" "\\n" "\t" "\\t"] $s]
    return "\"$s\""
}

# The engine-reachable key set, when the caller supplied one. Absent,
# the catalog is written whole (the pre-trim behaviour) — this script
# stays usable on its own, and says which mode it ran in.
set eng {}
set have_eng 0
if {$engkeys ne "" && [file exists $engkeys]} {
    set fh [open $engkeys r]
    fconfigure $fh -encoding utf-8
    set data [read $fh]
    close $fh
    foreach line [split $data \n] {
        if {$line eq ""} { continue }
        dict set eng $line 1
    }
    set have_eng 1
}

set rows [dict create]
set order {}

set fh [open $tsv r]
fconfigure $fh -encoding utf-8
set data [read $fh]
close $fh
foreach line [split $data \n] {
    if {[string trim $line] eq ""} { continue }
    if {[string match "#*" [string trimleft $line]]} { continue }
    set tab [string first "\t" $line]
    if {$tab < 0} { continue }
    set k [string range $line 0 [expr {$tab - 1}]]
    set v [string range $line [expr {$tab + 1}] end]
    if {![dict exists $rows $k]} { lappend order $k }
    dict set rows $k $v
}

# Preserve anything the existing catalog carries that the TSV omits —
# including hand translations from an earlier round.
if {$preserve ne "" && [file exists $preserve]} {
    set fh [open $preserve r]
    fconfigure $fh -encoding utf-8
    set data [read $fh]
    close $fh
    foreach line [split $data \n] {
        # BOTH key spellings. This used to read bare `key: "…"` only —
        # but this script writes every key QUOTED, so a rebuild from
        # anything less than the full TSV silently dropped every
        # English-as-key row it was meant to preserve. The symbolic keys
        # (bare, from en.yml-shaped files) are still read.
        if {[regexp {^"((?:[^"\\]|\\.)*)":[ ]*"((?:[^"\\]|\\.)*)"[ ]*$} $line -> k v]} {
            set k [string map [list {\n} "\n" {\t} "\t" {\"} "\"" {\\} "\\"] $k]
        } elseif {![regexp {^([A-Za-z0-9_]+):[ ]*"(.*)"[ ]*$} $line -> k v]} {
            continue
        }
        if {$k eq "_version"} { continue }
        if {[dict exists $rows $k]} { continue }
        lappend order $k
        # Un-escape the two escapes this script writes.
        dict set rows $k [string map [list {\n} "\n" {\t} "\t" {\"} "\"" {\\} "\\"] $v]
    }
}

set fh [open $out w]
fconfigure $fh -encoding utf-8
puts $fh "# veles UI strings — locale $locale."
puts $fh "# GENERATED by scripts/i18n-tsv2yml.sh from a translator's TSV."
puts $fh "# Keys match the desktop catalogs (msgs/<id>.msg) so one"
puts $fh "# translation pass feeds both surfaces. A key missing here"
puts $fh "# falls back to the English text. Read at runtime by"
puts $fh "# crates/veles-core/src/i18n.rs — not compiled into the binary."
if {$have_eng} {
    puts $fh "#"
    puts $fh "# TRIMMED to the engine-reachable key set: the TSV feeds the"
    puts $fh "# desktop's .msg catalog too, which needs every key, but a key"
    puts $fh "# no Rust code can pass to lookup() is never read from here."
    puts $fh "# The untrimmed translations live in veles-desktop/msgs/$locale.msg."
}
puts $fh "_version: 1"
set n 0
set skipped 0
foreach k [lsort $order] {
    set v [dict get $rows $k]
    # An English-as-key row whose value IS the key is not a translation;
    # writing it would make the catalog claim coverage it lacks. A
    # symbolic key (agents_none) is different — its value is never the
    # key, because the key is not English prose.
    if {$v eq $k} { continue }
    # Engine-reachable only. Checked here rather than while reading the
    # TSV so the PRESERVED rows are filtered by the same rule.
    if {$have_eng && ![dict exists $eng $k]} { incr skipped ; continue }
    set src [expr {[dict exists $en_sym $k] ? [dict get $en_sym $k] : $k}]
    set v [edge_normalize $src $v]
    puts $fh "[yaml_str $k]: [yaml_str $v]"
    incr n
}
close $fh
puts "$n $skipped $have_eng"
TCL

run() {
    if command -v direnv >/dev/null 2>&1; then
        direnv exec "$ROOT" tclsh "$GEN" "$TSV" "$OUT" "$PRESERVE" "$LOC" "$ENGKEYS" \
            "$ROOT/crates/veles-core/locales/en.yml" 2>/dev/null
    else
        tclsh "$GEN" "$TSV" "$OUT" "$PRESERVE" "$LOC" "$ENGKEYS" \
            "$ROOT/crates/veles-core/locales/en.yml"
    fi
}
read -r n skipped have_eng < <(run | tail -1)
rm -f "$GEN"
if [ "$have_eng" = "1" ]; then
    echo "$LOC.yml: $n engine-reachable entries ($skipped desktop-only rows not carried)"
else
    echo "$LOC.yml: $n translated entries (no engine-key filter)"
fi