# veles-desktop/lib/comptest.tcl — the completion popup carries WHAT a
# command does, not just its name.
#
# WHY THIS SUITE EXISTS
#
# PENDING's residual, verbatim: "Autocomplete/help carries no description
# text: complete.tcl `_source` flattens the rich {name args desc cap}
# rows to `[list $name $name cmd]`". The engine had been serving
# `/api/commands/builtin` with a description on every row, `::vcmds::all`
# was already reading and passing it, and the popup threw it away one
# line before rendering — so Tab showed a list of names and the user had
# to already know which command they wanted. The whole point of
# autocomplete is the case where you DO NOT.
#
# These cases drive the real popup on a real window: the flattening bug
# lives in the render path, and reading the source cannot see what a row
# ends up looking like on screen.
#
# Run: scripts/tcl_tests.sh

set _dir [file dirname [file normalize [info script]]]

set failed 0
set passed 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name"
        incr ::failed
    }
}

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

# ── the world complete.tcl expects ──────────────────────────────────
namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
namespace eval ::vi18n { proc label {w key args} { if {![winfo exists $w]} { return 0 } ; catch { $w configure -text [::vi18n::mc $key {*}$args] } ; return 1 }
                    proc aname {w key args} { if {![winfo exists $w]} { return 0 } ; catch { ::va11y::name $w [::vi18n::mc $key {*}$args] } ; return 1 } }
namespace eval ::vstatus {
    variable last ""
    proc set_msg {m} { variable last $m }
}
namespace eval ::vt {
    variable T
    array set T {
        bg #000 bg_panel #222 bg_input #111 fg #eee fg_dim #888
        select_bg #446 hairline #333 accent #f0f
    }
}

# The engine's command table, in the shape `::vcmds::all` returns it
# ({name args desc cap}) — descriptions that are unambiguous enough to
# assert on. `/export` is here because its usage carries an argument
# ("[md|json]") that the matcher must NOT treat as a name.
namespace eval ::vcmds {
    proc all {} {
        return {
            {/model {[name]} {show or switch model} cmd-session}
            {/status {} {session, model, lifecycle} cmd-status}
            {/export {[md|json]} {export session} cmd-session}
            {/quota {[team|personal|zai]} {upstream plan credits} cmd-status}
            {/secret {…} {secrets, redacted from history} cmd-admin}
        }
    }
}

source [file join $_dir vlist.tcl]
source [file join $_dir complete.tcl]

# The composer the popup anchors to and reads. complete.tcl reaches for
# this exact path, so the test builds it rather than stubbing the proc.
wm geometry . 900x600+0+0
frame .mid
frame .mid.chatwrap
frame .mid.chatwrap.chat
text .mid.chatwrap.chat.input -height 3 -width 60
pack .mid -fill both -expand 1
pack .mid.chatwrap -fill both -expand 1
pack .mid.chatwrap.chat -fill both -expand 1
pack .mid.chatwrap.chat.input -fill x
update

proc type {s} {
    .mid.chatwrap.chat.input delete 1.0 end
    .mid.chatwrap.chat.input insert 1.0 $s
    .mid.chatwrap.chat.input mark set insert end
    update
}

# The popup's rendered rows, read off the real listbox.
proc rows {} {
    if {![winfo exists .vcomp.list]} { return {} }
    set out {}
    for {set i 0} {$i < [.vcomp.list size]} {incr i} {
        lappend out [.vcomp.list get $i]
    }
    return $out
}

# ── the source carries the description ──────────────────────────────
puts "the candidate rows carry what the command does"

set ::vcomp::prefix_kind slash
set cands [::vcomp::_source]
ok "slash rows are built" {[llength $cands] == 5}
set model ""
foreach c $cands { if {[lindex $c 1] eq "/model"} { set model $c } }
ok "the display is the usage" {[lindex $model 0] eq "/model \[name\]"}
ok "the completion is the bare name" {[lindex $model 1] eq "/model"}
# THE regression: this field was simply absent before.
ok "the description survives to the renderer" \
    {[lindex $model 3] eq "show or switch model"}
set secret ""
foreach c $cands { if {[lindex $c 1] eq "/secret"} { set secret $c } }
ok "a command whose args are an ellipsis keeps its prose" \
    {[lindex $secret 3] eq "secrets, redacted from history"}

# ── matching is by NAME, not by usage ───────────────────────────────
#
# The display now carries argument hints. Scoring the display would let
# a query match the ARGUMENT vocabulary of a command the user is not
# typing — "md" hitting "/export [md|json]" and offering `export` for a
# word that appears nowhere in its name.
puts "\nthe matcher reads the name, not the usage hints"

proc completions {cands q {field 1}} {
    set out {}
    foreach c [::vcomp::_filter $cands $q $field] { lappend out [lindex $c 1] }
    return $out
}

ok "a usage hint does not surface its command" {
    "/export" ni [completions $cands "md"]
}
# …whereas the display WOULD have matched it, which is what makes the
# assertion above mean something rather than pass vacuously.
ok "the display really is where that word lives" {
    "/export" in [completions $cands "md" 0]
}
ok "the name itself still matches" {[completions $cands "/export"] eq {/export}}
ok "…and on a prefix, not just exactly" {[completions $cands "/exp"] eq {/export}}

# ─ the popup renders them ──────────────────────────────────────────
puts "\nthe popup renders the description"

type "/"
::vcomp::trigger
update
set r [rows]
ok "Tab opens the popup" {[winfo exists .vcomp]}
ok "…with every command listed" {[llength $r] == 5}
set joined [join $r "\n"]
ok "the description is on the row" {[string first "show or switch model" $joined] >= 0}
ok "…beside the command name" \
    {[regexp {/model.+show or switch model} $joined]}
# The category column is uniform for slash commands; repeating "cmd" on
# every row would spend the width the description needs.
ok "…with no repeated category column" {[string first " cmd " $joined] < 0}
# Arguments stay visible, since they are part of what the command takes.
ok "…and the usage arguments too" {[string first "/model \[name\]" $joined] >= 0}

# ─ the doc line ───────────────────────────────────────────────────
#
# Rows are clipped to fit the popup; the SELECTED one gets its full text
# on a line of its own, which is what makes a long description readable
# rather than merely present.
puts "\nthe selected row's full text is shown below the list"

::vcomp::_close
type "/"
::vcomp::trigger
update
ok "the doc line exists" {[winfo exists .vcomp.doc]}
set first_sel $::vcomp::sel
set doc [.vcomp.doc cget -text]
ok "it names the selected command" {[string first "/" $doc] == 0}
ok "…and says what it does" {[string first "—" $doc] >= 0}

::vcomp::move 1
update
set moved_doc [.vcomp.doc cget -text]
ok "walking the list updates it" {
    $::vcomp::sel != $first_sel && $moved_doc ne $doc
}
# The doc line must describe the row the LIST is on, not the one it
# started on — a stale doc line is worse than none.
set sel_row [lindex $r $::vcomp::sel]
set sel_name [lindex [split [string trim $sel_row]] 0]
ok "…to the row the cursor is actually on" {
    [string first $sel_name $moved_doc] >= 0
}

# ─ accepting inserts the name, never the usage ─────────────────────
#
# The completion gained an argument list for DISPLAY. Pasting it into
# the composer would turn Tab into "type a command and some syntax you
# did not ask for".
puts "\naccepting inserts the bare command name"

::vcomp::_close
type "/model"
::vcomp::trigger
update
set before [.mid.chatwrap.chat.input get 1.0 end]
ok "the popup opened on a partial word" {[winfo exists .vcomp]}
::vcomp::_accept
update
set after [string trim [.mid.chatwrap.chat.input get 1.0 end]]
ok "the line is the command alone" {$after eq "/model"}
ok "…and no usage syntax came with it" {[string first "\[name\]" $after] < 0}

# ── clipping marks itself ───────────────────────────────────────────
#
# A row that silently stops mid-word reads as a short description rather
# than a cut one — the text disappearing without saying so is the exact
# bug class the transcript's elisions exist to prevent.
puts "\na clipped row shows that it was clipped"

ok "a fitting string is untouched" {[::vcomp::_clip "short" 20] eq "short"}
ok "a long one is cut" {[string length [::vcomp::_clip "abcdefghij" 5]] == 5}
ok "…with the cut marked" {[string match "*…" [::vcomp::_clip "abcdefghij" 5]]}
ok "…keeping the head, not the tail" {[string match "abcd…" [::vcomp::_clip "abcdefghij" 5]]}
ok "zero room yields nothing" {[::vcomp::_clip "abc" 0] eq ""}

# ── path and word candidates are unchanged ──────────────────────────
#
# They are three-field rows with no prose. The four-field uniform shape
# is made in `_filter`, so a three-field source must still work —
# including for the doc line, which indexes field 3.
puts "\nrows without a description still work"

set ::vcomp::prefix_kind word
set wc [::vcomp::_filter {{alpha alpha history} {beta beta history}} "al"]
ok "a three-field row survives filtering" {[llength $wc] == 1}
ok "…padded to the uniform shape" {[llength [lindex $wc 0]] == 4}
ok "…with an empty description" {[lindex [lindex $wc 0] 3] eq ""}

# Word completion's vocabulary comes from the editor's history.
namespace eval ::vedit { variable hist {alpha beta gamma} }
::vcomp::_close
# Completes the LAST word typed, so the query is "gam" and the answer
# is the history word it spells the start of.
type "gam"
::vcomp::trigger
update
ok "a word popup opens with no doc line" {
    [winfo exists .vcomp] && ![winfo exists .vcomp.doc]
}
ok "…and lists the history word it completes to" \
    {[string first "gamma" [join [rows] "\n"]] >= 0}
::vcomp::_close

puts "\ncomptest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]