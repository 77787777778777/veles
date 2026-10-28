# veles-desktop/lib/settingsformtest.tcl — the All settings page
# (lib/settingsform.tcl, PLAN-sessions-and-phone.md round P2): the real
# window, fed a /api/config/form payload the gateway's route produces,
# with the wire recorded. What each kind of option draws, what each
# change sends back (typed JSON, `raw` for text nothing typed), and what
# the page then says.
#
# The gateway's half — the form, typed by the TOML library, and `raw` read
# as a literal — is pinned in Rust (`settings_form::tests`,
# `config_set_reads_raw_text_as_a_toml_literal_and_the_form_shows_it`).
#
# Run: scripts/tcl_tests.sh

package require Tk
package require json
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

source [file join $_dir paths.tcl]
source [file join $_dir i18n.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]
namespace eval ::voverlay { proc _header {w title} {} }
namespace eval ::va11y { proc name {w t} {} }
# The real scrollframe: what the page packs into.
source [file join $_dir manage.tcl]

# ── the wire, recorded ───────────────────────────────────────────────
set ::posts {}
set ::post_reply {{"saved":"x","effect":"new_sessions","note":"n"}}
set ::post_fail ""
set ::get_fail ""
# What /api/config/get says a path holds after a write.
set ::stored [dict create]
proc ::vapi::post {path body {ms 0}} {
    lappend ::posts [list $path $body]
    if {$::post_fail ne ""} { error $::post_fail }
    set b [::json::json2dict $body]
    if {[dict exists $b value]} { dict set ::stored [dict get $b path] [dict get $b value] }
    if {[dict exists $b raw]} { dict set ::stored [dict get $b path] [dict get $b raw] }
    return $::post_reply
}
proc ::vapi::get_json {path} {
    if {$path eq "/api/config/form"} {
        if {$::get_fail ne ""} { error $::get_fail }
        return [::json::json2dict $::form_json]
    }
    if {[string match /api/config/get?path=* $path]} {
        set p [string range $path [string length /api/config/get?path=] end]
        return [dict create set true value [dict get $::stored $p]]
    }
    error "unexpected GET $path"
}
proc last_body {} { ::json::json2dict [lindex $::posts end 1] }
# A key goes to the focused widget, as a finger's Enter does.
proc press_return {e} {
    focus -force $e
    update
    event generate $e <Return>
    update
}

# A form as the route builds it: two sections, every kind.
set ::form_json {{"sections":[
 {"name":"agent.guards","knobs":[
  {"path":"agent.guards.max_iterations","key":"max_iterations","kind":"int","default":90,"value":42,"set":true,"effect":"new_sessions","note":null,"env":null},
  {"path":"agent.guards.verification_nudge","key":"verification_nudge","kind":"bool","default":true,"value":null,"set":false,"effect":"new_sessions","note":null,"env":null}]},
 {"name":"tui","knobs":[
  {"path":"tui.ratio","key":"ratio","kind":"float","default":0.5,"value":null,"set":false,"effect":"live","note":null,"env":null},
  {"path":"tui.theme","key":"theme","kind":"string","default":"dark","value":null,"set":false,"effect":"live","note":null,"env":null},
  {"path":"tui.extra","key":"extra","kind":"list","default":[],"value":["a","b"],"set":true,"effect":"restart_gateway","note":null,"env":null},
  {"path":"tui.width","key":"width","kind":"text","default":null,"value":null,"set":false,"effect":"live","note":null,"env":null}]}
]}}

source [file join $_dir settingsform.tcl]
set w [::vsettings::build .ov_settings]
update
set b $::vsettings::body
proc row_for {path} {
    foreach r [winfo children $::vsettings::body] {
        if {[$r.name cget -text] in [list $path [lindex [split $path .] end]]} { return $r }
    }
    return ""
}

# ── one section at a time ────────────────────────────────────────────
ok "the sections are the picker's choices" \
    {[.ov_settings.bar.section cget -values] eq {agent.guards tui}}
ok "the first section is shown" {[llength [winfo children $b]] == 2}
set r [row_for agent.guards.max_iterations]
ok "a number shows the value config.toml holds" {[$r.c get] eq "42"}
ok "its default is named" {[$r.d cget -text] eq "default: 90"}
set rb [row_for agent.guards.verification_nudge]
ok "a switch unset shows its default" {$::vsettings::bool(agent.guards.verification_nudge) == 1}

# ── each kind sends typed JSON ───────────────────────────────────────
set ::posts {}
$rb.c invoke
ok "a switch sends false" {[lindex $::posts 0 1] eq {{"path":"agent.guards.verification_nudge","value":false}}}
ok "and the page says when it takes effect" \
    {[.ov_settings.status cget -text] eq "agent.guards.verification_nudge: applies to new sessions; open ones keep the old value"}

set ::posts {}
$r.c delete 0 end ; $r.c insert 0 lots
press_return $r.c
ok "a number that is not one is not sent" {[llength $::posts] == 0}
ok "and the page says so" {[.ov_settings.status cget -text] eq "not a number"}
$r.c delete 0 end ; $r.c insert 0 75
press_return $r.c
ok "a number goes as a number" {[lindex $::posts 0 1] eq {{"path":"agent.guards.max_iterations","value":75}}}
set ::posts {}
focus -force .ov_settings.bar.q ; update
ok "leaving an unchanged field writes nothing" {[llength $::posts] == 0}

set ::posts {}
$r.reset invoke
ok "reset sends null" {[lindex $::posts 0 1] eq {{"path":"agent.guards.max_iterations","value":null}}}
ok "and the field shows the default again" {[$r.c get] eq "90"}

.ov_settings.bar.section set tui
event generate .ov_settings.bar.section <<ComboboxSelected>>
update
ok "picking a section shows its options" {[llength [winfo children $b]] == 4}

set ::post_reply {{"saved":"x","effect":"live"}}
set ::posts {}
set rf [row_for tui.ratio]
$rf.c delete 0 end ; $rf.c insert 0 0.25
press_return $rf.c
ok "a fraction goes as a number" {[lindex $::posts 0 1] eq {{"path":"tui.ratio","value":0.25}}}
ok "a live option says so" {[.ov_settings.status cget -text] eq "tui.ratio: in effect now"}

set ::post_reply {{"saved":"x","effect":"live"}}
set ::posts {}
set rs [row_for tui.theme]
$rs.c delete 0 end ; $rs.c insert 0 {light "x"}
press_return $rs.c
ok "a string goes as a JSON string, escaped" {[lindex $::posts 0 1] eq {{"path":"tui.theme","value":"light \"x\""}}}

set ::posts {}
set rl [row_for tui.extra]
ok "a list shows comma-separated" {[$rl.c get] eq "a, b"}
ok "an empty default is not shown as \"default:\"" {![winfo exists $rl.d]}
$rl.c delete 0 end ; $rl.c insert 0 {a, c d ,, e}
press_return $rl.c
ok "a list goes as an array, blanks dropped" {[lindex $::posts 0 1] eq {{"path":"tui.extra","value":["a","c d","e"]}}}

set ::posts {}
set rt [row_for tui.width]
$rt.c insert 0 120
press_return $rt.c
ok "text nothing typed goes as raw, for the gateway's TOML reader" \
    {[lindex $::posts 0 1] eq {{"path":"tui.width","raw":"120"}}}

set ::post_reply {{"saved":"x","effect":"restart_gateway"}}
set ::posts {}
$rl.c delete 0 end ; $rl.c insert 0 z
press_return $rl.c
ok "a restart is named" {[.ov_settings.status cget -text] eq "tui.extra: needs a gateway restart"}

set ::post_fail {HTTP 400: {"error":"the edit would make config.toml unreadable: bad"}}
$rt.c delete 0 end ; $rt.c insert 0 999
press_return $rt.c
ok "a refusal is shown in the gateway's words" \
    {[string match "refused: *unreadable*" [.ov_settings.status cget -text]]}
set ::post_fail ""

# ── the picker is legible ────────────────────────────────────────────
::vt::apply_theme
ok "a readonly picker keeps the theme's field, not clam's grey" \
    {[ttk::style lookup TCombobox -fieldbackground readonly] eq $::vt::T(bg_input)
     && [ttk::style lookup TCombobox -selectforeground readonly] eq $::vt::T(fg)}

# ── search across every section ──────────────────────────────────────
.ov_settings.bar.q insert 0 iter
::vsettings::render .ov_settings
ok "a search finds options in any section" \
    {[llength [winfo children $b]] == 1 && [[lindex [winfo children $b] 0].name cget -text] eq "agent.guards.max_iterations"}
.ov_settings.bar.q delete 0 end
::vsettings::render .ov_settings
ok "clearing it shows the section again" {[llength [winfo children $b]] == 4}

# ── no gateway ───────────────────────────────────────────────────────
destroy .ov_settings
set ::get_fail "transport: error (connection refused)"
::vsettings::build .ov_settings
update
ok "unreachable says so" {[string match "unavailable: *" [.ov_settings.status cget -text]]}
destroy .ov_settings

# ── reachable by touch ───────────────────────────────────────────────
source [file join $_dir palette.tcl]
ok "the palette offers it, under settings" \
    {[lindex [lsearch -inline -exact -index 1 [::vpalette::actions] {::voverlay::open settings}] 2] eq "settings"}
set _o [open [file join $_dir overlays.tcl] r]
set _osrc [read $_o]; close $_o
ok "the overlay switch delegates to the module" \
    {[string first {::vsettings::build $w} $_osrc] >= 0}

puts "\nsettingsformtest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
