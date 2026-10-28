# veles-desktop/lib/statusbar.tcl — bottom status bar.
# Shows: gateway health · current session · streaming state · usage.
# The leading dot is a neon health light (green = gateway up,
# red = unreachable).

namespace eval ::vstatus {
    # Every status line ever shown, newest last. The bar itself holds
    # exactly one message and overwrites it, so anything you glanced away
    # from was gone forever — this keeps the last 200 for the Log panel.
    variable history {}
    variable history_cap 200
    variable attachment ""
    variable plan_mode 0
    variable poll_after ""
    # The provider half of the current route (set by set_route), which
    # _paint_quota reads to pick the plan pool this session draws from.
    variable route_prov ""
    ;# The last GOOD context reading, kept across polls: mid-turn the
    ;# endpoint answers busy:true (a session_tap snapshot exists) and
    ;# blanking the segment every 15s made the bar flicker empty exactly
    ;# when you are watching a long turn eat window. Greying it and
    ;# saying why beats nothing. Both belong to last_ctx_sid — a session
    ;# switch must not show the PREVIOUS session's retained reading (the
    ;# exact staleness complained of).
    ;#
    ;# CONTRACT: busy is present on EVERY answer (desktop_api pins this
    ;# in a_turn_from_another_surface_reads_busy_here); it means "a turn
    ;# is live", NOT "the agent lock is held". Compaction holds the lock
    ;# yet answers busy:false with real figures from the store. An
    ;# ABSENT key (older gateway) reads as busy:false — the figures, if
    ;# any, render. The `history-changed` bus event (not any key)
    ;# announces transcript changes.
    variable last_ctx_line ""
    variable last_ctx {}
    variable last_ctx_sid ""
    ;# Throttle for the per-step context refresh (route/tool events), so a
    ;# tool-heavy turn does not turn the bar into an HTTP flood. Turn-end
    ;# refresh_context is called directly, unthrottled.
    variable last_step_refresh 0
    ;# In-flight right-side/status reads. No HTTP helper is called from
    ;# inside another read's nested vwait; replies are generations.
    variable painting 0
    variable paint_again 0
    variable paint_after ""
    variable paint_generation 0
    variable paint_pending
    array set paint_pending {}
    variable context_generation 0
    variable quota_generation 0
    variable status_generation 0
}

proc ::vstatus::set_msg {msg} {
    variable history
    variable history_cap
    set stamp [clock format [clock seconds] -format %H:%M:%S]
    lappend history [list $stamp $msg]
    if {[llength $history] > $history_cap} {
        set history [lrange $history end-[expr {$history_cap - 1}] end]
    }
    catch { .statusbar.msg configure -text $msg }
}

proc ::vstatus::history {} {
    variable history
    return $history
}

proc ::vstatus::set_health {ok} {
    variable T
    catch {
        .statusbar.dot configure -foreground \
            [expr {$ok ? $::vt::T(green) : $::vt::T(red)}]
    }
}

proc ::vstatus::build {} {
    ttk::frame .statusbar
    ttk::label .statusbar.dot -text "●" -foreground $::vt::T(fg_dim)
    ttk::label .statusbar.msg -text [::vi18n::mc {connecting…}] -foreground $::vt::T(fg_dim)
    ttk::label .statusbar.right -text "" -foreground $::vt::T(fg_dim)
    # The route gets its OWN label rather than riding ::vstatus::set_msg.
    #
    # USER: "during the turn the status bar only seems to show provider,
    # should be full provider->model chain" and "it's hard to see what
    # the model selected is before first prompt".
    #
    # set_msg is transient — ::vstatus::poll overwrites it with
    # "gateway up Ns" every 15 seconds — so anything written there is
    # gone before you look at it. This label is only ever set by the
    # route, so it survives.
    ttk::label .statusbar.route -text "" -foreground $::vt::T(accent)
    # The UPSTREAM plan, which .statusbar.right is not: that one counts
    # what we spent, this one is what the account says is left. Painted
    # from ::vstatus::poll (15s) and NEVER from _paint_right, which also
    # fires per provider round and per turn end — /api/quota is a local
    # cache read, but putting any foreign figure on that path is how it
    # becomes a Singapore round trip the first time somebody "fixes" the
    # endpoint to fetch.
    ttk::label .statusbar.quota -text "" -foreground $::vt::T(fg_dim)
    # The language chip: the "dedicated button to swap" (USER ask
    # 2026-09-20). Packed with the fixed-width right group, so a long
    # status message can never squeeze it off the bar (the lesson
    # .statusbar.quota already carries above). Click = swap primary ⇄
    # secondary; the picker is the right-click.
    ttk::label .statusbar.lang -text "" -foreground $::vt::T(fg_dim)
    # PACKING ORDER IS ALLOCATION ORDER, and that is the whole point of
    # this arrangement. Tk hands each slave a slab out of the remaining
    # cavity in the order they were packed, so whoever is packed first
    # gets the width it asks for and the rest divide what is left.
    #
    # .statusbar.msg used to come second, ahead of the right-hand group,
    # and it is the one label here whose content is unbounded — a stream
    # error, a failover line, an accessibility hint. Measured at 700px
    # wide with a 90-character message: msg grew 85px -> 582px, .right
    # was squeezed 95 -> 56 and slid right, and .route and .quota went
    # UNMAPPED. USER 2026-09-18: hovering "plan 55%" made "plan 55%"
    # leave the bar, which is why it could not be clicked.
    #
    # The right-hand group is fixed-width and load-bearing (the route
    # you are sending to, the plan you are spending, the context you are
    # filling), so it is packed FIRST and always gets its width. The
    # message takes the remainder and is clipped, which is the correct
    # thing to lose.
    pack .statusbar.dot -side left -padx {8 2}
    pack .statusbar.right -side right -padx 8
    pack .statusbar.route -side right -padx {8 12}
    pack .statusbar.quota -side right -padx {0 4}
    pack .statusbar.lang -side right -padx {0 8}
    pack .statusbar.msg -side left -padx 4
    pack .statusbar -side bottom -fill x -padx 8 -pady {0 4}
    # The bar is the doorway to the full log (engine + status history).
    foreach w {.statusbar.dot .statusbar.msg} {
        bind $w <Button-1> { ::voverlay::open log }
        bind $w <Enter> { %W configure -cursor hand2 }
        bind $w <Leave> { %W configure -cursor {} }
    }
    # The usage/context figures open their own detail window — the bar
    # abbreviates (in/out totals), and the FULL context report is what
    # you want when "how full is the window" becomes the question.
    bind .statusbar.right <Button-1> { ::vstatus::show_context }
    catch { ::va11y::name .statusbar.right \
        [::vi18n::mc {tokens in/out · cache hit · context window fill — click for the full report}] }
    catch { ::va11y::name .statusbar.route \
        [::vi18n::mc {the provider/model route this session sends to}] }
    # APPEND (after the tooltip binds va11y::name just installed) —
    # a plain bind here would delete the tip. The `+` is INSIDE the
    # braces: `bind w <E> +{...}` is three words to Tcl and a "wrong #
    # args" at startup, which is exactly what killed the first
    # relaunch of this build.
    bind .statusbar.right <Enter> {+ %W configure -cursor hand2 }
    bind .statusbar.right <Leave> {+ %W configure -cursor {} }
    bind .statusbar.quota <Button-1> { ::voverlay::open usage }
    catch { ::va11y::name .statusbar.quota \
        [::vi18n::mc {how much of the upstream plan is used — click for the full report}] }
    bind .statusbar.quota <Enter> {+ %W configure -cursor hand2 }
    bind .statusbar.quota <Leave> {+ %W configure -cursor {} }

    # The language chip. Left click swaps; right click opens the picker
    # (the ctxmenu convention: the quick action on the button, the full
    # surface one gesture away). Its text is rendered by ::vlang so the
    # chip and the picker cannot disagree about what is active.
    bind .statusbar.lang <Button-1> [list ::vlang::swap_click]
    bind .statusbar.lang <Button-3> [list ::vlang::picker]
    catch { ::va11y::name .statusbar.lang \
        [::vi18n::mc {UI language — click to swap, right-click to choose}] }
    bind .statusbar.lang <Enter> {+ %W configure -cursor hand2 }
    bind .statusbar.lang <Leave> {+ %W configure -cursor {} }
    ::vstatus::refresh_lang
    # The chip must also follow a swap, and its text is NOT catalog text —
    # it is "en ⇄ de", computed from the active locale plus the configured
    # secondary — so the reverse sweep cannot fix it. It stayed stale until
    # this hook existed: the comment below claimed refresh_lang was "called
    # at build and from poll", but grep found ONE caller (the build), so
    # the chip kept showing whatever pair was current when the bar was
    # built. A locale change now re-registers a callback instead.
    ::vi18n::on_locale_change ::vstatus::refresh_lang
}

# Paint the language chip. Called at build and from the locale-change
# hook (::vi18n::run_locale_hooks), which every change of THIS WINDOW's
# language or swap target runs — the picker's buttons, the chip itself,
# /language. (This comment used to promise a poll caller and "/locale on
# another surface"; neither existed, and /locale sets a session's engine
# language, not this window's.)
proc ::vstatus::refresh_lang {} {
    if {![winfo exists .statusbar.lang]} { return }
    set cur [::vi18n::current]
    set sec $::vi18n::secondary
    # "en ⇄ ru" when a swap target is configured, the bare code when
    # there is nowhere to swap to — the chip must not imply a toggle
    # that would refuse.
    set text [expr {$sec eq "" ? $cur : "$cur ⇄ $sec"}]
    .statusbar.lang configure -text $text
}

# The whole context report the engine computed, one window. Reads the
# last fetched report (poll keeps it at most 15s old, session switches
# poll immediately) and re-fetches on open when nothing is cached.
#
# The report is computed from the STORE without taking the agent lock,
# so it is real even mid-turn — busy says who is running, it does not
# suppress the figures (the old !busy gate made this window say "no
# context report yet" forever once busy became unconditional).
# Open first; fetch an uncached report only after the click has returned.
# A cached report is already local and paints immediately. Reopening an
# existing window never starts another request.
proc ::vstatus::show_context {} {
    variable last_ctx
    variable last_ctx_sid
    variable context_generation
    if {[winfo exists .ov_context]} {
        wm deiconify .ov_context
        raise .ov_context
        return
    }
    set sid ""
    catch { set sid $::vchat::session }
    set cached [expr {$sid ne "" && $last_ctx ne "" && $last_ctx_sid eq $sid}]
    _paint_context_window $sid [expr {$cached ? $last_ctx : ""}]
    if {$sid ne "" && !$cached} {
        set generation [incr context_generation]
        if {[catch {
            ::vapi::get_json_async "/api/sessions/$sid/context" \
                [list ::vstatus::_context_answer $sid $generation]
        } err]} {
            after idle [list ::vstatus::_context_answer $sid $generation 0 $err]
        }
    }
}

proc ::vstatus::_context_answer {sid generation ok cx} {
    variable last_ctx
    variable last_ctx_sid
    variable context_generation
    if {$generation != $context_generation || ![winfo exists .ov_context]} {
        return
    }
    set current ""
    catch { set current $::vchat::session }
    if {$current ne $sid} { return }
    if {$ok} {
        set last_ctx $cx
        set last_ctx_sid $sid
    }
    _paint_context_window $sid [expr {$ok ? $cx : ""}]
}

proc ::vstatus::_context_closed {which w} {
    variable context_generation
    if {$which eq $w} { incr context_generation }
}

proc ::vstatus::_paint_context_window {sid cx} {
    if {![winfo exists .ov_context]} {
        toplevel .ov_context
        bind .ov_context <Destroy> \
            [list ::vstatus::_context_closed %W .ov_context]
        wm title .ov_context [::vi18n::mc {Context window}]
        wm geometry .ov_context 560x420
        ::voverlay::_header .ov_context "Context window"
        text .ov_context.body -wrap word -relief flat -padx 10 -pady 8 \
            -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
            -font {monospace 10}
        pack .ov_context.body -fill both -expand 1 -padx 8 -pady {0 8}
    }
    .ov_context.body configure -state normal
    .ov_context.body delete 1.0 end
    if {$sid eq ""} {
        .ov_context.body insert end "no session selected"
    } elseif {$cx eq ""} {
        .ov_context.body insert end "no context report yet"
    } else {
        foreach k [dict keys $cx] {
            set v [dict get $cx $k]
            if {$k eq "prefix_blocks"} {
                .ov_context.body insert end "$k:\n"
                foreach b $v {
                    set nm "" ; set ch "" ; set tk ""
                    catch { set nm [dict get $b name] }
                    catch { set ch [dict get $b chars] }
                    catch { set tk [dict get $b tokens_est] }
                    .ov_context.body insert end "    $nm: [::vstatus::_group $ch] chars ≈ [::vstatus::_group $tk] tok\n"
                }
                continue
            }
            if {[string is integer -strict $v]} { set v [::vstatus::_group $v] }
            .ov_context.body insert end "$k: $v\n"
        }
    }
    ::vtext::readonly .ov_context.body
}

# The route in effect: "provider/model", with a marker when it is a
# session override rather than the configured default.
proc ::vstatus::set_route {text {overridden 0}} {
    variable route_prov
    # The provider half FIRST, before any widget guard: _paint_quota
    # reads it on surfaces where the route label does not exist (a
    # stripped status bar in a test, or a build where the label is
    # undocked), and a stale provider there is the exact wrong-pool
    # figure this variable exists to prevent. Clearing the route must
    # clear it too.
    if {$text eq ""} {
        set route_prov ""
    } else {
        # The label may be "prov/model" or "prov/model (effort high)"
        # — and refresh_route's "(pending)" tail rides the MODEL, so
        # splitting at the first "/" also strips that.
        set route_prov [lindex [split $text /] 0]
    }
    if {![winfo exists .statusbar.route]} { return }
    if {$text eq ""} {
        .statusbar.route configure -text ""
        return
    }
    # `⇄ $text` is a MARKER GLYPH plus a value: the text beside it is the
    # route (provider/model), which is data, and the arrow says only
    # "override in force". There are no words to translate, so each arm
    # is one line carrying its own exemption marker.
    if {$overridden} {
        .statusbar.route configure -text "⇄ $text" -foreground $::vt::T(gold) ;# i18n-exempt: marker glyph + a route value
    } else {
        .statusbar.route configure -text $text -foreground $::vt::T(accent)
    }
    return
}

# The ATTACHMENT, always visible while attached: `● demo —
# vps.example.org:8642`. Unattached shows `○ unattached`. A client talking
# to a gateway with no visible indication of WHICH is how you act on
# the wrong machine (PLAN §Part 6 "Status surface").
proc ::vstatus::set_attachment {spec} {
    variable attachment
    if {$spec eq ""} {
        set attachment ""
    } else {
        set attachment $spec
    }
    _paint_attachment
}

proc ::vstatus::_paint_attachment {} {
    variable attachment
    if {![winfo exists .statusbar.msg]} { return }
    # Ride the health dot's neighbour: the msg label is transient, so
    # the attachment gets its own persistent slot right of the dot.
    if {![winfo exists .statusbar.inst]} {
        catch {
            ttk::label .statusbar.inst -text "" -foreground $::vt::T(accent)
            pack .statusbar.inst -side left -padx {6 2} -after .statusbar.msg
        }
    }
    if {![winfo exists .statusbar.inst]} { return }
    if {$attachment eq ""} {
        .statusbar.inst configure -text [::vi18n::mc {○ unattached}] \
            -foreground $::vt::T(fg_dim)
        return
    }
    set name "" ; set url ""
    catch { set name [dict get $attachment name] }
    catch { set url [dict get $attachment url] }
    set host [::vinst::host_of $url]
    ::vi18n::label .statusbar.inst {● %1$s — %2$s} $name $host
    catch { .statusbar.inst configure -foreground $::vt::T(green) }
}

# PLAN MODE, in its own persistent slot beside the attachment.
#
# Same shape and same reason as _paint_attachment above: the msg label is
# transient, and a client that is refusing every edit has to say so
# somewhere that does not scroll away. Gold rather than dim — a mode that
# changes what the next turn will do is not decoration.
#
# The slot is REMOVED when the mode is off rather than blanked, so an
# off session's bar looks exactly as it did before plan mode existed.
proc ::vstatus::set_mode {on} {
    variable plan_mode
    set plan_mode [expr {$on ? 1 : 0}]
    _paint_mode
}

proc ::vstatus::_paint_mode {} {
    variable plan_mode
    if {![winfo exists .statusbar.msg]} { return }
    if {!$plan_mode} {
        catch { destroy .statusbar.plan }
        return
    }
    if {![winfo exists .statusbar.plan]} {
        catch {
            ttk::label .statusbar.plan -text "" -foreground $::vt::T(gold)
            pack .statusbar.plan -side left -padx {6 2} -after .statusbar.msg
        }
    }
    if {![winfo exists .statusbar.plan]} { return }
    .statusbar.plan configure -text [::vi18n::mc {▲ PLAN}] \
        -foreground $::vt::T(gold)
}

# 1234 -> "1.2k". A six-digit token count in a status bar is noise.
proc ::vstatus::_hn {n} {
    if {![string is integer -strict $n]} { return $n }
    if {$n >= 1000000} { return [format "%.1fM" [expr {$n / 1000000.0}]] }
    if {$n >= 1000}    { return [format "%.1fk" [expr {$n / 1000.0}]] }
    return $n
}

# 1234567 -> "1,234,567". The context segment shows EXACT figures —
# the abbreviated ones are for the in/out totals where a sixth digit
# is noise; the window fill is where "how much room do I have left"
# is the question and an exact answer is the point.
proc ::vstatus::_group {n} {
    if {![string is integer -strict $n]} { return $n }
    set s [string reverse $n]
    set out ""
    set i 0
    foreach c [split $s ""] {
        if {$i > 0 && $i % 3 == 0} { append out "," }
        append out $c
        incr i
    }
    return [string reverse $out]
}

proc ::vstatus::poll {} {
    variable poll_after
    variable status_generation
    # One timer, ever. poll_now (post-attach refresh) would otherwise
    # arm a SECOND `after` alongside the pending one, and N attaches
    # later the bar would poll N times per tick.
    if {$poll_after ne ""} {
        catch { after cancel $poll_after }
        set poll_after ""
    }
    # Unattached: no request leaves the process. Say so plainly rather
    # than reporting the default URL unreachable.
    if {[::vapi::unattached]} {
        set_msg [::vi18n::mc {unattached — pick an instance in the Instances tab}]
        set_health 0
        set poll_after [after 15000 ::vstatus::poll]
        return
    }
    set generation [incr status_generation]
    if {[catch {
        set spath "/api/status"
        catch {
            if {$::vchat::session ne ""} {
                append spath "?session=$::vchat::session"
            }
        }
        ::vapi::get_json_async $spath \
            [list ::vstatus::_status_answer $generation]
    } e]} {
        ::vstatus::_status_answer $generation 0 $e
    }
    # One timer per poll, even while this tick's requests are out.
    set poll_after [after 15000 ::vstatus::poll]
}

# Only window chrome changes here. The data panels have their own
# asynchronous generation, so a failed status request cannot erase them.
proc ::vstatus::_status_answer {generation ok st} {
    variable status_generation
    if {$generation != $status_generation} { return }
    if {!$ok} {
        puts stderr "veles-desktop: statusbar poll failed: $st"
        if {[string match -nocase "*refused*" $st] || [string match -nocase "*timeout*" $st]
                || [string match -nocase "*unreachable*" $st] || [string match -nocase "*reset*" $st]} {
            set_msg [::vi18n::mc {gateway unreachable — is `veles gateway run` up?}]
        } else {
            set_msg [format [::vi18n::mc {status error: %s}] $st]
        }
        set_health 0
        return
    }
    set up [dict get $st uptime_s]
    set plats [join [dict get $st platforms] ","]
    set line [format [::vi18n::mc {gateway up %1$ss · %2$s}] $up $plats]
    set waiting 0
    catch { set waiting [dict get $st configsync_waiting] }
    if {[string is integer -strict $waiting] && $waiting > 0} {
        append line " · " [format [::vi18n::mc {%s config change(s) wait for you — /configsync}] $waiting]
    }
    set_msg $line
    set_health 1
    catch {
        set pm [dict get $st plan_mode]
        if {$pm eq "true" || $pm == 1} {
            set_mode 1
        } elseif {$pm eq "false" || $pm == 0} {
            set_mode 0
        }
    }
    _paint_right
    _paint_quota
}

# The phone, backgrounded (lib/android.tcl): the 15 s poll stops; a
# foregrounded app's first poll re-arms it.
proc ::vstatus::poll_stop {} {
    variable poll_after
    if {$poll_after ne ""} {
        catch { after cancel $poll_after }
        set poll_after ""
    }
}

# Render ONLY the right-hand usage/context segment, against the current
# session. Shared by poll (full cycle) and refresh_context (turn-end,
# which must not stomp the left "done ·" message).
proc ::vstatus::_paint_right {} {
    variable painting
    variable paint_again
    variable paint_generation
    variable paint_pending
    if {$painting} {
        set paint_again 1
        return
    }
    if {[::vapi::unattached]} { return }
    set painting 1
    set paint_again 0
    set generation [incr paint_generation]
    set sid ""
    catch { set sid $::vchat::session }
    set usage "/api/usage"
    if {$sid ne ""} { append usage "?session=$sid" }
    set paint_pending($generation) [dict create sid $sid usage {} context {}]
    set context ""
    if {$sid ne ""} { set context "/api/sessions/$sid/context" }
    if {$context eq ""} {
        dict set paint_pending($generation) context [list 1 {}]
    }
    foreach {kind path} [list usage $usage context $context] {
        if {$path eq ""} { continue }
        if {[catch {
            ::vapi::get_json_async $path \
                [list ::vstatus::_paint_answer $generation $kind]
        } err]} {
            after idle [list ::vstatus::_paint_answer $generation $kind 0 $err]
        }
    }
}

# Complete one outstanding request; never wait for another inside it.
# A session switch makes every older answer stale, even if it arrived
# after a newer request was started.
proc ::vstatus::_paint_answer {generation kind ok value} {
    variable painting
    variable paint_again
    variable paint_generation
    variable paint_pending
    if {$generation != $paint_generation || ![info exists paint_pending($generation)]} {
        if {!$painting && !$paint_again} { return }
        # A superseded request's callbacks may never arrive. Preserve
        # one fresh follow-up, but do not schedule another for every
        # duplicate/late answer after the painter has gone idle.
        _paint_soon
        return
    }
    set pending $paint_pending($generation)
    dict set pending $kind [list $ok $value]
    set paint_pending($generation) $pending
    if {[dict get $pending usage] eq "" || [dict get $pending context] eq ""} {
        return
    }
    unset paint_pending($generation)
    set painting 0
    set sid [dict get $pending sid]
    set current ""
    catch { set current $::vchat::session }
    if {$current ne $sid} {
        # The next poll may be the only refresh this window asks for;
        # do not strand a session switch behind an obsolete answer.
        _paint_soon
        return
    }
    lassign [dict get $pending usage] usage_ok u
    lassign [dict get $pending context] context_ok cx_value
    set cx [expr {$context_ok ? $cx_value : ""}]
    if {$usage_ok} {
        _paint_right_payload $sid $u $cx
    }
    if {$paint_again} { _paint_soon }
}

# One paint, a moment from now, however many were asked for meanwhile.
proc ::vstatus::_paint_soon {} {
    variable paint_after
    if {$paint_after ne ""} { return }
    set paint_after [after 250 {
        set ::vstatus::paint_after ""
        if {[catch { ::vstatus::_paint_right } paint_err]} {
            puts stderr "veles-desktop: statusbar _paint_soon: $paint_err"
        }
    }]
}

# All inputs have arrived. This is a pure renderer: no GET and no vwait.
proc ::vstatus::_paint_right_payload {sid u cx} {
    # This session's figures, not a lifetime total mistaken for a turn.
    set scope "all sessions"
    set pt 0
    set ct 0
    set cached 0
    set sess ""
    catch { set sess [dict get $u session] }
    if {$sess ne "" && $sess ne "null" && ![catch {dict size $sess}]} {
        catch { set pt [dict get $sess prompt_tokens] }
        catch { set ct [dict get $sess completion_tokens] }
        catch { set cached [dict get $sess cached_tokens] }
        set scope "this session"
    } else {
        catch { set pt [dict get $u prompt_tokens] }
        catch { set ct [dict get $u completion_tokens] }
        catch { set cached [dict get $u cached_tokens] }
    }
    set right "[::vstatus::_hn $pt] in / [::vstatus::_hn $ct] out"
    # Reasoning, when the route splits it out: the completion figure
    # above is really answer + thinking, so name both — the point of
    # the separate count (2026-09-13) is that an overthinking model
    # reads legible instead of just expensive-looking. `null` (older
    # gateway, or a route that folds thinking into completion) keeps
    # the plain "N out" — never a misleading "0 thinking".
    set rt ""
    if {$cx ne ""} { catch { set rt [dict get $cx session_reasoning_tokens] } }
    if {$rt ne "" && $rt ne "null" && [string is integer -strict $rt]} {
        set ans [expr {$ct > $rt ? $ct - $rt : 0}]
        set right "[::vstatus::_hn $pt] in / [::vstatus::_hn $ans] answer · [::vstatus::_hn $rt] thinking"
    }
    # The cache share, at both scopes. The LAST CALL is the
    # operational number — a prefix regression moves it the
    # round it happens — and the session mean is the billing
    # one. Showing only the mean is what made a cache running
    # at 98% read as 87.9%: it can never shake off the cold
    # first call. Neither number appearing at all means this
    # provider reports no cache field, which is not the same as
    # a 0% hit and must not look like one.
    set lastpct ""
    if {$cx ne ""} { catch { set lastpct [dict get $cx cache_hit_rate_last_call] } }
    set sesspct ""
    if {$pt > 0 && $cached > 0} {
        set sesspct [format %.0f [expr {100.0 * $cached / $pt}]]
    }
    if {$lastpct ne "" && $lastpct ne "null" && [string is double -strict $lastpct]} {
        append right " · cache [format %.0f [expr {100.0 * $lastpct}]]% last call"
        # The last-call cache rate belongs to the route that
        # MADE it. After a /model switch, the newest row is still
        # the old model's until the new one makes a call — show
        # that instead of implying the new model posted it
        # (USER: "cache stats don't follow model changes").
        set lcp "" ; set lcm "" ; set cur ""
        catch { set lcp [dict get $cx last_call_provider] }
        catch { set lcm [dict get $cx last_call_model] }
        catch { set cur [dict get $cx model] }
        if {$cur ne "" && $cur ne "null" && $lcm ne "" && $lcm ne "null" \
            && $lcm ne $cur} {
            append right " ([expr {$lcp ne "null" && $lcp ne "" ? "$lcp/" : ""}]$lcm, before)"
        }
        if {$sesspct ne ""} { append right " / $sesspct% session" }
    } elseif {$sesspct ne ""} {
        append right " · cache $sesspct% session"
    }
    append right " · $scope"
    # Context window fill, so a compaction is predictable rather
    # than a surprise. USER: "add stats about the context window".
    #
    # TWO staleness bugs lived here:
    #  * the segment only refreshed on the 15s poll, so a session
    #    switch showed the PREVIOUS session's number for up to
    #    15s — open_session now calls poll_now;
    #  * mid-turn the endpoint answers busy:true (a turn is live),
    #    which used to BLANK the segment exactly when a long turn
    #    is eating window. Keep the last good reading and mark it
    #    instead. The mark cannot be inferred from key ABSENCE any
    #    more — busy is on every answer — so read its VALUE.
    #
    # The numbers are EXACT and comma-grouped — a "9% of 1M"
    # label hides whether the window is 90k in or 120k, and the
    # user asked for the full figures.
    set busy 0
    if {$cx ne ""} { catch { set busy [dict get $cx busy] } }
    if {$cx ne "" && !$busy} {
        set pct ""
        catch { set pct [dict get $cx history_percent_of_budget] }
        set win ""
        catch { set win [dict get $cx context_window_tokens] }
        if {$pct ne "" && [string is double -strict $pct]} {
            set trig 85
            catch { set trig [dict get $cx compaction_trigger_percent] }
            set tok_est [expr {$win > 0 ? int($win * $pct / 100.0) : 0}]
            set seg "  ·  ctx [format %.1f $pct]% · [::vstatus::_group $tok_est]/[::vstatus::_group $win] tok"
            # Crossing the trigger is a CONDITION, not an event. Mid-turn
            # compaction is a conditional valve — it may or may not fire
            # this round — so say what is true (the window is at or past
            # the trigger), never "(compacting)", which claims the
            # summarizer is running when only a percent comparison
            # happened. (PENDING 2026-09-12 round, "compaction due".)
            if {$pct >= $trig} {
                append seg " (compaction due)"
            } elseif {$pct >= [expr {$trig * 0.8}]} {
                append seg " (near compaction)"
            }
            variable last_ctx_line
            variable last_ctx
            variable last_ctx_sid
            set last_ctx_line $seg
            set last_ctx $cx
            set last_ctx_sid $sid
            append right $seg
        }
    } elseif {$busy} {
        # Turn in flight. The context figures are STILL live — they come
        # off the store's usage rows and history, not the turn's snapshot
        # lock — so paint them fresh and mark the turn, rather than
        # dropping to a stale retained line. The figures provably move
        # DURING a multi-round turn this way (10.2% → 12.4% → …), which
        # is the point of a long turn: watching the window be eaten.
        # retain-and-grey remains ONLY for an older gateway whose busy
        # answer carries no figures (or a fetch that failed).
        variable last_ctx_line
        variable last_ctx_sid
        set pct ""
        catch { set pct [dict get $cx history_percent_of_budget] }
        set win ""
        catch { set win [dict get $cx context_window_tokens] }
        if {$pct ne "" && [string is double -strict $pct]} {
            set trig 85
            catch { set trig [dict get $cx compaction_trigger_percent] }
            set tok_est [expr {$win > 0 ? int($win * $pct / 100.0) : 0}]
            set seg "  ·  ctx [format %.1f $pct]% · [::vstatus::_group $tok_est]/[::vstatus::_group $win] tok"
            # Same honesty rule as the idle arm above: a crossed trigger
            # is "compaction due", never "(compacting)".
            if {$pct >= $trig} {
                append seg " (compaction due)"
            } elseif {$pct >= [expr {$trig * 0.8}]} {
                append seg " (near compaction)"
            }
            append seg " (turn running)"
            set last_ctx_line $seg
            set last_ctx_sid $sid
            append right $seg
        } elseif {$last_ctx_line ne "" && $last_ctx_sid eq $sid} {
            append right "$last_ctx_line (turn running)"
        }
    }
    .statusbar.right configure -text $right
}

# The upstream plan's headline figure: the fraction USED of the pool
# THIS SESSION draws from.
#
# CACHE-ONLY. `/api/quota` without `?refresh=1` never leaves the box —
# the gateway's background warm is what keeps it current — so this is a
# loopback read on the same 15s tick as everything else here.
#
# THE PLAN IS NAMED (USER 2026-09-20: "the existing usage is confusing,
# it conflates the personal and team alibaba tokens … clearly label the
# plans separately"). A bare "plan 59%" said neither which pool nor
# whose bill: four providers on this box draw on four credential sets,
# and the old field could show any of them under the same label. It now
# reads "team 59%" / "personal 12%" / "zai 60%" / "kimi 100%", so the
# number is never separable from the pool it belongs to — including on
# the surfaces that only ever show this one figure.
#
# WHICH PLAN IS RESOLVED BY THE ENGINE, not here. The provider string
# goes up with the request and `active_plan` comes back — the desktop
# used to re-derive the mapping with its own substring tests, and the
# engine's copy was wrong for `ali-responses` (a team credential with
# neither "team" nor "personal" in its name, so both were showing the
# PERSONAL pool's number to a session spending team credits).
#
# Nothing implicit: every blank state says which one it is in the
# tooltip, and it never shows a number it had to invent.
proc ::vstatus::_paint_quota {} {
    variable route_prov
    variable quota_generation
    if {![winfo exists .statusbar.quota]} { return }
    set generation [incr quota_generation]
    set path /api/quota
    if {[info exists route_prov] && $route_prov ne ""} {
        append path "?provider=[::vapi::urlencode $route_prov]"
    }
    if {[catch {
        ::vapi::get_json_async $path \
            [list ::vstatus::_quota_answer $generation]
    } err]} {
        _quota_answer $generation 0 $err
    }
}

proc ::vstatus::_quota_answer {generation ok q} {
    variable quota_generation
    if {$generation != $quota_generation || ![winfo exists .statusbar.quota]} { return }
    if {!$ok} {
        .statusbar.quota configure -text ""
        return
    }
    if {[dict exists $q enabled] && ![string is true -strict [dict get $q enabled]]} {
        .statusbar.quota configure -text ""
        return
    }
    if {[dict exists $q cold] && [string is true -strict [dict get $q cold]]} {
        .statusbar.quota configure -text [::vi18n::mc {plan —}]
        catch { ::va11y::name .statusbar.quota \
            [::vi18n::mc {no plan reading yet; the gateway warms it in the background}] }
        return
    }
    # The engine's own resolution: which plan, and its fraction.
    set plan ""
    catch { set plan [dict get $q active_plan] }
    if {$plan eq "null"} { set plan "" }
    set f ""
    if {$plan ne ""} {
        catch { set f [dict get $q active_plan_fraction] }
    }
    if {$f eq "" || $f eq "null" || ![string is double -strict $f]} {
        # No plan for this provider, or no reading for the plan it has.
        # TWO DIFFERENT FACTS, said differently: a session on OpenRouter
        # is on no plan at all, while a team session whose fetch failed
        # has a plan and no number. The label carries the plan either
        # way — that is the whole point of this change.
        set label [expr {$plan ne "" ? $plan : [::vi18n::mc {no plan}]}]
        ::vi18n::label .statusbar.quota {%s —} $label
        catch { ::va11y::name .statusbar.quota \
            [::vi18n::mc {no upstream reading for %s} \
                [expr {$plan ne "" ? [::vi18n::mc {the %s plan} $plan] : [::vi18n::mc {this route}]}]] }
        return
    }
    set pct [expr {round($f * 100)}]
    # THE PLAN NAME IS PART OF THE FIGURE. Every variant below keeps it.
    ::vi18n::label .statusbar.quota {%s %s%%} $plan $pct
    # Dim under half, gold past three quarters, red past ninety — the
    # bar says "look at me" only when the pool is actually short.
    if {$pct >= 90} {
        .statusbar.quota configure -foreground $::vt::T(red)
    } elseif {$pct >= 75} {
        .statusbar.quota configure -foreground $::vt::T(gold)
    } else {
        .statusbar.quota configure -foreground $::vt::T(fg_dim)
    }
    set age 0
    catch { set age [dict get $q age_secs] }
    set poll 0
    catch { set poll [dict get $q poll_secs] }
    set which [expr {$plan ne "" ? "the $plan plan (this session's)" : "the tightest of all plan pools"}]
    set tip "$which is ${pct}% used"
    append tip " · read [::voverlay::_qage $age]"
    # PAST THE TTL IS OLD, NOT ABSENT. The engine used to answer "cold"
    # for any reading older than [tokenplan] cache_ttl_secs, and with
    # the shipped 300/900 pair that was two thirds of every cycle — so
    # this field read "plan —" for ten minutes at a stretch and looked
    # like it had switched itself off (USER 2026-09-16). It now serves
    # the last reading with its age; the number is still the truth, and
    # the tooltip is where the age stops being implicit.
    set stale 0
    catch { set stale [string is true -strict [dict get $q stale]] }
    if {$stale} { append tip [::vi18n::mc { · past its ttl, a refresh is due}] }
    if {$poll > 0} { append tip [::vi18n::mc { · refreshed every %sm} [expr {$poll / 60}]] }
    append tip [::vi18n::mc { — click for the full report}]
    catch { ::va11y::name .statusbar.quota $tip } ;# i18n-exempt: composed from translated parts above
}

proc ::vstatus::detach_async {} {
    variable painting
    variable paint_again
    variable paint_after
    variable paint_generation
    variable paint_pending
    variable context_generation
    variable quota_generation
    variable status_generation
    variable poll_after
    if {$paint_after ne ""} { catch { after cancel $paint_after } }
    set paint_after ""
    set painting 0
    set paint_again 0
    incr paint_generation
    array unset paint_pending
    incr context_generation
    incr quota_generation
    incr status_generation
    catch { destroy .ov_context }
    catch { after cancel $poll_after }
    set poll_after ""
}

# Open the log overlay with the current gateway status (called from
# the palette — the statusbar's health dot is the always-visible indicator).
proc ::vstatus::show_gateway {} {
    ::voverlay::open log
}

# One immediate poll, used by ::vapp::rebuild_regions after an attach
# so the bar reflects the NEW gateway without waiting 15s.
proc ::vstatus::poll_now {} {
    poll
}

# Refresh just the context segment for one session, right now. Called
# from on_done (every turn end) so "how full is the window" tracks the
# turn that just ran instead of the 15s poll AFTER control returns.
#
# _paint_right re-fetches context AND renders only the right-hand
# segment — it does not touch the left status message, so the "· done"
# line commits to the user survives.
proc ::vstatus::refresh_context {sid} {
    if {$sid eq ""} { return }
    if {[::vapi::unattached]} { return }
    # Called from stream handlers: schedule, never fetch from inside one.
    _paint_soon
}

# Per-step context refresh: called on each provider round (route/tool
# events) so "how full is the window" tracks a LONG turn eating it, not
# just the 15s poll or turn end. Throttled so a tool-heavy turn does not
# turn the bar into an HTTP flood — the figures are recomputed from the
# store per round, so ~1.5s of settling still catches each round.
proc ::vstatus::step_refresh {sid} {
    if {$sid eq ""} { return }
    if {[::vapi::unattached]} { return }
    variable last_step_refresh
    set now [clock milliseconds]
    if {$now - $last_step_refresh < 1500} { return }
    set last_step_refresh $now
    _paint_soon
}
