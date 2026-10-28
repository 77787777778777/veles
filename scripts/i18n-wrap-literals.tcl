# scripts/i18n-wrap-literals.tcl — wrap a LISTED set of literals.
#
# The audit reported 57 sites the earlier rounds could not see (a stuck
# foreach blinded it; see commit aa3a91b). This wraps the ones that are
# plain or %s-shaped, from an explicit list, so every edit is one a human
# named rather than a regexp's guess about where a sentence ends.
#
# Usage: tclsh scripts/i18n-wrap-literals.tcl veles-desktop/lib
#
# Each row is a proper 4-element list: {file line old new}. The OLD text
# must appear EXACTLY ONCE in the file, or the row is refused and
# reported — a replace that silently hits the wrong site is worse than a
# missed one.
#
# QUOTING NOTE, learned the hard way: a row cannot be written as a bare
# brace group when the old text CONTAINS quotes, because Tcl splits
# `-title "Save attachment as"` into several list elements and the row
# silently becomes 6 elements. Every row below is therefore written as
# `[list ...]`, where the quotes are ordinary characters.

set rows [list \
    [list media.tcl 587 \
        {-title "Save attachment as"} \
        {-title [::vi18n::mc \{Save attachment as\}]}] \
    [list chat.tcl 2032 \
        {-title "Attach to the next message"} \
        {-title [::vi18n::mc \{Attach to the next message\}]}] \
    [list chat.tcl 2444 \
        {emit_message system "no skills found"} \
        {emit_message system [::vi18n::mc \{no skills found\}]}] \
    [list chat.tcl 2590 \
        {emit_message system "agents unavailable (gateway offline?)"} \
        {emit_message system [::vi18n::mc \{agents unavailable (gateway offline?)\}]}] \
    [list chat.tcl 1590 \
        {log_insert "▲ VELES — neon impressionism\n" system} \
        {log_insert "[::vi18n::mc \{▲ VELES — neon impressionism\}]\n" system}] \
    [list sidebar.tcl 606 \
        {-title "Delete session"} \
        {-title [::vi18n::mc \{Delete session\}]}] \
    [list sidebar.tcl 607 \
        {-message "Delete session $sid and all its messages?"} \
        {-message [format [::vi18n::mc \{Delete session %s and all its messages?\}] $sid]}] \
    [list sidebar.tcl 672 \
        {wm title $w "Rename session"} \
        {wm title $w [::vi18n::mc \{Rename session\}]}] \
    [list overlays.tcl 1550 \
        {tk_messageBox -title "Cron history" -message $msg} \
        {tk_messageBox -title [::vi18n::mc \{Cron history\}] -message $msg}] \
    [list failgraph.tcl 351 \
        {wm title $w "Failover blueprint"} \
        {wm title $w [::vi18n::mc \{Failover blueprint\}]}] \
    [list failgraph.tcl 741 \
        {wm title .vfgpick "Choose a failover script"} \
        {wm title .vfgpick [::vi18n::mc \{Choose a failover script\}]}] \
    [list panes.tcl 647 \
        {-message "Revert $path to HEAD? Uncommitted changes in this file are lost."} \
        {-message [format [::vi18n::mc \{Revert %s to HEAD? Uncommitted changes in this file are lost.\}] $path]}] \
    [list panes.tcl 810 \
        {$bar.src configure -text [expr {$preview_source ? "Rendered" : "Source"}]} \
        {if {$preview_source} { $bar.src configure -text [::vi18n::mc \{Rendered\}] } else { $bar.src configure -text [::vi18n::mc \{Source\}] }}] \
    [list panes.tcl 984 \
        {$w.bar.mode configure -text [expr {$plan_mode ? "▲ PLAN MODE" : ""}]} \
        {$w.bar.mode configure -text [expr {$plan_mode ? [::vi18n::mc \{▲ PLAN MODE\}] : \{\}}]}] \
    [list chat.tcl 2770 \
        {} {}] \
]

set dir [lindex $::argv 0]
if {$dir eq ""} { puts stderr "usage: i18n-wrap-literals.tcl <libdir>" ; exit 2 }

set done 0
set skipped 0
set failed {}
foreach row $rows {
    if {[llength $row] != 4} {
        lappend failed "malformed row ([llength $row] elements): $row"
        continue
    }
    lassign $row file line old new
    if {$old eq ""} { incr skipped ; continue }
    set path [file join $dir $file]
    set fh [open $path r] ; fconfigure $fh -encoding utf-8
    set src [read $fh] ; close $fh
    # Count exact occurrences with string first, in a loop. NO regexp:
    # `-fixed` does not exist in this Tcl, and a regexp would read the
    # `[` and `$` in the old text as syntax — which is exactly the text
    # being replaced.
    set n 0
    set at 0
    while {1} {
        set at [string first $old $src $at]
        if {$at < 0} { break }
        incr n
        incr at [string length $old]
    }
    if {$n != 1} {
        lappend failed "$file:$line — found $n occurrences of <$old>"
        continue
    }
    set src [string map [list $old $new] $src]
    set fh [open $path w] ; fconfigure $fh -encoding utf-8
    puts -nonewline $fh $src
    close $fh
    incr done
}
puts "wrapped $done"
foreach f $failed { puts "  REFUSED $f" }