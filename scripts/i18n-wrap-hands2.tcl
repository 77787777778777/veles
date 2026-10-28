# scripts/i18n-wrap-hands2.tcl — the last 15, each by hand.
#
# Everything still unwrapped after the two automated passes has an
# interpolated `[command]` or a `${braced}` value in the middle, which
# the message wrapper refuses by design (a `%` in a substituted value is
# a format bug waiting to happen). Each row below splits the message into
# a WRAPPED head and the computed tail as a separate argument, so the
# words travel and the values stay values.
#
# Usage: tclsh scripts/i18n-wrap-hands2.tcl veles-desktop/lib

proc sub1 {dir file old new} {
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
    if {$n != 1} { return "$file — found $n occurrences of <[string range $old 0 50]>" }
    set fh [open $path w] ; fconfigure $fh -encoding utf-8
    puts -nonewline $fh [string map [list $old $new] $src]
    close $fh
    return ""
}

set dir [lindex $::argv 0]
if {$dir eq ""} { puts stderr "usage: i18n-wrap-hands2.tcl <libdir>" ; exit 2 }

set fail {}
set done 0
proc do1 {dir failv donev file old new} {
    upvar 1 $failv fail
    upvar 1 $donev done
    set e [sub1 $dir $file $old $new]
    if {$e ne ""} { lappend fail $e } else { incr done }
}

# ── the banner's window title ──
do1 $dir fail done chat.tcl \
    {wm title . "veles"} \
    {wm title . [::vi18n::mc {veles}]}
do1 $dir fail done chat.tcl \
    {wm title . "veles — $label"} \
    {wm title . [format [::vi18n::mc {veles — %s}] $label]}

# ── the empty-state banner's two lines ──
do1 $dir fail done chat.tcl \
    {log_insert "  type a message below to begin. $pal for the command palette,\n" status} \
    {log_insert "[format [::vi18n::mc {  type a message below to begin. %s for the command palette,}] $pal]\n" status}
do1 $dir fail done chat.tcl \
    {log_insert "  $pal → Themes to switch the palette, F1 for help.\n" status} \
    {log_insert "[format [::vi18n::mc {  %s → Themes to switch the palette, F1 for help.}] $pal]\n" status}

# ── steer parked/queued: the message, then a parenthetical line ──
do1 $dir fail done chat.tcl \
    {log_insert "↳ steer parked: $txt\n  (no live turn — it opens the next one)\n" system} \
    {log_insert "[format [::vi18n::mc {↳ steer parked: %s}] $txt]\n[::vi18n::mc {  (no live turn — it opens the next one)}]\n" system}
do1 $dir fail done chat.tcl \
    {log_insert "↳ steer queued: $txt\n" system} \
    {log_insert "[format [::vi18n::mc {↳ steer queued: %s}] $txt]\n" system}

# ── /tools and /skills: a wrapped head, then the computed list ──
do1 $dir fail done chat.tcl \
    {emit_message system "builtin ([dict get $data builtin_count]):\n  [join $b "\n  "]\n\ndeferred — tiered builtins + MCP ([dict get $data deferred_count]):\n  [join $d "\n  "]"} \
    {emit_message system "[format [::vi18n::mc {builtin (%s):}] [dict get $data builtin_count]]\n  [join $b "\n  "]\n\n[format [::vi18n::mc {deferred — tiered builtins + MCP (%s):}] [dict get $data deferred_count]]\n  [join $d "\n  "]"}
do1 $dir fail done chat.tcl \
    {emit_message system "$count skill(s):\n[join $lines "\n"]"} \
    {emit_message system "[format [::vi18n::mc {%s skill(s):}] $count]\n[join $lines "\n"]"}
do1 $dir fail done chat.tcl \
    {emit_message system "active profile: $active\nprofiles: [join $names {, }]"} \
    {emit_message system "[format [::vi18n::mc {active profile: %s}] $active]\n[::vi18n::mc {profiles:}] [join $names {, }]"}
do1 $dir fail done chat.tcl \
    {emit_message system "compressed: [dict get $data before] → [dict get $data after] messages, [dict get $data archived] archived"} \
    {emit_message system [format [::vi18n::mc {compressed: %1$s → %2$s messages, %3$s archived}] [dict get $data before] [dict get $data after] [dict get $data archived]]}

# ── the transcript row separator: no words at all ──
#
# `${brk}${text}` is the section break plus the row's own text, both
# already assembled. `brk` is a separator glyph and `text` is the row
# content — there is nothing here a translator would change.
do1 $dir fail done chat.tcl \
    {log_insert "${brk}${text}" status $sid} \
    {log_insert "${brk}${text}" status $sid ;# i18n-exempt: separator glyph + row text, no words}
do1 $dir fail done chat.tcl \
    {log_insert "${brk}⚙ [_tool_inv $name $args]\n" tool $sid} \
    {log_insert "${brk}[format [::vi18n::mc {⚙ %s}] [_tool_inv $name $args]]\n" tool $sid ;# i18n-exempt: the glyph is a marker; the invocation is tool syntax}

# ── the route footer: values either side of a separator ──
do1 $dir fail done chat.tcl \
    {log_insert "· $provider/$model · ${turn_prompt}→${turn_completion} tok\n" status} \
    {log_insert "· $provider/$model · ${turn_prompt}→${turn_completion} tok\n" status ;# i18n-exempt: provider/model and token counts, no words}

puts "hand-wrapped $done"
foreach f $fail { puts "  REFUSED $f" }