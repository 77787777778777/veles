#!/usr/bin/env wish
# veles-desktop/lib/asktest.tcl — the ask dialog must ALWAYS build.
#
# USER (live): the failover dialog collapsed mid-build with
#   can't read "w.btns.e": no such variable
#     while executing "set w.btns.e"
#     (procedure "::vask::show" line 59)
# Root cause: the amend-entry Return bind was
#   bind $w.btns.e <Return> [list ::vask::_answer $id $first_key [set w.btns.e]]
# and `[set w.btns.e]` SUBSTITUTES AT BIND TIME — Tcl reads the unset
# variable w.btns.e, throws, aborts ::vask::show, and the dialog is a
# half-built toplevel with no button text. The fix defers the read to
# keypress time (_answer_key_entry takes the entry as an arg).
#
# Two promises: an approval WITH choices and an amend entry builds
# without error and renders a labelled button per choice; and the
# Return bind reads the entry AT PRESS TIME, not at build time.

set _dir [file dirname [file normalize [info script]]]

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

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

source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir i18n.tcl]
::vi18n::init
# The ticket dialog signs through `exec`, and ::vask::_ticket_sign wraps
# its data words in ::vsafe::arg — the harness has to carry that proc the
# way main.tcl does, or the suite aborts inside the proc instead of
# testing it (the irctest/mdtest pattern).
source [file join $_dir safeexec.tcl]

# Collaborators ::vask reaches for (grab, va11y, api).
namespace eval ::vstatus { proc set_msg {m} { set ::vstatus::last $m } }
set ::vstatus::last ""
namespace eval ::va11y {
    variable names {}
    proc tab_order {w} {}
    proc name {w label} { variable names; dict set names $w $label }
}
namespace eval ::vapi {
    variable decided {}
    proc ask_decide {id choice text} {
        variable decided
        lappend decided [list $id $choice $text]
        return {}
    }
}
namespace eval ::vchat {
    variable session ""
    # WHERE each line went. The report these pin: the ⟶ lines landed in
    # whatever transcript was ON SCREEN, not in the one that asked.
    variable logged {}
    proc log_insert {t tag {sid ""}} { lappend ::vchat::logged [list $sid $t] }
    variable opened {}
    proc open_session {sid} { lappend ::vchat::opened $sid }
}
namespace eval ::vsidebar {
    proc label_for {sid} {
        return [expr {$sid eq "ASK-SESSION" ? "the asking session" : ""}]
    }
}

# ask.tcl reads the ask's fields through ::vapi::field (a JSON null is
# no text).
source [file join $_dir jsonfield.tcl]
source [file join $_dir ask.tcl]

# ── the crash: an approval WITH choices + an amend entry ─────────────
puts "\napproval with choices and an amend entry builds"

set d [dict create id "ask-1" kind approval prompt "run rm -rf /tmp/x?" \
    detail "dangerous" choices [list \
        [dict create key allow label "allow once"] \
        [dict create key deny label "deny"]]]

set err [catch { ::vask::show $d } msg]
ok "show() does not throw" {$err == 0}
ok "…and the dialog exists" {[winfo exists .ask]}
ok "…with a labelled row per choice" \
    {[winfo exists .ask.body.c.inner.ch1.l]
     && [string match "*allow once*" [.ask.body.c.inner.ch1.l cget -text]]
     && [string match "*deny*" [.ask.body.c.inner.ch2.l cget -text]]}
ok "…and the amend entry exists" {[winfo exists .ask.btns.e]}

# Pressing Enter in the amend entry sends the typed text — read AT
# PRESS TIME, not when the bind was built. Since 2026-10-10 it sends the
# text ALONE: "choice 1 + text" approved whatever the note was written
# to change. An `amend: {…}` still rewrites: the engine applies a
# rewrite from the text before it looks at the choice.
.ask.btns.e insert 0 "amend: {\"cmd\": \"safe\"}"
update
focus .ask.btns.e
update
event generate .ask.btns.e <Return>
update
ok "Return answers with the typed amend, read at press time, and no choice" \
    {[lsearch -exact $::vapi::decided [list ask-1 "" "amend: {\"cmd\": \"safe\"}"]] >= 0}

# ── failover shape: four choices, no amend entry is REQUIRED ─────────
puts "\nfailover asks build too"
destroy .ask
set f [dict create id "ask-2" kind failover prompt "route failed" \
    detail "" choices [list \
        [dict create key retry label "retry"] \
        [dict create key next label "next"] \
        [dict create key wait label "wait 20m"] \
        [dict create key stop label "stop"]]]
set err2 [catch { ::vask::show $f } msg2]
ok "a failover ask does not throw" {$err2 == 0}
ok "…and renders every choice" \
    {[winfo exists .ask.body.c.inner.ch4.l]
     && [string match "*stop*" [.ask.body.c.inner.ch4.l cget -text]]}
destroy .ask

# ── RM3 #3: long choice sets stack, wrap, and fit the screen ────────
# USER: "the clarify tool gives all the choices in one horizontal line
# that can't be scrolled and has to be expanded to see them, if there
# even is enough width on the screen. Should be stacked vertically,
# text wrapped, and scrollable if they're still too long."
puts "\nlong choices stack, wrap, and fit"

destroy .ask
set l1 "No floating tip on large panes (name stays for AT, keyboard focus-in, status bar). Small controls keep tips, placed pointer-anchored and refused when they'd cover the composer."
set l2 "Tip everywhere but with a hard 200ms delay and dismissal on any pointer motion leaving the anchor widget, plus a keep-out zone registry every surface must honour."
set l3 "Tips only on demand: a modifier key (shift) reveals them while held, otherwise nothing ever floats over text surfaces larger than the composer."
set l4 "Drop hover tips entirely on the big surfaces and move every hint into the status bar with a keystroke to echo the last one again."
set d3 [dict create id "ask-3" kind clarify prompt "How should the hover tooltip behave on the big text surfaces (transcript, composer, markdown preview, IRC transcript/composer, task-list and log bodies)?" \
    detail "" choices [list \
        [dict create key c1 label $l1] \
        [dict create key c2 label $l2] \
        [dict create key c3 label $l3] \
        [dict create key c4 label $l4]]]
set err3 [catch { ::vask::show $d3 } msg3]
ok "a four-long-choice ask does not throw" {$err3 == 0}
update idletasks
update
ok "every choice row exists" \
    {[winfo exists .ask.body.c.inner.ch1] && [winfo exists .ask.body.c.inner.ch4]}
ok "choices stack vertically — distinct rows, not one line" \
    {[winfo rooty .ask.body.c.inner.ch1.l] != [winfo rooty .ask.body.c.inner.ch2.l]
     && [winfo rooty .ask.body.c.inner.ch2.l] != [winfo rooty .ask.body.c.inner.ch3.l]
     && [winfo rooty .ask.body.c.inner.ch3.l] != [winfo rooty .ask.body.c.inner.ch4.l]}
ok "labels wrap instead of one endless line" \
    {[winfo reqwidth .ask.body.c.inner.ch1.l] <= 500
     && [winfo reqheight .ask.body.c.inner.ch1.l] > 30}
ok "the dialog stays within the screen" \
    {[winfo height .ask] <= int([winfo screenheight .] * 0.85)}
ok "…and the content is reachable — visible in full, or scrollable" \
    {[winfo reqheight .ask.body.c.inner] <= [winfo height .ask.body.c]
     || [winfo ismapped .ask.body.vsb]}
ok "clicking a long choice still answers" \
    {[set ::vapi::decided {}] eq ""
     && ![catch { event generate .ask.body.c.inner.ch2.l <Button-1> }]
     && [lsearch -exact $::vapi::decided [list ask-3 c2 ""]] >= 0}
destroy .ask

# ── PLAN MODE FIX 1: a long plan in the approval detail is readable ──
# USER (2026-09-13): "1. The approval window doesn't display the whole
# plan and while the approval window is up I can't scroll the plan in
# the Tasks pane". The detail text used to be a hardcoded -height 6
# with no scrollbar, and the enclosing canvas never packed its own
# scrollbar either (the inner text's REQUESTED height made the canvas
# believe everything fit). A 200-line plan had no reachable remainder
# anywhere in the dialog.
puts "\na long plan in the approval detail is readable"

set plan_lines {}
for {set i 1} {$i <= 200} {incr i} {
    lappend plan_lines "## Step $i — do the thing $i" \
        "- sub-item a$i with a description" \
        "- sub-item b$i with a description" \
        ""
}
set big_plan [join $plan_lines "\n"]
set d4 [dict create id "ask-4" kind approval prompt "Plan ready for approval" \
    detail $big_plan choices [list \
        [dict create key approve label "approve and start work"] \
        [dict create key keep label "keep planning"]]]
set err4 [catch { ::vask::show $d4 } msg4]
ok "a 200-line plan does not throw" {$err4 == 0}
update idletasks
update
set det .ask.body.c.inner.dbox.detail
ok "the detail widget exists" {[winfo exists $det]}
# Height scales with content, capped: not the old fixed 6, not 800.
ok "detail height scales with the plan (capped at 24 lines)" \
    {[$det cget -height] == 24}
# The whole plan is IN the widget — the reachable-end property.
ok "the whole plan is in the widget" \
    {[llength [split [$det get 1.0 end-1c] "\n"]] >= 200}
# Wheel over the detail scrolls THE DETAIL: after a wheel-down the
# view's first line has moved past 1.
set before [lindex [$det yview] 0]
event generate $det <Button-5>
update
set after [lindex [$det yview] 0]
ok "the wheel scrolls the detail (not the canvas)" {$after > $before}
# …and reaches the end.
$det yview moveto 1.0
update
ok "the end of the plan is reachable" \
    {[lindex [$det yview] 1] >= 0.999}
ok "the dialog still fits the screen" \
    {[winfo height .ask] <= int([winfo screenheight .] * 0.90)}
# A SHORT detail keeps the old behaviour: no scroll chrome for a
# 3-line detail.
destroy .ask
set d5 [dict create id "ask-5" kind approval prompt "ok?" \
    detail "one\nshort\ndetail" choices [list \
        [dict create key allow label "allow"] \
        [dict create key deny label "deny"]]]
catch { ::vask::show $d5 }
update idletasks
update
ok "a short detail renders without scroll chrome" \
    {[$det cget -height] == 3
     && ([lindex [$det yview] 0] <= 0.0 && [lindex [$det yview] 1] >= 1.0)}
destroy .ask

# ── ask-resolved: answered elsewhere closes the dialog, silently ────
# The multi-subscriber contract: another surface's human answered, the
# engine published `ask-resolved`, and this window's dialog must close
# WITHOUT posting its own answer (a second decide on a spent id is a
# lie waiting to render). And an unrelated id must NOT close it.
puts "\nask-resolved closes a foreign-answered dialog"
set ::vapi::decided {}
set d6 [dict create id "ask-6" kind clarify prompt "which file?" \
    choices [list]]
catch { ::vask::show $d6 }
update idletasks
update
ok "dialog is up" {[winfo exists .ask]}
# An unrelated resolution must leave it alone.
catch { ::vask::resolved [dict create id "ask-other" by "answer" answer "x"] }
update
ok "an unrelated id does not close it" {[winfo exists .ask]}
# The real one closes it and posts NOTHING.
catch { ::vask::resolved [dict create id "ask-6" by "answer" answer "main.tcl"] }
update
ok "the matching id closes it" {![winfo exists .ask]}
ok "no decide POST was sent" {$::vapi::decided eq {}}

# by=cancel: withdrawn, same close, still no POST.
set d7 [dict create id "ask-7" kind approval prompt "again?" \
    choices [list [dict create key allow label "allow"]]]
catch { ::vask::show $d7 }
update idletasks
update
catch { ::vask::resolved [dict create id "ask-7" by "cancel"] }
update
ok "a cancelled ask closes its dialog too" {![winfo exists .ask]}
ok "still no decide POST" {$::vapi::decided eq {}}

# ── an ask belongs to the session that raised it ────────────────────
#
# USER 2026-09-17: "approvals arrive in both the session that requested
# it AND whatever was attached at the time of the approval request …
# transcript lines land in BOTH transcripts, but fortunately only the
# correct one seems to act on them".
#
# ::vask never recorded WHICH session asked, and all five of its
# transcript writes called log_insert with no session argument — which
# resolves to the transcript on screen. Meanwhile the engine's own
# approval notes go to the asking session, so approval traces appeared
# in two places at once, one of them wrong.
puts "\nthe ⟶ lines go to the session that asked"

set ::vchat::session "ON-SCREEN"
set ::vchat::logged {}
set ::vchat::opened {}
set ::vapi::decided {}
set d8 [dict create id "ask-8" session_id "ASK-SESSION" kind approval \
    prompt "run rm -rf /?" \
    choices [list [dict create key once label "once"] \
                  [dict create key deny label "deny"]]]
catch { ::vask::show $d8 }
update idletasks
update
ok "a background ask names the session that is asking" \
    {[winfo exists .ask.asked]
     && [string match "*the asking session*" [.ask.asked.t cget -text]]}
ok "…and offers to go there" {[winfo exists .ask.asked.go]}
.ask.asked.go invoke
update
ok "…which opens THAT session, not the one on screen" \
    {$::vchat::opened eq [list "ASK-SESSION"]}
ok "…and lets go of the grab, so the session can be read" \
    {[grab current .ask] eq ""}

::vask::_answer "ask-8" once ""
update
proc misrouted {} {
    set n 0
    foreach e $::vchat::logged {
        if {[lindex $e 0] ne "ASK-SESSION"} { incr n }
    }
    return $n
}
ok "the answer is logged" {[llength $::vchat::logged] >= 1}
ok "…in the asking session, and nowhere else" {[misrouted] == 0}

# answered elsewhere / withdrawn / relayed: same rule, same session.
set ::vchat::logged {}
catch { ::vask::show $d8 }
update
::vask::relayed [dict create id "ask-8" to "telegram:owner" after_secs 30]
::vask::resolved [dict create id "ask-8" by "answer" answer "once"]
update
ok "the relay notice and the foreign answer go there too" \
    {[llength $::vchat::logged] == 2 && [misrouted] == 0}

set ::vchat::logged {}
catch { ::vask::show $d8 }
update
::vask::resolved [dict create id "ask-8" by "cancel"]
update
ok "…and so does a withdrawn question" \
    {[llength $::vchat::logged] == 1 && [misrouted] == 0}

# The session you are ALREADY looking at needs no banner — the absence
# of one is what says "this is yours".
set ::vchat::session "ASK-SESSION"
catch { ::vask::show $d8 }
update idletasks
update
ok "an ask from the session on screen carries no banner" \
    {![winfo exists .ask.asked]}
destroy .ask

# An ask with no session_id at all (an older gateway) must still build,
# and falls back to today's behaviour rather than inventing an owner.
set ::vchat::logged {}
set d9 [dict create id "ask-9" kind clarify prompt "which file?" choices [list]]
catch { ::vask::show $d9 }
update
::vask::_answer "ask-9" "" "main.tcl"
ok "an ask with no session still answers" {[llength $::vchat::logged] == 1}
ok "…and claims no owner it was not told about" \
    {[lindex [lindex $::vchat::logged 0] 0] eq ""}

# ── a detail must not push the choices off the dialog ───────────────
#
# Found with scripts/desktop_demo.sh, whose fixture approval carries a
# detail — as every guard approval does, since the detail IS the command
# — and which showed no choice buttons at all.
#
# The detail was packed `-side left -fill x -expand 1` into the same
# frame the choice rows then packed `-side top` into. A text widget with
# no -width asks for 80 columns, which is wider than the 500px canvas,
# so the detail's slab took the whole cavity and every choice row was
# laid out PAST the right edge of its own parent: existing, bound, and
# unmapped. The only way to answer such an approval was the number keys
# (undiscoverable) or Escape (deny).
puts "\nthe choices stay on screen when the approval has a detail"

destroy .ask
set ::vchat::session "ASK-SESSION"
set d10 [dict create id "ask-10" session_id "ASK-SESSION" kind approval \
    prompt "Delete 412 files from /var/log/veles?" \
    detail "rm -rf /var/log/veles/2026-08*\n\nThe oldest is 41 days; the newest is 8." \
    choices [list [dict create key once label "allow once"] \
                  [dict create key deny label "deny"]]]
catch { ::vask::show $d10 }
update idletasks
update
set inner .ask.body.c.inner
ok "the first choice row is actually on screen" {[winfo ismapped $inner.ch1]}
ok "…and so is the last" {[winfo ismapped $inner.ch2]}
ok "…and they sit inside the dialog, not past its right edge" \
    {[winfo x $inner.ch1] + [winfo width $inner.ch1] <= [winfo width $inner]}
ok "…stacked BELOW the detail, not beside it" \
    {[winfo y $inner.ch1] >= [winfo y $inner.dbox] + [winfo height $inner.dbox]}
ok "…and the detail is still there, and still the whole thing" \
    {[winfo ismapped $inner.dbox.detail]
     && [string match "*newest is 8*" [$inner.dbox.detail get 1.0 end-1c]]}
# Clicking one still answers, which is the point of being able to see it.
set ::vapi::decided {}
event generate $inner.ch1 <Button-1>
update
ok "…and clicking one answers with its key" \
    {[lindex [lindex $::vapi::decided 0] 1] eq "once"}
destroy .ask

# ── multi-select and multi-question dialogs, checked at the WIRE ─────
# From here on the real api.tcl is loaded and only ::vapi::post is
# stubbed: what a test asserts is the JSON body the engine would get.
# (The parity round stubbed ::vapi::ask_decide_answers itself, so the
# hand-built JSON was never checked by anything — the rule in
# lib/memfilestest.tcl, "stub the wire, not the function".)
puts "\nmulti-select and multi-question dialogs (the wire)"
source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]
set ::posted {}
proc ::vapi::post {path body {ms 0}} {
    lappend ::posted [list $path $body]
    return {{"ok":true}}
}
proc ::_last_path {} { lindex [lindex $::posted end] 0 }
proc ::_last_body {} { ::json::json2dict [lindex [lindex $::posted end] 1] }

# ── the note box: a note travels with the choice (USER 2026-10-10) ──
# "Plan approval ignores notes? Does the input textbox even do
# anything?" — and not only on plans. Three faults, each pinned here at
# the WIRE: a clicked row sent an EMPTY text, Enter sent the note glued
# to choice 1 (approve), and a digit typed into the note answered that
# choice on the keystroke.
puts "\nthe note box (the wire)"
set dp [dict create id "ask-n1" kind approval prompt "Plan ready for approval" \
    detail "1. do x\n2. do y" choices [list \
        [dict create key approve label "approve and start work"] \
        [dict create key keep-planning label "keep planning"]]]
set ::posted {}
catch { ::vask::show $dp }
update idletasks
update
ok "the note box has a label" \
    {[winfo exists .ask.btns.l] && [.ask.btns.l cget -text] ne ""}
focus -force .ask.btns.e
update
.ask.btns.e insert end "drop step "
event generate .ask.btns.e <KeyPress-2>
update
ok "a digit typed into the note does not answer" \
    {[winfo exists .ask] && [llength $::posted] == 0}
event generate .ask.body.c.inner.ch2.l <Button-1>
update
ok "a clicked row sends its choice WITH the note" \
    {[llength $::posted] == 1
     && [dict get [::_last_body] choice] eq "keep-planning"
     && [string match "drop step*" [dict get [::_last_body] text]]}
catch { destroy .ask }

set ::posted {}
catch { ::vask::show [dict replace $dp id ask-n2] }
update idletasks
update
.ask.btns.e insert end "use a dry run first"
focus -force .ask.btns.e
update
event generate .ask.btns.e <Return>
update
ok "Enter sends the note alone — no choice, so never an approval" \
    {[llength $::posted] == 1
     && [dict get [::_last_body] choice] eq ""
     && [dict get [::_last_body] text] eq "use a dry run first"}
catch { destroy .ask }

set ::posted {}
catch { ::vask::show [dict replace $dp id ask-n3] }
update idletasks
update
focus -force .ask.btns.e
update
event generate .ask.btns.e <Return>
update
ok "Enter on an empty note answers nothing" \
    {[winfo exists .ask] && [llength $::posted] == 0}
.ask.btns.e insert end "go ahead"
focus -force .ask
update
event generate .ask <KeyPress-1>
update
ok "…while a digit outside the box still answers, note and all" \
    {[llength $::posted] == 1 && [dict get [::_last_body] choice] eq "approve"
     && [dict get [::_last_body] text] eq "go ahead"}
catch { destroy .ask }

# A single-select clarify: the box is an answer, and Enter must not
# prefix it with choice 1.
set ::posted {}
catch { ::vask::show [dict create id "ask-n4" kind clarify prompt "which file?" \
    choices [list [dict create key c1 label "main.tcl"] [dict create key c2 label "ask.tcl"]]] }
update idletasks
update
.ask.btns.e insert end "neither, the api"
focus -force .ask.btns.e
update
event generate .ask.btns.e <Return>
update
ok "a typed clarify answer is the text alone" \
    {[llength $::posted] == 1
     && [dict get [::_last_body] choice] eq ""
     && [dict get [::_last_body] text] eq "neither, the api"}
catch { destroy .ask }

# A MULTI-SELECT single question: check rows, toggle on click, OK
# sends the set.
set dm [dict create id "ask-6" kind clarify prompt "pick any themes" \
    multi true choices [list \
        [dict create key c1 label "synthwave"] \
        [dict create key c2 label "amber"] \
        [dict create key c3 label "mono"]]]
set errm [catch { ::vask::show $dm } msgm]
ok "a multi-select ask builds" {$errm == 0}
update idletasks
update
ok "check rows render" {[winfo exists .ask.body.c.inner.ch1.box]}
ok "the OK button exists" {[winfo exists .ask.btns.ok]}
event generate .ask.body.c.inner.ch1.l <Button-1>
event generate .ask.body.c.inner.ch3.l <Button-1>
update
ok "toggling flips the first row's glyph" \
    {[.ask.body.c.inner.ch1.box cget -text] eq "☑"}
ok "toggling flips the third row's glyph" \
    {[.ask.body.c.inner.ch3.box cget -text] eq "☑"}
ok "the untouched row stays unchecked" \
    {[.ask.body.c.inner.ch2.box cget -text] eq "☐"}
# Untick ch1, leaving exactly one: the plain choice decide.
event generate .ask.body.c.inner.ch1.l <Button-1>
update
.ask.btns.ok invoke
update
ok "one checked row posts that choice" \
    {[::_last_path] eq "/api/approvals/ask-6/decide"
     && [dict get [::_last_body] choice] eq "c3"}
catch { destroy .ask }

# RETURN ON A CHECK ROW toggles it — it must not ALSO submit the
# dialog. The row binds <Return> to toggle and the toplevel binds it to
# OK, and Tk runs both unless the row's binding breaks.
set ::posted {}
set dm2 [dict create id "ask-7" kind clarify prompt "pick any" \
    multi true choices [list \
        [dict create key c1 label "a"] \
        [dict create key c2 label "b"] \
        [dict create key c3 label "c"]]]
catch { ::vask::show $dm2 }
update idletasks
update
focus -force .ask.body.c.inner.ch2
update
event generate .ask.body.c.inner.ch2 <Return>
update
ok "Return on a check row ticks it" \
    {[winfo exists .ask] && [.ask.body.c.inner.ch2.box cget -text] eq "☑"}
ok "…and sends nothing" {[llength $::posted] == 0}
# Two ticked → the multi list.
catch { event generate .ask.body.c.inner.ch1.l <Button-1> }
update
catch { .ask.btns.ok invoke }
update
ok "two checked rows post the list" \
    {[llength $::posted] == 1 && [lsort [dict get [::_last_body] answers]] eq {c1 c2}}
catch { destroy .ask }

# A MULTI-QUESTION ask, in the engine's wire shape (AskQuestion
# serializes the text as `prompt`).
set ::posted {}
proc ::_mq {id} {
    dict create id $id kind clarify prompt "theme?" \
        questions [list \
            [dict create key q1 prompt "theme?" choices [list \
                [dict create key c1 label "synthwave"] \
                [dict create key c2 label "amber"]] multi false] \
            [dict create key q2 prompt "art?" choices [list \
                [dict create key c1 label "yes"] \
                [dict create key c2 label "no"]] multi true] \
            [dict create key q3 prompt "project name?"]]
}
set errq [catch { ::vask::show [::_mq ask-8] } msgq]
ok "a three-question ask builds" {$errq == 0}
update idletasks
update
ok "per-question headers render" \
    {[winfo exists .ask.body.c.inner.hq1] && [winfo exists .ask.body.c.inner.hq2]}
ok "per-question rows are namespaced" \
    {[winfo exists .ask.body.c.inner.chq2_1.l]}
ok "the free-text question keeps its plain entry" \
    {[winfo exists .ask.body.c.inner.oq3.e]}
event generate .ask.body.c.inner.chq1_1.l <Button-1>
event generate .ask.body.c.inner.chq2_1.l <Button-1>
event generate .ask.body.c.inner.chq2_2.l <Button-1>
.ask.body.c.inner.oq3.e insert 0 "Imperial Grid"
update
.ask.btns.ok invoke
update
set got {}
catch { set got [dict get [::_last_body] answers] }
ok "the answers object carries all three questions" \
    {[llength $::posted] == 1 && [dict size $got] == 3}
ok "q1 answered with its choice" \
    {[dict get [dict get $got q1] selected] eq [list c1]}
ok "q2 answered with BOTH choices" \
    {[dict get [dict get $got q2] selected] eq [list c1 c2]}
ok "q3 answered with the typed text" \
    {[dict get [dict get $got q3] text] eq "Imperial Grid"}
catch { destroy .ask }

# ONE question of three answered: still keyed by its question. The
# parity round took the single-question shortcut whenever ONE answer
# was present, posting a bare {"choice":"c2"} that the engine files
# under q1 — the human answered "art?" and the model heard "theme?".
set ::posted {}
catch { ::vask::show [::_mq ask-9] }
update idletasks
update
event generate .ask.body.c.inner.chq2_2.l <Button-1>
update
.ask.btns.ok invoke
update
set got {}
catch { set got [dict get [::_last_body] answers] }
ok "a lone answer to q2 is posted as q2's" \
    {[llength $::posted] == 1 && [dict exists $got q2]
     && [dict get [dict get $got q2] selected] eq [list c2]
     && ![dict exists $got q1]}
catch { destroy .ask }

# OK with nothing selected refuses to answer.
set ::posted {}
catch { ::vask::show $dm2 }
update idletasks
update
.ask.btns.ok invoke
update
ok "an empty OK does not answer" \
    {[llength $::posted] == 0 && [winfo exists .ask]}
catch { destroy .ask }

# ── the ⟶ lines carry the answer, not a raw placeholder ─────────────
# The 2026-09-26 l10n round wrapped these as `"[mc {… %s}] $shown"`: mc
# got no argument, so the line read "⟶ answered: %s blue". Rendered
# here through the real procs, compared whole.
puts "\nthe transcript lines are formatted"
set ::vchat::logged {}
catch { ::vask::show [dict create id "ask-fmt1" kind clarify prompt "q?"] }
update
::vask::_answer ask-fmt1 "" "blue"
ok "an answer logs the answer itself" \
    {[lindex [lindex $::vchat::logged end] 1] eq "  ⟶ answered: blue\n"}
set ::vchat::logged {}
catch { ::vask::show [dict create id "ask-fmt2" kind clarify prompt "q?"] }
update
::vask::resolved [dict create id "ask-fmt2" by answer answer "green"]
ok "an answer from elsewhere logs that answer" \
    {[lindex [lindex $::vchat::logged end] 1] eq "  ⟶ answered elsewhere: green\n"}
catch { destroy .ask }

# ── secrets: a masked field whose value never reaches the answer ─────
# On the wire stub above (`::posted`): the value's one route and the
# ask's answer are both checked as the JSON the gateway would get.
puts "\nsecret asks"

# The gateway answers whether THIS desktop may send it values.
set ::gw_accepts true
proc ::vapi::get {path {ms 0}} {
    if {$path eq "/api/secrets"} {
        return "{\"secrets\": \[\], \"accepts_values\": $::gw_accepts}"
    }
    error "unexpected GET $path"
}
proc ::_posts_to {path} {
    set out {}
    foreach p $::posted { if {[lindex $p 0] eq $path} { lappend out [lindex $p 1] } }
    return $out
}
set ::posted {}
set ::vchat::logged {}
set VAL "sk-typed-in-the-dialog-0042"
catch { ::vask::show [dict create id "ask-s1" session_id "ASK-SESSION" kind secret \
    prompt "REPLICATE_API_TOKEN" detail "for the image job"] } msg
update
ok "a secret ask opens its own dialog" \
    {[winfo exists .ask.value] && [wm title .ask] eq "Secret needed"}
ok "…with a masked field" {[.ask.value cget -show] eq "•"}
ok "…named for the model's request, with its purpose" \
    {[string match "*REPLICATE_API_TOKEN*" [.ask.title cget -text]] \
     && [.ask.purpose cget -text] eq "for the image job"}
ok "…and says where the value goes" {[string match "*encrypted vault*" [.ask.where cget -text]]}
::vask::_secret_store REPLICATE_API_TOKEN ask-s1
ok "an empty field stores nothing and says so" \
    {[llength $::posted] == 0 && [.ask.err cget -text] eq "Nothing typed yet."}
set ::vask::secret_value $VAL
::vask::_secret_store REPLICATE_API_TOKEN ask-s1
update
set sb [::json::json2dict [lindex [::_posts_to /api/secrets/set] end]]
ok "the value goes to the vault route" \
    {[dict get $sb value] eq $VAL && [dict get $sb name] eq "REPLICATE_API_TOKEN" \
     && [dict get $sb ask_id] eq "ask-s1"}
set db [::json::json2dict [lindex [::_posts_to /api/approvals/ask-s1/decide] end]]
ok "the ask is answered 'set', with no text" \
    {[dict get $db choice] eq "set" && [dict get $db text] eq ""}
ok "the value is in no other request" \
    {[llength [lsearch -all -glob $::posted "*$VAL*"]] == 1}
ok "the transcript says stored, never the value" \
    {[string match "*stored \$REPLICATE_API_TOKEN in the vault*" [join $::vchat::logged]] \
     && ![string match "*$VAL*" [join $::vchat::logged]]}
ok "the field's variable is empty again" {$::vask::secret_value eq ""}

# A refusal keeps the dialog, says why, and still forgets the value.
set ::posted {}
catch { ::vask::show [dict create id "ask-s2" kind secret prompt "X_KEY" detail ""] }
update
set ::vask::secret_value $VAL
rename ::vapi::post ::_post_ok
proc ::vapi::post {path body {ms 0}} {
    lappend ::posted [list $path $body]
    error {HTTP 403: {"ok": false, "error": "a secret is stored only from the machine it is for"}}
}
::vask::_secret_store X_KEY ask-s2
rename ::vapi::post ""
rename ::_post_ok ::vapi::post
ok "a refusal is said in the dialog, in the engine's words" \
    {[winfo exists .ask] && [string match "*only from the machine*" [.ask.err cget -text]]}
ok "…the ask stays open, and the value is forgotten" \
    {[llength [::_posts_to /api/approvals/ask-s2/decide]] == 0 && $::vask::secret_value eq ""}
::vask::_secret_close ask-s2
set db [::json::json2dict [lindex [::_posts_to /api/approvals/ask-s2/decide] end]]
ok "closing it declines" {[dict get $db choice] eq "decline"}

# A gateway that takes no values from here: no field, where to type it,
# Done answers set.
set ::gw_accepts false
set ::posted {}
catch { ::vask::show [dict create id "ask-s3" kind secret prompt "Y_KEY" detail ""] }
update
ok "a gateway that takes no values from here gets no field" {![winfo exists .ask.value]}
ok "…and the line that says where to enter it, or what to allow" \
    {[string match "*veles secret set Y_KEY*trusted_networks*" [.ask.where cget -text]]}
::vask::_secret_done ask-s3
set db [::json::json2dict [lindex [::_posts_to /api/approvals/ask-s3/decide] end]]
ok "Done answers set and stores nothing here" \
    {[dict get $db choice] eq "set" && [llength [::_posts_to /api/secrets/set]] == 0}
set ::gw_accepts true

# The operator's own `/secret set NAME`: no ask behind it.
set ::posted {}
::vask::secret_dialog OWN_KEY ""
set ::vask::secret_value $VAL
::vask::_secret_store OWN_KEY ""
set sb [::json::json2dict [lindex [::_posts_to /api/secrets/set] end]]
ok "/secret set stores with no ask id and answers no ask" \
    {[dict get $sb ask_id] eq "" && [llength $::posted] == 1 && ![winfo exists .ask]}

# ── tickets (PLAN-pool-round.md D3) ─────────────────────────────────
# A call homed on another node needs a ticket: signed here by a
# `veles ticket sign` child (a stub on PATH that checks what it is fed),
# or pasted. The stub signs only when the passphrase arrives on STDIN.
set tdir [file join [expr {[info exists ::env(TMPDIR)] && $::env(TMPDIR) ne ""
    ? $::env(TMPDIR) : "/tmp"}] "asktest-tickets-[pid]"]
file mkdir [file join $tdir bin] [file join $tdir cfg veles-signer]
set fp [open [file join $tdir cfg veles-signer phone.key] w]; close $fp
set fp [open [file join $tdir bin veles] w]
puts $fp "#!/bin/sh
printf '%s\\n' \"\$*\" > [file join $tdir argv]
read pass
if \[ \"\$pass\" = right-pass \]; then echo veles-ticket:1.KID.PAYLOAD.SIG; exit 0; fi
echo 'error: the passphrase does not open this key' >&2; exit 1"
close $fp
file attributes [file join $tdir bin veles] -permissions 0755
set ::env(PATH) "[file join $tdir bin]:$::env(PATH)"
set ::env(XDG_CONFIG_HOME) [file join $tdir cfg]
unset -nocomplain ::env(VELES_HOME)
unset -nocomplain ::env(VELES_EXE)
catch { unset -nocomplain ::auto_execs(veles) }
set REQ "veles-ticket-request:1.eyJpZCI6IngifQ"
set ::posted {}
catch { ::vask::show [dict create id "ask-t1" session_id "ASK-SESSION" kind ticket \
    prompt "cronjob on node laptop needs a ticket" \
    detail "cronjob on node laptop, as owner\nargs:\n{}\n\n$REQ"] } msg
update
ok "a ticket ask opens its own dialog" \
    {[winfo exists .ask.paste] && [wm title .ask] eq "Ticket needed"}
ok "…showing the call and the request to sign" \
    {[string match "*$REQ*" [.ask.detail get 1.0 end]]}
ok "…and a way to sign it with this machine's key" \
    {[winfo exists .ask.sign.pass] && [.ask.sign.pass cget -show] eq "•" \
     && $::vask::ticket_key eq "phone"}
set ::vask::ticket_pass "wrong-pass"
::vask::_ticket_sign ask-t1 $REQ
update
ok "a wrong passphrase signs nothing and says why" \
    {[winfo exists .ask] && [string match "*does not open this key*" [.ask.err cget -text]] \
     && [llength [::_posts_to /api/approvals/ask-t1/decide]] == 0}
ok "…and the passphrase is forgotten" {$::vask::ticket_pass eq ""}
set ::vask::ticket_pass "right-pass"
::vask::_ticket_sign ask-t1 $REQ
update
set db [::json::json2dict [lindex [::_posts_to /api/approvals/ask-t1/decide] end]]
ok "the signed ticket answers the ask" \
    {[dict get $db text] eq "veles-ticket:1.KID.PAYLOAD.SIG" && [dict get $db choice] eq ""}
set fp [open [file join $tdir argv]]; set av [read $fp]; close $fp
ok "…the child was asked for exactly this request, by key" \
    {[string match "ticket sign $REQ --yes --key phone*" $av]}
ok "…and never saw the passphrase in its arguments" {![string match "*right-pass*" $av]}

set ::posted {}
catch { ::vask::show [dict create id "ask-t2" kind ticket prompt "p" detail "no request here"] }
update
ok "no request in the detail: no signing, the paste stays" \
    {![winfo exists .ask.sign] && [winfo exists .ask.paste]}
set ::vask::ticket_paste "just some words"
::vask::_ticket_use ask-t2
ok "pasting something that is not a ticket is refused in the dialog" \
    {[winfo exists .ask] && [.ask.err cget -text] eq "That is not a ticket." \
     && [llength $::posted] == 0}
set ::vask::ticket_paste "  here: veles-ticket:1.K.P.S  "
::vask::_ticket_use ask-t2
set db [::json::json2dict [lindex [::_posts_to /api/approvals/ask-t2/decide] end]]
ok "a pasted ticket answers the ask, cut from what surrounds it" \
    {[dict get $db text] eq "veles-ticket:1.K.P.S"}
catch { ::vask::show [dict create id "ask-t3" kind ticket prompt "p" detail ""] }
update
::vask::_ticket_close ask-t3
set db [::json::json2dict [lindex [::_posts_to /api/approvals/ask-t3/decide] end]]
ok "closing it declines" {[dict get $db choice] eq "decline"}

# The binary that launched the window signs, not the first `veles` on
# PATH: here PATH held an installed build a week older than the running
# one, with no `ticket` subcommand, so every signature failed.
file mkdir [file join $tdir own]
set fp [open [file join $tdir own veles] w]
puts $fp "#!/bin/sh
read pass
echo veles-ticket:1.OWN.PAYLOAD.SIG"
close $fp
file attributes [file join $tdir own veles] -permissions 0755
set ::env(VELES_EXE) [file join $tdir own veles]
set ::posted {}
catch { ::vask::show [dict create id "ask-t4" kind ticket prompt "p" \
    detail "x\n\n$REQ"] }
update
set ::vask::ticket_pass "any-pass"
::vask::_ticket_sign ask-t4 $REQ
set posts [::_posts_to /api/approvals/ask-t4/decide]
ok "the window's own binary signs, not the first veles on PATH" \
    {[llength $posts] > 0
     && [dict get [::json::json2dict [lindex $posts end]] text] eq "veles-ticket:1.OWN.PAYLOAD.SIG"}
unset -nocomplain ::env(VELES_EXE)
file delete -force $tdir

# ── bot challenges (PLAN-pool-round.md Part E) ──────────────────────
# The page arrives as a screenshot; a click on it is the answer, in the
# PAGE's pixels even when the picture had to be shrunk to fit.
proc _png {w h} {
    set img [image create photo -width $w -height $h]
    $img put #336699 -to 0 0 $w $h
    set data [$img data -format png]
    image delete $img
    return $data
}
proc _last_post {id} {
    set posts [::_posts_to /api/approvals/$id/decide]
    if {![llength $posts]} { return {} }
    return [::json::json2dict [lindex $posts end]]
}
set ::posted {}
catch { ::vask::show [dict create id "ask-c1" session_id "ASK-SESSION" kind challenge \
    prompt "cloudflare challenge on shop.example — solve it for the agent" \
    detail "https://shop.example/item\nstub gate\n\nClick the picture…" \
    image [_png 200 100] \
    choices [list [dict create key solved label solved] [dict create key give_up label "give up"]]] } msg
update
ok "a challenge opens its own dialog, with the page as a picture" \
    {[winfo exists .ask.shot] && [wm title .ask] eq "Solve this for the agent"}
ok "…and no window button unless the node running the browser can open one" \
    {![winfo exists .ask.btns.win]}
event generate .ask.shot <Button-1> -x 30 -y 40
update
set db [_last_post ask-c1]
ok "a click on the picture is the answer, in the page's pixels" \
    {[llength $db] && [dict get $db text] eq "click 30 40" && [dict get $db choice] eq ""}

set ::posted {}
catch { ::vask::show [dict create id "ask-c2" kind challenge prompt "p" detail "u" \
    image [_png 2000 1000] \
    choices [list [dict create key solved label solved] [dict create key window label "open a window"]]] }
update
ok "a page too big for the screen is shrunk to fit" \
    {[image width [.ask.shot cget -image]] < 2000 && $::vask::challenge_scale > 1}
ok "…and the window button is there when it is offered" {[winfo exists .ask.btns.win]}
set s $::vask::challenge_scale
event generate .ask.shot <Button-1> -x 10 -y 12
update
set db [_last_post ask-c2]
ok "…and a click on the shrunk picture lands where it would on the page" \
    {[llength $db] && [dict get $db text] eq "click [expr {10 * $s}] [expr {12 * $s}]"}

set ::posted {}
catch { ::vask::show [dict create id "ask-c3" kind challenge prompt "p" detail "u" image [_png 50 50]] }
update
set ::vask::challenge_text "  hunter2  "
::vask::_challenge_type ask-c3
set db [_last_post ask-c3]
ok "typing goes to the page" {[llength $db] && [dict get $db text] eq "type hunter2"}
set ::posted {}
catch { ::vask::show [dict create id "ask-c4" kind challenge prompt "p" detail "u" image [_png 50 50]] }
update
.ask.btns.solved invoke
set db [_last_post ask-c4]
ok "Solved is a verdict, not text" {[llength $db] && [dict get $db choice] eq "solved"}

puts "\nasktest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
