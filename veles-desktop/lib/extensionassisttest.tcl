#!/usr/bin/env wish
# D1.4: AI assistance proposes a diff; it cannot silently edit an extension.
set _dir [file dirname [file normalize [info script]]]
if {![info exists ::env(XVFB_RUNNING)] && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}
source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir vtext.tcl]
source [file join $_dir extensionguide.tcl]
namespace eval ::vi18n { proc mc {s args} { return $s } }
namespace eval ::va11y { proc name {w d} {} }
namespace eval ::vcoach { proc attach {w} {} }
namespace eval ::vstatus { proc set_msg {m} { set ::status $m } }
namespace eval ::vmanage {
    proc window {w title geometry minsize} {
        toplevel $w
        ttk::frame $w.bar
        pack $w.bar -fill x
    }
    proc button {bar name label cmd {help ""}} {
        ttk::button $bar.$name -text $label -command $cmd
        pack $bar.$name -side left
        return $bar.$name
    }
    proc confirm {w title detail} { return $::confirm_answer }
    proc say {w msg {kind info}} { set ::last_message $msg }
    proc _reason {e} { return $e }
}
source [file join $_dir api.tcl]
namespace eval ::vapi {
    variable preview_calls 0
    variable apply_calls 0
    variable last_wire {}
    variable fail_apply 0
    proc post_json {path body} {
        variable preview_calls
        variable apply_calls
        variable last_wire
        variable last_body
        variable fail_apply
        set last_body $body
        set last_wire [::json::json2dict [::vapi::_json_value $body]]
        if {$path eq "/api/extensions/assist/preview"} {
            incr preview_calls
            return [dict create path /user/plugins/demo/plugin.tcl \
                before {proc demo {} {return old}} \
                after {proc demo {} {return new}} \
                diff "--- a/plugin.tcl\n+++ b/plugin.tcl\n-return old\n+return new" \
                base_sha base-1 proposal_id proposal-1 \
                provider test-provider model test-model \
                api 0.11 checks_run 0 recommended_checks {Run Tcl info complete after applying} extension_first 1]
        }
        if {$path eq "/api/extensions/assist/apply"} {
            incr apply_calls
            if {$fail_apply} {
                return [dict create error {source changed on disk}]
            }
            return [dict create ok 1 path /user/plugins/demo/plugin.tcl sha256 after-1]
        }
        error "unexpected endpoint $path"
    }
    # Model calls are asynchronous with a model's timeout (0.2.0). The
    # stub answers at once, through whatever post_json is at the time,
    # and records the timeout asked for.
    variable async_timeouts {}
    proc post_json_async {path body on_err on_ok {ms 0}} {
        lappend ::vapi::async_timeouts $ms
        if {[catch {::vapi::post_json $path $body} response]} {
            uplevel #0 [list {*}$on_err $response]
        } else {
            uplevel #0 [list {*}$on_ok $response]
        }
    }
}
set failed 0
set passed 0
proc ok {name condition} {
    if {[uplevel 1 [list expr $condition]]} {
        puts "  ✓ $name"; incr ::passed
    } else { puts "  ✗ $name"; incr ::failed }
}
namespace eval ::vdev {
    variable editor_sha
    proc editor_path {} { return /user/plugins/demo/plugin.tcl }
    proc editor_widget {} { return .editor }
    proc _editor_fill {w text} { $w delete 1.0 end; $w insert end $text }
}
text .editor
.editor insert end {proc demo {} {return old}}
set ::vdev::editor_sha(.editor) before-1
set ::vdev::editor_diagnostics(.editor) [list [list 1 0 WARNING {fixture diagnostic}]]
::vdev::extension_assist
ok "a new AI preview cannot apply anything" {
    [.vdev_extension_assist.bar.apply instate disabled]
}
ok "AI assist opens an editable request and a diff preview" {
    [winfo exists .vdev_extension_assist.request] &&
    [winfo exists .vdev_extension_assist.diff] &&
    [winfo exists .vdev_extension_assist.bar.preview]
}
.vdev_extension_assist.request insert end {Make the tool return new}
set ::confirm_answer 0
::vdev::_extension_assist_preview
ok "preview sends the current buffer, path and instruction" {
    $::vapi::preview_calls == 1 &&
    [dict get $::vapi::last_wire path] eq {/user/plugins/demo/plugin.tcl} &&
    [dict get $::vapi::last_wire content] eq {proc demo {} {return old}} &&
    [dict get $::vapi::last_wire instruction] eq {Make the tool return new} &&
    [string match "*fixture diagnostic*" [dict get $::vapi::last_wire diagnostics]]
}
ok "preview leaves the editor unchanged" {
    [.editor get 1.0 end-1c] eq {proc demo {} {return old}}
}
ok "preview displays the proposed diff and route" {
    [string match "*return new*" [.vdev_extension_assist.diff get 1.0 end]] &&
    [string match "*test-provider*" [.vdev_extension_assist.bar.route cget -text]]
}
.vdev_extension_assist.request insert end { with a different instruction}
update
ok "editing the request invalidates the old proposal" {
    [.vdev_extension_assist.bar.apply instate disabled] &&
    $::vdev::assist_preview eq {}
}
.vdev_extension_assist.request delete 1.0 end
.vdev_extension_assist.request insert end {Make the tool return new}
::vdev::_extension_assist_preview
::vdev::_extension_assist_apply
ok "declining apply sends no write" {$::vapi::apply_calls == 0}
set ::confirm_answer 1
::vdev::_extension_assist_apply
ok "confirmed apply updates the editor with the reviewed replacement" {
    $::vapi::apply_calls == 1 &&
    [.editor get 1.0 end-1c] eq {proc demo {} {return new}}
}
ok "confirmed apply sends only the server-held proposal id" {
    [dict keys $::vapi::last_wire] eq {proposal_id} &&
    [dict get $::vapi::last_wire proposal_id] eq {proposal-1}
}
ok "successful apply advances the saved editor hash" {
    $::vdev::editor_sha(.editor) eq {after-1} &&
    [.vdev_extension_assist.bar.apply instate disabled]
}
::vdev::_extension_assist_preview
.editor delete 1.0 end
.editor insert end {proc demo {} {return local edit}}
::vdev::_extension_assist_apply
ok "a changed buffer cannot apply an old proposal" {
    $::vapi::apply_calls == 1 &&
    [.editor get 1.0 end-1c] eq {proc demo {} {return local edit}} &&
    [.vdev_extension_assist.bar.apply instate disabled]
}
.editor delete 1.0 end
.editor insert end {proc demo {} {return old}}
::vdev::_extension_assist_preview
set ::vapi::fail_apply 1
::vdev::_extension_assist_apply
ok "failed apply leaves the buffer and reviewed proposal intact" {
    $::vapi::apply_calls == 2 &&
    [.editor get 1.0 end-1c] eq {proc demo {} {return old}} &&
    [llength $::vdev::assist_preview] > 0 &&
    [.vdev_extension_assist.bar.apply instate disabled]
}
set ::vapi::fail_apply 0
::vdev::_extension_assist_apply
ok "a failed apply requires a fresh preview before another write" {
    $::vapi::apply_calls == 2 &&
    [.editor get 1.0 end-1c] eq {proc demo {} {return old}}
}
ok "the preview says that checks have not run" {
    [string match "*Checks run*: 0*" [.vdev_extension_assist.diff get 1.0 end]]
}
ok "a model call waits a model's time, not the 10 s gateway default" {
    [lindex $::vapi::async_timeouts end] >= 300000
}

# ── every level (dev-ext-10, owner ask 287976.2) ─────────────────────
puts "\nAI assistance on every level"
namespace eval ::vapi {
    variable calls {}
    variable changeset_apply_error ""
}
proc ::vapi::post_json {path body} {
    set wire [::json::json2dict [::vapi::_json_value $body]]
    lappend ::vapi::calls [list $path $wire]
    set ::vapi::last_wire $wire
    set route [list provider test-provider model test-model api 0.11]
    switch -- $path {
        /api/extensions/assist/ask {
            return [dict create operation answer answer {Use a pre_tool_call hook.} applied 0 {*}$route]
        }
        /api/extensions/assist/explain {
            return [dict create operation explain answer {Line 1 never closes its brace.} applied 0 {*}$route]
        }
        /api/extensions/assist/propose {
            return [dict create operation changeset proposal_id set-1 checks_run 0 applied 0 \
                summary {Split the helper out.} \
                files [list \
                    [dict create path plugin.tcl created 0 before {proc demo {} {return old}} after {source lib/util.tcl}] \
                    [dict create path lib/util.tcl created 1 before null after {proc util {} {}}]] \
                diff "--- a/plugin.tcl\n+++ b/plugin.tcl\n--- /dev/null\n+++ b/lib/util.tcl" {*}$route]
        }
        /api/extensions/assist/generate {
            return [dict create operation generate proposal_id gen-1 checks_run 0 applied 0 \
                name [dict get $wire name] kind [dict get $wire kind] summary {A rate guard.} \
                files [list \
                    [dict create path plugin.tcl created 1 after {::veles::hook pre_tool_call ::rate_guard::check}] \
                    [dict create path plugin.toml created 1 after "name = \"rate-guard\"\nentry = \"plugin.tcl\""]] \
                {*}$route]
        }
        /api/extensions/assist/apply {
            if {$::vapi::changeset_apply_error ne ""} {
                error "HTTP 409: {\"error\":\"$::vapi::changeset_apply_error\"}"
            }
            if {[dict get $wire proposal_id] eq "set-1"} {
                return [dict create applied 1 created 0 files [list \
                    [dict create path /user/plugins/demo/plugin.tcl relative plugin.tcl content {source lib/util.tcl} sha256 set-sha] \
                    [dict create path /user/plugins/demo/lib/util.tcl relative lib/util.tcl content {proc util {} {}} sha256 u-sha]]]
            }
            return [dict create applied 1 created 1 reloaded 1 files [list \
                [dict create path /user/plugins/rate-guard/plugin.toml relative plugin.toml] \
                [dict create path /user/plugins/rate-guard/plugin.tcl relative plugin.tcl]]]
        }
    }
    error "unexpected endpoint $path"
}
proc ::vapi::calls_to {path} {
    set n 0
    foreach c $::vapi::calls { if {[lindex $c 0] eq $path} { incr n } }
    return $n
}

# A design question needs no buffer at all.
rename ::vdev::editor_path ::vdev::_real_editor_path
proc ::vdev::editor_path {} { return "" }
::vdev::extension_assist
ok "with no file open, only the design question is offered" {
    $::vdev::assist_mode eq "ask" &&
    [.vdev_extension_assist.modes.file instate disabled] &&
    [.vdev_extension_assist.modes.extension instate disabled] &&
    [.vdev_extension_assist.modes.explain instate disabled] &&
    ![.vdev_extension_assist.modes.ask instate disabled]
}
.vdev_extension_assist.request insert end {Hook or service for a rate limit?}
update
::vdev::_extension_assist_preview
ok "a design question goes to ask, with no source and no path" {
    [::vapi::calls_to /api/extensions/assist/ask] == 1 &&
    [dict get $::vapi::last_wire question] eq {Hook or service for a rate limit?} &&
    ![dict exists $::vapi::last_wire content] && ![dict exists $::vapi::last_wire path]
}
ok "…the answer is shown and nothing can be applied" {
    [string match "*pre_tool_call hook*" [.vdev_extension_assist.diff get 1.0 end]] &&
    [.vdev_extension_assist.bar.apply instate disabled]
}
rename ::vdev::editor_path ""
rename ::vdev::_real_editor_path ::vdev::editor_path

# Explaining the diagnostics of the open buffer.
::vdev::extension_assist
set ::vdev::assist_mode explain
::vdev::_assist_mode_changed
ok "the explain level has its own button" {[.vdev_extension_assist.bar.preview cget -text] eq "Explain"}
set ::vdev::editor_diagnostics(.editor) {}
::vdev::_extension_assist_preview
ok "no diagnostics: nothing is asked" \
    {[::vapi::calls_to /api/extensions/assist/explain] == 0 && [string match "*no diagnostics*" $::last_message]}
set ::vdev::editor_diagnostics(.editor) [list [list 1 10 ERROR {missing close-brace}]]
::vdev::_extension_assist_preview
ok "the diagnostics and the buffer go to explain" {
    [::vapi::calls_to /api/extensions/assist/explain] == 1 &&
    [string match "*1:10 ERROR missing close-brace*" [dict get $::vapi::last_wire diagnostics]] &&
    [dict get $::vapi::last_wire content] eq [.editor get 1.0 end-1c]
}
ok "…and the explanation is text only" {
    [string match "*never closes*" [.vdev_extension_assist.diff get 1.0 end]] &&
    [.vdev_extension_assist.bar.apply instate disabled]
}

# A change set across the extension's files.
set ::vdev::assist_mode extension
::vdev::_assist_mode_changed
.vdev_extension_assist.request delete 1.0 end
.vdev_extension_assist.request insert end {Move the helper to lib/util.tcl}
update
::vdev::_extension_assist_preview
ok "a change set is proposed for the extension the buffer is in" {
    [::vapi::calls_to /api/extensions/assist/propose] == 1 &&
    [dict get $::vapi::last_wire path] eq {/user/plugins/demo/plugin.tcl}
}
ok "…every file is listed (the new one marked) and nothing is written yet" {
    [string match "*plugin.tcl, lib/util.tcl (new)*" [.vdev_extension_assist.diff get 1.0 end]] &&
    [.editor get 1.0 end-1c] eq {proc demo {} {return old}} &&
    ![.vdev_extension_assist.bar.apply instate disabled]
}
set ::vdev::editor_dirty(.editor) 1
set ::calls_before [::vapi::calls_to /api/extensions/assist/apply]
::vdev::_extension_assist_apply
ok "an unsaved buffer blocks a change set" \
    {[::vapi::calls_to /api/extensions/assist/apply] == $::calls_before}
set ::vdev::editor_dirty(.editor) 0
set ::confirm_answer 1
::vdev::_extension_assist_apply
ok "a confirmed change set sends only its id" {
    [::vapi::calls_to /api/extensions/assist/apply] == $::calls_before + 1 &&
    [dict get $::vapi::last_wire proposal_id] eq "set-1"
}
ok "…and the open buffer shows what was written" {
    [.editor get 1.0 end-1c] eq {source lib/util.tcl} && $::vdev::editor_sha(.editor) eq "set-sha" &&
    [.vdev_extension_assist.bar.apply instate disabled]
}

# A whole extension from its purpose.
puts "\ngenerating a new extension"
namespace eval ::voverlay { variable opened {} ; proc _open_in_dev {p} { lappend ::voverlay::opened $p } }
::vdev::new_extension
set ::vdev_new_name rate-guard
set ::vdev_new_purpose {Refuse more than ten tool calls a minute}
set ::vdev_new_kind tcl
::vdev::_extension_generate
ok "generate sends the name, language and purpose" {
    [::vapi::calls_to /api/extensions/assist/generate] == 1 &&
    [dict get $::vapi::last_wire name] eq "rate-guard" &&
    [dict get $::vapi::last_wire kind] eq "tcl" &&
    [string match "*ten tool calls*" [dict get $::vapi::last_wire description]]
}
ok "…the generated files are previewed and nothing is written" {
    [string match "*nothing has been written*── plugin.tcl ──*pre_tool_call*" [.vdev_new_extension.preview get 1.0 end]] &&
    [::vapi::calls_to /api/extensions/assist/apply] == $::calls_before + 1
}
set ::vdev_new_purpose {something else}
::vdev::_extension_create
ok "a changed purpose voids the generated preview" {
    [::vapi::calls_to /api/extensions/assist/apply] == $::calls_before + 1 &&
    $::vdev::extension_generated eq {} && [string match "*changed since*" $::last_message]
}
set ::vdev_new_purpose {Refuse more than ten tool calls a minute}
::vdev::_extension_generate
::vdev::_extension_create
ok "Create writes exactly the reviewed generated files" {
    [::vapi::calls_to /api/extensions/assist/apply] == $::calls_before + 2 &&
    [dict get $::vapi::last_wire proposal_id] eq "gen-1"
}
ok "…and opens the manifest's entry" \
    {[lindex $::voverlay::opened end] eq "/user/plugins/rate-guard/plugin.tcl"}
destroy .vdev_new_extension
destroy .vdev_extension_assist
# dev-ext-10: a core or repository file is refused by the gateway with
# draft_as_extension; the pane offers New extension, carrying the request
# over as its purpose, instead of a bare refusal.
set ::new_extension_opened 0
proc ::vdev::new_extension {} { incr ::new_extension_opened; set ::vdev_new_purpose "" }
proc ::vdev::_extension_preview {} {}
proc ::vapi::post_json {path body} {
    error {HTTP 400: {"error":"AI assistance is available only for a user plugin source file","draft_as_extension":true}}
}
set ::confirm_titles {}
proc ::vmanage::confirm {w title detail} { lappend ::confirm_titles $title; return $::confirm_answer }
::vdev::extension_assist
.vdev_extension_assist.request insert end {Log every failover decision}
set ::confirm_answer 0
set ::last_message ""
::vdev::_extension_assist_preview
ok "a non-extension file is offered as a new extension; declining keeps the refusal" {
    $::confirm_titles eq {{Draft this as an extension?}} && $::new_extension_opened == 0 &&
    [string match "*only for a user plugin*" $::last_message] &&
    [winfo exists .vdev_extension_assist]
}
set ::confirm_answer 1
::vdev::_extension_assist_preview
ok "accepting opens New extension with the request as its purpose" {
    $::new_extension_opened == 1 && ![winfo exists .vdev_extension_assist] &&
    $::vdev_new_purpose eq {Log every failover decision}
}
proc ::vapi::post_json {path body} {
    error {HTTP 400: {"error":"the current buffer does not match its saved source","draft_as_extension":false}}
}
set ::confirm_titles {}
::vdev::extension_assist
.vdev_extension_assist.request insert end {anything}
::vdev::_extension_assist_preview
ok "an extension file's refusal is not turned into the offer" {
    $::confirm_titles eq {} && $::new_extension_opened == 1
}

puts "\nextensionassisttest: $passed passed, $failed failed"
exit [expr {$failed ? 1 : 0}]
