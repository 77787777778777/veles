# scripts/i18n-wrap.tcl — mechanically wrap simple visible literals.
#
# args: <lib dir> [--dry]
#
# Wraps the SHAPE it can prove correct and reports everything else for
# a human. A wrong wrap is worse than an unwrapped string (it either
# breaks the Tcl or silently changes the text), so the transform is
# deliberately narrow:
#
#   -text "Words here"        ->  -text [::vi18n::mc {Words here}]
#   set_msg "Words here"      ->  set_msg [::vi18n::mc {Words here}]
#
# and it REFUSES a literal containing any of:
#
#   $  [  ]  \  %      (interpolation, command substitution, escapes,
#                       format placeholders — each needs a hand edit
#                       because the right shape differs: format-wrapping
#                       vs a plain key)
#
# A literal that is pure punctuation/digits (a glyph, a separator) is
# skipped: not prose, nothing to translate.
#
# Literal braces inside the text are refused too: the key is written as
# a brace group, and the key extractor's depth counting reads the same
# way. Strings that legitimately contain braces use the explicit
# spelling in the source (see acl.tcl's bracket-wrapper case).

set dir [file normalize [lindex $argv 0]]
set dry 0
foreach a $argv { if {$a eq "--dry"} { set dry 1 } }

set wrapped 0
set skipped {}
set files 0

proc is_prose {s} {
    # Two or more consecutive letters anywhere: real words, not glyphs.
    return [regexp {[A-Za-z]{2,}} $s]
}

proc wrap_line {line} {
    # Returns {newline status}. status is "wrapped", "n/a", or
    # "refused:<reason>".
    set out ""
    set rest $line
    set did 0
    while {1} {
        # Find the next `-text "` or `set_msg "`.
        set hit -1
        set kind ""
        foreach pat [list {-text "} {set_msg "}] {
            set at [string first $pat $rest]
            if {$at >= 0 && ($hit < 0 || $at < $hit)} { set hit $at ; set kind $pat }
        }
        if {$hit < 0} { append out $rest ; break }
        append out [string range $rest 0 [expr {$hit - 1}]]
        set after [string range $rest [expr {$hit + [string length $kind]}] end]
        # Find the closing quote on THIS line (a literal split across
        # lines is a different shape; report it).
        set close [string first "\"" $after]
        if {$close < 0} {
            return [list $line "refused:unterminated-quote"]
        }
        set lit [string range $after 0 [expr {$close - 1}]]
        set tail [string range $after [expr {$close + 1}] end]
        # Trailing content that would change meaning if wrapped.
        if {$lit eq ""} {
            append out $kind $lit "\"" ; set rest $tail ; continue
        }
        if {![is_prose $lit]} {
            append out $kind $lit "\"" ; set rest $tail ; continue
        }
        if {[regexp {[\[\]\\$%]} $lit]} {
            return [list $line "refused:interpolated-or-escaped \[$lit\]"]
        }
        append out [string range $kind 0 end-1]
        # Build the replacement WITHOUT evaluating anything: the braces
        # are spelled through escapes so Tcl never sees a command to run.
        append out "\[::vi18n::mc \{" $lit "\}\]"
        set did 1
        set rest $tail
    }
    if {$did} { return [list $out "wrapped"] }
    return [list $out "n/a"]
}

foreach f [lsort [glob -nocomplain [file join $dir *.tcl]]] {
    if {[string match "*test.tcl" $f]} { continue }
    if {[file tail $f] eq "i18n.tcl"} { continue }
    set fh [open $f r] ; set src [read $fh] ; close $fh
    set lines [split $src \n]
    set new {}
    set n 0
    set changed 0
    foreach line $lines {
        incr n
        # Comments are left alone (they are not user-visible text, and
        # the extractor would otherwise pick their braces up as keys).
        if {[string match "#*" [string trimleft $line]]} { lappend new $line ; continue }
        lassign [wrap_line $line] out status
        if {$status eq "wrapped"} {
            incr changed
            incr wrapped
        } elseif {$status ne "n/a"} {
            lappend skipped "[file tail $f]:$n: $status"
        }
        lappend new $out
    }
    if {$changed && !$dry} {
        set fh [open $f w] ; fconfigure $fh -encoding utf-8
        puts -nonewline $fh [join $new \n]
        close $fh
        incr files
    }
}

puts "wrapped $wrapped literals in $files files"
if {[llength $skipped]} {
    puts "needs a hand edit ([llength $skipped]):"
    foreach s $skipped { puts "  $s" }
}