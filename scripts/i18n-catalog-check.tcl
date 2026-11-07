# scripts/i18n-catalog-check.tcl — check the SHIPPED catalogs against
# their English source.
#
# WHY THIS EXISTS. The 2026-09-21 round reported "47 locales, every one
# complete at 977 keys", and every check it ran passed. What they
# checked was that each KEY was present. Nothing checked what the value
# SAID against the English it translates, and the review of 2026-09-25
# found:
#
#   * ~125 engine rows in 42 locales translated from the key's NAME
#     rather than its English text: `tasks_cancelled` ("cancelled task
#     %{id}") became "Aufgaben abgebrochen" — the task id gone. In ja
#     and th the failover prompt no longer named the provider that
#     failed. i18n-verify.tcl compared format specifiers against the
#     KEY (which, for a symbolic key, has none) and did not know
#     the engine's `%{name}` at all, so all of it passed.
#   * ~128 desktop rows that lost a leading or trailing space, so a
#     suffix fragment like " · next: %s" welded onto the text before it.
#   * a dropped opening bracket or quote here and there.
#
# So this checks every row of every shipped catalog (the desktop's
# .msg and the engine's .yml) against its ENGLISH SOURCE — en.yml's
# text for a symbolic key, the key itself for an English-as-key one:
#
#   1. placeholders: the same multiset of %s %d %N$s %% and %{name}
#   2. edge whitespace: the same leading and trailing spaces
#   3. brackets: the same ( ) and [ ] balance as the source
#
# A value identical to its English source is NOT a failure (a brand, a
# command name, "OK" can be the right translation); it is counted and
# reported, so an untranslated catalog is visible without being red.
#
# Usage:
#   tclsh scripts/i18n-catalog-check.tcl [root]      # report, exit 1 on problems
#   source it and call ::i18ncheck::run $root        # returns the problems

namespace eval ::i18ncheck {}

# Every placeholder in a string, sorted — a multiset, so a source that
# uses %s twice needs two in the translation. Positional %1$s keeps its
# index (reordering is legitimate, dropping is not).
proc ::i18ncheck::specs {s} {
    set out {}
    foreach m [regexp -all -inline {%(?:\d+\$)?[sdifxcu]|%%|%\{[A-Za-z0-9_]+\}} $s] {
        lappend out $m
    }
    return [lsort $out]
}

proc ::i18ncheck::edges {s} {
    regexp {^( *)} $s -> lead
    regexp {( *)$} $s -> trail
    return [list [string length $lead] [string length $trail]]
}

# Open-minus-close for the bracket pairs a translator can drop.
proc ::i18ncheck::brackets {s} {
    set out {}
    foreach {o c} {( ) [ ]} {
        set no [llength [lsearch -all -exact [split $s ""] $o]]
        set nc [llength [lsearch -all -exact [split $s ""] $c]]
        lappend out [expr {$no - $nc}]
    }
    return $out
}

# The problems with one translated value, as a list of tags.
proc ::i18ncheck::check_pair {src v} {
    set bad {}
    if {[specs $src] ne [specs $v]} { lappend bad placeholders }
    if {[edges $src] ne [edges $v]} { lappend bad whitespace }
    if {[brackets $src] ne [brackets $v]} { lappend bad brackets }
    return $bad
}

proc ::i18ncheck::_read {path} {
    set fh [open $path r]
    fconfigure $fh -encoding utf-8
    set data [read $fh]
    close $fh
    return $data
}

proc ::i18ncheck::_unyaml {s} {
    return [string map [list {\\} "\\" {\"} "\"" {\n} "\n" {\t} "\t"] $s]
}

# A generated .yml (or en.yml) as a dict. Both spellings of a key occur:
# en.yml writes bare symbolic keys, the generator quotes every key.
proc ::i18ncheck::load_yml {path} {
    set d [dict create]
    foreach line [split [_read $path] \n] {
        if {[regexp {^"((?:[^"\\]|\\.)*)": "((?:[^"\\]|\\.)*)"\s*$} $line -> k v]} {
            dict set d [_unyaml $k] [_unyaml $v]
        } elseif {[regexp {^([A-Za-z0-9_]+): "((?:[^"\\]|\\.)*)"\s*$} $line -> k v]} {
            if {$k eq "_version"} { continue }
            dict set d $k [_unyaml $v]
        }
    }
    return $d
}

# A .msg catalog as a dict, read the way msgcat reads it: as Tcl.
proc ::i18ncheck::load_msg {path locale} {
    set d [dict create]
    foreach line [split [_read $path] \n] {
        set line [string trim $line]
        if {![string match "::msgcat::mcset $locale *" $line]} { continue }
        if {[catch { llength $line } n] || $n != 4} { continue }
        dict set d [lindex $line 2] [lindex $line 3]
    }
    return $d
}

# Check every shipped catalog. Returns a dict:
#   locale -> {problems {{file key source value tags}...} identical N rows N}
# `msgs` defaults to the repo layout; the flake's Tcl sandbox copies the
# desktop tree to `desktop/`, so a suite passes its own catalog directory
# (the first version assumed the repo layout, read NOTHING in the sandbox,
# and only the suite's "read every locale" guard noticed).
proc ::i18ncheck::run {root {msgs ""}} {
    set locales [file join $root crates veles-core locales]
    if {$msgs eq ""} { set msgs [file join $root veles-desktop msgs] }
    # The English text of each symbolic key. A symbolic key's own name
    # is NOT its source — comparing against the name is precisely how
    # the name-translated rows passed the old verifier.
    set en [load_yml [file join $locales en.yml]]
    set result [dict create]
    foreach f [lsort [glob -nocomplain [file join $msgs *.msg]]] {
        set loc [file rootname [file tail $f]]
        if {$loc eq "en"} { continue }
        set problems {}
        set identical 0
        set rows 0
        foreach {kind path d} [list \
                msg $f [load_msg $f $loc] \
                yml [file join $locales $loc.yml] \
                [expr {[file exists [file join $locales $loc.yml]]
                    ? [load_yml [file join $locales $loc.yml]] : {}}]] {
            dict for {k v} $d {
                incr rows
                set src [expr {[dict exists $en $k] ? [dict get $en $k] : $k}]
                if {$v eq $src} { incr identical ; continue }
                set tags [check_pair $src $v]
                if {[llength $tags]} {
                    lappend problems [list $kind $k $src $v $tags]
                }
            }
        }
        dict set result $loc [dict create problems $problems identical $identical rows $rows]
    }
    return $result
}

# ── the seam: what the desktop translated, the engine must carry ────
#
# The two catalogs share one key space but are WRITTEN separately: a
# translation round's TSV went through i18n-tsv2msg.sh, and the .yml
# side only if someone also ran i18n-tsv2yml.sh. The 2026-11 rounds did
# not, so `/translate` and `/fleet` (and ~50 rows per locale) were
# translated in every .msg and English on the TUI, Telegram and IRC,
# with every check green. The rule here: an engine-reachable key (any
# Rust string literal, the same superset `--engine-keys` trims .yml to)
# that a locale's .msg translates must appear in its .yml with the same
# text. scripts/i18n-msg2yml.tcl (run by i18n-sync.sh) writes them.

# The engine-reachable key set, as a dict. Files are read as UTF-8
# explicitly: the extractor's own CLI reads in the system encoding, and
# the flake sandbox has no UTF-8 locale.
proc ::i18ncheck::engine_keys {root} {
    if {![llength [info commands ::rust_literals_in]]} {
        uplevel #0 [list source [file join $root scripts i18n-keys.tcl]]
    }
    set files {}
    ::walk_rs [file join $root crates] files
    set keys [dict create]
    foreach f $files {
        foreach k [::rust_literals_in [_read $f]] { dict set keys $k 1 }
    }
    return $keys
}

# locale -> {key msg-value yml-value-or-""} for every gap in the seam.
# `engine` may be passed in (a dict from engine_keys) to walk once.
proc ::i18ncheck::seam {root {msgs ""} {engine ""}} {
    set locales [file join $root crates veles-core locales]
    if {$msgs eq ""} { set msgs [file join $root veles-desktop msgs] }
    if {$engine eq ""} { set engine [engine_keys $root] }
    set en [load_yml [file join $locales en.yml]]
    set result [dict create]
    foreach f [lsort [glob -nocomplain [file join $msgs *.msg]]] {
        set loc [file rootname [file tail $f]]
        if {$loc eq "en"} { continue }
        set ypath [file join $locales $loc.yml]
        set y [expr {[file exists $ypath] ? [load_yml $ypath] : {}}]
        set gaps {}
        dict for {k v} [load_msg $f $loc] {
            if {![dict exists $engine $k]} { continue }
            set src [expr {[dict exists $en $k] ? [dict get $en $k] : $k}]
            if {$v eq $src || $v eq $k} { continue }
            if {![dict exists $y $k]} {
                lappend gaps [list $k $v ""]
            } elseif {[dict get $y $k] ne $v} {
                lappend gaps [list $k $v [dict get $y $k]]
            }
        }
        dict set result $loc $gaps
    }
    return $result
}

# locale -> {key ...}: .yml rows whose key no surface uses any more —
# neither an engine-reachable literal nor a desktop (en.msg) key. The
# same rule as veles-core's catalog_keys_and_code_do_not_drift, so a
# reworded key's old row is dropped by msg2yml instead of turning that
# test red in every locale.
proc ::i18ncheck::stale_yml_rows {root {engine ""} {msgs ""}} {
    set locales [file join $root crates veles-core locales]
    if {$msgs eq ""} { set msgs [file join $root veles-desktop msgs] }
    if {$engine eq ""} { set engine [engine_keys $root] }
    set desk [load_msg [file join $msgs en.msg] en]
    set result [dict create]
    foreach f [lsort [glob -nocomplain [file join $locales *.yml]]] {
        set loc [file rootname [file tail $f]]
        if {$loc eq "en"} { continue }
        set stale {}
        foreach k [dict keys [load_yml $f]] {
            if {![dict exists $engine $k] && ![dict exists $desk $k]} { lappend stale $k }
        }
        dict set result $loc $stale
    }
    return $result
}

# ── CLI ─────────────────────────────────────────────────────────────
if {[info exists argv0] && [file tail $argv0] eq [file tail [info script]]} {
    set root [lindex $argv 0]
    if {$root eq ""} { set root [file normalize [file join [file dirname [info script]] ..]] }
    set r [::i18ncheck::run $root]
    set total 0
    dict for {loc gaps} [::i18ncheck::seam $root] {
        if {![llength $gaps]} { continue }
        incr total [llength $gaps]
        puts [format "%-9s %4d engine rows the .msg translates and the .yml lacks — run scripts/i18n-sync.sh" \
            $loc [llength $gaps]]
        puts "    first: [list [lindex $gaps 0 0]]"
    }
    dict for {loc info} $r {
        set p [dict get $info problems]
        incr total [llength $p]
        puts [format "%-9s %4d rows  %3d problems  %4d identical to English" \
            $loc [dict get $info rows] [llength $p] [dict get $info identical]]
        foreach x [lrange $p 0 2] {
            lassign $x kind k src v tags
            puts "    $kind [join $tags ,]: [list $src] => [list $v]"
        }
    }
    puts "total problems: $total"
    exit [expr {$total > 0}]
}
