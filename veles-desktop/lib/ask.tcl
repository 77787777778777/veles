# veles-desktop/lib/ask.tcl — modal questions from the agent (M11).
#
# Two kinds arrive on the chat SSE stream as an `ask` event:
#   approval — the agent wants to run something risky; choices are
#              once / session / always / deny
#   clarify  — the agent is asking the user a question, optionally with
#              up to four choices, otherwise free text
#
# The turn is BLOCKED on the answer, so the dialog is modal (grab) and
# always resolves: closing the window counts as "deny"/empty, and the
# agent side has its own timeout as a backstop.
#
# 2026-09-26 parity round: multi-select questions render CHECKBUTTON
# rows (tap toggles, OK sends the set); multi-question asks render one
# labelled group per question; every choices question gains an
# "Other…" row (entry) so free text is first-class next to buttons —
# the affordance the amend entry already had, now per question.

namespace eval ::vask {
    variable current ""   ;# id of the ask on screen
    ;# WHICH SESSION RAISED IT. The turn asking the question is not
    ;# necessarily the one you are looking at — switching away from a
    ;# running turn is the point of switching — and every line this file
    ;# writes belongs in the ASKER's transcript.
    ;#
    ;# USER 2026-09-17: "approvals arrive in both the session that
    ;# requested it AND whatever was attached at the time … transcript
    ;# lines land in BOTH transcripts". They did: log_insert with no
    ;# session argument resolves to the transcript on screen, while the
    ;# engine's own approval notes go to the asker.
    ;#
    ;# "" means the payload did not say (an older gateway), and the
    ;# fallback is then exactly the old behaviour rather than a guess.
    variable current_sid ""
    ;# Toggle state for check-row questions: dict qk -> list of choice
    ;# keys currently checked.
    variable checked
}

# The questions of an ask payload, normalised: a list of dicts
#   {key k prompt p choices {{key cK label cL} …} multi 0/1}
# A payload without `questions` synthesises its single question from
# prompt/choices/multi (the engine's all_questions, mirrored here so
# the dialog never depends on gateway age).
proc ::vask::_questions {d} {
    set out {}
    if {[catch {dict get $d questions} qs]} { set qs {} }
    if {[llength $qs] == 0} {
        set ch {}
        catch {
            foreach c [dict get $d choices] {
                lappend ch $c
            }
        }
        set multi 0
        catch { set multi [dict get $d multi] }
        if {![string is boolean -strict $multi]} { set multi 0 }
        set out [list [dict create key q1 \
            prompt [dict get $d prompt] choices $ch multi [expr {$multi ? 1 : 0}]]]
    } else {
        set i 0
        foreach q $qs {
            incr i
            set k ""
            catch { set k [dict get $q key] }
            if {$k eq ""} { set k "q$i" }
            # The question text: the ENGINE wire says `prompt`
            # (AskQuestion's serde), the TOOL schema says `question` —
            # read both, never guess.
            set text ""
            catch { set text [dict get $q prompt] }
            if {$text eq ""} { catch { set text [dict get $q question] } }
            set ch {}
            catch {
                foreach c [dict get $q choices] { lappend ch $c }
            }
            set multi 0
            catch { set multi [dict get $q multi] }
            if {![string is boolean -strict $multi]} { set multi 0 }
            lappend out [dict create key $k \
                prompt $text choices $ch multi [expr {$multi ? 1 : 0}]]
        }
    }
    return $out
}

proc ::vask::show {d} {
    variable current
    variable current_sid
    variable checked
    if {[catch {dict get $d id} id]} {
        puts stderr "veles-desktop: ask show: no id in event, dropped: $d"
        return
    }
    set w .ask
    # One dialog at a time: a second question waits for the first to be
    # answered (the agent asks serially anyway).
    if {[winfo exists $w]} { destroy $w }
    set current $id
    set current_sid ""
    catch { set current_sid [dict get $d session_id] }
    set checked [dict create]

    set kind "clarify"
    catch { set kind [dict get $d kind] }
    # A clarify's detail is JSON null, which `dict get` returns as the
    # word: the question showed "null" under it (emulator, 2026-09-30).
    set prompt [::vapi::field $d prompt]
    set detail [::vapi::field $d detail]
    set choices {}
    catch { set choices [dict get $d choices] }

    # A secret the agent asked for (`secret_request`) has its own dialog:
    # a masked field whose value goes to the vault and never into the
    # answer, which is only "set" or "decline".
    if {$kind eq "secret"} {
        ::vask::secret_dialog $prompt $detail $id $current_sid
        return
    }
    # A call homed on another node that needs a ticket: signed here with
    # a key on this machine, or pasted (PLAN-pool-round.md D3).
    if {$kind eq "ticket"} {
        ::vask::ticket_dialog $prompt $detail $id $current_sid
        return
    }
    # A bot challenge on a page the agent's browser opened: the page as a
    # picture, answered by clicking it (PLAN-pool-round.md Part E).
    if {$kind eq "challenge"} {
        set image ""
        catch { set image [dict get $d image] }
        ::vask::challenge_dialog $prompt $detail $id $current_sid $image $choices
        return
    }

    set questions [::vask::_questions $d]
    set multi_q [expr {[llength $questions] > 1}]

    toplevel $w
    # `mc` of an [expr] result is a NO-OP: the argument is a computed
    # string ("Approval needed"), which is not a key, and msgcat answers
    # the string it was given. So this title said "Approval needed" in
    # every language while the {Question} arm translated fine — the exact
    # shape of the steer/Send half-bug beside it. Branch over KEYS and
    # let mc resolve each one.
    if {$kind eq "approval"} {
        wm title $w [::vi18n::mc {Approval needed}]
    } elseif {$kind eq "failover"} {
        wm title $w [::vi18n::mc {Failover needed}]
    } elseif {$multi_q} {
        wm title $w [::vi18n::mc {Questions}]
    } else {
        wm title $w [::vi18n::mc {Question}]
    }
    wm transient $w .
    wm protocol $w WM_DELETE_WINDOW [list ::vask::_answer $id deny ""]
    wm minsize $w 560 120

    if {$multi_q} {
        # Every question at once: the title is the count, each
        # question heads its own group.
        set n [llength $questions]
        ttk::label $w.title -wraplength 520 -justify left \
            -font {TkDefaultFont 12 bold}
        ::vi18n::label $w.title {%1$d questions — answer each} $n
        pack $w.title -side top -fill x -padx 12 -pady {12 6}
    } else {
        ttk::label $w.title -text $prompt -wraplength 520 -justify left \
            -font {TkDefaultFont 12 bold}
        pack $w.title -side top -fill x -padx 12 -pady {12 6}
    }

    # WHOSE QUESTION IS THIS. A turn keeps running when you switch away
    # from its session, so the dialog that pops up is often raised by a
    # session you are not looking at — and with nothing saying so it
    # reads as if the transcript in front of you asked it. Approving the
    # wrong session's rm is not a mistake the window should make easy.
    #
    # Only when it differs: a banner on every approval would be noise,
    # and its ABSENCE is then what says "this one is yours".
    #
    # The button switches the view so the question can be read in
    # context, and RELEASES THE GRAB on the way — a modal dialog over a
    # transcript you cannot scroll would be a switch in name only. The
    # dialog stays up: the relay adds a place to read, it does not move
    # the question.
    set onscreen ""
    catch { set onscreen $::vchat::session }
    if {$current_sid ne "" && $current_sid ne $onscreen} {
        set label $current_sid
        catch {
            set l [::vsidebar::label_for $current_sid]
            if {$l ne ""} { set label $l }
        }
        ttk::frame $w.asked
        ttk::label $w.asked.t -justify left -wraplength 400
        ::vi18n::label $w.asked.t {asked by: %s} $label
        ttk::button $w.asked.go -text [::vi18n::mc {go to session}] \
            -command [list ::vask::_goto $current_sid]
        pack $w.asked.t -side left -fill x -expand 1
        pack $w.asked.go -side right
        pack $w.asked -side top -fill x -padx 12 -pady {0 6}
    }

    # The answer row: free text lives BELOW the choices now, on its own
    # full-width row — packed before the body so `-side bottom` pins it
    # however long the choice list gets.
    ttk::frame $w.btns
    pack $w.btns -side bottom -fill x -padx 12 -pady {6 12}

    # ── the choices: STACKED, WRAPPED, SCROLLABLE (RM3 #3) ───────────
    # USER: "the clarify tool gives all the choices in one horizontal
    # line that can't be scrolled and has to be expanded to see them,
    # if there even is enough width on the screen to display all of
    # them. Should be stacked vertically, text wrapped, and scrollable
    # if they're still too long." ttk::button cannot wrap, so each
    # choice is a focusable clickable frame carrying a wrapping label;
    # the whole content area is a canvas that scrolls when even
    # stacked rows exceed 70% of the screen.
    set cap [expr {int([winfo screenheight $w] * 0.70)}]
    ttk::frame $w.body
    pack $w.body -side top -fill both -expand 1 -padx 12 -pady 4
    canvas $w.body.c -highlightthickness 0 -bd 0 -width 500 \
        -yscrollcommand [list $w.body.vsb set]
    pack $w.body.c -side left -fill both -expand 1
    ttk::scrollbar $w.body.vsb -orient vertical -command [list $w.body.c yview]
    frame $w.body.c.inner
    $w.body.c create window 0 0 -anchor nw -window $w.body.c.inner -tags inner
    # The inner frame tracks the canvas width (so wraplength is honest
    # against the real dialog width) and the scrollregion tracks the
    # content height.
    bind $w.body.c <Configure> { %W itemconfigure inner -width %w }
    bind $w.body.c.inner <Configure> [list ::vask::_scrolled $w]

    if {[string trim $detail] ne ""} {
        # PLAN MODE FIX 1 (USER 2026-09-13, verbatim: "The approval
        # window doesn't display the whole plan"). The detail used to
        # be a `-height 6` text with no scrollbar of its own, and the
        # canvas around it never packed ITS scrollbar either — the
        # inner text reports a small REQUESTED height, so the canvas
        # believed the content fit. A 200-line plan had no reachable
        # remainder anywhere in the dialog. Now: the height scales
        # with the content up to 24 lines, the widget gets its own
        # scrollbar, and the wheel (both Tk 8.6 MouseWheel and X11
        # Button-4/5) scrolls it.
        set dlines [llength [split $detail "\n"]]
        set dheight [expr {min(max($dlines, 3), 24)}]
        # THE DETAIL GETS ITS OWN ROW, and that is not cosmetic.
        #
        # It used to pack `-side left -fill x -expand 1` straight into
        # `inner`, which the choice rows then packed `-side top` into. A
        # text widget with no -width asks for 80 columns — wider than
        # this 500px canvas — so the detail's slab took the whole cavity
        # and every choice was laid out PAST the right edge of its own
        # parent: existing, bound, and unmapped. An approval with a
        # detail is every guard approval (the detail IS the command), so
        # the only ways to answer one were the number keys, which
        # nothing advertises, and Escape, which denies. Found with
        # scripts/desktop_demo.sh; asktest pins it.
        #
        # A frame of its own, packed -side top, keeps the text and its
        # scrollbar side by side inside that row and leaves the rest of
        # the cavity to the choices — which is what "stacked vertically,
        # text wrapped, and scrollable" asked for in the first place.
        ttk::frame $w.body.c.inner.dbox
        pack $w.body.c.inner.dbox -side top -fill x
        text $w.body.c.inner.dbox.detail -height $dheight -wrap word -relief flat \
            -background $::vt::T(bg_input) -foreground $::vt::T(fg)
        $w.body.c.inner.dbox.detail insert end $detail
        $w.body.c.inner.dbox.detail configure -state disabled
        ttk::scrollbar $w.body.c.inner.dbox.dvsb -orient vertical \
            -command [list $w.body.c.inner.dbox.detail yview]
        $w.body.c.inner.dbox.detail configure \
            -yscrollcommand [list ::vask::_detail_scroll $w.body.c.inner.dbox.dvsb]
        # The scrollbar is managed but only SHOWN when the content
        # overflows the (already content-scaled) height — a 6-line
        # detail needs no chrome.
        pack $w.body.c.inner.dbox.detail -side left -fill x -expand 1 -pady 2
        pack $w.body.c.inner.dbox.dvsb -side right -fill y
        foreach ev {<MouseWheel> <Button-4> <Button-5>} {
            bind $w.body.c.inner.dbox.detail $ev \
                [list ::vask::_detail_wheel $w.body.c.inner.dbox.detail %D %b]
        }
        # The wheel over the scrollbar itself drives the same text.
        foreach ev {<MouseWheel> <Button-4> <Button-5>} {
            bind $w.body.c.inner.dbox.dvsb $ev \
                [list ::vask::_detail_wheel $w.body.c.inner.dbox.detail %D %b]
        }
    }

    # ── the choices: per-question groups, check rows when multi ──────
    # A multi-select question toggles rows (checkbox semantics) and a
    # single OK sends the set — one tap per row must not answer a
    # "pick any" question. Single-select rows keep the instant-answer
    # behaviour. Every choices question gains an "Other…" entry row so
    # free text is an answer, not a workaround.
    set any_toggle 0
    set row 0
    foreach q $questions {
        set qk [dict get $q key]
        set qmulti [dict get $q multi]
        if {$multi_q} {
            incr row
            ttk::label $w.body.c.inner.h$qk -wraplength 470 -justify left \
                -anchor w -padding {2 6 0 2} -font {TkDefaultFont 10 bold}
            ::vi18n::label $w.body.c.inner.h$qk {%1$d. %2$s} $row [dict get $q prompt]
            pack $w.body.c.inner.h$qk -side top -fill x -pady {6 2}
        }
        set i 0
        foreach c [dict get $q choices] {
            set key ""
            catch { set key [dict get $c key] }
            set label ""
            catch { set label [dict get $c label] }
            incr i
            # Single-question keeps the chN widget names the a11y
            # cycle and the tests have always known; multi-question
            # namespaces them per question (chq2_1).
            set fname [expr {$multi_q ? "ch${qk}_$i" : "ch$i"}]
            if {$qmulti || $multi_q} {
                # CHECK ROW: click toggles membership in ::vask::checked.
                set f $w.body.c.inner.$fname
                frame $f -bd 0 -highlightthickness 2 \
                    -highlightbackground $::vt::T(bg_panel) \
                    -highlightcolor $::vt::T(accent) \
                    -takefocus 1 -cursor hand2
                ttk::label $f.box -text "☐" -width 2 -anchor center \
                    -font {TkDefaultFont 11}
                ttk::label $f.l -wraplength 430 -justify left \
                    -anchor w -padding {6 8}
                ::vi18n::label $f.l {%1$s) %2$s} $i $label
                pack $f.box -side left -fill y
                pack $f.l -side left -fill x -expand 1
                pack $f -side top -fill x -pady 2
                # Return/Space TOGGLE and then `break`: the toplevel binds
                # <Return> to OK, and without the break Tk ran both — a
                # keyboard user ticking a row submitted the dialog.
                set tog [list ::vask::_toggle $id $qk $key $f.box [expr {$qmulti ? 0 : 1}]]
                foreach sub [list $f $f.l $f.box] {
                    bind $sub <Button-1> $tog
                    bind $sub <Return> "$tog; break"
                    bind $sub <space> "$tog; break"
                    bind $sub <Enter> [list ::vask::_hover $f 1]
                    bind $sub <Leave> [list ::vask::_hover $f 0]
                }
                set any_toggle 1
            } else {
                set f $w.body.c.inner.$fname
                frame $f -bd 0 -highlightthickness 2 \
                    -highlightbackground $::vt::T(bg_panel) \
                    -highlightcolor $::vt::T(accent) \
                    -takefocus 1 -cursor hand2
                ttk::label $f.l -wraplength 470 -justify left \
                    -anchor w -padding {10 8}
                ::vi18n::label $f.l {%1$s) %2$s} $i $label
                pack $f.l -fill x
                pack $f -side top -fill x -pady 2
                # The note typed in the bottom entry rides with the
                # choice (USER 2026-10-10: "Plan approval ignores notes?
                # Does the input textbox even do anything?" — every row
                # used to answer with an EMPTY text, so a note was only
                # ever sent by Enter in the box, glued to choice 1).
                foreach sub [list $f $f.l] {
                    bind $sub <Button-1> [list ::vask::_answer_key_entry $id $key $w.btns.e]
                    bind $sub <Return> [list ::vask::_answer_key_entry $id $key $w.btns.e]
                    bind $sub <space> [list ::vask::_answer_key_entry $id $key $w.btns.e]
                    bind $sub <Enter> [list ::vask::_hover $f 1]
                    bind $sub <Leave> [list ::vask::_hover $f 0]
                }
            }
        }
        if {$multi_q && [llength [dict get $q choices]] == 0} {
            # A free-text question inside a multi-question ask: the
            # entry IS its answer field (no choices to pick instead).
            ttk::frame $w.body.c.inner.o$qk
            ttk::entry $w.body.c.inner.o$qk.e
            pack $w.body.c.inner.o$qk.e -side left -fill x -expand 1 \
                -padx {10 10} -pady 2
            pack $w.body.c.inner.o$qk -side top -fill x
        } elseif {$kind eq "clarify" && ($qmulti || $multi_q)
            && [llength [dict get $q choices]] > 0} {
            # The Other row: free text for THIS question, sent with the
            # OK button alongside checked rows. Only where the OK
            # button exists (toggle mode) — a single-select clarify
            # already has the bottom entry as its free-text path, and
            # an approval's amend entry / a failover's verbs want no
            # Other row at all.
            ttk::frame $w.body.c.inner.o$qk
            ttk::label $w.body.c.inner.o$qk.l -text [::vi18n::mc {Other…}]
            ttk::entry $w.body.c.inner.o$qk.e
            pack $w.body.c.inner.o$qk.l -side left -padx {10 6}
            pack $w.body.c.inner.o$qk.e -side left -fill x -expand 1 \
                -padx {0 10} -pady 2
            pack $w.body.c.inner.o$qk -side top -fill x
        }
    }

    if {$any_toggle || $multi_q} {
        # Toggle mode: OK assembles the whole answer (checked rows per
        # question + each Other entry's text). Cancel stays Escape.
        ttk::button $w.btns.ok -text [::vi18n::mc {OK}] \
            -command [list ::vask::_commit $id]
        pack $w.btns.ok -side right -padx 4
        bind $w <Return> [list ::vask::_commit $id]
        bind $w <Escape> [list ::vask::_answer $id deny ""]
        # A single-select multi-question ask has no entry of its own;
        # keep the number keys answering per question: the i-th choice
        # of the FIRST question answers instantly when only one
        # question exists (legacy), else digits select within groups.
    } elseif {[llength $questions] == 1 && [llength [dict get [lindex $questions 0] choices]] > 0} {
        set q [lindex $questions 0]
        set i 0
        foreach c [dict get $q choices] {
            set key ""
            catch { set key [dict get $c key] }
            incr i
            # 1..4 answer straight from the keyboard (legacy) — but not
            # while the note box has the focus: the toplevel's binding
            # fired for a digit TYPED INTO THE NOTE, so "keep step 2"
            # answered choice 2 on the keystroke.
            bind $w <Key-$i> [list ::vask::_digit $id $key $w.btns.e %W]
        }
        # The note box. Its text rides with whichever choice is picked
        # (the rows above read it), and the engine hands it to the
        # model: an approval's note, a plan's revision notes, and the
        # S10 rewrite when it starts with "amend:". Enter sends the note
        # ALONE — no choice — and the engine decides what a bare note
        # means (on an approval: declined with that note; on a plan:
        # keep planning with it; on a question: the answer). It used to
        # send choice 1 with it, which approved a plan the note was
        # written to change.
        ttk::label $w.btns.l -text [expr {$kind eq "clarify"
            ? [::vi18n::mc {Or type an answer:}]
            : [::vi18n::mc {Note for the agent (optional):}]}]
        pack $w.btns.l -side left -padx {0 4}
        ttk::entry $w.btns.e
        pack $w.btns.e -side left -fill x -expand 1
        # Deferred, NOT `[set w.btns.e]`: that substitution runs AT BIND
        # TIME (inside the quotes-as-list), reads the unset variable
        # `w.btns.e`, throws TCL READ VARNAME, aborts ::vask::show and
        # leaves a half-built dialog with buttonless text — the exact
        # crash a failover ask triggered. The entry's contents must be
        # read when the key is PRESSED, not when the bind is built.
        bind $w.btns.e <Return> [list ::vask::_answer_note $id $w.btns.e]
    } else {
        ttk::entry $w.btns.e
        pack $w.btns.e -side left -fill x -expand 1
        ttk::button $w.btns.send -text [::vi18n::mc {Send}] \
            -command [list ::vask::_answer_entry $id $w.btns.e]
        pack $w.btns.send -side left -padx 4
        bind $w.btns.e <Return> [list ::vask::_answer_entry $id $w.btns.e]
        focus $w.btns.e
    }

    # Size the canvas to the content, capped at 70% of the screen; the
    # scrollbar appears only when the stacked content overflows.
    update idletasks
    set want [winfo reqheight $w.body.c.inner]
    set chrome [expr {max(0, [winfo reqheight $w] - [winfo height $w.body.c])}]
    set avail [expr {$cap - $chrome}]
    if {$want > $avail && $avail > 40} {
        $w.body.c configure -height $avail
        pack $w.body.vsb -side right -fill y -before $w.body.c
        bind $w.body.c <MouseWheel> { %W yview scroll [expr {-%D/120}] units }
    } else {
        $w.body.c configure -height $want
    }

    bind $w <Escape> [list ::vask::_answer $id deny ""]
    raise $w
    focus -force $w
    # a11y: explicit Tab cycle across the dialog's interactive widgets
    # (choice rows → Other entries → amend entry/OK), so keyboard-only
    # users can reach everything without guessing creation order. In
    # toggle mode the Other entries and check rows all cycle.
    set cycle {}
    if {$any_toggle || $multi_q} {
        foreach kid [winfo children $w.body.c.inner] {
            set nm [winfo name $kid]
            if {[string match "ch*" $nm]} {
                lappend cycle $kid
            }
            if {[string match "o*" $nm] && [winfo exists $kid.e]} {
                lappend cycle $kid.e
            }
        }
        lappend cycle $w.btns.ok
    } elseif {[llength $choices] > 0} {
        for {set i 1} {$i <= [llength $choices]} {incr i} {
            lappend cycle $w.body.c.inner.ch$i
        }
        lappend cycle $w.btns.e
    } else {
        lappend cycle $w.btns.e $w.btns.send
    }
    ::va11y::tab_order $cycle
    # Modal: the turn cannot proceed anyway, and a stray click in the
    # transcript must not lose the question.
    catch { grab set $w }
}

# Keep the scrollregion honest as the content grows or wraps.
proc ::vask::_scrolled {w} {
    set c $w.body.c
    set inner $c.inner
    catch {
        $c configure -scrollregion \
            [list 0 0 [winfo width $inner] [winfo reqheight $inner]]
    }
}

# Detail-text scroll plumbing: the scrollbar is only VISIBLE when the
# content overflows, and the wheel scrolls the text (never the dialog
# canvas while the pointer is over the detail — the two fighting was
# part of plan-mode fix 1).
proc ::vask::_detail_scroll {vsb first last} {
    catch { $vsb set $first $last }
    if {$first <= 0.0 && $last >= 1.0} {
        catch { pack forget $vsb }
    } else {
        catch { pack $vsb -side right -fill y -before [winfo parent $vsb].detail }
    }
}

proc ::vask::_detail_wheel {txt D b} {
    # MouseWheel carries %D, X11 buttons carry %b (4 = up, 5 = down).
    if {$b eq "4"} {
        catch { $txt yview scroll -3 units }
        return break
    }
    if {$b eq "5"} {
        catch { $txt yview scroll 3 units }
        return break
    }
    catch { $txt yview scroll [expr {-$D / 120}] units }
    return break
}

# The banner's button: put the asking session on screen so the question
# can be read in context, and let go of the grab so it can actually be
# scrolled. The dialog stays — going to look at something is not an
# answer, and the question must still be there when you come back.
proc ::vask::_goto {sid} {
    catch { grab release .ask }
    if {[catch { ::vchat::open_session $sid } goto_err]} {
        puts stderr "veles-desktop: ask goto session $sid failed: $goto_err"
        ::vstatus::set_msg [::vi18n::mc {session switch failed}]
    }
}

proc ::vask::_hover {f on} {
    catch {
        $f configure -background \
            [expr {$on ? $::vt::T(bg_input) : $::vt::T(bg)}]
    }
}

proc ::vask::_answer_entry {id entry} {
    set txt ""
    catch { set txt [$entry get] }
    ::vask::_answer $id "" $txt
}

# Choice + amend path: answer with the CHOICE key AND the free text
# (the approval amend). Split from _answer_entry so the bind can be a
# deferred [list] — see the comment at the Return bind above.
proc ::vask::_answer_key_entry {id choice entry} {
    set txt ""
    catch { set txt [$entry get] }
    ::vask::_answer $id $choice [string trim $txt]
}

# Enter in the note box: the note alone, no choice. An EMPTY box answers
# nothing — Enter there used to mean "choice 1", and a stray Enter must
# not now mean "declined" instead.
proc ::vask::_answer_note {id entry} {
    set txt ""
    catch { set txt [string trim [$entry get]] }
    if {$txt eq ""} {
        bell
        return
    }
    ::vask::_answer $id "" $txt
}

# A digit pressed anywhere in the dialog answers that choice (with the
# note) — unless it was typed into a text field, where it is text.
proc ::vask::_digit {id choice entry target} {
    set cls ""
    catch { set cls [winfo class $target] }
    if {$cls in {TEntry Entry Text}} {
        return
    }
    ::vask::_answer_key_entry $id $choice $entry
}

# A single multi-select question answered with several checked rows:
# POST the bare-list answers shape and log it.
proc ::vask::_answer_multi {id sel} {
    variable current_sid
    variable current
    if {[winfo exists .ask]} {
        catch { grab release .ask }
        destroy .ask
    }
    set sid $current_sid
    if {$current eq $id} { set current "" ; set current_sid "" }
    if {[catch {::vapi::ask_decide_multi $id $sel} err]} {
        ::vstatus::set_msg [format [::vi18n::mc {answer failed: %s}] $err]
        return
    }
    ::vchat::log_insert "[::vi18n::mc {  ⟶ answered: %s} [join $sel +]]\n" tool $sid
}

# Toggle a check row: flip membership in ::vask::checked($qk) and the
# glyph (☐/☑). A single-select question still toggles like this only in
# multi-question asks; in a single-question ask its rows are instant.
proc ::vask::_toggle {id qk key box {radio 0}} {
    variable checked
    set cur {}
    catch { set cur [dict get $checked $qk] }
    set ix [lsearch -exact $cur $key]
    if {$ix >= 0} {
        set cur [lreplace $cur $ix $ix]
        catch { $box configure -text "☐" }
    } else {
        if {$radio} {
            # Single-select question: checking a row unchecks its
            # siblings (their glyphs live in the dialog's row frames).
            set cur [list $key]
            catch {
                # Sibling rows are the dialog's ch${qk}_N frames; walk
                # children so no separate registry is needed.
                foreach kid [winfo children .ask.body.c.inner] {
                    if {[string match "ch${qk}_*" [winfo name $kid]]
                        && [winfo exists $kid.box]} {
                        $kid.box configure -text "☐"
                    }
                }
            }
            catch { $box configure -text "☑" }
        } else {
            lappend cur $key
            catch { $box configure -text "☑" }
        }
    }
    dict set checked $qk $cur
}

# OK: assemble the whole answer from checked rows + Other entries and
# POST it as the `answers` payload (list for one question, per-question
# objects for several). Refuses (status line) when nothing is checked
# and no Other entry has text — an accidental OK must not answer.
proc ::vask::_commit {id} {
    variable checked
    variable current_sid
    set w .ask
    set answers {}
    set any 0
    # Which questions this dialog holds (rebuilt from the row widgets:
    # the o$qk entries and ch${qk}_N rows are the source of truth).
    set qks {}
    foreach kid [winfo children $w.body.c.inner] {
        if {[regexp {^o(.+)$} [winfo name $kid] -> qk]} {
            lappend qks $qk
        }
    }
    foreach qk $qks {
        set sel {}
        catch { set sel [dict get $checked $qk] }
        set txt ""
        if {[catch { set txt [string trim [$w.body.c.inner.o$qk.e get]] } read_err]} {
            puts stderr "veles-desktop: ask _commit: could not read answer text for $qk: $read_err"
        }
        if {[llength $sel] > 0 || $txt ne ""} {
            set any 1
            lappend answers $qk [list \
                selected $sel \
                text $txt]
        }
    }
    if {!$any} {
        ::vstatus::set_msg [::vi18n::mc {nothing selected — check a row or type an answer}]
        return
    }
    # A ONE-QUESTION dialog with ONE checked row: the plain
    # choice+text decide (the legacy wire shape) rather than the
    # answers object — a single-selection multi-select dialog is
    # answered exactly like a single-choice one. Keyed on the questions
    # the DIALOG holds, not on how many were answered: one answer out
    # of three is still that question's answer, and a bare choice is
    # filed under q1.
    set one_q [expr {[llength $qks] == 1}]
    if {$one_q && [llength $answers] == 2
        && [llength [dict get [lindex $answers 1] selected]] == 1
    } then {
        set a [lindex $answers 1]
        ::vask::_answer $id [lindex [dict get $a selected] 0] [dict get $a text]
        return
    }
    # ONE question, several checked or text: answers as a ONE-KEY
    # object is fine — but the single-question answer shape is a bare
    # LIST on the wire, so the engine's top-level selected carries it.
    if {$one_q && [llength $answers] == 2} {
        set a [lindex $answers 1]
        set sel [dict get $a selected]
        set txt [string trim [dict get $a text]]
        if {$txt eq ""} {
            ::vask::_answer_multi $id $sel
            return
        }
    }
    if {[winfo exists $w]} {
        catch { grab release $w }
        destroy $w
    }
    set sid $current_sid
    variable current
    if {$current eq $id} { set current "" ; set current_sid "" }
    if {[catch {::vapi::ask_decide_answers $id $answers} err]} {
        ::vstatus::set_msg [format [::vi18n::mc {answer failed: %s}] $err]
        return
    }
    # The shown line: the checked labels would be nicer, but keys are
    # honest and short; the transcript already shows the question.
    set shown {}
    foreach {qk a} $answers {
        set s [dict get $a selected]
        set t [dict get $a text]
        if {[llength $s] > 0} { lappend shown [join $s +] }
        if {$t ne ""} { lappend shown $t }
    }
    ::vchat::log_insert "[::vi18n::mc {  ⟶ answered: %s} [join $shown {, }]]\n" tool $sid
}

# C-g / Escape on the dialog: refuse to answer. For approvals this is
# a DENY (the safe default — an unanswered question denies at timeout
# anyway); for clarify it sends an empty free-form reply.
proc ::vask::cancel {w} {
    variable current
    set id $current
    if {$id eq ""} { destroy $w; return }
    ::vask::_answer $id deny ""
}

# `ask-resolved` SSE: this ask was decided elsewhere (another window,
# telegram, REST) or the turn was cancelled. Close the dialog WITHOUT
# sending an answer of our own — the answer already exists, and a
# second decide POST would race the resolved id (the engine treats it
# as unknown and the human's "answered: X" line would lie).
proc ::vask::resolved {d} {
    variable current
    variable current_sid
    if {[catch {dict get $d id} id]} {
        puts stderr "veles-desktop: ask resolved: no id in event, dropped: $d"
        return
    }
    if {$current ne $id} return
    if {[winfo exists .ask]} {
        catch { grab release .ask }
        destroy .ask
    }
    set current ""
    # The ask's own session, captured while the dialog was up: these
    # events carry only the id, and the line belongs in the transcript
    # that asked — not in whichever one happens to be on screen when
    # somebody else answers.
    set sid $current_sid
    set current_sid ""
    set by ""
    catch { set by [dict get $d by] }
    if {$by eq "cancel"} {
        ::vchat::log_insert "[::vi18n::mc {  ⟶ question withdrawn (turn cancelled)}]\n" system $sid
    } else {
        set shown ""
        catch { set shown [dict get $d answer] }
        ::vchat::log_insert "[::vi18n::mc {  ⟶ answered elsewhere: %s} $shown]\n" system $sid
    }
}

# The question has been sent on to the owner's other surface because
# nobody answered it here.
#
# NOT SILENT, on purpose. A question quietly forwarded to somebody's
# phone is the kind of automation this app refuses to do behind your
# back — and without this line the first you hear of it is a foreign
# "answered elsewhere" for a question you were still looking at. The
# dialog STAYS OPEN: the relay adds a place to answer, it does not move
# the question, and whichever human answers first closes both.
proc ::vask::relayed {d} {
    variable current
    variable current_sid
    set id ""
    catch { set id [dict get $d id] }
    if {$id eq "" || $current ne $id} { return }
    set to ""
    catch { set to [dict get $d to] }
    set secs ""
    catch { set secs [dict get $d after_secs] }
    set where [expr {$to ne "" ? $to : [::vi18n::mc {your other surfaces}]}]
    if {$secs ne ""} {
        ::vchat::log_insert \
            "  ⟶ [format [::vi18n::mc {unanswered for %1$ss — also asked on %2$s}] $secs $where]\n" system $current_sid
    } else {
        ::vchat::log_insert \
            "  ⟶ [format [::vi18n::mc {also asked on %s}] $where]\n" system $current_sid
    }
}

# ── secrets ─────────────────────────────────────────────────────────
#
# One dialog for two doors: the agent's `secret_request` (an ask, `id`
# set) and the operator's own `/secret set NAME` (no ask). The value is
# POSTed to /api/secrets/set — loopback-only on the gateway — and never
# reaches the ask's answer, the transcript, the status bar or a log: the
# answer is "set" or "decline", and the transcript says "stored $NAME".
# A gateway that does not take values from this machine (not loopback,
# not in its [vault] trusted_networks) gets no field at all, only the
# line that says where to type it instead.

proc ::vask::secret_dialog {name purpose {id ""} {sid ""}} {
    variable current
    variable current_sid
    variable secret_value
    set w .ask
    if {[winfo exists $w]} { destroy $w }
    set current $id
    set current_sid $sid
    set secret_value ""
    set name [string trimleft $name {$}]
    toplevel $w
    wm title $w [::vi18n::mc {Secret needed}]
    wm transient $w .
    wm protocol $w WM_DELETE_WINDOW [list ::vask::_secret_close $id]
    wm minsize $w 520 120
    ttk::label $w.title -wraplength 500 -justify left -font {TkDefaultFont 12 bold}
    if {$id ne ""} {
        ::vi18n::label $w.title {The agent asks for a secret: %s} $name
    } else {
        ::vi18n::label $w.title {Store a secret: %s} $name
    }
    pack $w.title -side top -fill x -padx 12 -pady {12 4}
    if {[string trim $purpose] ne ""} {
        ttk::label $w.purpose -text $purpose -wraplength 500 -justify left
        pack $w.purpose -side top -fill x -padx 12 -pady 2
    }
    ttk::label $w.where -wraplength 500 -justify left -foreground $::vt::T(fg_dim)
    ttk::frame $w.btns
    if {[::vapi::secrets_accepted]} {
        ttk::entry $w.value -show • -width 50 -textvariable ::vask::secret_value
        pack $w.value -side top -fill x -padx 12 -pady 6
        ::va11y::name $w.value [::vi18n::mc {secret value, hidden while typed}]
        ::vi18n::label $w.where {It goes into the encrypted vault on this machine. The agent never sees it: it can only use it as $%s in commands, or send it to the hosts it is allowed.} $name
        ttk::button $w.btns.ok -command [list ::vask::_secret_store $name $id]
        ::vi18n::label $w.btns.ok {Store it}
        bind $w.value <Return> [list ::vask::_secret_store $name $id]
        focus $w.value
    } else {
        ::vi18n::label $w.where {This gateway does not take secret values from this machine: enter it there with veles secret set %s, or list this network in its [vault] trusted_networks} $name
        ttk::button $w.btns.ok -command [list ::vask::_secret_done $id]
        ::vi18n::label $w.btns.ok {Done}
    }
    pack $w.where -side top -fill x -padx 12 -pady {2 6}
    ttk::label $w.err -wraplength 500 -justify left -foreground $::vt::T(pink)
    pack $w.err -side top -fill x -padx 12
    ttk::button $w.btns.no -command [list ::vask::_secret_close $id]
    if {$id ne ""} {
        ::vi18n::label $w.btns.no {Decline}
    } else {
        ::vi18n::label $w.btns.no {Cancel}
    }
    pack $w.btns -side bottom -fill x -padx 12 -pady {6 12}
    pack $w.btns.ok $w.btns.no -side right -padx {6 0}
    bind $w <Escape> [list ::vask::_secret_close $id]
    catch { grab $w }
}

proc ::vask::_secret_store {name id} {
    variable secret_value
    set w .ask
    if {$secret_value eq ""} {
        catch { ::vi18n::label $w.err {Nothing typed yet.} }
        return
    }
    set rc [catch { ::vapi::secret_set $name $secret_value $id } err]
    # Out of this process's memory whatever happened: a refusal asks for
    # it again rather than keeping it.
    set secret_value ""
    if {$rc} {
        if {[catch { ::vi18n::label $w.err {not stored: %s} [::vapi::reason $err] } lbl_err]} {
            # The label write itself failed (dialog destroyed by a racing
            # ask-resolved). The error still goes to stderr.
            puts stderr "veles-desktop: secret $name not stored: $err (label write failed: $lbl_err)"
        }
        return
    }
    if {$id ne ""} {
        ::vask::_answer $id set ""
    } else {
        catch { grab release $w }
        destroy $w
    }
    ::vchat::log_insert "[::vi18n::mc {  ⟶ stored $%s in the vault} $name]\n" tool
}

# A gateway that takes no values from here: the operator says it was
# entered there.
proc ::vask::_secret_done {id} {
    variable secret_value
    set secret_value ""
    if {$id ne ""} {
        ::vask::_answer $id set ""
    } elseif {[winfo exists .ask]} {
        catch { grab release .ask }
        destroy .ask
    }
}

proc ::vask::_secret_close {id} {
    variable secret_value
    set secret_value ""
    if {$id ne ""} {
        ::vask::_answer $id decline ""
    } elseif {[winfo exists .ask]} {
        catch { grab release .ask }
        destroy .ask
    }
}

# ── tickets ─────────────────────────────────────────────────────────
#
# A call homed on another node needs a ticket (PLAN-pool-round.md D3).
# The ask's detail describes the exact call — its arguments in full — and
# ends with the request token. The answer is the TICKET, as free text:
# signed here by `veles ticket sign`, a child that reads the passphrase on
# its stdin (Tcl has no Ed25519, and a gateway never holds a key or a
# passphrase), or pasted after signing it on another device. Any trusted
# key signs, wherever it lives (ticket.rs).

# The request token inside an ask's detail.
proc ::vask::ticket_request {detail} {
    if {[regexp {veles-ticket-request:1\.[A-Za-z0-9_-]+} $detail m]} { return $m }
    return ""
}

# The signing keys on this machine, by name: `<name>.key` in the key
# directory `veles ticket keygen` writes (ticket::key_dir — beside veles'
# config directory, never inside VELES_HOME).
proc ::vask::ticket_keys {} {
    if {[info exists ::env(VELES_HOME)] && $::env(VELES_HOME) ne ""} {
        set dir [file join [file dirname $::env(VELES_HOME)] veles-signer]
    } elseif {[info exists ::env(XDG_CONFIG_HOME)] && $::env(XDG_CONFIG_HOME) ne ""} {
        set dir [file join $::env(XDG_CONFIG_HOME) veles-signer]
    } else {
        set dir [file join [file normalize ~] .config veles-signer]
    }
    set out {}
    foreach f [lsort [glob -nocomplain -directory $dir -types f *.key]] {
        lappend out [file rootname [file tail $f]]
    }
    return $out
}

# The `veles` that signs: the binary that launched this window
# (VELES_EXE, set by `veles desktop`), else the first on PATH. PATH alone
# found an installed build a week older than the running one, which has
# no `ticket` subcommand — so the dialog offered to sign and every
# signature failed.
proc ::vask::_veles_exe {} {
    if {[info exists ::env(VELES_EXE)] && [file executable $::env(VELES_EXE)]} {
        return $::env(VELES_EXE)
    }
    return [lindex [auto_execok veles] 0]
}

proc ::vask::ticket_dialog {prompt detail id sid} {
    variable current
    variable current_sid
    variable ticket_pass
    variable ticket_key
    variable ticket_paste
    set w .ask
    if {[winfo exists $w]} { destroy $w }
    set current $id
    set current_sid $sid
    set ticket_pass ""
    set ticket_paste ""
    set keys [::vask::ticket_keys]
    set ticket_key [lindex $keys 0]
    toplevel $w
    wm title $w [::vi18n::mc {Ticket needed}]
    wm transient $w .
    wm protocol $w WM_DELETE_WINDOW [list ::vask::_ticket_close $id]
    wm minsize $w 560 160
    ttk::label $w.title -text $prompt -wraplength 540 -justify left \
        -font {TkDefaultFont 12 bold}
    pack $w.title -side top -fill x -padx 12 -pady {12 4}
    # The call in full with the request at its end: read-only but
    # selectable, so it can be copied to a device that signs.
    text $w.detail -height 10 -width 72 -wrap char -relief flat \
        -background $::vt::T(bg) -foreground $::vt::T(fg)
    $w.detail insert end $detail
    $w.detail configure -state disabled
    pack $w.detail -side top -fill both -expand 1 -padx 12 -pady 4
    set req [::vask::ticket_request $detail]
    set exe [::vask::_veles_exe]
    if {$req ne "" && $exe ne "" && [llength $keys]} {
        ttk::frame $w.sign
        ttk::label $w.sign.kl
        ::vi18n::label $w.sign.kl {Signing key}
        ttk::combobox $w.sign.key -values $keys -textvariable ::vask::ticket_key \
            -state readonly -width 16
        ttk::entry $w.sign.pass -show • -width 28 -textvariable ::vask::ticket_pass
        ::va11y::name $w.sign.pass [::vi18n::mc {passphrase for the signing key, hidden while typed}]
        ttk::button $w.sign.go -command [list ::vask::_ticket_sign $id $req]
        ::vi18n::label $w.sign.go {Sign}
        pack $w.sign.kl $w.sign.key $w.sign.pass $w.sign.go -side left -padx {0 6}
        pack $w.sign -side top -fill x -padx 12 -pady 6
        bind $w.sign.pass <Return> [list ::vask::_ticket_sign $id $req]
        focus $w.sign.pass
    }
    ttk::label $w.pl
    ::vi18n::label $w.pl {Or paste a ticket signed on another device}
    pack $w.pl -side top -fill x -padx 12 -pady {6 0}
    ttk::entry $w.paste -textvariable ::vask::ticket_paste
    pack $w.paste -side top -fill x -padx 12 -pady 2
    bind $w.paste <Return> [list ::vask::_ticket_use $id]
    ttk::label $w.err -wraplength 540 -justify left -foreground $::vt::T(pink)
    pack $w.err -side top -fill x -padx 12
    ttk::frame $w.btns
    ttk::button $w.btns.use -command [list ::vask::_ticket_use $id]
    ::vi18n::label $w.btns.use {Use this ticket}
    ttk::button $w.btns.no -command [list ::vask::_ticket_close $id]
    ::vi18n::label $w.btns.no {Decline}
    pack $w.btns -side bottom -fill x -padx 12 -pady {6 12}
    pack $w.btns.use $w.btns.no -side right -padx {6 0}
    bind $w <Escape> [list ::vask::_ticket_close $id]
    catch { grab $w }
}

# Sign the request with a key on this machine: `veles ticket sign`, the
# passphrase on its stdin (argv would show it to `ps`), and out of this
# process's memory before the child's answer is read.
proc ::vask::_ticket_sign {id req} {
    variable ticket_pass
    variable ticket_key
    set w .ask
    set exe [::vask::_veles_exe]
    set words [list ticket sign [::vsafe::arg $req] --yes]
    if {$ticket_key ne ""} { lappend words --key [::vsafe::arg $ticket_key] }
    set pass $ticket_pass
    set ticket_pass ""
    catch { exec $exe {*}$words << "$pass\n" 2>@1 } out
    set pass ""
    if {[regexp {veles-ticket:1\.[A-Za-z0-9_.-]+} $out t]} {
        ::vask::_answer $id "" $t
        return
    }
    set why [::vask::_ticket_why $out]
    if {[catch { ::vi18n::label $w.err {not signed: %s} $why }]} {
        puts stderr "veles-desktop: ticket not signed: $why"
    }
}

# The signer child's own last line. `exec` APPENDS its own status line
# when a child exits non-zero — "child process exited abnormally" — and
# with `2>@1` that lands after the child's stderr in the same result. So
# taking the last line verbatim showed the reader a Tcl status word
# instead of the signer's reason ("the passphrase does not open this
# key"). Measured on tclsh 8.6.16, not assumed: `exec sh -c {echo E >&2;
# exit 1} 2>@1` returns "E\nchild process exited abnormally".
proc ::vask::_ticket_why {out} {
    set keep {}
    foreach l [split $out "\n"] {
        set l [string trim $l]
        if {$l eq ""} { continue }
        if {[regexp {^child (process exited|killed|suspended)} $l]} { continue }
        lappend keep $l
    }
    if {![llength $keep]} { return "the signer child failed" }
    return [lindex $keep end]
}

proc ::vask::_ticket_use {id} {
    variable ticket_paste
    set w .ask
    if {[regexp {veles-ticket:1\.[A-Za-z0-9_.-]+} $ticket_paste t]} {
        set ticket_paste ""
        ::vask::_answer $id "" $t
        return
    }
    catch { ::vi18n::label $w.err {That is not a ticket.} }
}

# ── bot challenges (PLAN-pool-round.md Part E) ──────────────────────
#
# USER 2026-09-29: "Any captcha challenge or similar that is shown should
# be shown to the user so that the user can solve it for the agent." The
# engine sends the page as a screenshot; a click on the picture answers
# `click X Y` in the page's own pixels (the picture may be shrunk to fit
# this screen — the scale is undone here), typing and keys go the same
# way, and Solved / Give up / Open a window are the verdicts. One answer
# is one round: the relay looks again and asks again with a fresh
# screenshot until the page is clear.
proc ::vask::challenge_dialog {prompt detail id sid image choices} {
    variable current
    variable current_sid
    variable challenge_text
    variable challenge_img
    variable challenge_scale
    set w .ask
    if {[winfo exists $w]} { destroy $w }
    # Tk images are not collected with their widget: last round's goes.
    if {[info exists challenge_img] && $challenge_img ne ""} {
        catch { image delete $challenge_img }
    }
    set challenge_img ""
    set challenge_scale 1
    set current $id
    set current_sid $sid
    set challenge_text ""
    toplevel $w
    wm title $w [::vi18n::mc {Solve this for the agent}]
    wm transient $w .
    wm protocol $w WM_DELETE_WINDOW [list ::vask::_answer $id give_up ""]
    ttk::label $w.title -text $prompt -wraplength 900 -justify left \
        -font {TkDefaultFont 12 bold}
    pack $w.title -side top -fill x -padx 12 -pady {12 2}
    ttk::label $w.url -text [lindex [split $detail "\n"] 0] -foreground $::vt::T(fg_dim)
    pack $w.url -side top -fill x -padx 12 -pady {0 4}
    if {$image ne ""} {
        if {[catch { image create photo -data $image -format png } src]} {
            puts stderr "veles-desktop: challenge image decode failed: $src"
        } else {
        # Shrink to fit this screen by a whole factor (Tk's subsample),
        # and remember it: a click is sent in the PAGE's pixels.
        set maxw [expr {max(320, [winfo screenwidth .] - 120)}]
        set maxh [expr {max(240, [winfo screenheight .] - 280)}]
        set scale 1
        while {[image width $src] / $scale > $maxw || [image height $src] / $scale > $maxh} {
            incr scale
        }
        if {$scale > 1} {
            set img [image create photo]
            $img copy $src -subsample $scale $scale
            image delete $src
        } else {
            set img $src
        }
        set challenge_img $img
        set challenge_scale $scale
        label $w.shot -image $img -cursor crosshair -borderwidth 0
        pack $w.shot -side top -padx 12 -pady 4
        bind $w.shot <Button-1> [list ::vask::_challenge_click $id %x %y]
        }
    }
    ttk::label $w.hint -wraplength 900 -justify left -foreground $::vt::T(fg_dim)
    ::vi18n::label $w.hint {Click the picture where you would click on the page.}
    pack $w.hint -side top -fill x -padx 12 -pady 2
    ttk::frame $w.type
    ttk::entry $w.type.e -textvariable ::vask::challenge_text -width 40
    ttk::button $w.type.go -command [list ::vask::_challenge_type $id]
    ::vi18n::label $w.type.go {Type}
    ttk::button $w.type.enter -text "Enter" -command [list ::vask::_answer $id "" "key Enter"] ;# i18n-exempt: a key's name
    ttk::button $w.type.tab -text "Tab" -command [list ::vask::_answer $id "" "key Tab"] ;# i18n-exempt: a key's name
    pack $w.type.e -side left -fill x -expand 1
    pack $w.type.go $w.type.enter $w.type.tab -side left -padx {6 0}
    pack $w.type -side top -fill x -padx 12 -pady 4
    bind $w.type.e <Return> [list ::vask::_challenge_type $id]
    ttk::frame $w.btns
    ttk::button $w.btns.solved -command [list ::vask::_answer $id solved ""]
    ::vi18n::label $w.btns.solved {Solved}
    ttk::button $w.btns.give -command [list ::vask::_answer $id give_up ""]
    ::vi18n::label $w.btns.give {Give up}
    pack $w.btns.solved $w.btns.give -side right -padx {6 0}
    foreach c $choices {
        if {![catch { dict get $c key } k] && $k eq "window"} {
            ttk::button $w.btns.win -command [list ::vask::_answer $id window ""]
            ::vi18n::label $w.btns.win {Open a window}
            pack $w.btns.win -side left
        }
    }
    pack $w.btns -side bottom -fill x -padx 12 -pady {4 12}
}

proc ::vask::_challenge_click {id x y} {
    variable challenge_scale
    set s [expr {[info exists challenge_scale] ? $challenge_scale : 1}]
    ::vask::_answer $id "" "click [expr {$x * $s}] [expr {$y * $s}]"
}

proc ::vask::_challenge_type {id} {
    variable challenge_text
    set t [string trim $challenge_text]
    if {$t eq ""} { return }
    set challenge_text ""
    ::vask::_answer $id "" "type $t"
}

proc ::vask::_ticket_close {id} {
    variable ticket_pass
    set ticket_pass ""
    ::vask::_answer $id decline ""
}

proc ::vask::_answer {id choice text} {
    variable current
    variable current_sid
    if {[winfo exists .ask]} {
        catch { grab release .ask }
        destroy .ask
    }
    # Captured BEFORE the dialog state is cleared: the answer belongs in
    # the transcript that asked the question, which is not necessarily
    # the one on screen when the button is pressed.
    set sid $current_sid
    if {$current eq $id} { set current "" ; set current_sid "" }
    if {[catch {::vapi::ask_decide $id $choice $text} err]} {
        ::vstatus::set_msg [format [::vi18n::mc {answer failed: %s}] $err]
        return
    }
    set shown [expr {$choice ne "" ? $choice : $text}]
    if {$choice ne "" && $text ne ""} {
        set shown "$choice — $text"
    }
    ::vchat::log_insert "[::vi18n::mc {  ⟶ answered: %s} $shown]\n" tool $sid
}
