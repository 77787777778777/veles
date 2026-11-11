# Selection translation through the gateway's dedicated translator route.
# No chat session, no automatic source replacement and no local model fallback.
namespace eval ::vtranslate {
    variable generation 0
    variable popup_generation 0
    variable source ""
    variable target ""
    variable window .vtranslate
    variable pending ""
    variable pending_generation 0
    variable config_pending 0
    variable configured 0
    variable flow_key
    variable target_edited 0
    # How long one translation can take on the gateway, in seconds: its
    # /api/translate/config `timeout_secs` (translate::BUDGET_SECS, two
    # provider calls of 600 s). The request waits this plus `margin_secs`
    # for the HTTP round trip; the desktop client's own default (10 s)
    # cut a translation off while the engine was still working on it.
    variable budget_secs 1200
    variable margin_secs 15
}

proc ::vtranslate::focused {} {
    popup [focus]
}

proc ::vtranslate::selection {w} {
    if {![winfo exists $w]} { return "" }
    set class [winfo class $w]
    if {$class eq "Text"} {
        if {[catch {$w get sel.first sel.last} text]} { return "" }
        return $text
    }
    if {$class in {Entry TEntry}} {
        if {[catch {$w selection present} present] || !$present} { return "" }
        if {[catch {$w index sel.first} first] || [catch {$w index sel.last} last]} { return "" }
        return [string range [$w get] $first [expr {$last - 1}]]
    }
    return ""
}

proc ::vtranslate::for_selection {w} {
    return [expr {[selection $w] ne ""}]
}

proc ::vtranslate::popup {w} {
    variable generation
    variable popup_generation
    variable source
    variable window
    set text [selection $w]
    if {$text eq ""} {
        ::vstatus::set_msg [::vi18n::mc {Select text to translate first.}]
        return
    }
    set source $text
    cancel_pending
    # Invalidate the old popup before destroying it. Its Destroy handler
    # must not invalidate callbacks belonging to the replacement popup.
    incr popup_generation
    if {[winfo exists $window]} { destroy $window }
    toplevel $window
    wm title $window [::vi18n::mc {Translate selection}]
    wm geometry $window [geometry [winfo screenwidth .] [winfo screenheight .]]
    wm minsize $window 260 240
    ttk::frame $window.bar
    pack $window.bar -fill x -padx 8 -pady 6
    ttk::label $window.bar.target -text [::vi18n::mc {Target language:}]
    ttk::entry $window.bar.lang -width 18 -textvariable ::vtranslate::target
    bind $window.bar.lang <KeyRelease> {+set ::vtranslate::target_edited 1}
    ttk::button $window.bar.retry -text [::vi18n::mc {Translate}] -command ::vtranslate::request
    ttk::button $window.bar.cancel -text [::vi18n::mc {Cancel}] -command ::vtranslate::cancel
    ttk::button $window.bar.copy -text [::vi18n::mc {Copy}] -command ::vtranslate::copy
    ttk::button $window.bar.settings -text [::vi18n::mc {Configure translator}] -command ::vtranslate::configure
    ttk::button $window.bar.close -text [::vi18n::mc {Close}] -command ::vtranslate::close
    ttk::label $window.bar.route -text "" -justify left
    # Everything in the bar is gridded by _flow, which wraps the buttons to
    # the width the window has and puts the route text on a row of its own:
    # a long route or error can no longer push a button out (SMOKE-9).
    bind $window.bar <Configure> [list ::vtranslate::_flow $window]
    bind $window.bar.lang <Return> ::vtranslate::request
    array unset ::vtranslate::flow_key
    _flow $window
    ttk::panedwindow $window.panes -orient vertical
    pack $window.panes -fill both -expand 1
    ttk::frame $window.sourceframe
    ttk::label $window.sourceframe.label -text [::vi18n::mc {Source text}]
    pack $window.sourceframe.label -anchor w
    text $window.sourceframe.text -height 6 -wrap word -background $::vt::T(bg_input) -foreground $::vt::T(fg)
    pack $window.sourceframe.text -side left -fill both -expand 1
    $window.sourceframe.text insert end $source
    ::vtext::readonly $window.sourceframe.text
    $window.panes add $window.sourceframe -weight 1
    ttk::scrollbar $window.sourceframe.sb -command [list $window.sourceframe.text yview]
    $window.sourceframe.text configure -yscrollcommand [list $window.sourceframe.sb set]
    pack $window.sourceframe.sb -side right -fill y -before $window.sourceframe.text
    ttk::frame $window.resultframe
    ttk::label $window.resultframe.label -text [::vi18n::mc {Translation result}]
    pack $window.resultframe.label -anchor w
    $window.panes add $window.resultframe -weight 1
    ttk::frame $window.resultframe.body
    pack $window.resultframe.body -fill both -expand 1
    text $window.text -wrap word -undo 0 -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -insertbackground $::vt::T(fg)
    ttk::scrollbar $window.sb -command [list $window.text yview]
    $window.text configure -yscrollcommand [list $window.sb set]
    pack $window.sb -in $window.resultframe.body -side right -fill y
    pack $window.text -in $window.resultframe.body -fill both -expand 1
    ::vtext::readonly $window.text
    catch { ::va11y::name $window.text [::vi18n::mc {translation result}] }
    bind $window <Escape> ::vtranslate::close
    bind $window <Destroy> {+::vtranslate::_closed %W}
    focus $window.bar.lang
    set target ""
    set ::vtranslate::target_edited 0
    # Configuration belongs to this popup, not to a later retry.
    # Retried configuration reads belong to THIS retry, not just this
    # popup. An older response may update neither the target nor requests.
    set ::vtranslate::config_pending [incr generation]
    ::vapi::get_json_async /api/translate/config \
        [list ::vtranslate::_config_loaded $popup_generation $generation]
}

# The popup's size: the usual 760x520, never larger than the screen it opens
# on (a phone is 360x640 or so).
proc ::vtranslate::geometry {screen_w screen_h} {
    set w [expr {min(760, max(260, $screen_w - 16))}]
    set h [expr {min(520, max(240, $screen_h - 96))}]
    return ${w}x${h}
}

# Greedy rows of [list widget width] pairs into the given width: returns
# a list of rows, each a list of widget names. A widget wider than the width
# still gets a row of its own.
proc ::vtranslate::rows {items width gap} {
    set rows {}
    set row {}
    set used 0
    foreach item $items {
        lassign $item name w
        if {$row ne {} && $used + $gap + $w > $width} {
            lappend rows $row
            set row {}
            set used 0
        }
        if {$row ne {}} { incr used $gap }
        lappend row $name
        incr used $w
    }
    if {$row ne {}} { lappend rows $row }
    return $rows
}

proc ::vtranslate::_flow {window} {
    variable flow_key
    set bar $window.bar
    if {![winfo exists $bar.route]} { return }
    set width [winfo width $bar]
    if {$width < 20} { return }
    set items {}
    foreach b {retry cancel copy settings close} {
        lappend items [list $bar.$b [winfo reqwidth $bar.$b]]
    }
    set rows [rows $items $width 6]
    set key [list $width $rows]
    $bar.route configure -wraplength [expr {max(40, $width - 4)}]
    if {[info exists flow_key($window)] && $flow_key($window) eq $key} { return }
    set flow_key($window) $key
    foreach w [grid slaves $bar] { grid forget $w }
    grid $bar.target -row 0 -column 0 -sticky w
    grid $bar.lang -row 0 -column 1 -columnspan 4 -sticky ew -padx 5
    set r 1
    foreach row $rows {
        set c 0
        foreach w $row {
            grid $w -row $r -column $c -sticky ew -padx 3 -pady 2
            incr c
        }
        incr r
    }
    grid $bar.route -row $r -column 0 -columnspan 5 -sticky ew -pady {4 0}
    for {set c 0} {$c < 5} {incr c} { grid columnconfigure $bar $c -weight 1 }
}

proc ::vtranslate::_config_loaded {requested_generation config_generation ok response} {
    variable target
    variable configured
    variable window
    if {$requested_generation != $::vtranslate::popup_generation ||
        $config_generation != $::vtranslate::generation ||
        ![winfo exists $window]} { return }
    set ::vtranslate::config_pending 0
    if {!$ok} {
        set configured 0
        $window.bar.route configure -text \
            "[::vi18n::mc {Translator configuration unavailable}]: [::vapi::reason $response]"
        request 0
        return
    }
    set configured [expr {[::vapi::field $response configured] eq "true"}]
    # How long the gateway can take for one translation; a gateway that
    # does not say has the same 1200 s.
    set budget [::vapi::field $response timeout_secs ""]
    set ::vtranslate::budget_secs [expr {
        [string is integer -strict $budget] && $budget > 0 ? $budget : 1200}]
    if {$target eq "" && !$::vtranslate::target_edited} {
        catch { set target [dict get $response target_language] }
    }
    if {$target eq "" && !$::vtranslate::target_edited} {
        # No configured default: the language this desktop is shown in,
        # by its own name (the gateway's locale may be another one).
        catch { set target [string trim [::vi18n::locale_name [::vi18n::current]]] }
    }
    if {!$configured} {
        # Nothing to send to: say how to set it up, in the route line and in
        # the result pane, and keep both (a request would only replace the
        # guidance with the server's refusal).
        set hint [::vi18n::mc {No dedicated translator configured. Use Configure translator to choose its provider, model and target language.}]
        $window.bar.route configure -text $hint
        _write $hint
        _idle
        return
    }
    request 0
}

proc ::vtranslate::configure {} {
    variable window
    close
    ::voverlay::open settings
    if {[winfo exists .ov_settings.bar.q]} {
        .ov_settings.bar.q delete 0 end
        .ov_settings.bar.q insert end translation
        ::vsettings::render .ov_settings
    }
}

proc ::vtranslate::cancel {} {
    variable generation
    variable window
    variable pending
    if {$pending eq ""} { return }
    cancel_pending
    incr generation
    if {[winfo exists $window]} {
        _write [::vi18n::mc {Translation cancelled.}]
        _idle
    }
}

proc ::vtranslate::_idle {} {
    variable window
    if {![winfo exists $window]} { return }
    $window.bar.retry state !disabled
    $window.bar.cancel state disabled
}

proc ::vtranslate::_busy {} {
    variable window
    if {![winfo exists $window]} { return }
    $window.bar.retry state disabled
    $window.bar.cancel state !disabled
}

proc ::vtranslate::request {{refresh_config 1}} {
    variable generation
    variable source
    variable target
    variable window
    variable pending
    variable pending_generation
    variable configured
    variable popup_generation
    if {![winfo exists $window]} { return }
    cancel_pending
    set current [incr generation]
    if {$refresh_config} {
        # Retry refreshes readiness and then starts in _config_loaded.
        # The popup generation fences a reply from a closed popup.
        set ::vtranslate::config_pending $current
        ::vapi::get_json_async /api/translate/config \
            [list ::vtranslate::_config_loaded $popup_generation $current]
        return
    }
    _busy
    _write [::vi18n::mc {Translating…}]
    # Selected text and target language are scalars, even when Tcl's
    # list representation contains spaces. The generic API serializer
    # otherwise emits arrays, which serde correctly rejects as a string.
    set body [dict create text [::vapi::json_string $source]]
    if {[string trim $target] ne ""} {
        # Omit an unset target: an explicitly empty value would be [].
        dict set body target_language [::vapi::json_string $target]
    }
    set pending_generation $current
    set pending [::vapi::post_json_async /api/translate $body \
        [list ::vtranslate::_failed $current] \
        [list ::vtranslate::_done $current] \
        [expr {($::vtranslate::budget_secs + $::vtranslate::margin_secs) * 1000}]]
}

proc ::vtranslate::cancel_pending {} {
    variable pending
    variable pending_generation
    if {$pending ne ""} {
        catch { ::http::reset $pending }
        set pending ""
    }
    set pending_generation 0
}

proc ::vtranslate::_settle {generation} {
    variable pending_generation
    variable pending
    if {$generation == $pending_generation} {
        set pending_generation 0
        set pending ""
    }
    _idle
}

proc ::vtranslate::_done {generation response} {
    variable window
    if {$generation != $::vtranslate::generation || ![winfo exists $window]} { return }
    _settle $generation
    if {[catch {dict get $response text} translated]} {
        _failed $generation [::vi18n::mc {The translator returned no text.}]
        return
    }
    catch { set route "[dict get $response model] @ [dict get $response provider]" }
    _write $translated
    set shown "[::vi18n::mc {Dedicated translator}]: $route"
    # The gateway checks that placeholders and markup came back unchanged;
    # a loss it could not cure is said here, not hidden (TR-1).
    set warnings {}
    catch { set warnings [dict get $response warnings] }
    if {[llength $warnings] > 0} {
        append shown " - [::vi18n::mc {Warning}]: [join $warnings {; }]"
    }
    catch { $window.bar.route configure -text $shown }
}

proc ::vtranslate::_failed {generation why} {
    variable window
    if {$generation != $::vtranslate::generation || ![winfo exists $window]} { return }
    _settle $generation
    # The request's own timeout (the gateway's budget and the margin)
    # ran out: say that in words, not as "transport: timeout".
    if {$why eq "transport: timeout"} {
        set why [format [::vi18n::mc {The translator did not answer within %s seconds.}] \
            $::vtranslate::budget_secs]
    }
    # The error goes to the result pane only; the route line keeps saying
    # which translator this is (SMOKE-9).
    _write "[::vi18n::mc {Translation failed}]: $why"
}

proc ::vtranslate::_write {text} {
    variable window
    if {![winfo exists $window.text]} { return }
    $window.text configure -state normal
    $window.text delete 1.0 end
    $window.text insert end $text
    $window.text configure -state disabled
}

proc ::vtranslate::copy {} {
    variable window
    if {![winfo exists $window.text]} { return }
    set text [$window.text get 1.0 end-1c]
    clipboard clear
    clipboard append $text
    ::vstatus::set_msg [::vi18n::mc {Translation copied.}]
}

proc ::vtranslate::close {} {
    variable window
    cancel_pending
    if {[winfo exists $window]} { destroy $window }
}

proc ::vtranslate::_closed {w} {
    variable window
    variable generation
    variable popup_generation
    if {$w eq $window} {
        cancel_pending
        incr popup_generation
        incr generation
    }
}
