#!/usr/bin/env tclsh
# scripts/check-pitfall-numbers.tcl — the numbered pitfalls stay
# numbered once each (AGENTS.md: global, append-only, never reused).
#
# Usage: tclsh scripts/check-pitfall-numbers.tcl ?repo-root?
#
# 1. Every numbered pitfall in docs/issues/pitfalls/*.md (a line that
#    opens `<n>. **`) has a number no other entry uses, in any file.
#    Number 105 was used three times until 2026-11-10 and nothing
#    noticed: code and docs cite pitfalls by number, so a duplicate
#    makes every citation of it ambiguous.
# 2. HANDOFF.md §7's index table names only numbers that exist. A
#    range is written `302–304` (or with a hyphen).
#
# Exits 1 with one line per finding; prints a one-line summary when
# clean. Renumbering is never the fix (numbers are cited); a duplicate
# gets the next free number and a note in its file, by hand.

set root [expr {$argc > 0 ? [lindex $argv 0] : [file dirname [file dirname [file normalize [info script]]]]}]
set findings {}

proc slurp {path} {
    set f [open $path r]
    fconfigure $f -encoding utf-8
    set text [read $f]
    close $f
    return $text
}

# ── 1. each number once ──────────────────────────────────────────────
# A public export carries neither the pitfalls nor HANDOFF.md
# (scripts/release/public-exclude.txt): there is nothing to check there,
# and that is not a failure. One without the other is.
if {![file exists [file join $root docs issues pitfalls]] && ![file exists [file join $root HANDOFF.md]]} {
    puts "pitfall numbers: no pitfall records in this tree (a public export), nothing to check"
    exit 0
}
set files [lsort [glob -nocomplain -directory [file join $root docs issues pitfalls] *.md]]
if {![llength $files]} {
    puts stderr "check-pitfall-numbers: no docs/issues/pitfalls/*.md under $root"
    exit 1
}
array set where {}
foreach path $files {
    set lineno 0
    foreach line [split [slurp $path] \n] {
        incr lineno
        if {[regexp {^([0-9]+)\. \*\*} $line -> n]} {
            set n [scan $n %d]
            lappend where($n) "[file tail $path]:$lineno"
        }
    }
}
foreach n [lsort -integer [array names where]] {
    if {[llength $where($n)] > 1} {
        lappend findings "pitfall $n is numbered [llength $where($n)] times: [join $where($n) {, }]"
    }
}

# ── 2. HANDOFF §7 names only numbers that exist ──────────────────────
set handoff [file join $root HANDOFF.md]
if {![file exists $handoff]} {
    lappend findings "HANDOFF.md is missing"
} else {
    set in7 0
    set rows 0
    set lineno 0
    foreach line [split [slurp $handoff] \n] {
        incr lineno
        if {[regexp {^## } $line]} {
            set in7 [regexp {^## 7\.} $line]
            continue
        }
        if {!$in7} { continue }
        # A table row naming a theme file: | `docs/issues/pitfalls/x.md` | numbers | count |
        if {![regexp {^\|\s*`docs/issues/pitfalls/[^`]+`\s*\|([^|]*)\|} $line -> cell]} { continue }
        incr rows
        # Ranges first (en dash or hyphen), then single numbers.
        set named {}
        while {[regexp {([0-9]+)\s*[–-]\s*([0-9]+)} $cell whole a b]} {
            set a [scan $a %d]
            set b [scan $b %d]
            if {$b < $a || $b - $a > 1000} {
                lappend findings "HANDOFF.md:$lineno: range $whole is not a range"
            } else {
                for {set n $a} {$n <= $b} {incr n} { lappend named $n }
            }
            set cell [string map [list $whole " "] $cell]
        }
        foreach n [regexp -all -inline {[0-9]+} $cell] { lappend named [scan $n %d] }
        foreach n $named {
            if {![info exists where($n)]} {
                lappend findings "HANDOFF.md:$lineno: §7 names pitfall $n, which no pitfalls file has"
            }
        }
    }
    if {$rows == 0} {
        lappend findings "HANDOFF.md §7 has no index rows (| `docs/issues/pitfalls/<file>` | numbers | count |)"
    }
}

if {[llength $findings]} {
    foreach f $findings { puts $f }
    exit 1
}
puts "pitfall numbers: [array size where] pitfalls, each number once; HANDOFF §7 names only these"
exit 0
