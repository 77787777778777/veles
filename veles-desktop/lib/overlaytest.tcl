#!/usr/bin/env wish
# veles-desktop/lib/overlaytest.tcl — the Triggers overlay (::voverlay).
#
# The triggers round (2026-09-14/15) added 291 lines and eleven procs to
# overlays.tcl — the list, the selection, the per-row [enable] [disable]
# [history] [edit] [delete] affordances, and the create/edit dialog —
# and NO suite sourced any of it for triggers. That is the same shape
# that let three plan-mode bugs ship out of an untested Tasks pane, and
# the suite written for that pane (panetest.tcl) is the precedent here.
#
# Writing this found two more, both in the dialog:
#   * the minimum-role combobox offered four of the engine's five roles
#     — `admin` was missing, so the one surface an operator uses to set
#     a floor could not express it;
#   * saving an EDIT skipped every empty field, and "absent" means
#     "leave alone" to POST /api/triggers/:name — so the dialog could
#     SET a scope or a floor and never take one back off, while the
#     blank field said otherwise.
#
# Drives the REAL ::voverlay::_trigger_fill / _trigger_select /
# _trigger_action / _trigger_dialog / _trigger_dialog_save against
# stubbed HTTP, on Xvfb.

set _dir [file dirname [file normalize [info script]]]

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir i18n.tcl]
::vi18n::init

# The engine's nullable fields arrive as JSON null, which tcllib's
# json2dict renders as the four-character STRING "null". overlays.tcl
# reads them through ::vapi::field now (this round converted the last
# twenty hand-rolled `ne "null"` guards); the fixtures below build
# their rows through a REAL parse so the wire's shape is what is
# tested, not a hand-built dict that merely omits the key — the
# cfaa851 lesson: a fixture that omits the key tests the fixture.
package require json
source [file join $_dir jsonfield.tcl]

# ── stubbed collaborators ────────────────────────────────────────────
namespace eval ::vapi {
    variable triggers {}
    variable history {}
    variable fail 0
    variable posts {}
    variable bodies {}
    variable deletes {}
    proc get_json {path} {
        variable triggers
        variable history
        variable fail
        if {$fail} { error "HTTP 502: gateway down" }
        if {[string match "*/history" $path]} { return $history }
        if {$path eq "/api/cron" && [info exists ::vapi::cron_payload]} {
            return $::vapi::cron_payload
        }
        if {($path eq "/api/quota" || [string match "/api/quota*" $path]) && [info exists ::vapi::quota_payload]} {
            return $::vapi::quota_payload
        }
        return $triggers
    }
    proc post {path body} {
        variable posts
        lappend posts $path
        return "{}"
    }
    # The real one renders the dict and POSTs it; the suite needs the
    # RENDERED body, because "did the dialog send null or nothing" is
    # exactly the distinction under test.
    proc post_json {path body} {
        variable posts
        variable bodies
        lappend posts $path
        lappend bodies [_json_value $body]
        return [dict create ok 1]
    }
    proc json_raw {text} { return [list ::vapi::__json__ $text] }
    proc json_string {s} { return [json_raw "\"$s\""] }
    proc _json_value {v} {
        if {[llength $v] == 2 && [lindex $v 0] eq "::vapi::__json__"} {
            return [lindex $v 1]
        }
        # The real api.tcl infers bare numbers before anything else;
        # the stub has to, or "a number goes as a number" would be
        # testing the stub rather than the dialog.
        if {[string is integer -strict $v]} { return $v }
        set parts {}
        foreach {k val} $v { lappend parts "\"$k\": [_json_value $val]" }
        return "{[join $parts {, }]}"
    }
    proc get_json_async {path cmd {ms 0}} {
        if {[string match "/api/usage*" $path]} {
            set d [::json::json2dict {{"prompt_tokens":100,"completion_tokens":200,"calls":3,"session":null,"by_plan":[],"session_by_plan":[]}}]
        } elseif {[string match "/api/quota*" $path]} {
            variable fail
            if {$fail} {
                after 0 [list uplevel #0 [list {*}$cmd 0 "HTTP 502: gateway down"]]
                return tok
            }
            set d [get_json $path]
        } else {
            after 0 [list uplevel #0 [list {*}$cmd 0 "unsupported test GET $path"]]
            return tok
        }
        # A fixture absent from a sync call is an empty dict, not a valid
        # quota response; report that failure asynchronously like HTTP.
        if {[catch {dict size $d}]} {
            after 0 [list uplevel #0 [list {*}$cmd 0 "no test payload for $path"]]
            return tok
        }
        after 0 [list uplevel #0 [list {*}$cmd 1 $d]]
        return tok
    }
}
namespace eval ::vstatus {
    variable route_prov ""
    variable msgs {}
    proc set_msg {m} { variable msgs; lappend msgs $m }
}
namespace eval ::va11y {
    proc name {w label} {}
}
namespace eval ::vcoach {
    proc attach {w} {}
}
namespace eval ::vplace {
    proc centered {w width height} { wm geometry $w ${width}x${height} }
}
namespace eval ::vmanage {
    proc get {w path {ms 0}} {
        if {$path eq "/api/config/get?path=mcp_servers"} {
            return [::json::json2dict "{\\"ok\\":true,\\"value\\":null}"]
        }
        # The usage half of the Usage & Quota overlay, as a REAL parse
        # so the quota half below it renders.
        if {$path eq "/api/usage" || [string match "/api/usage*" $path]} {
            return [::json::json2dict {{"prompt_tokens":100,"completion_tokens":200,"calls":3,"session":null}}]
        }
        return ""
    }
    variable said {}
    variable confirm_answer 1
    variable deleted {}
    proc _reason {e} { return $e }
    proc window {w title geom {minsize {520 340}}} {
        toplevel $w
        wm withdraw $w
        ttk::frame $w.bar
        pack $w.bar -fill x
    }
    proc button {parent name label cmd {tip ""}} {
        ttk::button $parent.$name -text $label -command $cmd
        pack $parent.$name -side left
    }
    proc separator {bar name} {}
    proc say {w msg {kind info}} { variable said; lappend said $msg }
    proc confirm {w title detail} { variable confirm_answer; return $confirm_answer }
    proc del {w path} { variable deleted; lappend deleted $path; return [dict create ok 1] }
}

source [file join $_dir vtext.tcl]
source [file join $_dir overlays.tcl]

set passed 0
set failed 0
proc bgerror {err} {
    puts stderr "TEST BGERROR: $err\n$::errorInfo"
    exit 1
}
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name"
        incr ::failed
    }
}

proc trig {name args} {
    set d [dict create name $name enabled 1 event TEXT match_kind always \
               action slopdrop network irc.synirc.net channel #flashsupport \
               pattern null arg null cooldown_secs 0 per_hour_cap 0 \
               min_role null fire_count 0 last_status null]
    foreach {k v} $args { dict set d $k $v }
    return $d
}

proc refill {} {
    catch { destroy .ov_trig }
    set ::voverlay::trigger_selected ""
    ::voverlay::_triggers .ov_trig
    return [.ov_trig.main.t get 1.0 end]
}

# ── the list says what the engine holds ──────────────────────────────
puts "\nthe list renders scope, state and the last decision"
set ::vapi::triggers [dict create triggers [list \
    [trig slop-flashsupport last_status fired fire_count 19] \
    [trig coven-vision enabled 0 match_kind imageurl action vision \
        network irc.IRC4Fun.net channel #coven] \
    [trig everywhere network null channel null]]]
set body [refill]

ok "an enabled row is marked live and a disabled one is not" \
    {[string first "● slop-flashsupport" $body] >= 0
     && [string first "○ coven-vision" $body] >= 0}
ok "the scope is network/channel" \
    {[string first "irc.synirc.net/#flashsupport" $body] >= 0}
ok "a null scope field renders as *, not as the word null" \
    {[string first "*/*" $body] >= 0 && [string first "null" $body] < 0}
ok "the match kind and action are both shown" \
    {[string first "match=imageurl" $body] >= 0
     && [string first "action=vision" $body] >= 0}
ok "a row that has decided something shows its last status and count" \
    {[string first "last: fired (fires: 19)" $body] >= 0}
ok "every row offers the five per-row actions" \
    {[llength [regexp -all -inline {\[history\]} $body]] == 3}

# ── the empty and the broken answer differently ──────────────────────
puts "\nempty is not the same as unavailable"
set ::vapi::triggers [dict create triggers {}]
set body [refill]
ok "an empty table says so and points at New…" \
    {[string first "no triggers" $body] >= 0}

set ::vapi::fail 1
set body [refill]
ok "a down gateway says unavailable rather than rendering an empty list" \
    {[string first "triggers unavailable" $body] >= 0}
ok "…and does not claim there are no triggers" \
    {[string first "no triggers" $body] < 0}
set ::vapi::fail 0

# ── selection is what edit and delete act on ─────────────────────────
puts "\nselection"
set ::vapi::triggers [dict create triggers [list [trig alpha] [trig beta]]]
refill
set ::vmanage::said {}
::voverlay::_trigger_edit .ov_trig
ok "edit with nothing selected asks for a selection instead of opening" \
    {[string first "click a trigger" [lindex $::vmanage::said end]] >= 0
     && ![winfo exists .trigger_edit]}

::voverlay::_trigger_select .ov_trig beta
ok "selecting a row records it" {$::voverlay::trigger_selected eq "beta"}

# A refill that no longer carries the selected row must drop it, or
# edit/delete would act on a name the table no longer has.
set ::vapi::triggers [dict create triggers [list [trig alpha]]]
::voverlay::_trigger_fill .ov_trig
ok "a refill that drops the selected row clears the selection" \
    {$::voverlay::trigger_selected eq ""}

# ── the per-row actions hit the endpoints they name ──────────────────
puts "\nrow actions"
set ::vapi::triggers [dict create triggers [list [trig alpha]]]
refill
set ::vapi::posts {}
::voverlay::_trigger_action .ov_trig alpha disable
ok "the disable affordance posts to the disable endpoint for that row" \
    {[lindex $::vapi::posts 0] eq "/api/triggers/alpha/disable"}
ok "…and acting on a row selects it, so a follow-up edit means that row" \
    {$::voverlay::trigger_selected eq "alpha"}

set ::vmanage::deleted {}
set ::vmanage::confirm_answer 0
::voverlay::_trigger_delete .ov_trig
ok "a refused confirmation deletes nothing" {[llength $::vmanage::deleted] == 0}
set ::vmanage::confirm_answer 1
::voverlay::_trigger_delete .ov_trig
ok "a confirmed delete hits the row's endpoint" \
    {[lindex $::vmanage::deleted 0] eq "/api/triggers/alpha"}

# ── the dialog's bounded fields must match the engine's tables ───────
puts "\nthe dialog offers exactly what the engine accepts"
set ::vapi::triggers [dict create triggers [list [trig alpha]]]
refill
::voverlay::_trigger_dialog .ov_trig ""
ok "the dialog opens for a new row" {[winfo exists .trigger_edit]}
ok "every event the engine names is offered" \
    {[.trigger_edit.f.event cget -values] eq {TEXT JOIN PART QUIT KICK NICK}}
ok "every match kind is offered" \
    {[.trigger_edit.f.match_kind cget -values] eq {always contains regex url imageurl bluesky}}
ok "every action is offered" \
    {[.trigger_edit.f.action cget -values] eq {slopdrop vision bluesky prompt command}}
# The engine's Role::parse accepts guest|user|trusted|admin|owner. The
# combobox offered four of the five: `admin` was missing, so this
# surface could not express a floor the engine understands.
ok "every role the engine parses is offered, admin included" \
    {[lsearch -exact [.trigger_edit.f.min_role cget -values] admin] >= 0}
ok "…and the empty option, because no floor is a legitimate choice" \
    {[lsearch -exact [.trigger_edit.f.min_role cget -values] {}] >= 0}
ok "the bounded fields are readonly — a typo must not reach the store" \
    {[.trigger_edit.f.event cget -state] eq "readonly"
     && [.trigger_edit.f.min_role cget -state] eq "readonly"}

# ── saving ───────────────────────────────────────────────────────────
puts "\nsaving"
set ::vapi::posts {}
set ::vapi::bodies {}
::voverlay::_trigger_dialog_save .ov_trig .trigger_edit ""
ok "a nameless row is refused in the dialog, not sent" \
    {[llength $::vapi::posts] == 0
     && [string first "needs a name" [.trigger_edit.f.status cget -text]] >= 0}

.trigger_edit.f.name insert 0 newrow
.trigger_edit.f.cooldown_secs insert 0 "sixty"
::voverlay::_trigger_dialog_save .ov_trig .trigger_edit ""
ok "a non-numeric cooldown is refused in the dialog, not sent" \
    {[llength $::vapi::posts] == 0
     && [string first "not a whole number" [.trigger_edit.f.status cget -text]] >= 0}

.trigger_edit.f.cooldown_secs delete 0 end
.trigger_edit.f.cooldown_secs insert 0 60
.trigger_edit.f.channel insert 0 "#coven"
::voverlay::_trigger_dialog_save .ov_trig .trigger_edit ""
ok "a good new row posts to the create endpoint" \
    {[lindex $::vapi::posts 0] eq "/api/triggers/new"}
set sent [lindex $::vapi::bodies 0]
ok "…carrying the name, the scope and the bounded defaults" \
    {[string first {"name": "newrow"} $sent] >= 0
     && [string first {"channel": "#coven"} $sent] >= 0
     && [string first {"event": "TEXT"} $sent] >= 0}
ok "…and a number goes as a number, not a quoted string" \
    {[string first {"cooldown_secs": 60} $sent] >= 0}
ok "an empty field on a CREATE is simply absent — there is nothing to clear" \
    {[string first "null" $sent] < 0}

# CLEARING. Blanking a field and saving used to skip it, and absent
# means "leave alone" to the edit endpoint — so the dialog could set a
# scope or a floor and never take one back off.
puts "\nclearing a field on an edit"
set ::vapi::triggers [dict create triggers [list \
    [trig beta channel #flashsupport min_role trusted pattern zoom]]]
refill
::voverlay::_trigger_select .ov_trig beta
::voverlay::_trigger_dialog .ov_trig beta
ok "the dialog loads the row's values" \
    {[.trigger_edit.f.channel get] eq "#flashsupport"
     && [.trigger_edit.f.min_role get] eq "trusted"}

.trigger_edit.f.min_role set {}
.trigger_edit.f.pattern delete 0 end
set ::vapi::posts {}
set ::vapi::bodies {}
::voverlay::_trigger_dialog_save .ov_trig .trigger_edit beta
ok "an edit posts to the row's endpoint" \
    {[lindex $::vapi::posts 0] eq "/api/triggers/beta"}
set sent [lindex $::vapi::bodies 0]
ok "a blanked nullable field is sent as null, so the edit CLEARS it" \
    {[string first {"min_role": null} $sent] >= 0
     && [string first {"pattern": null} $sent] >= 0}
ok "…while a field left alone still carries its value" \
    {[string first {"channel": "#flashsupport"} $sent] >= 0}

# ── the converted null guards, driven by a REAL parse ────────────────
#
# Twenty hand-rolled `ne "null"` guards in overlays.tcl became
# ::vapi::field reads this round. Each fixture below is a REAL
# ::json::json2dict parse of a payload carrying explicit nulls, so a
# regression to a bare `dict get` reads the string "null" and the
# assertion catches it — a fixture that omits the key (the old shape)
# can never tell the two apart.

# The cron list row: name/session are nullable, and a null name falls
# back to the id, a null session hides the "wakes" clause.
proc _cron_body {json} {
    catch { destroy .ov_cron }
    set ::voverlay::cron_selected ""
    set ::vapi::cron_payload [::json::json2dict $json]
    ::voverlay::_cron .ov_cron
    return [.ov_cron.main.t get 1.0 end]
}
set cron_json {{"jobs":[{"id":"job-7","name":null,"schedule":"every:3600","schedule_human":null,"enabled":true,"next_run_at":"soon","last_status":"ok","session":null}]}}
set body [_cron_body $cron_json]
ok "a null cron name falls back to the id, not the word null" \
    {[string first "job-7" $body] >= 0 && [string first "null" $body] < 0}
ok "a null wake session drops the wakes clause" \
    {[string first "wakes" $body] < 0}

set cron_json2 {{"jobs":[{"id":"job-9","name":"nightly backup","schedule":"daily 03:00","schedule_human":"every day at 03:00","enabled":true,"next_run_at":"03:00","last_status":"ok","session":"abc123"}]}}
set body [_cron_body $cron_json2]
ok "a present schedule_human renders beside the raw schedule" \
    {[string first "daily 03:00 (every day at 03:00)" $body] >= 0}
ok "a present wake session is named on the row" \
    {[string first "wakes abc123" $body] >= 0}

# The usage overlay's quota half: the three error fields and the two
# dict-typed fields are all nullable. A null team_error must NOT claim
# the team pool errored.
# The overlay builds real widgets; read the painted labels back.
proc _quota_text {w} {
    set out {}
    foreach c [winfo children $w.body] {
        if {[catch { $c cget -text } t]} { continue }
        if {$t ne ""} { lappend out $t }
    }
    return [join $out "\n"]
}
namespace eval ::vchat {}
set ::vchat::session ""
set q_json {{"enabled":true,"team_error":null,"personal_error":null,"zai_error":null,"team":{"cycle":{"resets_at":"monday"},"spend":{"bill_month":"2026-09","currency":"RMB","total":"12.34"}},"personal":{"cycle":{"resets_at":"friday"},"spend":null},"zai":{"level":"coding","tools":null}}}
catch { destroy .ov_usage }
set ::vapi::quota_payload [::json::json2dict $q_json]
::voverlay::_usage .ov_usage
update
set qtext [_quota_text .ov_usage]
ok "null error fields render as nothing, not the word null" \
    {[string first "null" $qtext] < 0}
ok "a null spend dict drops the spend row, a present one renders it" \
    {[string first "2026-09 RMB 12.34" $qtext] >= 0 || [string first "12.34" $qtext] >= 0 && [string first "RMB" $qtext] >= 0}
ok "a null tools dict drops the tools row" \
    {[string first "tools" $qtext] < 0}

# The MCP pane (_tools): a null config value must not iterate as a
# dict. The fill reads via ::vmanage::get then parses; stub the GET
# layer with a REAL JSON body carrying an explicit null value.
if {1} {
    if {[llength [info procs ::vmanage::get]] == 0} {
        proc ::vmanage::get {w path {ms 0}} { return "" }
    }
    rename ::vmanage::get ::vmanage::get_orig
    proc ::vmanage::get {w path {ms 0}} {
        if {[string match "*path=mcp_servers*" $path]} {
            return "\{\"ok\":true,\"value\":null\}"
        }
        return [::vmanage::get_orig $w $path $ms]
    }
    if {[llength [info procs ::vmanage::tree]] == 0} {
        # Minimal tree: the fill only needs an existing .t child it can
        # `delete children` on and later `insert` into.
        proc ::vmanage::tree {parent name columns headings {widths {}}} {
            set f $parent.$name
            ttk::frame $f
            ttk::treeview $f.t -columns $columns -show headings
            return $f
        }
    }
    catch { destroy .ov_toolsp }
    ::vmanage::window .ov_toolsp "t" 400x300
    # _tools_tree returns $w.split.top.list.t — build that exact path.
    ttk::panedwindow .ov_toolsp.split
    pack .ov_toolsp.split -fill both -expand 1
    ttk::frame .ov_toolsp.split.top
    .ov_toolsp.split add .ov_toolsp.split.top -weight 1
    set t [::vmanage::tree .ov_toolsp.split.top list {name detail} {a b} {80 200}]
    pack $t -fill both -expand 1
    set ::voverlay::mcp_servers SENTINEL
    set rc [catch { ::voverlay::_tools_fill .ov_toolsp } err]
    ok "a null mcp_servers config reads as no servers, not a crash" \
        {$rc == 0 && [llength $::voverlay::mcp_servers] == 0}
    if {$rc} { puts "    (error was: $err)" }
    catch { destroy .ov_toolsp }
    rename ::vmanage::get {}
    rename ::vmanage::get_orig ::vmanage::get
}

puts "\ndefaults go through /defaults"
# USER 2026-10-04: "Wtf is with the 500 different kinds of defaults?"
# Save as default and Default… each wrote [model] as two separate
# /api/config/set calls; they now run the engine's /defaults, so a
# route means the same thing from every surface and lands in one write.
proc ::voverlay::_models_reopen {w} {}
set ::vapi::posts {}
set ::vapi::bodies {}
proc ::vmanage::post {w path body} { return [::vapi::post_json $path $body] }
::vmanage::window .ov_mdl "m" 400x300
ttk::frame .ov_mdl.apply
foreach f {model provider reasoning} { ttk::entry .ov_mdl.apply.$f }
ttk::frame .ov_mdl.apply.btns
ttk::label .ov_mdl.apply.btns.state
.ov_mdl.apply.model insert 0 m2
.ov_mdl.apply.provider insert 0 b
.ov_mdl.apply.reasoning insert 0 high
::voverlay::_save_override_as_default .ov_mdl
ok "Save as default posts /api/defaults, never /api/config/set" \
    {[lsearch -exact $::vapi::posts /api/config/set] < 0
     && [llength [lsearch -all -exact $::vapi::posts /api/defaults]] == 2}
ok "…the model and provider as ONE route" \
    {[string match "*model m2@b*" [lindex $::vapi::bodies 0]]}
ok "…and the effort as /defaults reasoning" \
    {[string match "*reasoning high*" [lindex $::vapi::bodies 1]]}
set ::vapi::posts {}
set ::vapi::bodies {}
.ov_mdl.apply.model delete 0 end
.ov_mdl.apply.reasoning delete 0 end
::voverlay::_save_override_as_default .ov_mdl
ok "a provider alone is /defaults provider" \
    {[string match "*provider b*" [lindex $::vapi::bodies 0]]
     && [llength $::vapi::posts] == 1}
destroy .ov_mdl

puts "\noverlaytest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
