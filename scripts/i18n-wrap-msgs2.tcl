# scripts/i18n-wrap-msgs2.tcl — wrap composed messages with N values.
#
# The first version (i18n-wrap-msgs.tcl) handled ONE trailing value and
# refused everything else, on the principle that a wrong wrap silently
# changes what a user reads. That principle is right; the threshold was
# too tight, and 35 chat.tcl messages sat unwrapped because of it.
#
# This handles the general shape safely:
#
#     "steer parks: $txt"        → [format [::vi18n::mc {steer parks: %s}] $txt]
#     "$cmd failed: $e"          → [format [::vi18n::mc {%s failed: %s}] $cmd $e]
#
# THE RULE THAT MAKES IT SAFE: every `$name` in the literal becomes one
# `%s`, in the ORDER THEY APPEAR, and the args are passed in that same
# order. Position is preserved by construction, so the rendered message
# is byte-identical to the original English — the only change is that
# the WORDS now travel through the catalog.
#
# REFUSED, because the substitution rule cannot express them:
#   * `[cmd]` inside the literal — a command substitution whose result
#     may itself contain `%` (a % in an argument is a format bug);
#   * `${name}` — supported, but its braces make the literal ambiguous
#     with a Tcl group, so it is reported rather than guessed at;
#   * a literal that is ONLY values (`"${brk}${text}"`) — no words to
#     translate, so it is not a message at all;
#   * anything where the values-out-of-order trick would be needed.
#
# Usage: tclsh scripts/i18n-wrap-msgs2.tcl <file.tcl> [--dry]

# The words of a literal: everything with every $value taken out. Empty
# means there is nothing to translate.
proc words_of {text} {
    set w [regsub -all {\$[A-Za-z_][A-Za-z0-9_]*} $text "\x00"]
    set w [regsub -all {\$\{[A-Za-z_][A-Za-z0-9_]*\}} $w "\x00"]
    return $w
}

proc wrap_line {line} {
    # A message may be MULTI-LINE — `"...\n  (no live turn ...)\n"`. The
    # `\n` is structure, not prose, so the literal splits on it: the TEXT
    # segments travel through the catalog, the newlines stay outside as
    # plain Tcl escapes. No key in en.msg contains a literal backslash-n
    # (checked: zero of 1262), and this is the shape the already-wrapped
    # multi-line sites use — see the banner at chat.tcl:1590.
    set pat {^(\s*)((?:::vchat::)?(?:emit_message|log_insert))\s+([a-z_]+)\s+"((?:[^"\\]|\\.)*)"(.*)$}
    if {![regexp $pat $line -> indent fn kind text tail]} { return {} }
    # Refusals, each stated rather than guessed at.
    if {[string first "\[" $text] >= 0} { return {} }
    if {[string first "\n" $text] >= 0} {
        # A REAL newline (not the two-character escape) would break the
        # rebuilt line; this wrapper works one line at a time.
        return {}
    }
    if {[string first "\\n" $text] >= 0} {
        # Rebuild as concatenation: {segment}\n{segment}…
        set parts [split $text "\\n"]
        set built ""
        set nseg 0
        foreach p $parts {
            if {$p eq ""} {
                # A bare `\n` between segments.
                if {$nseg > 0} { append built "\\n" }
                incr nseg
                continue
            }
            if {$nseg > 0} { append built "\\n" }
            incr nseg
            set r [one_segment $p]
            if {$r eq ""} { return {} }
            lassign $r expr key
            append built $expr
        }
        if {[string first "vi18n" $built] < 0} { return {} }
        return [list "${indent}${fn} ${kind} \"${built}\"$tail" {}]
    }
    set r [one_segment $text]
    if {$r eq ""} { return {} }
    lassign $r expr key
    return [list "${indent}${fn} ${kind} ${expr}$tail" $key]
}

# One contiguous text run: either a plain brace-group key, or a
# format-wrapped key with its values as arguments. Returns
# {expression key} or {} when the run cannot be wrapped safely.
proc one_segment {text} {
    if {$text eq ""} { return {} }
    if {[string first "\[" $text] >= 0} { return {} }
    if {[string first "\\" $text] >= 0} { return {} }
    set words [words_of $text]
    # No letters left after the values come out: nothing to translate.
    # A run that is ONLY values is a CONCATENATION, not a message.
    if {![regexp {[A-Za-z]{2}} $words]} {
        # No words at all means an all-value run: keep it verbatim by
        # returning it as a plain string, so the caller can still join it.
        return [list "\"$text\"" {}]
    }
    set vals {}
    foreach m [regexp -all -inline {\$\{[A-Za-z_][A-Za-z0-9_]*\}|\$[A-Za-z_][A-Za-z0-9_]*} $text] {
        lappend vals [string map {\{ "" \} "" \$ ""} $m]
    }
    set key $text
    regsub -all {\$\{[A-Za-z_][A-Za-z0-9_]*\}} $key "%s" key
    regsub -all {\$[A-Za-z_][A-Za-z0-9_]*} $key "%s" key
    # A literal `%` in the words (not from a value) would be a format
    # error; refuse rather than double-escape it silently.
    if {[regexp {%(?!s)} [regsub -all {%s} $key ""]]} { return {} }
    if {![llength $vals]} {
        return [list "\[::vi18n::mc {$key}\]" $key]
    }
    set args ""
    foreach v $vals { append args " \$$v" }
    return [list "\[format \[::vi18n::mc {$key}\]$args\]" $key]
}

set file [lindex $::argv 0]
set dry 0
foreach a $::argv { if {$a eq "--dry"} { set dry 1 } }
if {$file eq ""} { puts stderr "usage: i18n-wrap-msgs2.tcl <file> \[--dry\]" ; exit 2 }

set fh [open $file r] ; fconfigure $fh -encoding utf-8
set src [read $fh] ; close $fh

set out {}
set n 0
set wrapped 0
set refused {}
foreach line [split $src \n] {
    incr n
    if {[string match "#*" [string trimleft $line]]} { lappend out $line ; continue }
    if {[string first "vi18n::mc" $line] >= 0} { lappend out $line ; continue }
    set r [wrap_line $line]
    if {$r eq ""} {
        if {[regexp {\b(emit_message|log_insert)\s+[a-z_]+\s+"[^"]*\$} $line]} {
            lappend refused $n
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