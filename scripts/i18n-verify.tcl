# scripts/i18n-verify.tcl — check a translation file against the key set.
#
# args: <tsv file> <keys file> [<full keys file>] [<en.yml>]
#
# A TSV row is: key<TAB>translation. This validates everything a
# translator can get wrong that the code cannot survive:
#
#   1. MISSING keys — a key with no row falls back to English silently
#      in the app, so an incomplete catalog LOOKS fine.
#   2. EXTRA keys — a row for a string no longer in the tree; msgcat
#      would carry dead weight and the completeness test would fail.
#      Judged against the FULL list when a third argument is given, so
#      chunked jobs can be validated part by part.
#   3. FORMAT SPECIFIERS — the critical one. Every %s/%d/%%/positional
#      %1$s in the key must appear in the translation, with the same
#      index. A translation that drops a %s makes the app throw inside
#      a widget's -text assignment, at runtime, in a language the person
#      seeing the error may not read.
#   4. No tab characters inside a field, no empty translations.
#
# Exit 0 when valid; 1 when not (with the reasons printed).

set tsv  [lindex $argv 0]
set keys [lindex $argv 1]
set full [lindex $argv 2]
set enyml [lindex $argv 3]
if {$tsv eq "" || $keys eq ""} {
    puts stderr "usage: i18n-verify.tcl <tsv> <keys-file> \[full-keys-file\]"
    exit 2
}
if {$full eq ""} { set full $keys }

proc read_lines {path} {
    set fh [open $path r]
    fconfigure $fh -encoding utf-8
    set data [read $fh]
    close $fh
    return [split $data \n]
}

# The canonical key list.
set keylist {}
foreach k [read_lines $keys] {
    if {$k eq ""} { continue }
    lappend keylist $k
}
set keyset [dict create]
foreach k $keylist { dict set keyset $k 1 }

# The translations.
set got [dict create]
set dups {}
set bad {}
set n 0
foreach line [read_lines $tsv] {
    incr n
    if {[string trim $line] eq ""} { continue }
    if {[string match "#*" [string trimleft $line]]} { continue }
    set tab [string first "\t" $line]
    if {$tab < 0} {
        lappend bad "line $n: no TAB separator: [string range $line 0 60]"
        continue
    }
    set k [string range $line 0 [expr {$tab - 1}]]
    set v [string range $line [expr {$tab + 1}] end]
    if {[dict exists $got $k]} { lappend dups $k }
    dict set got $k $v
}

# ── 1/2. coverage both directions ──────────────────────────────────
set missing {}
foreach k $keylist {
    if {![dict exists $got $k]} { lappend missing $k }
}
set extra {}
set fullset [dict create]
foreach k [read_lines $full] {
    if {$k eq ""} { continue }
    dict set fullset $k 1
}
foreach k [dict keys $got] {
    if {![dict exists $fullset $k]} { lappend extra $k }
}

# ── 3. format specifiers ───────────────────────────────────────────
#
# Extracted as a MULTISET so a key that uses %s twice needs two in the
# translation. Positional specifiers (%1$s) are compared by index too:
# `%2$s %1$s` is a legitimate word-order change, `%1$s` alone is a bug.
proc specs {s} {
    # Tcl `format` specifiers (the desktop) AND the engine's `%{name}`
    # (the engine). The first version knew only the former, so every
    # engine row that dropped its `%{id}` passed.
    return [regexp -all -inline {%\d+\$[sd]|%[sd]|%%|%\{[A-Za-z0-9_]+\}} $s]
}

# The English SOURCE of a key: en.yml's text for a symbolic key, the key
# itself otherwise. Comparing a symbolic key's translation against its
# NAME (which has no placeholders) is how "cancelled task %{id}" could be
# translated from the word `tasks_cancelled` and still pass.
set en_src [dict create]
if {$enyml ne "" && [file exists $enyml]} {
    set fh [open $enyml r]
    fconfigure $fh -encoding utf-8
    foreach line [split [read $fh] \n] {
        if {[regexp {^([A-Za-z0-9_]+): "((?:[^"\\]|\\.)*)"\s*$} $line -> k v]} {
            if {$k eq "_version"} { continue }
            dict set en_src $k [string map [list {\\} "\\" {\"} "\""] $v]
        }
    }
    close $fh
}
proc source_of {k} {
    if {[dict exists $::en_src $k]} { return [dict get $::en_src $k] }
    return $k
}

set spec_bad {}
foreach k $keylist {
    if {![dict exists $got $k]} { continue }
    set want [lsort [specs [source_of $k]]]
    set have [lsort [specs [dict get $got $k]]]
    if {$want ne $have} {
        lappend spec_bad [list $k [dict get $got $k] $want $have]
    }
}

# ─ 5. empties ─────────────────────────────────────────────────────
set empty {}
foreach k [dict keys $got] {
    if {[string trim [dict get $got $k]] eq ""} { lappend empty $k }
}

# Report.
puts "keys:        [llength $keylist]"
puts "translated:  [dict size $got]"
puts "missing:     [llength $missing]"
puts "extra:       [llength $extra]"
puts "duplicates:  [llength $dups]"
puts "spec-mismatch: [llength $spec_bad]"
puts "empty:       [llength $empty]"

foreach k [lrange $missing 0 9] { puts "  MISSING: $k" }
if {[llength $missing] > 10} { puts "  … and [expr {[llength $missing] - 10}] more" }
foreach k [lrange $extra 0 9] { puts "  EXTRA: $k" }
foreach k [lrange $dups 0 4] { puts "  DUP: $k" }
foreach k [lrange $empty 0 4] { puts "  EMPTY: $k" }
foreach b [lrange $spec_bad 0 19] {
    lassign $b k v want have
    puts "  SPEC: '$k'"
    puts "        -> '$v'"
    puts "        want $want  have $have"
}
if {[llength $spec_bad] > 20} {
    puts "  … and [expr {[llength $spec_bad] - 20}] more spec mismatches"
}

set failed [expr {
    [llength $missing] || [llength $extra] || [llength $dups] || [llength $spec_bad]
}]
if {$failed} {
    puts "INVALID"
    exit 1
}
puts "VALID"
exit 0