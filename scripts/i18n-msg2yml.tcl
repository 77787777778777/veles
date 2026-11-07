# scripts/i18n-msg2yml.tcl — carry the desktop's translations of
# engine-reachable keys into the engine's catalogs.
#
#   tclsh scripts/i18n-msg2yml.tcl [root] [locale …]
#
# WHY. One translation pass is meant to feed both surfaces (the keys are
# shared), but the two catalogs are written by two scripts, and a round
# that runs only i18n-tsv2msg.sh leaves crates/veles-core/locales/*.yml
# behind. The 2026-11 rounds did exactly that: `/translate` and `/fleet`
# were translated in all 46 .msg files and rendered English on the TUI,
# Telegram and IRC. The .msg is the complete catalog, so for every key
# the engine can reach (scripts/i18n-keys.tcl --engine-keys, the same
# superset i18n-tsv2yml.sh trims to), its translation is written into
# the .yml. The desktop's text wins where the two disagree, since it is
# the one a translator's latest TSV reached.
#
# What is NOT touched: rows only the .yml has, while the engine can still
# reach their key (a row no surface uses any more is dropped,
# ::i18ncheck::stale_yml_rows),
# the file's comment header, and en.yml (the engine's identity catalog,
# hand-kept). Rows are written in i18n-tsv2yml.sh's form, sorted the
# same way, so a run with nothing to carry rewrites nothing.
#
# The check that keeps this from drifting again is
# ::i18ncheck::seam (scripts/i18n-catalog-check.tcl), run by i18ntest.tcl.

set _here [file dirname [file normalize [info script]]]
source [file join $_here i18n-catalog-check.tcl]

proc ::i18ncheck::yaml_str {s} {
    set s [string map [list "\\" "\\\\" "\"" "\\\"" "\n" "\\n" "\t" "\\t"] $s]
    return "\"$s\""
}

# Rewrite one locale's .yml with `rows` (a dict) as its body. Returns
# the number of rows written.
proc ::i18ncheck::write_yml {path rows} {
    set header {}
    if {[file exists $path]} {
        foreach line [split [_read $path] \n] {
            lappend header $line
            if {$line eq "_version: 1"} { break }
        }
    }
    if {[lindex $header end] ne "_version: 1"} { set header [list "_version: 1"] }
    set fh [open $path w]
    fconfigure $fh -encoding utf-8 -translation lf
    foreach line $header { puts $fh $line }
    foreach k [lsort [dict keys $rows]] {
        puts $fh "[yaml_str $k]: [yaml_str [dict get $rows $k]]"
    }
    close $fh
    return [dict size $rows]
}

proc ::i18ncheck::msg2yml {root {only {}}} {
    set locales [file join $root crates veles-core locales]
    set engine [engine_keys $root]
    set stale [stale_yml_rows $root $engine]
    set report {}
    dict for {loc gaps} [seam $root "" $engine] {
        if {[llength $only] && $loc ni $only} { continue }
        set dead [expr {[dict exists $stale $loc] ? [dict get $stale $loc] : {}}]
        if {![llength $gaps] && ![llength $dead]} { continue }
        set path [file join $locales $loc.yml]
        set rows [expr {[file exists $path] ? [load_yml $path] : {}}]
        foreach g $gaps {
            lassign $g k v
            dict set rows $k $v
        }
        foreach k $dead { dict unset rows $k }
        write_yml $path $rows
        lappend report $loc [llength $gaps] [llength $dead]
    }
    return $report
}

if {[info exists argv0] && [file tail $argv0] eq [file tail [info script]]} {
    set root [lindex $argv 0]
    if {$root eq ""} { set root [file normalize [file join $_here ..]] }
    set rest [lrange $argv 1 end]
    if {[lindex $rest 0] eq "--check"} {
        # Report only: exit 1 while any locale's .yml lacks a row or keeps
        # one no surface uses.
        set n 0
        dict for {loc gaps} [::i18ncheck::seam $root] {
            if {![llength $gaps]} { continue }
            incr n [llength $gaps]
            puts [format "  %-9s %4d engine row(s) translated in .msg, missing from .yml (first: %s)" \
                $loc [llength $gaps] [list [lindex $gaps 0 0]]]
        }
        set d 0
        dict for {loc keys} [::i18ncheck::stale_yml_rows $root] {
            if {![llength $keys]} { continue }
            incr d [llength $keys]
            puts [format "  %-9s %4d .yml row(s) no surface uses (first: %s)" \
                $loc [llength $keys] [list [lindex $keys 0]]]
        }
        set bad [expr {$n + $d}]
        puts "engine catalogs: $n row(s) to carry, $d unused row(s) to drop[expr {$bad ? { — run scripts/i18n-sync.sh} : {}}]"
        exit [expr {$bad > 0}]
    }
    set n 0
    set d 0
    foreach {loc carried dropped} [::i18ncheck::msg2yml $root $rest] {
        puts "$loc.yml: $carried row(s) carried from $loc.msg, $dropped unused row(s) dropped"
        incr n $carried
        incr d $dropped
    }
    puts "engine catalogs: $n row(s) carried, $d unused row(s) dropped"
}
