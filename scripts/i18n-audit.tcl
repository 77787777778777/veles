# scripts/i18n-audit.tcl — the strings that reach a user, and whether the
# catalog has them.
#
# WHY THIS EXISTS
#
# USER report (2026-09-26), five complaints that are ONE root cause:
#
#   "the steer button is untranslated"
#   "all the pane names are untranslated"
#   "many system messages"
#   "the tooltips for the right panel are untranslated"
#   "the help text in most places also seems to be untranslated"
#
# Not one of those is a missing ::vi18n::mc call. Every site is either
# wrapped already (`::vi18n::mc $tip`) or is not a call site at all. The
# cause is the EXTRACTOR'S KEY SHAPE: scripts/i18n-keys.tcl collects a
# key only when it is a literal brace group sitting at one of three call
# shapes — mc, label, aname. A key that arrives any other way never
# enters en.msg, so no translator is ever asked for it, and ::vi18n::mc
# is handed the English text it falls back to and returns it unchanged.
#
# The gap is invisible from the call site, and i18ntest's "nothing left
# unwrapped" scan passes on all of it, because that scan looks for the
# CALL and these have one (or need none).
#
# WHY THIS SCRIPT IS CALL-SITE DRIVEN, NOT A LITERAL CRAWL
#
# The first version walked every string literal in every module and
# asked "is this prose?". Measured: 1712 hits, almost all of them Tcl
# fragments (`[list ::vlist::_click $list]; break`), widget paths
# (`.mid.chatwrap`), JSON assembly, and test fixtures. A gate that is
# red on correct code gets ignored, which is worse than no gate.
#
# So the question is narrowed to one that can be answered exactly:
#
#     which literals are being handed to a widget, a window, or the
#     transcript — the places text becomes VISIBLE?
#
# Those call sites are enumerable, and this file enumerates them. A
# literal at one of them is a visible string by construction; there is
# no prose heuristic to get wrong.
#
# THE VERDICT PER STRING
#
#   in-catalog   the literal IS a key in en.msg  → a translator has it
#   MISSING      not in en.msg and not exempt    → renders English
#                                                    everywhere, silently
#   exempt       the line carries `i18n-exempt:`
#   composed     has interpolated values; the key needs %s slots while
#                the values stay arguments
#
# Usage:
#   tclsh scripts/i18n-audit.tcl             summary
#   tclsh scripts/i18n-audit.tcl --list      every missing string
#   tclsh scripts/i18n-audit.tcl --keys      the missing strings as keys
#   tclsh scripts/i18n-audit.tcl --json      file:line:kind:class:string
#
# Exit 0 always — this reports. i18ntest.tcl is where it becomes a gate.

# SOURCED or RUN?
#
# Both, like scripts/i18n-keys.tcl and for the same reason: the GATE has
# to measure exactly what this report measures. i18ntest.tcl sources
# this file and calls ::i18naudit::run, so the "0 missing" contract and
# this report can never disagree about what "missing" means.
#
# A sourced file must define its procs and DO NOTHING ELSE — `$::argv` is
# not even set in that case, or worse, holds the host's options.
namespace eval ::i18naudit {}

# ── the catalog, which is the ground truth ──────────────────────────
#
# en.msg's keys ARE the English source strings, so membership is exactly
# "a translator was asked for this string". Nothing about how the key
# travelled matters.
proc ::i18naudit::load_catalog {path} {
    set d [dict create]
    if {![file exists $path]} { return $d }
    set fh [open $path r] ; fconfigure $fh -encoding utf-8
    foreach line [split [read $fh] \n] {
        set line [string trim $line]
        if {![string match "::msgcat::mcset en *" $line]} { continue }
        if {[catch { llength $line } n] || $n != 4} { continue }
        dict set d [lindex $line 2] 1
    }
    close $fh
    return $d
}

# ── where the desktop tree is ───────────────────────────────────────
#
# `veles-desktop` in the repo, `desktop` in the flake's test sandbox: the
# nix derivation `cp -r veles-desktop desktop` (flake.nix's tclCheck)
# because the suites run with desktop/ and scripts/ as siblings. Every
# other path here is relative to the module's OWN directory and so never
# noticed; this file was the first to name the tree from $root, and it
# hardcoded the repo spelling. The gate then passed on the host and died
# in `nix flake check` with "couldn't open /build/veles-desktop/main.tcl"
# — a sandbox-only failure with no local signal, which is the worst kind.
#
# Resolve it once, here, so there is a single answer to "where is the
# desktop tree" rather than a guess per proc.
proc ::i18naudit::desktop_dir {root} {
    foreach cand {veles-desktop desktop} {
        if {[file exists [file join $root $cand main.tcl]]} {
            return [file join $root $cand]
        }
    }
    return [file join $root veles-desktop]
}

# ── the modules a user actually runs ────────────────────────────────
#
# main.tcl's source lines, NOT a glob of lib/*.tcl. The glob pulled in
# transcriptspec.tcl (a data file of test cases that transcriptspectest
# runs — 211 of the first version's hits were its fixtures), plus
# layout_probe.tcl and demo.tcl, which are diagnostics. Parsing
# main.tcl means this list cannot drift from what ships: a module that
# is not sourced is not a surface.
#
# i18n.tcl is EXCLUDED, exactly as the extractor and i18ntest's scans
# exclude it: it is the machinery, so its literals are the keys and the
# error strings a developer sees, not UI text. Including it reported 41
# hits — the locale endonym table, the normalizer's language tags — all
# of them deliberately not translated (an endonym is the same word in
# every locale, which is the whole point of the table).
proc ::i18naudit::shipped_modules {root} {
    set dir [::i18naudit::desktop_dir $root]
    set main [file join $dir main.tcl]
    set out {}
    set fh [open $main r] ; set src [read $fh] ; close $fh
    foreach line [split $src \n] {
        if {[string match "#*" [string trimleft $line]]} { continue }
        if {[regexp {source\s+\[file join \$_script_dir\s+lib\s*/?\s*([A-Za-z0-9_]+\.tcl)} $line -> t]} {
            if {$t eq "i18n.tcl"} { continue }
            lappend out [file join $dir lib $t]
        }
    }
    return $out
}

# One quoted string starting at the first quote at or after $from on the
# line. Returns {start end text} or {} when the line has none.
proc ::i18naudit::quoted_at {line from} {
    set i [string first "\"" $line $from]
    if {$i < 0} { return {} }
    set j [expr {$i + 1}]
    set buf ""
    set len [string length $line]
    while {$j < $len} {
        set ch [string index $line $j]
        if {$ch eq "\\"} {
            append buf [string range $line $j [expr {$j + 1}]]
            incr j 2 ; continue
        }
        if {$ch eq "\""} { return [list $i $j $buf] }
        append buf $ch
        incr j
    }
    return {}
}

# The text of the Nth (1-based) TOP-LEVEL brace group in $text, "" when
# there is no such group. Escapes and nesting are honoured, which is what
# distinguishes the data list of a foreach table from its body.
proc ::i18naudit::nth_group {text n} {
    set depth 0
    set start -1
    set seen 0
    set len [string length $text]
    for {set i 0} {$i < $len} {incr i} {
        set ch [string index $text $i]
        if {$ch eq "\\"} { incr i ; continue }
        if {$ch eq "\{"} {
            incr depth
            if {$depth == 1} {
                incr seen
                if {$seen == $n} { set start [expr {$i + 1}] }
            }
            continue
        }
        if {$ch eq "\}"} {
            incr depth -1
            if {$depth == 0 && $seen == $n && $start >= 0} {
                return [string range $text $start [expr {$i - 1}]]
            }
        }
    }
    return ""
}

# Is the Nth top-level group present AND closed?
proc ::i18naudit::group_complete {text n} {
    return [expr {[nth_group $text $n] ne ""}]
}

# The data cells of a foreach table: the qwords of its SECOND top-level
# group (`{files "Files" review "Review"}` → files, Files, review, …).
#
# Both spellings a table uses come back as cells — bare words and quoted
# words — because a table may write `{files Files}` or `{files "Files"}`
# and both put the same text on screen. Bare words are left to is_prose
# at the call site (`files` is not a sentence; `browse the repo's files`
# is).
proc ::i18naudit::table_cells {text} {
    set data [nth_group $text 2]
    if {$data eq ""} { return {} }
    # A DATA list is not a SCRIPT, and the difference is decidable:
    # a script references variables and commands, a data list is a run
    # of literal words. Measured against the tree's false positives —
    # `{ lappend mods Control }`, `{+::vemacs::_track_shift_motion %W}`,
    # `foreach mode {normie emacs vi}`, `foreach row [::vbindings::
    # table $mode]` — every one is excluded by "no variable, command,
    # substitution or statement syntax". The three conditions below are
    # what separate `foreach {tab label} {files "Files" review "Review"}`
    # (a table) from a loop over a computed list (a script).
    if {[string first "\[" $data] >= 0} { return {} }
    if {[string first "\$" $data] >= 0} { return {} }
    if {[string first ";" $data] >= 0} { return {} }
    set out {}
    # Only the two PROSE spellings of a table cell, never bare words.
    #
    # A bare word in a data list is a token: `files`, `en`, `model`,
    # `ask`. Extracting those measured 2754 hits across the tree, nearly
    # all of them identifiers (dev.tcl alone accounting for 192), which
    # is the same usable-signal collapse the whole-literal crawl had.
    # Quoted and multi-word-braced cells are prose by construction.
    #
    #   {files "Files" review "Review"}      → quoted cells
    #   {files {browse the repo's files}}    → braced, multi-word
    #
    # NO CAPTURE GROUP in the quoted pattern. `regexp -all -inline`
    # returns the full match AND every capture interleaved, so a pattern
    # with one group reported each quoted cell twice — once with its
    # quotes, once without — and the report looked like two findings.
    foreach w [regexp -all -inline {"[^"]*"} $data] {
        set t [string range $w 1 end-1]
        if {[regexp {[A-Za-z]{2}} $t]} { lappend out $t }
    }
    # A braced cell is prose when it is several plain words — which is
    # also the test that refuses a symbol path or a format arg.
    foreach g [regexp -all -inline {\{[^{}\n]*\}} $data] {
        # `regexp` returns the match INCLUDING its delimiters: the braces
        # come off here, so the key matches what a call site passes.
        set t [string range $g 1 end-1]
        set t [string trim $t]
        # PROSE, not Tcl — the same rule the extractor applies, kept in
        # step with it on purpose: a cell that is a script (`%W mark set
        # insert {insert linestart}`, `lappend mods Control`) shares
        # every surface feature of a label, and a report that disagrees
        # with the key set is worse than no report.
        if {[regexp {^%[A-Z]|^[-;]|;\s|^\s*(set|append|lappend|return|bind|puts|if|foreach|proc|expr|string|list|catch|upvar|global|incr|array|dict|mark|see)\s} $t]} {
            continue
        }
        if {[regexp {[a-z]{3}} $t] && [regexp {\s} $t] \
            && ![regexp {[\[\]$;%+:@]} $t]} {
            lappend out $t
        }
    }
    return $out
}

# ── literals at a VISIBLE call site ─────────────────────────────────
proc ::i18naudit::visible_literals {src KEYS} {
    set out {}
    set n 0
    set intab 0
    set tabtxt ""
    set tabline 0
    foreach line [split $src \n] {
        incr n
        if {[string match "#*" [string trimleft $line]]} { continue }
        set exempt [expr {[string first "i18n-exempt" $line] >= 0}]

        # ── data tables, FIRST ──
        #
        # This runs before the site check and CONTINUES, because a table
        # line carries no widget option at all: `foreach {tab label}
        # {files "Files" …}` has no -text and no emit, so a site-first
        # order skips it — which is exactly how panes.tcl's six tab names
        # and its six right-panel tooltips went unreported by the first
        # version of this file while it still looked like a working
        # audit.
        #
        # A table spans lines (panes.tcl's tooltip table is six lines),
        # so text accumulates until the data list — the SECOND top-level
        # group — is complete.
        #
        # TWO WAYS TO NOT BE A TABLE, and missing the second one was a
        # silent whole-file hole. A foreach over a COMMAND SUBSTITUTION —
        # `foreach sid [dict keys $logs]` — has no second brace GROUP at
        # all, so a "wait until group 2 closes" accumulator never closed
        # and swallowed EVERY LATER LINE of the file. chat.tcl's Steer
        # label sits ~2500 lines downstream of one and was never visited:
        # the audit reported a clean file it had stopped reading. A
        # foreach whose list is not a brace group is not a table, and
        # saying so is what keeps the walk going.
        if {!$intab && [regexp {^\s*foreach\s} $line]} {
            # Is the list a brace group at all? `foreach x $list` and
            # `foreach x [cmd]` are not tables — and the trap is that
            # such a line still CONTAINS a brace, from its own body
            # (`foreach sid [dict keys $logs] { catch { … } }`), so
            # "there is an open brace" is not the test.
            #
            # The decidable form: the text between the foreach keyword
            # and the first open brace must contain NO `$` and NO `[`.
            # For a real table that text is the variable list
            # (`{tab label} ` → nothing but names); for a list-driven
            # foreach the list expression sits there and carries one or
            # the other.
            #
            # No literal open brace appears in this comment or in the
            # pattern below on purpose: Tcl's brace counter reads one
            # inside a braced regexp and miscounts the whole file, which
            # is this tree's oldest trap.
            set OB [format %c 123]
            set b [string first $OB $line]
            if {$b >= 0} {
                set head [string range $line 0 [expr {$b - 1}]]
                if {[regexp {[\$\[]} $head]} {
                    # foreach x $list   /   foreach x [cmd]
                    continue
                }
            }
            set tabtxt $line
            set tabline $n
            if {[group_complete $tabtxt 2]} {
                foreach cell [table_cells $tabtxt] {
                    if {![regexp {[A-Za-z]{2}} $cell]} { continue }
                    lappend out [list $tabline table $cell 0 $exempt]
                }
            } else {
                set intab 1
            }
            continue
        }
        if {$intab} {
            append tabtxt "\n$line"
            if {[group_complete $tabtxt 2]} {
                set intab 0
                foreach cell [table_cells $tabtxt] {
                    if {![regexp {[A-Za-z]{2}} $cell]} { continue }
                    lappend out [list $tabline table $cell 0 $exempt]
                }
            } elseif {[llength [split $tabtxt \n]] > 60} {
                # SAFETY NET. The bug above was silent for exactly as long
                # as nobody checked: an accumulator that never closed made
                # the walk stop reading a file, and "0 missing" reads as
                # "clean". No real table is 60 lines long, so a body this
                # big means the accumulator is confused — give up on it
                # and scan the lines normally rather than skipping them.
                # A reported false positive costs a look; a silent skip
                # costs the whole file.
                set intab 0
                lappend out [list $tabline overlong-table "" 0 $exempt]
            }
            continue
        }

        # ── literals at a visible call site ──
        set sites {}
        foreach opt {-text -label -message -title -heading} {
            set at [string first " $opt " $line]
            if {$at >= 0} { lappend sites [list $at "opt"] }
        }
        if {[regexp {^\s*(?:catch\s+\{\s*)?wm title} $line]} { lappend sites [list 0 title] }
        foreach fn {emit_message log_insert set_msg} {
            set at [string first $fn $line]
            if {$at >= 0} { lappend sites [list $at transcript] }
        }
        foreach fn {va11y::name vi18n::label vi18n::aname} {
            set at [string first $fn $line]
            if {$at >= 0} { lappend sites [list $at a11y] }
        }
        if {![llength $sites]} { continue }

        # AN mc CALL'S KEY IS CHECKED FIRST, before any quote is needed.
        # `::vi18n::label $w {Steer}` has no quoted string at all — the
        # key is a brace group — so a quote-first order dropped the site
        # entirely. THAT is why deleting `Steer` from en.msg left this
        # reporting 0 missing: not a bad skip, a missing visit.
        #
        # Either the wrapped key IS the literal (`-text [::vi18n::mc
        # {value (TOML: …)}]`) or the message is COMPOSED from a wrapped
        # key plus a value (`log_insert "[::vi18n::mc {  ⟶ answered: %s}]
        # $shown\n"`). Either way the KEY is what the catalog question is
        # about, and it is what gets looked up.
        set mc_keys [regexp -all -inline \
            {vi18n::(?:mc|label|aname)[^{}]*\{([^{}]*)\}} $line]
        if {[llength $mc_keys]} {
            # ODD ELEMENTS are the captures, even ones the full match —
            # `-all -inline` interleaves them whenever the pattern HAS a
            # capture group. Taking every element reported
            # `vi18n::mc {value …}` as its own missing key; the same
            # doubling trap table_cells documents.
            foreach k [lmap {full cap} $mc_keys { set cap }] {
                if {$exempt} { continue }
                if {[dict exists $KEYS $k]} { continue }
                lappend out [list $n mc $k 0 $exempt]
            }
            continue
        }

        foreach site $sites {
            lassign $site at kind
            set q [quoted_at $line $at]
            if {$q eq ""} { continue }
            lassign $q _ _ text
            # A literal that is pure format specifiers or a glyph is not
            # a sentence ("%s", "·", "").
            if {![regexp {[A-Za-z]{2}} $text]} { continue }
            # A comparison OPERAND is not visible text: `ne "yes"` is
            # what tk_messageBox RETURNS, not something it shows. Found
            # by tripping over it — four sites in instpane/irc, each a
            # `ne "yes"` beside a real -title. The operand follows the
            # operator, so the test is what precedes the quote.
            if {[regexp {(ne|eq|==|!=)\s*$} [string range $line 0 [expr {[lindex $q 0] - 1}]]]} {
                continue
            }
            set composed [expr {[string first "\$" $text] >= 0 \
                                || [string first "\[" $text] >= 0}]
            lappend out [list $n $kind $text $composed $exempt]
        }
    }
    return $out
}

# ── the help-description column of bindings.tcl ─────────────────────
#
# `{keys target action "help" category}` — the help column is read by
# the help overlay, the which-key popup AND the palette hint column, so
# one shared string drives three surfaces. It is data, so no call-site
# rule can see it.
#
# The extraction rule is "the LAST quoted string on the row", not a
# positional regexp: measured over the file, 113 of 115 row lines carry
# exactly one quoted string and 2 carry two (a quoted action column
# like `"/agents"` AND the help). The last one is the help in every
# case, and the positional alternative — a regexp needing a literal
# brace — is what made the first version of this file fail to parse at
# all (Tcl counts braces inside its own regexp braces).
proc ::i18naudit::help_column {path} {
    set out {}
    if {![file exists $path]} { return $out }
    set fh [open $path r] ; fconfigure $fh -encoding utf-8
    set n 0
    foreach line [split [read $fh] \n] {
        incr n
        if {[string match "#*" [string trimleft $line]]} { continue }
        # A registry row: opens with a brace group and ends with a category.
        if {![regexp {^\s+\{.*\}\s*$} $line]} { continue }
        if {![regexp {[a-z_]+\s*\}?\s*$} $line]} { continue }
        set qs [regexp -all -inline {"[^"]*"} $line]
        if {![llength $qs]} { continue }
        set h [string range [lindex $qs end] 1 end-1]
        if {![regexp {[A-Za-z]{2}} $h]} { continue }
        lappend out [list $n help $h 0 0]
    }
    close $fh
    return $out
}

# ── the desktop-local command table ─────────────────────────────────
#
# `{/name {args} {description}}` — the trailing brace group is the
# description. (It was a QUOTED string until 2026-09-26; both spellings
# are accepted here so the audit keeps working whichever one the table
# uses.)
proc ::i18naudit::cmds_table {path} {
    set out {}
    if {![file exists $path]} { return $out }
    set fh [open $path r] ; fconfigure $fh -encoding utf-8
    set n 0
    foreach line [split [read $fh] \n] {
        incr n
        if {[string match "#*" [string trimleft $line]]} { continue }
        if {![regexp {^\s*\{/[a-z]+} $line]} { continue }
        set groups [regexp -all -inline {\{[^{}]*\}} $line]
        if {[llength $groups] >= 3} {
            set t [string range [lindex $groups end] 1 end-1]
            if {[regexp {[A-Za-z]{2}} $t]} { lappend out [list $n cmddesc $t 0 0] }
            continue
        }
        set qs [regexp -all -inline {"[^"]*"} $line]
        if {![llength $qs]} { continue }
        set d [string range [lindex $qs end] 1 end-1]
        if {![regexp {[A-Za-z]{2}} $d]} { continue }
        lappend out [list $n cmddesc $d 0 0]
    }
    close $fh
    return $out
}

# ── label lists that are a whole surface ────────────────────────────
#
# Some modules keep their visible labels in a named proc that returns a
# table, rather than at the widget that displays them. `::vpalette::
# actions` is the case that matters: 54 rows of a quoted label then its
# command, and the label column is what the Ctrl-K palette paints in
# its listbox — the M-x command list, in other words, entirely
# untranslated and invisible to every scan that looks for widget calls.
#
# Enumerated rather than guessed: a proc returning a table of quoted
# labels is a label list only when someone says so. Guessing from the
# shape would sweep up half the tree — measured, the naive rule hit
# 266 sites in palette.tcl alone.
#
# Rows are `file proc-name kind` (no prose examples here: Tcl counts
# braces in comments, and this file has already failed to parse twice
# for that reason).
#
# A PROC, not a bare variable, so a sourced caller reads the same list
# the CLI does — i18ntest used to keep its own copy of these rows, which
# is the drift this whole file exists to prevent.
proc ::i18naudit::label_lists {} {
    return {
        {palette.tcl actions   palette}
        {dev.tcl     _popup_actions popup}
    }
}

proc ::i18naudit::label_list {path procname kind} {
    set out {}
    if {![file exists $path]} { return $out }
    set fh [open $path r] ; fconfigure $fh -encoding utf-8
    set src [read $fh] ; close $fh
    set start [string first "proc ::vpalette::${procname} " $src]
    if {$start < 0} { set start [string first "proc ::vdev::${procname} " $src] }
    if {$start < 0} { return $out }
    # The proc's body: to the first close brace at column 0 after it.
    set end [string first "\n\}\n" $src $start]
    if {$end < 0} { return $out }
    set body [string range $src $start $end]
    # Row shape: a line opening with a quoted label — the label column.
    foreach line [split $body \n] {
        if {[regexp {^\s*\{"([^"]*)"} $line -> label]} {
            if {[regexp {[A-Za-z]{2}} $label]} {
                lappend out [list 0 $kind $label 0 0]
            }
        }
    }
    return $out
}

# ── the API ─────────────────────────────────────────────────────────
#
# SOURCEABLE, like scripts/i18n-keys.tcl and for the same reason: the
# GATE has to measure exactly what this script measures. i18ntest.tcl
# sources this file and calls ::i18naudit::run, so the "0 missing"
# contract and the report can never disagree about what "missing" means.
#
# Returns {in-catalog exempt missing-rows} where missing-rows is a list
# of {file line kind class text}.
proc ::i18naudit::run {root {label_lists {}}} {
    set _dd [::i18naudit::desktop_dir $root]
    set KEYS [load_catalog [file join $_dd msgs en.msg]]
    set rows {}
    foreach f [shipped_modules $root] {
        if {![file exists $f]} { continue }
        set fh [open $f r] ; fconfigure $fh -encoding utf-8
        set src [read $fh] ; close $fh
        set t [file tail $f]
        foreach r [visible_literals $src $KEYS] { lappend rows [linsert $r 0 $t] }
        if {$t eq "bindings.tcl"} {
            foreach r [help_column $f] { lappend rows [linsert $r 0 $t] }
        }
        if {$t eq "cmds.tcl"} {
            foreach r [cmds_table $f] { lappend rows [linsert $r 0 $t] }
        }
        foreach spec $label_lists {
            lassign $spec want procname kind
            if {$t ne $want} { continue }
            foreach r [label_list $f $procname $kind] { lappend rows [linsert $r 0 $t] }
        }
    }
    set missing {}
    set exempt_n 0
    set incat 0
    foreach r $rows {
        lassign $r file line kind text composed exempt
        if {$exempt} { incr exempt_n ; continue }
        if {[dict exists $KEYS $text]} { incr incat ; continue }
        set cls [expr {$composed ? "composed" : "plain"}]
        lappend missing [list $file $line $kind $cls $text]
    }
    return [list $incat $exempt_n $missing [llength $rows]]
}

# ── CLI ─────────────────────────────────────────────────────────────
#
# Guarded on the script path, the way scripts/i18n-keys.tcl is: a sourced
# file must define its procs and do nothing else. `info exists argv` is
# NOT enough — a sourced file inherits the host's argv, which for
# i18ntest.tcl is its own option list.
if {[info exists ::argv0] && [file normalize $::argv0] ne [file normalize [info script]]} {
    return
}

set root [file normalize [file join [file dirname [info script]] ..]]
set mode summary
foreach a $::argv {
    if {$a in {--verbose --list}} { set mode list }
    if {$a eq "--keys"}            { set mode keys }
    if {$a eq "--json"}            { set mode json }
}

lassign [::i18naudit::run $root [::i18naudit::label_lists]] incat exempt_n missing total

switch -- $mode {
    json {
        foreach m $missing {
            lassign $m file line kind cls text
            puts "$file:$line:$kind:$cls:$text"
        }
    }
    keys {
        # One key per line, deduped — the shape a translator's TSV wants.
        set seen {}
        foreach m $missing {
            lassign $m file line kind cls text
            if {[lsearch -exact $seen $text] >= 0} { continue }
            lappend seen $text
            puts $text
        }
    }
    list {
        foreach m $missing {
            lassign $m file line kind cls text
            set t [string map {\n {\\n} \t {\\t}} $text]
            if {[string length $t] > 90} { set t "[string range $t 0 87]…" }
            puts [format "  %-16s %5d %-10s %-8s %s" $file $line $kind $cls $t]
        }
        puts ""
        puts "missing: [llength $missing]"
    }
    default {
        set byfile [dict create]
        set bykind [dict create]
        foreach m $missing {
            lassign $m file line kind cls text
            dict lappend byfile $file 1
            dict incr bykind $kind
        }
        puts "visible strings scanned : $total"
        puts "  in the catalog        : $incat"
        puts "  exempt (reason given) : $exempt_n"
        puts "  MISSING               : [llength $missing]"
        set comp 0
        foreach m $missing { if {[lindex $m 3] eq "composed"} { incr comp } }
        puts "    of which composed   : $comp  (need a %s-slot key)"
        puts ""
        if {[llength $missing]} {
            puts "missing by module:"
            foreach f [lsort [dict keys $byfile]] {
                puts [format "  %-16s %4d" $f [llength [dict get $byfile $f]]]
            }
            puts ""
            puts "missing by call site:"
            foreach k [lsort [dict keys $bykind]] {
                puts [format "  %-10s %4d" $k [dict get $bykind $k]]
            }
        }
    }
}