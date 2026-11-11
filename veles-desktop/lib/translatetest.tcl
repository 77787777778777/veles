#!/usr/bin/env wish
set _dir [file dirname [file normalize [info script]]]
if {![info exists ::env(XVFB_RUNNING)] && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}
source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir i18n.tcl]
::vi18n::init
source [file join $_dir a11y.tcl]
source [file join $_dir vtext.tcl]
source [file join $_dir ctxmenu.tcl]
source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]
source [file join $_dir translate.tcl]
namespace eval ::vstatus { proc set_msg {m} { set ::status $m } }
namespace eval ::vkeymap { proc key_for {cmd} { return "" } }
namespace eval ::vapi {
    variable request {}
    variable ms_given 0
    proc get_json_async {path callback} {
        {*}$callback 1 [dict create target_language German configured true]
    }
    # `ms` is the request's own timeout (api.tcl: 0 means its default).
    proc post_json_async {path body on_err on_ok {ms 0}} {
        variable request
        variable ms_given
        set request [list $path $body $on_err $on_ok]
        set ms_given $ms
        return token
    }
}
namespace eval ::http {
    variable resets {}
    proc reset {token} { lappend ::http::resets $token }
}
set passed 0
set failed 0
proc ok {name condition} {
    if {[uplevel 1 [list expr $condition]]} {
        puts "  ✓ $name"; incr ::passed
    } else { puts "  ✗ $name"; incr ::failed }
}
text .source
.source insert end "Hello, 世界"
.source tag add sel 1.0 1.6
::vctx::attach .source {}
::vctx::_rebuild .source.vctxmenu
set labels {}
for {set i 0} {$i <= [.source.vctxmenu index end]} {incr i} {
    if {[.source.vctxmenu type $i] eq "command"} { lappend labels [.source.vctxmenu entrycget $i -label] }
}
ok "the shared context menu offers translation for a text selection" {
    [lsearch -exact $labels {Translate selection}] >= 0
}
# tk_popup is recorded, not posted: a posted menu grabs the pointer and
# would wait for a click. Everything BEFORE it is the real path: the
# event runs every bindtag (widget, class, toplevel, all), and the menu
# entry is invoked afterwards, the way a click on it would be.
rename ::tk_popup ::_real_tk_popup_for_test
set ::posted {}
proc ::tk_popup {m x y args} { lappend ::posted $m }
proc entry_index {m label} {
    if {![winfo exists $m]} { return -1 }
    for {set i 0} {$i <= [$m index end]} {incr i} {
        if {[$m type $i] eq "command" && [$m entrycget $i -label] eq $label} { return $i }
    }
    return -1
}
proc close_popup {} { catch { ::vtranslate::close } }
foreach {kind class} {entry Entry tentry TEntry} {
    set w .source_$kind
    if {$kind eq "entry"} { entry $w } else { ttk::entry $w }
    pack $w
    $w insert end "Two words"
    $w selection range 0 end
    ::vctx::attach $w {}
    update
    ok "$class selection is translatable" {[::vtranslate::selection $w] eq "Two words"}
    set ::posted {}
    close_popup
    event generate $w <Button-3> -x 3 -y 3 -rootx 30 -rooty 30
    ok "$class right-click opens exactly its own menu" {$::posted eq [list $w.vctxmenu]}
    set i [entry_index $w.vctxmenu {Translate selection}]
    ok "$class context menu includes translation" {$i >= 0}
    if {$i >= 0} { $w.vctxmenu invoke $i }
    ok "$class menu entry translates the range selected before the click" {
        [winfo exists .vtranslate] && $::vtranslate::source eq "Two words"
    }
    close_popup
    destroy $w
}

# Text and entries that never called ::vctx::attach (overlays, ask
# dialogs, settings, help…) get the selection actions from the class.
text .plain
pack .plain
.plain insert end "Plain 世界 text"
foreach {kind ctor} {text {} entry entry tentry ttk::entry} {
    if {$kind eq "text"} {
        set w .plain
        $w tag remove sel 1.0 end
        $w tag add sel 1.0 1.8
    } else {
        set w .plain_$kind
        $ctor $w
        pack $w
        $w insert end "Plain 世界 text"
        $w selection range 0 8
    }
    update
    set ::posted {}
    close_popup
    event generate $w <Button-3> -x 3 -y 3 -rootx 30 -rooty 30
    ok "an unattached $kind gets a right-click menu for its selection" {
        $::posted eq {.vctxfallback}
    }
    set i [entry_index .vctxfallback {Translate selection}]
    ok "…which offers translation" {$i >= 0}
    if {$i >= 0} { .vctxfallback invoke $i }
    ok "…of exactly the selected range" {
        [winfo exists .vtranslate] && $::vtranslate::source eq "Plain 世界"
    }
    close_popup
    set c [entry_index .vctxfallback Copy]
    clipboard clear
    if {$c >= 0} { .vctxfallback invoke $c }
    ok "…and copies it" {$c >= 0 && [clipboard get] eq "Plain 世界"}
    if {$kind ne "text"} { destroy $w }
}
.plain tag remove sel 1.0 end
set ::posted {}
event generate .plain <Button-3> -x 3 -y 3 -rootx 30 -rooty 30
ok "no selection, no fallback menu" {$::posted eq {}}
destroy .plain
rename ::tk_popup {}
rename ::_real_tk_popup_for_test ::tk_popup

.source tag remove sel 1.0 end
.source tag add sel 1.0 end-1c
::vtranslate::popup .source
ok "the popup preserves the Unicode source" {$::vtranslate::source eq "Hello, 世界"}
.source tag remove sel 1.0 end
.source tag add sel 1.0 1.6
::vtranslate::popup .source
ok "the source and result are independently readable" {
    [.vtranslate.sourceframe.text get 1.0 end-1c] eq "Hello," &&
    [winfo exists .vtranslate.text] && [winfo exists .vtranslate.bar.settings]
}
update
ok "the result pane and its text have visible geometry" {
    [lsearch -exact [.vtranslate.panes panes] .vtranslate.resultframe] >= 0 &&
    [winfo ismapped .vtranslate.resultframe] &&
    [winfo ismapped .vtranslate.resultframe.body] &&
    [winfo ismapped .vtranslate.text] &&
    [winfo height .vtranslate.resultframe] > 40 &&
    [winfo height .vtranslate.text] > 20
}
ok "the popup is busy while a request is pending" {
    [.vtranslate.bar.retry instate disabled] && ![.vtranslate.bar.cancel instate disabled]
}
ok "the configured target is the initial target" {$::vtranslate::target eq "German"}
lassign $::vapi::request path body on_err on_ok
ok "selection goes to the dedicated endpoint" {$path eq "/api/translate"}
ok "the body carries selected text, not a session" {
    [dict get $body text] ne "" && ![dict exists $body session_id]
}
set wire [::vapi::_json_value $body]
set decoded [::json::json2dict $wire]
ok "the serialized request preserves text and target as strings" {
    [dict get $decoded text] eq "Hello," &&
    [dict get $decoded target_language] eq "German" &&
    [string match {*\"text\":*\"Hello,\"*} $wire] &&
    [string match {*\"target_language\":*\"German\"*} $wire]
}
set ::vtranslate::target ""
set ::vtranslate::target_edited 1
::vtranslate::request
lassign $::vapi::request path empty_body on_err on_ok
set empty_wire [::vapi::_json_value $empty_body]
ok "an empty target preserves the selected text and omits the target" {
    [string match {*\"text\":*\"Hello,\"*} $empty_wire] &&
    ![string match {*target_language*} $empty_wire]
}
{*}$on_ok [dict create text "Hallo, 世界" model translator-small provider test]
ok "the response is rendered with its actual route" {
    [.vtranslate.text get 1.0 end-1c] eq "Hallo, 世界" &&
    [string match "*translator-small @ test*" [.vtranslate.bar.route cget -text]]
}
ok "the source text was not replaced" {[.source get 1.0 end-1c] eq "Hello, 世界"}
# F11 through the registry and the keymap that binds it, not a direct
# call: the row in bindings.tcl is what a user presses.
source [file join $_dir safeexec.tcl]
source [file join $_dir bindings.tcl]
source [file join $_dir keymap.tcl]
::vbindings::init
frame .mid
frame .mid.chatwrap
frame .mid.chatwrap.chat
text .mid.chatwrap.chat.input
set ::vkeymap::persisted_mode 0
namespace eval ::vwhichkey { proc hide {} {} }
::vkeymap::use normie
::vtranslate::close
pack .source
update
focus -force .source
update
event generate .source <KeyPress-F11>
update
ok "F11 translates the focused text selection" {
    [winfo exists .vtranslate.text] && $::vtranslate::source eq "Hello,"
}
{*}$on_err "HTTP 400: no dedicated translator configured"
ok "an old response cannot overwrite a newer request" {
    ![string match "*no dedicated translator*" [.vtranslate.text get 1.0 end]]
}
::vtranslate::request
lassign $::vapi::request path body on_err on_ok
{*}$on_err "HTTP 400: no dedicated translator configured"
ok "configuration failures are visible" {
    [string match "*no dedicated translator*" [.vtranslate.text get 1.0 end]] ||
    [string match "*no dedicated translator*" [.vtranslate.bar.route cget -text]]
}
::vtranslate::request
set ::http::resets {}
::vtranslate::close
ok "closing resets the in-flight request" {$::http::resets eq {token}}
ok "the request state is cleared on close" {
    $::vtranslate::pending eq "" && $::vtranslate::pending_generation == 0
}
::vtranslate::popup .source
set ::http::resets {}
::vtranslate::cancel
ok "Cancel stops the request but leaves the popup usable" {
    $::http::resets eq {token} && [winfo exists .vtranslate] &&
    [.vtranslate.text get 1.0 end-1c] eq {Translation cancelled.} &&
    ![.vtranslate.bar.retry instate disabled]
}
namespace eval ::voverlay {
    variable opened ""
    proc open {which} { set ::voverlay::opened $which }
}
namespace eval ::vsettings {
    proc render {w} { set ::settings_rendered $w }
}
toplevel .ov_settings
ttk::frame .ov_settings.bar
ttk::entry .ov_settings.bar.q
pack .ov_settings.bar.q
.vtranslate.bar.settings invoke
ok "Configure opens the translation settings, not an arbitrary provider" {
    $::voverlay::opened eq "settings" &&
    [.ov_settings.bar.q get] eq "translation"
}
destroy .vtranslate

# Configuration responses belong to the popup that requested them. The
# callbacks below are delayed until explicitly answered, like real HTTP.
rename ::vapi::get_json_async ::vapi::_real_get_json_async_for_test
proc ::vapi::get_json_async {path callback} {
    lappend ::config_callbacks $callback
}
proc answer_config {response} {
    set callback [lindex $::config_callbacks end]
    uplevel #0 [list {*}$callback 1 $response]
}
set ::config_callbacks {}
set ::vtranslate::configured 0
set ::vtranslate::config_pending 0
set ::vtranslate::target ""
set ::vapi::request {}
::vtranslate::popup .source
set first_config [lindex $::config_callbacks end]
ok "the configuration callback captures this popup's generation" {
    [lindex $first_config 1] == $::vtranslate::popup_generation
}
answer_config [dict create target_language French configured false]
ok "an unconfigured translator offers a visible setup path" {
    [string match "*Configure translator*" [.vtranslate.bar.route cget -text]] &&
    $::vtranslate::target eq "French" && !$::vtranslate::configured
}
set ::vtranslate::target French
answer_config [dict create target_language German configured true]
ok "a config response cannot replace an explicit target" {
    $::vtranslate::target eq "French"
}
::vtranslate::request
set ::vapi::request {}
uplevel #0 [list {*}$first_config 1 [dict create target_language German configured true]]
ok "a superseded configuration callback cannot change the target" {
    $::vtranslate::target eq "French"
}
ok "a superseded configuration callback cannot start an extra translation" {
    [llength $::vapi::request] == 0
}
set failed_config [lindex $::config_callbacks end]
uplevel #0 [list {*}$failed_config 0 {HTTP 502: configuration failed}]
ok "a failed configuration read is visible" {
    [string match "*configuration failed*" [.vtranslate.bar.route cget -text]] &&
    !$::vtranslate::configured
}
::vtranslate::cancel
::vtranslate::popup .source
set old_config [lindex $::config_callbacks end]
::vtranslate::popup .source
set new_config [lindex $::config_callbacks end]
uplevel #0 [list {*}$old_config 1 [dict create target_language German configured true]]
ok "a replaced popup ignores its old configuration response" {
    $::vtranslate::target ne "German"
}
uplevel #0 [list {*}$new_config 1 [dict create target_language French configured true]]
ok "the replacement popup accepts its own configuration" {
    $::vtranslate::target eq "French" && $::vtranslate::configured
}
::vtranslate::close
# No configured target: the desktop's own display language, so a fresh
# setup translates into what the reader reads.
set ::vtranslate::target ""
::vtranslate::popup .source
answer_config [dict create target_language "" configured true]
ok "an unset target defaults to the desktop language" {$::vtranslate::target eq "English"}
::vtranslate::close
::vi18n::use de
set ::vtranslate::target ""
::vtranslate::popup .source
answer_config [dict create target_language "" configured true]
ok "…in that language's own name" {$::vtranslate::target eq "Deutsch"}
::vtranslate::close
::vi18n::use en

# The popup's real layout (SMOKE-9, TR-2): every button is MAPPED and lies
# inside the window at the desktop sizes and at a phone's size, with a long
# route text, configured or not. Measured on the live widgets.
proc inside {top w} {
    expr {[winfo ismapped $w] &&
        [winfo rootx $w] >= [winfo rootx $top] &&
        [winfo rooty $w] >= [winfo rooty $top] &&
        [winfo rootx $w] + [winfo width $w] <= [winfo rootx $top] + [winfo width $top] &&
        [winfo rooty $w] + [winfo height $w] <= [winfo rooty $top] + [winfo height $top]}
}
proc all_inside {} {
    set lost {}
    foreach b {target lang retry cancel copy settings close route} {
        if {![inside .vtranslate .vtranslate.bar.$b]} { lappend lost $b }
    }
    return $lost
}
ok "the popup's size is the usual one on a desktop screen and fits a phone screen" {
    [::vtranslate::geometry 1920 1080] eq "760x520" &&
    [::vtranslate::geometry 360 640] eq "344x520" &&
    [::vtranslate::geometry 320 480] eq "304x384"
}
set long "No dedicated translator configured. Use Configure translator to choose its provider, model and target language. And then some more words to make the route text longer than any bar."
foreach {label wxh configured} {
    default-configured {} 1
    default-unconfigured {} 0
    1080x720 1080x720 1
    1440x1000 1440x1000 0
    phone-360x640 360x640 1
    phone-unconfigured 360x640 0
} {
    set ::vtranslate::target ""
    ::vtranslate::popup .source
    answer_config [dict create target_language German configured [expr {$configured ? "true" : "false"}]]
    if {$wxh ne {}} { wm geometry .vtranslate $wxh }
    .vtranslate.bar.route configure -text $long
    update
    update idletasks
    ok "$label: every bar widget is mapped and inside the window ([all_inside])" {[all_inside] eq {}}
    ok "$label: the result pane is mapped and inside" {[inside .vtranslate .vtranslate.text]}
    if {$wxh eq {}} {
        ok "$label: the popup is at its own default size" {
            [winfo width .vtranslate] == 760 && [winfo height .vtranslate] == 520
        }
    }
    ::vtranslate::close
}

# TR-3: an unconfigured translator sends nothing and keeps its guidance.
set ::vapi::request {}
set ::vtranslate::target ""
::vtranslate::popup .source
answer_config [dict create target_language German configured false]
ok "unconfigured: no request is sent" {[llength $::vapi::request] == 0}
ok "unconfigured: the route line keeps the setup guidance" {
    [string match "No dedicated translator configured*" [.vtranslate.bar.route cget -text]]
}
ok "unconfigured: the result pane carries the guidance too" {
    [string match "No dedicated translator configured*" [.vtranslate.text get 1.0 end-1c]]
}
ok "unconfigured: Translate stays usable" {![.vtranslate.bar.retry instate disabled]}
::vtranslate::close

# A failed request leaves the route line alone (the error is in the pane).
set ::vtranslate::target ""
::vtranslate::popup .source
answer_config [dict create target_language German configured true]
.vtranslate.bar.route configure -text "Dedicated translator: m @ p"
lassign $::vapi::request path body on_err on_ok
{*}$on_err "HTTP 502: upstream down"
ok "a failure is written in the result pane" {
    [string match "*upstream down*" [.vtranslate.text get 1.0 end-1c]]
}
ok "a failure leaves the route line as it was" {
    [.vtranslate.bar.route cget -text] eq "Dedicated translator: m @ p"
}

# TR-1: a placeholder the translator lost is shown, not hidden.
{*}$on_ok [dict create text "Hallo!" model m provider p warnings [list "placeholder lost: {name}" "placeholder lost: %s"]]
ok "a lost placeholder is named in the route line" {
    [string match "*Warning: placeholder lost: {name}; placeholder lost: %s*" [.vtranslate.bar.route cget -text]]
}
{*}$on_ok [dict create text "Hallo!" model m provider p warnings {}]
ok "no warning when nothing was lost" {
    ![string match "*Warning*" [.vtranslate.bar.route cget -text]]
}

# Return in the target field translates into what was typed.
set ::vapi::request {}
.vtranslate.bar.lang delete 0 end
.vtranslate.bar.lang insert end French
focus -force .vtranslate.bar.lang
update
set before [llength $::config_callbacks]
event generate .vtranslate.bar.lang <Return>
update
ok "Return in the target field starts a request" {[llength $::config_callbacks] == $before + 1}
answer_config [dict create target_language German configured true]
ok "…into the language typed" {
    [llength $::vapi::request] > 0 && [string match "*French*" [lindex $::vapi::request 1]]
}
::vtranslate::close

# A slow translator (L3): the popup posted with the desktop client's
# default 10 s timeout, so a translation the engine was still working on
# (and the 0.2.2 retry doubles a slow one) ended in the popup as
# "transport: timeout". The request waits the engine's own budget, which
# /api/translate/config states (1200 s: two calls of 600 s), and a real
# timeout says so in words. The stubbed answer below arrives after
# `elapsed` ms: within the request's timeout it is delivered, past it the
# client reports what http::geturl -timeout does.
puts "\na slow translator"
proc slow_answer {elapsed response} {
    set ms $::vapi::ms_given
    if {$ms <= 0} { set ms $::vapi::timeout }
    lassign $::vapi::request path body on_err on_ok
    if {$elapsed > $ms} {
        {*}$on_err "transport: timeout"
    } else {
        {*}$on_ok $response
    }
}
set ::vtranslate::target ""
::vtranslate::popup .source
answer_config [dict create target_language German configured true timeout_secs 1200]
slow_answer 45000 [dict create text "Hallo (langsam)" model m provider p]
ok "a translation that takes 45 s arrives" {
    [.vtranslate.text get 1.0 end-1c] eq "Hallo (langsam)"
}
ok "…the request waits the engine's budget" {$::vapi::ms_given >= 1200000}
::vtranslate::request
answer_config [dict create target_language German configured true timeout_secs 1200]
slow_answer 1300000 {}
ok "past the budget the popup says the translator did not answer in time" {
    [string match "*did not answer within 1200 seconds*" [.vtranslate.text get 1.0 end-1c]]
}
::vtranslate::close
set ::vtranslate::target ""
::vtranslate::popup .source
answer_config [dict create target_language German configured true]
ok "a gateway that states no budget gets the same 1200 s" {$::vapi::ms_given >= 1200000}
::vtranslate::close

rename ::vapi::get_json_async {}
rename ::vapi::_real_get_json_async_for_test ::vapi::get_json_async
::vtranslate::close

puts "\ntranslatetest: $passed passed, $failed failed"
exit [expr {$failed > 0}]
