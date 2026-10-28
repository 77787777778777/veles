# scripts/i18n-wrap-msgs.tcl — wrap composed transcript/status messages.
#
# The shape it handles, and the only one it will touch:
#
#     emit_message system "steer failed: $e"
#  →  emit_message system [::vi18n::mc {steer failed: %s} $e]
#
# WHY A SEPARATE PASS. These 47 sites are the "many system messages"
# half of the USER report (2026-09-26). The words are a literal but the
# message is COMPOSED, so it is neither a key (no catalog can contain
# "steer failed: timeout") nor an unwrapped call (there is no vi18n call
# at all) — the two shapes every existing scan looks for. The fix is to
# split WORDS from VALUES: the sentence becomes a %s-slot key, the value
# stays an argument.
#
# IT REFUSES MORE THAN IT REWRITES, deliberately. A message with two
# values, a value that is itself a command substitution, a multi-line
# body, or a call the regexp does not fully match is REPORTED and left
# alone for a human — a wrong wrap silently changes what the user reads,
# which is worse than an unwrapped string.
#
# Usage:  tclsh scripts/i18n-wrap-msgs.tcl <file.tcl> [--dry]

proc wrap_line {line} {
    # The call shapes that take a message string.
    set pat {^(\s*)((?:::vchat::)?(?:emit_message|log_insert))\s+([a-z_]+)\s+"([^"]*)"(.*)$}
    if {![regexp $pat $line -> indent fn kind text tail]} { return {} }
    # A value at the END, and exactly one.
    if {![regexp {^(.*?)(\$[A-Za-z_][A-Za-z0-9_]*|\$\{[A-Za-z_][A-Za-z0-9_]*\})$} \
             [string trimright $text] -> words var]} {
        return {}
    }
    # NO trimright: the space after the separator is part of the
    # sentence, and the key is `words` + `%s` with the space intact.
    # Trimming first produced `steer failed:%s`, which renders as
    # "steer failed:timeout" — the value welded to the colon.
    # The words must read as a label ending in a separator (+ space).
    # A value with no separator at all (`compacting $before messages`)
    # is refused: there the split point is a judgement call.
    if {![regexp {(: |= |– |— |> |^ *)$} $words]} { return {} }
    # A SECOND value anywhere in the words means this is not the simple
    # shape; leave it.
    if {[regexp {\$} $words]} { return {} }
    if {[regexp {[\[\]]} $words]} { return {} }
    set key "${words}%s"
    if {[string first "{" $key] >= 0 || [string first "}" $key] >= 0} { return {} }
    # The tail is whatever followed the closing quote (a tag, a sid).
    return [list "${indent}${fn} ${kind} \[::vi18n::mc {$key} $var\]$tail" $key]
}

set file [lindex $::argv 0]
set dry 0
foreach a $::argv { if {$a eq "--dry"} { set dry 1 } }
if {$file eq ""} { puts stderr "usage: i18n-wrap-msgs.tcl <file> \[--dry\]" ; exit 2 }

set fh [open $file r] ; fconfigure $fh -encoding utf-8
set src [read $fh] ; close $fh

set out {}
set n 0
set wrapped 0
set refused {}
foreach line [split $src \n] {
    incr n
    if {[string match "#*" [string trimleft $line]]} { lappend out $line ; continue }
    set r [wrap_line $line]
    if {$r eq ""} {
        # Report the ones that LOOK like the shape but were refused, so
        # a human sees the count rather than trusting silence.
        if {[regexp {\b(emit_message|log_insert)\s+[a-z_]+\s+"[^"]*\$} $line]} {
            lappend refused "$n"
        }
        lappend out $line
        continue
    }
    lassign $r new key
    lappend out $new
    incr wrapped
}

if {$dry} {
    puts "would wrap $wrapped"
} else {
    set fh [open $file w] ; fconfigure $fh -encoding utf-8
    puts -nonewline $fh [join $out \n]
    close $fh
    puts "wrapped $wrapped in [file tail $file]"
}
if {[llength $refused]} {
    puts "refused (needs a hand): [llength $refused] — lines [join $refused {, }]"
}