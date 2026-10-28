# scripts/i18n-wrap-hands.tcl — the remaining hand cases.
#
# The audit's report after i18n-wrap-msgs2.tcl, minus the shapes it
# handles automatically. Each row is an explicit old → new pair, and the
# OLD text must appear exactly once or the row is refused.
#
# The shapes here, and why each needed a hand:
#
#   * a LEADING `\n` — `"\n· nothing to stop\n"`. The message wrapper
#     splits on `\n` and rebuilds as concatenation, but a run that is
#     ONLY values (`[::vi18n::mc {…}]\n`) in the middle of a string is
#     not something it can express as an argument list.
#   * an interpolated `[join …]` in the middle — the value is a computed
#     LIST, so it needs its own %s and its own argument.
#   * `${brk}${text}` — no words at all, so it is not a message: it is
#     the transcript's row separator plus its text, both already
#     assembled. Exempt, with the reason written down.
#
# Usage: tclsh scripts/i18n-wrap-hands.tcl veles-desktop/lib

set rows [list \
]

# Written as [[old]] / [[new]] pairs so the quotes and brackets in the
# text are ordinary characters (the same lesson as i18n-wrap-literals).
proc sub1 {dir file line old new} {
    set path [file join $dir $file]
    set fh [open $path r] ; fconfigure $fh -encoding utf-8
    set src [read $fh] ; close $fh
    set n 0 ; set at 0
    while {1} {
        set at [string first $old $src $at]
        if {$at < 0} { break }
        incr n
        incr at [string length $old]
    }
    if {$n != 1} { return "$file:$line — found $n occurrences" }
    set fh [open $path w] ; fconfigure $fh -encoding utf-8
    puts -nonewline $fh [string map [list $old $new] $src]
    close $fh
    return ""
}

set dir [lindex $::argv 0]
if {$dir eq ""} { puts stderr "usage: i18n-wrap-hands.tcl <libdir>" ; exit 2 }

# ── the rows ──
set fail {}
set done 0

proc do1 {dir failv donev file line old new} {
    upvar 1 $failv fail
    upvar 1 $donev done
    set e [sub1 $dir $file $line $old $new]
    if {$e ne ""} { lappend fail $e } else { incr done }
}

# chat.tcl — the two-line banners that begin with \n
do1 $dir fail done chat.tcl 2725 \
    {log_insert "\n· nothing to stop (no turn in flight)\n" system} \
    {log_insert "\n[::vi18n::mc {· nothing to stop (no turn in flight)}]\n" system}
do1 $dir fail done chat.tcl 2739 \
    {log_insert "\n⏹ stop requested\n" system} \
    {log_insert "\n[::vi18n::mc {⏹ stop requested}]\n" system}
do1 $dir fail done chat.tcl 2753 \
    {log_insert "\n· nothing to detach (the turn is not inside a foreground command)\n" system} \
    {log_insert "\n[::vi18n::mc {· nothing to detach (the turn is not inside a foreground command)}]\n" system}
do1 $dir fail done chat.tcl 2756 \
    {log_insert "\n· nothing to detach (no turn in flight)\n" system} \
    {log_insert "\n[::vi18n::mc {· nothing to detach (no turn in flight)}]\n" system}
do1 $dir fail done chat.tcl 2769 \
    {log_insert "\n⤵ detach requested — the command keeps running as a background shell\n" system} \
    {log_insert "\n[::vi18n::mc {⤵ detach requested — the command keeps running as a background shell}]\n" status}
do1 $dir fail done chat.tcl 4766 \
    {log_insert "\n⟳ the turn did not survive the gateway — showing what it persisted\n" system} \
    {log_insert "\n[::vi18n::mc {⟳ the turn did not survive the gateway — showing what it persisted}]\n" system}
do1 $dir fail done chat.tcl 5094 \
    {log_insert "\n· compaction finished — transcript reloaded\n" status} \
    {log_insert "\n[::vi18n::mc {· compaction finished — transcript reloaded}]\n" system}

# chat.tcl — \n plus a single value
do1 $dir fail done chat.tcl 2727 \
    {log_insert "\n✖ stop failed: $r\n" system} \
    {log_insert "\n[format [::vi18n::mc {✖ stop failed: %s}] $r]\n" system}
do1 $dir fail done chat.tcl 2758 \
    {log_insert "\n✖ detach failed: $r\n" system} \
    {log_insert "\n[format [::vi18n::mc {✖ detach failed: %s}] $r]\n" system}
do1 $dir fail done chat.tcl 5140 \
    {log_insert "\n✖ session stream: $why\n" system} \
    {log_insert "\n[format [::vi18n::mc {✖ session stream: %s}] $why]\n" system}
do1 $dir fail done chat.tcl 4426 \
    {log_insert "\n✖ $msg\n" system} \
    {log_insert "\n[format [::vi18n::mc {✖ %s}] $msg]\n" system}
do1 $dir fail done chat.tcl 4374 \
    {log_insert "\n⚠ $notice\n" system} \
    {log_insert "\n[format [::vi18n::mc {⚠ %s}] $notice]\n" status}
do1 $dir fail done chat.tcl 1507 \
    {log_insert "failed to paint cached history: $e\n" system} \
    {log_insert "[format [::vi18n::mc {failed to paint cached history: %s}] $e]\n" system}
do1 $dir fail done chat.tcl 1513 \
    {log_insert "failed to load history: $pe\n" system} \
    {log_insert "[format [::vi18n::mc {failed to load history: %s}] $pe]\n" system}
do1 $dir fail done chat.tcl 1517 \
    {log_insert "failed to load history: $e\n" system} \
    {log_insert "[format [::vi18n::mc {failed to load history: %s}] $e]\n" system}
do1 $dir fail done chat.tcl 5205 \
    {log_insert "failed to paint replayed history: $e\n" system} \
    {log_insert "[format [::vi18n::mc {failed to paint replayed history: %s}] $e]\n" system}

puts "hand-wrapped $done"
foreach f $fail { puts "  REFUSED $f" }