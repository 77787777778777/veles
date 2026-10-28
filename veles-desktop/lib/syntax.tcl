# veles-desktop/lib/syntax.tcl — Tcl syntax highlighting, hover docs,
# and inline LSP diagnostics for the Dev pane editor (lsp-mode feel).
#
# Design: one pass over the buffer text with a small state machine
# (normal / string / comment). Strings can span lines in Tcl, so the
# scanner is stateful across newlines; comments are to end-of-line.
# Highlighting is debounced in the editor (KeyRelease → after 250ms).
#
# Tags (configured once per widget, theme-aware):
#   syn_comment  dim italic      syn_string   gold
#   syn_keyword  pink bold       syn_proc     cyan bold
#   syn_var      accent          syn_number   purple
#   syn_brace    hairline        syn_diag     red underline (LSP errors)
#   syn_diagwarn gold underline  (LSP warnings)
#
# Hover: pointer over a word → tooltip with the proc's signature and
# first body line (lsp-ui-doc style). Resolves bare names against the
# app's own namespaces first, then global.
#
# Diagnostics: ::vsyn::diagnostics $w rows renders the tcl-lsp-check
# output ("line:col SEV message") as underlines + a clickable list is
# the caller's job (dev.tcl owns the layout).

namespace eval ::vsyn {
    variable kw {proc namespace variable upvar uplevel foreach for while
        if elseif else switch return break continue set unset lappend
        lassign lindex llength lrange lreplace lsearch lsort append
        incr expr catch error puts global array dict string regexp
        regsub split join format scan clock file open close read seek
        glob source package require provide after vwait update bind
        pack grid place wm winfo focus destroy image font text
        listbox entry label button frame canvas menu toplevel ttk}
    variable _tip_after ""

    # ── SIDELINE ────────────────────────────────────────────────────
    #
    # lsp-ui's sideline: the diagnostic's MESSAGE at the end of the line
    # it belongs to, not just an underline under the token. The rows
    # arriving from the server have carried the message all along —
    # `diagnostics` threw it away and drew a squiggle, so the editor
    # could tell you that something on line 40 was wrong and never what.
    #
    # Rendered as an embedded label rather than inserted text: a text
    # widget's `get` skips embedded windows, so C-x C-s and C-c C-c
    # still see the file the user wrote and not the annotations. That is
    # the whole reason for the choice — inserted text would silently
    # save the error messages into the source.
    # widget,kind -> list of embedded label paths. Two things annotate a
    # buffer this way now — diagnostics and folds — and they clear
    # independently, so the key carries the kind.
    variable anno
    variable anno_seq 0
    variable sideline_on 1   ;# the sideline itself, toggleable
}

proc ::vsyn::_tags {w} {
    $w tag configure syn_comment  -foreground $::vt::T(fg_dim) -font {monospace 10 italic}
    $w tag configure syn_string   -foreground $::vt::T(gold)
    $w tag configure syn_keyword  -foreground $::vt::T(pink) -font {monospace 10 bold}
    $w tag configure syn_proc     -foreground $::vt::T(cyan) -font {monospace 10 bold}
    $w tag configure syn_func     -foreground $::vt::T(cyan)
    $w tag configure syn_ns       -foreground $::vt::T(purple) -font {monospace 10 bold}
    $w tag configure syn_var      -foreground $::vt::T(accent)
    $w tag configure syn_number   -foreground $::vt::T(purple)
    $w tag configure syn_diag     -foreground $::vt::T(red) -underline 1
    $w tag configure syn_diagwarn -foreground $::vt::T(gold) -underline 1
}

proc ::vsyn::clear_diag {w} {
    catch { $w tag remove syn_diag 1.0 end }
    catch { $w tag remove syn_diagwarn 1.0 end }
    clear_sideline $w
}

proc ::vsyn::clear_sideline {w} { return [clear_annotations $w diag] }

# Put TEXT in the buffer's margin at IDX without putting it IN the
# buffer. An embedded label, never inserted text: a Text widget's `get`
# skips embedded windows, so save and C-c C-c still see the file the
# user wrote. Inserted text would silently write the annotations into
# the source.
proc ::vsyn::annotate {w idx text colour {kind diag}} {
    variable anno
    variable anno_seq
    if {![winfo exists $w]} { return "" }
    incr anno_seq
    set lbl $w.anno$anno_seq
    catch { destroy $lbl }
    # A plain label, not ttk: it lives inside a Text and has to take the
    # editor's own background, which a ttk style would not give it.
    label $lbl -text $text -background [$w cget -background] \
        -foreground $colour -font {monospace 9} -anchor w \
        -borderwidth 0 -highlightthickness 0
    if {[catch { $w window create $idx -window $lbl -padx 4 }]} {
        catch { destroy $lbl }
        return ""
    }
    if {![info exists anno($w,$kind)]} { set anno($w,$kind) {} }
    lappend anno($w,$kind) $lbl
    return $lbl
}

proc ::vsyn::clear_annotations {w {kind ""}} {
    variable anno
    foreach key [array names anno "$w,*"] {
        if {$kind ne "" && $key ne "$w,$kind"} { continue }
        foreach lbl $anno($key) { catch { destroy $lbl } }
        set anno($key) {}
    }
    return
}

proc ::vsyn::annotation_texts {w {kind diag}} {
    variable anno
    set out {}
    if {![info exists anno($w,$kind)]} { return $out }
    foreach lbl $anno($w,$kind) {
        if {[winfo exists $lbl]} { lappend out [string trim [$lbl cget -text]] }
    }
    return $out
}

# Every message on one line, in one label — several diagnostics on the
# same line would otherwise stack labels on top of each other.
proc ::vsyn::_sideline_put {w line text sev} {
    annotate $w "$line.0 lineend" "  $text" \
        [expr {$sev eq "ERROR" ? $::vt::T(red) : $::vt::T(gold)}] diag
    return
}

proc ::vsyn::sideline_texts {w} { return [annotation_texts $w diag] }

proc ::vsyn::toggle_sideline {} {
    variable sideline_on
    set sideline_on [expr {!$sideline_on}]
    return $sideline_on
}

# Extra passes for what tcl-lsp's semantic legend does NOT emit
# (strings + numbers): called after the tree-sitter tokens land so the
# buffer keeps gold strings and purple numbers under LSP mode too.
proc ::vsyn::extra_passes {w} {
    # Strings (rarely multi-line in this codebase; regex per line is fine).
    set idx 1.0
    while {1} {
        set hit [$w search -regexp {"(?:[^"\\]|\\.)*"} $idx end]
        if {$hit eq ""} break
        set line [$w get "$hit linestart" "$hit lineend"]
        set off [lindex [split $hit .] 1]
        set sub [string range $line $off end]
        if {[regexp -indices {^"(?:[^"\\]|\\.)*"} $sub m]} {
            lassign $m s e
            set end_i "$hit + [expr {$e + 1}] chars"
            $w tag add syn_string $hit $end_i
            set idx $end_i
        } else {
            set idx [$w index "$hit + 1 chars"]
        }
    }
    # Numbers.
    set idx 1.0
    while {1} {
        set hit [$w search -regexp {\y\d+(\.\d+)?\y} $idx end]
        if {$hit eq ""} break
        set tags [$w tag names $hit]
        if {"syn_string" in $tags || "syn_comment" in $tags} {
            set idx [$w index "$hit + 1 chars"]
            continue
        }
        set line [$w get "$hit linestart" "$hit lineend"]
        set off [lindex [split $hit .] 1]
        set sub [string range $line $off end]
        if {[regexp -indices {^\d+(\.\d+)?} $sub m]} {
            lassign $m s e
            set end_i "$hit + [expr {$e + 1}] chars"
            $w tag add syn_number $hit $end_i
            set idx $end_i
        } else {
            set idx [$w index "$hit + 1 chars"]
        }
    }
}

# Render LSP diagnostic rows: each is {line col sev message} (line is
# 1-based, col 0-based). Underlines the word/line region.
proc ::vsyn::diagnostics {w rows} {
    variable sideline_on
    _tags $w
    clear_diag $w
    set last [$w index end-1c]
    # line -> {sev msg msg ...}; the worst severity on a line wins its
    # colour, and every message on it shares one label.
    set byline [dict create]
    foreach r $rows {
        lassign $r line col sev msg
        set tag [expr {$sev eq "ERROR" ? "syn_diag" : "syn_diagwarn"}]
        set start "$line.$col"
        if {[$w compare $start > $last]} { set start $last }
        # Underline to the next whitespace/terminator, at least one char.
        # (The \} escape keeps Tcl's brace counter happy inside the
        # braced regex literal.)
        set end [$w search -regexp -- {[\s)\}\"]} "$start +1c" "$start lineend"]
        if {$end eq "" || [$w compare $end <= $start]} {
            set end "$start +1c"
        }
        $w tag add $tag $start $end
        if {$msg eq ""} { continue }
        if {[$w compare "$line.0" > $last]} { continue }
        if {![dict exists $byline $line]} { dict set byline $line [list "" {}] }
        lassign [dict get $byline $line] worst msgs
        if {$worst ne "ERROR"} { set worst $sev }
        lappend msgs $msg
        dict set byline $line [list $worst $msgs]
    }
    if {!$sideline_on} { return }
    dict for {line info} $byline {
        lassign $info sev msgs
        _sideline_put $w $line [join $msgs " · "] $sev
    }
}

# ── the scanner ──────────────────────────────────────────────────────
# One pass; emits (tag, start-char, end-char) runs over the flat text,
# then converts offsets to widget indices line-by-line. Offsets are
# easier than incremental index arithmetic while scanning.

proc ::vsyn::highlight {w} {
    _tags $w
    set txt [$w get 1.0 end-1c]
    foreach t {syn_comment syn_string syn_keyword syn_proc syn_var syn_number} {
        $w tag remove $t 1.0 end
    }
    if {$txt eq ""} { return }

    set runs {}
    set n [string length $txt]
    set i 0
    set state normal     ;# normal | string | bracestr
    set at_line_start 1

    while {$i < $n} {
        set c [string index $txt $i]

        if {$state eq "string"} {
            if {$c eq "\\"} { incr i 2; continue }
            if {$c eq "\""} {
                lappend runs [list syn_string $str_start [expr {$i + 1}]]
                set state normal
                incr i
                continue
            }
            # $var inside strings gets colored too.
            incr i
            continue
        }

        if {$state eq "bracestr"} {
            # Braced words: track nesting for ${}, plain word otherwise.
            if {$c eq "\}"} {
                incr depth -1
                if {$depth <= 0} {
                    lappend runs [list syn_string $str_start [expr {$i + 1}]]
                    set state normal
                }
            } elseif {$c eq "\{"} {
                incr depth
            }
            incr i
            continue
        }

        # normal state
        if {$c eq "\n"} {
            set at_line_start 1
            incr i
            continue
        }
        if {$c eq " " || $c eq "\t"} { incr i; continue }

        # Comment: # at line start (after optional ws) or after ;
        if {$c eq "#" && $at_line_start} {
            set eol [string first "\n" $txt $i]
            if {$eol < 0} { set eol $n }
            lappend runs [list syn_comment $i $eol]
            set i $eol
            set at_line_start 1
            continue
        }
        if {$c eq ";" && [string index $txt [expr {$i + 1}]] eq "#"} {
            # ;# comment idiom (common in this codebase)
            set eol [string first "\n" $txt $i]
            if {$eol < 0} { set eol $n }
            lappend runs [list syn_comment $i $eol]
            set i $eol
            continue
        }
        if {$c eq ";"} { incr i; set at_line_start 1; continue }

        # String open.
        if {$c eq "\""} {
            set state string
            set str_start $i
            incr i
            set at_line_start 0
            continue
        }

        # Variable.
        if {$c eq "\$" && $i + 1 < $n} {
            set rest [string range $txt $i end]
            if {[regexp -indices {^\$\{[^\}\n]+\}} $rest m]} {
                lassign $m s e
                lappend runs [list syn_var $i [expr {$i + $e + 1}]]
                incr i [expr {$e + 1}]
                set at_line_start 0
                continue
            }
            if {[regexp -indices {^\$[A-Za-z_][A-Za-z0-9_:]*} $rest m]} {
                lassign $m s e
                lappend runs [list syn_var $i [expr {$i + $e + 1}]]
                incr i [expr {$e + 1}]
                set at_line_start 0
                continue
            }
        }

        # Number.
        if {[string is digit $c]} {
            set rest [string range $txt $i end]
            if {[regexp -indices {^\d+(\.\d+)?} $rest m]} {
                lassign $m s e
                # Only color when not part of an identifier.
                set prev [expr {$i > 0 ? [string index $txt [expr {$i - 1}]] : " "}]
                if {![string is alnum $prev] && $prev ne "_"} {
                    lappend runs [list syn_number $i [expr {$i + $e + 1}]]
                }
                incr i [expr {$e + 1}]
                set at_line_start 0
                continue
            }
        }

        # Word (identifier) — keyword or proc-name check.
        if {[regexp {^[A-Za-z_:]} $c]} {
            set rest [string range $txt $i end]
            if {[regexp -indices {^[A-Za-z_][A-Za-z0-9_:]*} $rest m]} {
                lassign $m s e
                set word [string range $rest $s $e]
                set wend [expr {$i + $e + 1}]
                if {$word in $::vsyn::kw} {
                    lappend runs [list syn_keyword $i $wend]
                    # proc NAME: the next word after `proc`.
                    if {$word eq "proc" || $word eq "oo::class create"} {
                        set after [string range $txt $wend end]
                        if {[regexp -indices {^\s+(\S+)} $after am -> nm]} {
                            lassign $nm ns ne
                            lappend runs [list syn_proc \
                                [expr {$wend + $ns}] [expr {$wend + $ne}]]
                        }
                    }
                }
                set i $wend
                set at_line_start 0
                continue
            }
        }

        set at_line_start 0
        incr i
    }
    # EOF inside a string: color what we saw.
    if {$state eq "string" || $state eq "bracestr"} {
        lappend runs [list syn_string $str_start $n]
    }

    # Apply runs. Convert char offsets → line.col with a running
    # line counter (offsets are monotone, so this is O(n) total).
    set lineno 1
    set linebase 0     ;# offset of the current line's start
    set next_nl [string first "\n" $txt 0]
    foreach run $runs {
        lassign $run tag s e
        # Advance the line base until s is inside the current line.
        while {$next_nl >= 0 && $next_nl < $s} {
            incr lineno
            set linebase [expr {$next_nl + 1}]
            set next_nl [string first "\n" $txt $linebase]
        }
        set sidx "$lineno.[expr {$s - $linebase}]"
        # End may cross lines (multi-line strings) — clamp to its line.
        set e_line $lineno
        set e_base $linebase
        while {$next_nl >= 0 && $next_nl < $e} {
            incr e_line
            set e_base [expr {$next_nl + 1}]
            set next_nl [string first "\n" $txt $e_base]
        }
        set eidx "$e_line.[expr {$e - $e_base}]"
        catch { $w tag add $tag $sidx $eidx }
    }
}

# ── hover documentation ─────────────────────────────────────────────

# Resolve a word to a live proc name ("" when unknown).
proc ::vsyn::resolve_proc {word} {
    if {$word eq ""} { return "" }
    set candidates [list $word]
    if {![string match "::*" $word]} {
        foreach ns {::vchat ::vpanes ::vdev ::vsidebar ::vkeymap ::vedit
                    ::vcomp ::vapi ::vmd ::vt ::vstatus ::vhelp ::vfind
                    ::vpalette ::voverlay ::vdock ::vwhichkey ::vask
                    ::vhotpatch ::vbindings ::vsyn} {
            lappend candidates "${ns}::$word"
        }
        lappend candidates "::$word"
    }
    foreach c $candidates {
        if {[info procs $c] ne ""} { return $c }
    }
    return ""
}

# Show (or refresh) the tooltip for the word at widget-relative x,y.
proc ::vsyn::hover {w x y} {
    variable _tip_after
    catch { destroy .vsyn_tip }
    if {[catch { $w index @$x,$y } idx]} { return }
    set word [$w get "$idx wordstart" "$idx wordend"]
    set found [resolve_proc $word]
    if {$found eq ""} { return }
    set args [info args $found]
    set body [string trim [info body $found]]
    set lines [split $body "\n"]
    set doc ""
    # Take leading comment lines as the docstring (lsp-ui-doc style).
    foreach ln $lines {
        set t [string trim $ln]
        if {[string match "#*" $t] || $t eq ""} {
            append doc "[string trimleft $t #] \n"
        } else {
            break
        }
        if {[string length $doc] > 200} break
    }
    if {$doc eq ""} { set doc "(no docstring)" }
    set tip "proc $found \{$args\}\n[string trim $doc]"

    toplevel .vsyn_tip
    wm overrideredirect .vsyn_tip 1
    set rx [expr {[winfo rootx $w] + $x + 14}]
    set ry [expr {[winfo rooty $w] + $y + 18}]
    wm geometry .vsyn_tip "+${rx}+${ry}"
    text .vsyn_tip.t -width 60 -height [expr {min(8, [llength [split $tip "\n"]])}] \
        -wrap word -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -font {monospace 9} -relief solid -borderwidth 1 \
        -highlightthickness 0 -padx 6 -pady 4
    .vsyn_tip.t tag configure sig -foreground $::vt::T(cyan) -font {monospace 9 bold}
    .vsyn_tip.t insert end [lindex [split $tip "\n"] 0]\n sig
    .vsyn_tip.t insert end [join [lrange [split $tip "\n"] 1 end] "\n"]
    .vsyn_tip.t configure -state disabled
    pack .vsyn_tip.t
    raise .vsyn_tip
    catch { after cancel $_tip_after }
    set _tip_after [after 5000 { catch { destroy .vsyn_tip } }]
}

proc ::vsyn::hide_tip {} {
    catch { destroy .vsyn_tip }
}
