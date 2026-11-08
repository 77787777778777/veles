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
    variable target_edited 0
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
    wm geometry $window 760x520
    wm minsize $window 420 320
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
    pack $window.bar.target -side left
    pack $window.bar.lang -side left -padx 5
    ttk::label $window.bar.route -text ""
    pack $window.bar.route -side left -fill x -expand 1 -padx 8
    pack $window.bar.close $window.bar.settings $window.bar.copy $window.bar.cancel $window.bar.retry -side right -padx 3
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
    if {$target eq "" && !$::vtranslate::target_edited} {
        catch { set target [dict get $response target_language] }
    }
    if {$target eq "" && !$::vtranslate::target_edited} {
        # No configured default: the language this desktop is shown in,
        # by its own name (the gateway's locale may be another one).
        catch { set target [string trim [::vi18n::locale_name [::vi18n::current]]] }
    }
    if {!$configured} {
        $window.bar.route configure -text \
            [::vi18n::mc {No dedicated translator configured. Use Configure translator to choose its provider, model and target language.}]
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
        [list ::vtranslate::_done $current]]
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
    catch { $window.bar.route configure -text "[::vi18n::mc {Dedicated translator}]: $route" }
}

proc ::vtranslate::_failed {generation why} {
    variable window
    if {$generation != $::vtranslate::generation || ![winfo exists $window]} { return }
    _settle $generation
    catch { $window.bar.route configure -text "[::vi18n::mc {Translation failed}]: $why" }
    if {[$window.text get 1.0 end-1c] eq "" ||
            [$window.text get 1.0 end-1c] eq [::vi18n::mc {Translating…}]} {
        _write "[::vi18n::mc {Translation failed}]: $why"
    }
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
