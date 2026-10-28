#!/usr/bin/env wish
# veles-desktop/lib/setuptest.tcl — the first-run setup wizard (lib/setup.tcl).
#
# PLAN-setup-wizard-tk.md, work item 5: drive the REAL procs under Xvfb
# against the REAL api.tcl with only the WIRE stubbed (::vapi::get and
# ::vapi::post answer with the JSON text a gateway would send, and throw
# "HTTP <code>: <body>" exactly as the real ones do). The first cut
# stubbed post_json/json_string themselves — so a refusal came back as a
# 200 with ok:false, a shape the real wire never produces, and the
# wizard's "refused" branch was tested and unreachable at once (the
# memfilestest rule: stub the wire, not the function). The Rust half of the contract (block shapes,
# backup-not-clobber, 0600 secrets, parse-back validation) is pinned in
# veles_core::setup::tests and the desktop_api tests; this pins the Tk
# half, and specifically the five behaviours the plan calls out:
#
#   1. a live language swap RE-TEXTS AN OPEN WIZARD — the step is
#      rebuilt from ::vsetup::v, so nothing caches stale labels, and
#      typed answers survive the rebuild;
#   2. preview-then-apply: "Write it" POSTs /api/setup/apply ONLY when
#      the preview on screen was built from the CURRENT answers (the
#      sig gate — an edit after the preview must re-preview, not write);
#   3. first-run gating: maybe_first_run opens ONLY on first_run=true
#      AND no setup_done marker; an operator with a config never sees it;
#   4. the answers body is wire-correct: strings through json_string,
#      booleans through json_bool (a serde bool answers 422 to a JSON
#      number), the port a number;
#   5. a dead gateway still opens the wizard and SAYS so — the window
#      is not refused, and the state line names the failure.
#
# Plus the walk: seven steps, titles, nav-button states, Skip disabled
# on the summary, cancel-with-confirm, provider-skip greys the fields,
# and the finish screen reporting what apply returned.
#
# Run: scripts/tcl_tests.sh

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
package require json

# ── stubs ────────────────────────────────────────────────────────────
#
# The wire, the state blob, the status bar, placement and the titlebar
# strip. Real signatures (the acltest lesson: a stub that disagrees with
# the thing it stands in for validates a fiction).

source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]
namespace eval ::vapi {
    variable posts {}       ;# paths POSTed, in order
    variable bodies {}      ;# the JSON text of each POST body
    variable state_json {}  ;# GET /api/setup/state
    variable preview_json {};# POST /api/setup/preview
    variable preview_code 200
    variable apply_json {}  ;# POST /api/setup/apply
    variable apply_code 200
    variable fail_state 0
    variable fail_preview 0
}
# The wire: what `http::geturl` would have handed `_finish`/`post`.
proc ::vapi::get {path {ms 0}} {
    if {$path eq "/api/setup/state"} {
        if {$::vapi::fail_state} { error "transport: timeout" }
        return $::vapi::state_json
    }
    error "unexpected GET $path"
}
proc ::vapi::post {path body {ms 0}} {
    lappend ::vapi::posts $path
    lappend ::vapi::bodies $body
    switch -- $path {
        /api/setup/preview {
            if {$::vapi::fail_preview} { error "transport: eof" }
            set code $::vapi::preview_code
            set out $::vapi::preview_json
        }
        /api/setup/apply {
            set code $::vapi::apply_code
            set out $::vapi::apply_json
        }
        default { error "unexpected POST $path" }
    }
    if {$code >= 400} { error "HTTP $code: $out" }
    return $out
}
# A subset of /api/setup/state `presets` (veles_core::setup::PROVIDER_PRESETS):
# one with a suggested model, one without, and the custom-server row.
set ::PRESETS_JSON {[{"choice": "openrouter", "label": "OpenRouter", "entry_name": "openrouter", "base_url": "https://openrouter.ai/api/v1", "key_env": "OPENROUTER_API_KEY", "api_mode": "chat_completions", "model": "openrouter/auto", "key_url": "https://openrouter.ai/keys"}, {"choice": "openai", "label": "OpenAI", "entry_name": "openai", "base_url": "https://api.openai.com/v1", "key_env": "OPENAI_API_KEY", "api_mode": "chat_completions", "model": "", "key_url": "https://platform.openai.com/api-keys"}, {"choice": "openai_compatible", "label": "Other OpenAI-compatible server", "entry_name": "local", "base_url": "http://localhost:8000/v1", "key_env": "", "api_mode": "chat_completions", "model": "", "key_url": ""}]}
# POST /api/setup/models, answered on the next idle like a real reply.
namespace eval ::vapi { variable models_reply {ok true models {gpt-a gpt-b}} }
proc ::vapi::post_json_async {path body on_err on_ok} {
    lappend ::vapi::posts $path
    lappend ::vapi::bodies [::vapi::_json_value $body]
    if {$path ne "/api/setup/models"} { error "unexpected async POST $path" }
    after idle [list {*}$on_ok $::vapi::models_reply]
    return tok
}
namespace eval ::vstatus {
    variable msgs {}
    proc set_msg {m} { variable msgs; lappend msgs $m }
}
namespace eval ::vstate {
    variable blob {}
    variable error ""
    variable puts_n 0
    proc get {key {default ""}} {
        variable blob
        if {[dict exists $blob $key]} { return [dict get $blob $key] }
        return $default
    }
    proc put {key value} {
        variable blob
        variable puts_n
        incr puts_n
        dict set blob $key $value
        return 1
    }
}
namespace eval ::va11y {
    variable orders {}
    variable names {}
    proc name {w label} { variable names; dict set names $w $label }
    proc tab_order {widgets} { variable orders; lappend orders $widgets }
}
namespace eval ::vplace {
    proc centered {w width height args} { wm geometry $w ${width}x${height} }
}
namespace eval ::vtitle {
    variable titles {}
    proc attach {w title args} { variable titles; dict set titles $w $title }
}
# The cancel confirm. wish has no display-independent default answer, so
# the suite owns the dialog: record the call, answer from a variable.
set ::mb_calls {}
set ::mb_answer yes
rename tk_messageBox tk_messageBox_real
proc tk_messageBox {args} {
    lappend ::mb_calls $args
    return $::mb_answer
}

# The step scrolls in a ::vmanage::scrollframe.
source [file join $_dir manage.tcl]
source [file join $_dir install.tcl]
source [file join $_dir setup.tcl]

set passed 0
set failed 0
# A condition that THROWS is a failure with its error shown, not the end
# of the suite: a mutation that breaks a proc must still leave a tally
# (the lesson recorded in scripts/mutcheck-2026-09-26.tcl).
proc ok {name cond} {
    if {[catch { uplevel 1 [list expr $cond] } r]} {
        puts "  ✗ $name (threw: $r)"
        incr ::failed
        return
    }
    if {$r} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name"
        incr ::failed
    }
}

# Reset the module state between scenarios: close any window, clear the
# stubs, put the answers back to defaults, and force English.
proc reset {{locale en}} {
    catch { ::vsetup::_close }
    catch { destroy .setup_wizard }
    set ::vsetup::win ""
    set ::vsetup::cur 0
    set ::vsetup::state {}
    set ::vsetup::api_err ""
    set ::vsetup::preview_sig ""
    set ::vsetup::preview_base ""
    set ::vsetup::preview_plan ""
    set ::vsetup::install_mode 0
    ::vinstall::reset
    set ::vsetup::install_manifest {}
    set ::vsetup::install_instance ""
    set ::vsetup::install_apply_pending 0
    set ::vsetup::skipped {}
    set ::vsetup::applied {}
    foreach k [array names ::vsetup::v] { unset ::vsetup::v($k) }
    array set ::vsetup::v {
        provider openrouter     base_url {}       key_env {}
        api_mode {}             provider_name {}  model {}
        default_provider {}     model_touched 0   name_touched 0
        keyenv_touched 0        base_touched 0    dp_touched 0
        telegram 0              irc_server {}     irc_port 6697
        irc_tls 1               irc_nick {}       irc_channels {}
        acl_idx 0               acl_owner {}      locale {}
        irc_sasl {}
    }
    array set ::vsetup::secret { key {} bot {} sasl {} }
    set ::vapi::posts {}
    set ::vapi::bodies {}
    set ::vapi::fail_state 0
    set ::vapi::fail_preview 0
    set ::vapi::preview_code 200
    set ::vapi::apply_code 200
    set ::vapi::state_json [string map [list @P@ $::PRESETS_JSON] {{"first_run": true, "config_exists": false, "config_path": "/cfg/config.toml", "env_path": "/cfg/.env", "env_exists": false, "current": null, "current_error": null, "presets": @P@}}]
    set ::vapi::preview_json {{"ok": true, "mode": "fresh", "base_sig": "", "diff": "", "config_toml": "# preview config\n[model]\n", "env_text": "", "secrets": ["SOME_KEY"], "notes": ["a note"]}}
    set ::vapi::apply_json {{"ok": true, "mode": "fresh", "config_path": "/cfg/config.toml", "config_backup": null, "env_path": "/cfg/.env", "env_written": true, "secrets_needed": ["SOME_KEY"], "notes": ["a note"], "reloaded": true}}
    set ::vstate::blob {}
    set ::vstate::puts_n 0
    set ::mb_calls {}
    set ::mb_answer yes
    set ::vstatus::msgs {}
    if {[::vi18n::current] ne $locale} { ::vi18n::use $locale }
}

# The body of the last POST to `path`, parsed.
proc last_body {path} {
    set i [lsearch -exact -all $::vapi::posts $path]
    if {![llength $i]} { return {} }
    return [::json::json2dict [lindex $::vapi::bodies [lindex $i end]]]
}

# The text currently on a widget (-text), or "" when it has none.
proc wtext {w} {
    if {![winfo exists $w]} { return "<missing>" }
    if {[catch { $w cget -text } t]} { return "<no -text>" }
    return $t
}

reset
update idletasks

# ── 1. the walk: seven steps, titles, nav states ────────────────────
puts "── the walk ──"

set w [::vsetup::open]
ok "open builds the window" {[winfo exists .setup_wizard]}
ok "welcome title" {[wm title .setup_wizard] eq "Setup — welcome"}
ok "welcome heading is up" {[string match "*Welcome to veles*" [wtext .setup_wizard.body.c.inner.h]]}
ok "the state line says first run" {[string match "*First run*" [wtext .setup_wizard.body.c.inner.f.state]]}
ok "Back is disabled on step 0" {[lsearch -exact [.setup_wizard.nav.back state] disabled] >= 0}
ok "the language list is filled" {[.setup_wizard.body.c.inner.f.lang size] >= 40}
# A window shorter than the step (a phone's, the keyboard up): Next stays
# on it, the step scrolls, and a field that takes the focus is scrolled
# into sight. Next was pushed off the bottom on a 320x640 emulator, and
# with the keyboard up the API key field was out of sight.
update
set _geo [wm geometry .setup_wizard]
# The minimum is the laptop's; on a phone fit lowers it to the screen.
set _min [wm minsize .setup_wizard]
wm minsize .setup_wizard 1 1
set _short 300
wm geometry .setup_wizard [winfo width .setup_wizard]x$_short
update
proc _bottom {w} { expr {[winfo rooty $w] + [winfo height $w] - [winfo rooty .setup_wizard]} }
set _c .setup_wizard.body.c
set _def $_c.inner.f.lb.def
ok "a short window keeps Next on it, and the step scrolls" {
    [winfo height .setup_wizard] == $_short
    && [_bottom .setup_wizard.nav.next] <= $_short
    && [winfo height $_c.inner] > [winfo height $_c]
    && [winfo rooty $_def] > [winfo rooty $_c] + [winfo height $_c]
}
focus -force $_def
update
ok "…to the field that takes the focus" {
    [winfo rooty $_def] >= [winfo rooty $_c]
    && [winfo rooty $_def] + [winfo height $_def] <= [winfo rooty $_c] + [winfo height $_c]
}
$_c yview moveto 0
update
event generate $_c.inner.f <FocusIn> -detail NotifyVirtual
update
ok "…but not to a frame the focus passes through" {[$_c canvasy 0] == 0}
# A field mid-step, scrolled into sight in the short window: the
# window taller again shows more of what is above it, rather than
# staying scrolled (the IRC step opened below its own heading). Tk's
# confine does that for a field at the end of a step, not mid-step.
$_c yview moveto 0
update
set _lang $_c.inner.f.lang
focus -force $_lang
update
set _y0 [$_c canvasy 0]
set _grow 30
ok "…a field mid-step, the rest of the step below the window" {
    $_y0 > 0 && $_y0 + [winfo height $_c] + $_grow < [winfo height $_c.inner]
}
wm geometry .setup_wizard [winfo width .setup_wizard]x[expr {$_short + $_grow}]
update
ok "…and, the window taller again, more of what is above it" {
    [$_c canvasy 0] < $_y0
    && [winfo rooty $_lang] + [winfo height $_lang] <= [winfo rooty $_c] + [winfo height $_c]
}
$_c yview moveto 0
wm minsize .setup_wizard {*}$_min
wm geometry .setup_wizard $_geo
update

# Opening twice raises, never builds a second window.
set kids_before [winfo children .]
set w2 [::vsetup::open]
ok "open is idempotent (no second window)" \
    {$w2 eq ".setup_wizard" && [winfo children .] eq $kids_before}

# Walk forward through every step and check each title + a landmark widget.
::vsetup::next
ok "provider step" {[wm title .setup_wizard] eq "Setup — LLM provider" \
    && [winfo exists .setup_wizard.body.c.inner.f.cb]}
ok "defaults were suggested for the first preset (OpenRouter)" \
    {$::vsetup::v(provider_name) eq "openrouter" \
     && $::vsetup::v(key_env) eq "OPENROUTER_API_KEY" \
     && $::vsetup::v(model) eq "openrouter/auto"}
ok "…and the step says where to get its key" \
    {[string match "*openrouter.ai/keys*" [wtext .setup_wizard.body.c.inner.f.hk]]}
::vsetup::next
ok "model step" {[wm title .setup_wizard] eq "Setup — default model" \
    && [winfo exists .setup_wizard.body.c.inner.f.e0]}
::vsetup::next
ok "telegram step" {[wm title .setup_wizard] eq "Setup — Telegram" \
    && [winfo exists .setup_wizard.body.c.inner.f.on]}
::vsetup::next
ok "irc step" {[wm title .setup_wizard] eq "Setup — IRC" \
    && [winfo exists .setup_wizard.body.c.inner.f.tls]}
::vsetup::next
ok "acl step" {[wm title .setup_wizard] eq "Setup — access control" \
    && [winfo exists .setup_wizard.body.c.inner.f.r1]}
ok "the ACL curated warning is on screen" {[string match "*curated mode*" [wtext .setup_wizard.body.c.inner.f.h1]]}
ok "owner entry disabled while legacy-open" \
    {[lsearch -exact [.setup_wizard.body.c.inner.f.owner state] disabled] >= 0}
# Flip to curated: the owner field arms.
set ::vsetup::v(acl_idx) 1
::vsetup::_acl_sync .setup_wizard.body.c.inner.f
ok "owner entry arms under curated" \
    {[lsearch -exact [.setup_wizard.body.c.inner.f.owner state] disabled] < 0}
set ::vsetup::v(acl_idx) 0

::vsetup::next
ok "summary step" {[wm title .setup_wizard] eq "Setup — summary"}
ok "the summary fetched a preview on entry" \
    {[lsearch -exact $::vapi::posts /api/setup/preview] >= 0}
ok "the preview text is on screen" \
    {[string match "*preview config*" [.setup_wizard.body.c.inner.f.pv.preview get 1.0 end]]}
ok "the engine's notes ride along" \
    {[string match "*a note*" [.setup_wizard.body.c.inner.f.pv.preview get 1.0 end]]}
ok "the secret NAMES ride along, each with where it stands" \
    {[string match {*$SOME_KEY: not set yet*} [.setup_wizard.body.c.inner.f.pv.preview get 1.0 end]]}
ok "Skip is disabled on the summary" \
    {[lsearch -exact [.setup_wizard.nav.skip state] disabled] >= 0}
ok "Next became Write it on the summary" {[wtext .setup_wizard.nav.next] eq "Write it"}
::vsetup::back
ok "Back returns to the ACL step" {[wm title .setup_wizard] eq "Setup — access control"}
::vsetup::next

# ── 2. preview-then-apply: the sig gate ─────────────────────────────
puts "── preview gates apply ──"

set ::vapi::posts {}
::vsetup::next    ;# from the summary this is Write it, sig matches
ok "Write it with a matching sig POSTs apply" \
    {[lsearch -exact $::vapi::posts /api/setup/apply] >= 0}
ok "the finish screen is up" {[wm title .setup_wizard] eq "Setup — done" \
    && [string match "*Done*" [wtext .setup_wizard.body.h]]}
ok "the report names what was written" \
    {[string match "*/cfg/config.toml*" [.setup_wizard.body.report get 1.0 end]]}
ok "the report names the env fill" \
    {[string match "*SOME_KEY*" [.setup_wizard.body.report get 1.0 end]]}
ok "setup_done was persisted to the desktop state, not config.toml" \
    {$::vstate::puts_n == 1 && [dict exists $::vstate::blob setup_done]}
ok "the nav bar is gone on the finish screen" {![winfo exists .setup_wizard.nav]}

# The gate: edit an answer AFTER the preview, and Write it must refresh
# instead of applying — the bytes on screen are no longer the bytes that
# would be written.
reset
::vsetup::open
for {set i 0} {$i < 6} {incr i} { ::vsetup::next }
ok "back on the summary" {[wm title .setup_wizard] eq "Setup — summary"}
set ::vapi::posts {}
set ::vsetup::v(irc_server) "irc.libera.chat"   ;# an edit after the preview
::vsetup::next                                   ;# Write it
ok "an edit after the preview does NOT write" \
    {[lsearch -exact $::vapi::posts /api/setup/apply] < 0}
ok "…it re-previews instead" \
    {[lsearch -exact $::vapi::posts /api/setup/preview] >= 0}
ok "…and says why" \
    {[string match "*fresh preview*" [lindex $::vstatus::msgs end]]}
set ::vapi::posts {}
::vsetup::next                                   ;# second press, sig matches now
ok "the second press writes" \
    {[lsearch -exact $::vapi::posts /api/setup/apply] >= 0}
ok "the edited answer reached the wire" \
    {[string match "*irc.libera.chat*" [lindex $::vapi::bodies end]]}

# A failed preview leaves the gate SHUT, not armed.
reset
::vsetup::open
for {set i 0} {$i < 6} {incr i} { ::vsetup::next }
set ::vapi::fail_preview 1
set rc [::vsetup::preview_refresh]
set ::vapi::fail_preview 0
set ::vapi::posts {}
ok "a failed preview returns 0" {$rc == 0}
ok "a failed preview is shown, not swallowed" \
    {[string match "*preview failed*" [.setup_wizard.body.c.inner.f.pv.preview get 1.0 end]]}
::vsetup::next
ok "Write it after a failed preview re-previews, never applies" \
    {[lsearch -exact $::vapi::posts /api/setup/apply] < 0}

# A refusal from the engine is HTTP 400 {"ok":false,"error":…}, which
# the real `post` THROWS. It is painted as the engine's own sentence —
# not as "preview failed: HTTP 400: {json}".
reset
::vsetup::open
for {set i 0} {$i < 6} {incr i} { ::vsetup::next }
set ::vapi::preview_code 400
set ::vapi::preview_json {{"ok": false, "error": "unknown api_mode \"smoke\" — expected one of chat_completions"}}
::vsetup::preview_refresh
set pv [.setup_wizard.body.c.inner.f.pv.preview get 1.0 end]
ok "an engine refusal is shown by name" {[string match "*refused these answers*smoke*" $pv]}
ok "…as the engine's words, not raw JSON" {![string match "*\"ok\"*" $pv]}

# ── 3. the answers body: wire shape ─────────────────────────────────
puts "── the answers body ──"

# Rendered by the REAL encoder (api.tcl's _json_value), and pinned to the
# exact text desktop_api.rs's TK_SETUP_BODY posts to the real routes —
# one string, checked from both ends of the wire.
reset
array set ::vsetup::v {
    provider openai_compatible  base_url http://127.0.0.1:11434/v1  key_env OLLAMA_KEY
    api_mode chat_completions  provider_name ollama  model llama3
    default_provider ollama  telegram 1  irc_server {}  irc_port 6697
    irc_tls 1  irc_nick {}  irc_channels {}  acl_idx 0  acl_owner {}  locale de
}
set ::vsetup::skipped {irc}
set rendered [::vapi::_json_value [::vsetup::answers_body]]
set TK_SETUP_BODY {{"provider": "openai_compatible", "base_url": "http://127.0.0.1:11434/v1", "key_env": "OLLAMA_KEY", "api_mode": "chat_completions", "provider_name": "ollama", "model": "llama3", "default_provider": "ollama", "telegram": true, "irc_server": "", "irc_port": 6697, "irc_use_tls": true, "irc_nickname": "", "irc_channels": "", "irc_sasl_username": "", "acl": "legacy_open", "acl_owner": "", "locale": "de", "skipped": ["irc"]}}
ok "the body is byte-for-byte the one the Rust route test posts" {$rendered eq $TK_SETUP_BODY}
if {$rendered ne $TK_SETUP_BODY} { puts "      got: $rendered" }

reset
set rendered [::vapi::_json_value [::vsetup::answers_body]]
ok "an empty skip list is still an array" {[string first {"skipped": []} $rendered] >= 0}
# A free-text value that LOOKS dict-shaped must still serialize as a
# string — the json_string wrapper is what stops the heuristic from
# turning "#veles #dev" into an object or a two-element array.
set ::vsetup::v(irc_channels) "#veles #dev"
set ::vsetup::v(key_env) {a "quoted" \ value}
set rendered [::vapi::_json_value [::vsetup::answers_body]]
ok "a multi-word channels string survives as one string" \
    {[string match {*"irc_channels": "#veles #dev"*} $rendered]}
ok "a quote and a backslash are escaped (the stub this replaced did not)" \
    {[dict get [::json::json2dict $rendered] key_env] eq {a "quoted" \ value}}
# An acl flip reaches the wire as the enum the core expects.
set ::vsetup::v(acl_idx) 1
set ::vsetup::v(acl_owner) "telegram:123456"
set d [::json::json2dict [::vapi::_json_value [::vsetup::answers_body]]]
ok "curated ACL rides as its enum + owner" \
    {[dict get $d acl] eq "curated" && [dict get $d acl_owner] eq "telegram:123456"}
# A port field that is not a port goes as 0, which the engine refuses by
# name — the first cut sent 6697 in its place without a word.
set ::vsetup::v(irc_port) "66a"
set d [::json::json2dict [::vapi::_json_value [::vsetup::answers_body]]]
ok "a non-port is sent as 0, never guessed as 6697" {[dict get $d irc_port] == 0}

# ── 4. first-run gating ─────────────────────────────────────────────
puts "── first-run gating ──"

reset
ok "first_run + no marker opens the wizard" {[::vsetup::maybe_first_run] == 1 \
    && [winfo exists .setup_wizard]}
reset
set ::vapi::state_json {{"first_run": false, "config_exists": true, "config_path": "/cfg/config.toml", "current": null}}
ok "an existing config never auto-opens" {[::vsetup::maybe_first_run] == 0 \
    && ![winfo exists .setup_wizard]}
reset
set ::vstate::blob [dict create setup_done 1]
ok "a completed wizard here never auto-opens again" {[::vsetup::maybe_first_run] == 0}
reset
set ::vapi::fail_state 1
ok "a dead gateway auto-open stays quiet" {[::vsetup::maybe_first_run] == 0}
reset
set ::vapi::state_json {{"first_run": false, "config_exists": true, "config_path": "/cfg/config.toml", "current": null}}
set ::env(VELES_SETUP_INSTALL) 1
ok "installer bootstrap opens the installer even after first-run setup" {[::vsetup::boot] == 1 && $::vsetup::install_mode && [wm title .setup_wizard] eq "Setup — installation profile"}
unset ::env(VELES_SETUP_INSTALL)
reset
set ::vapi::state_json {{"first_run": true, "config_exists": false, "config_path": "/cfg/config.toml", "current": null}}
ok "ordinary boot retains first-run provider setup" {[::vsetup::boot] == 1 && !$::vsetup::install_mode}

# RE-ENTRY over an existing config: the answers start from the FILE,
# the screens say it is edited in place, and the summary shows the diff.
reset
set ::vapi::state_json {{"first_run": false, "config_exists": true, "config_path": "/cfg/config.toml", "env_path": "/cfg/.env", "env_exists": true, "current": {"provider": "openai_compatible", "base_url": "http://box:8000/v1", "key_env": "BOX_KEY", "api_mode": "chat_completions", "provider_name": "box", "model": "qwen3", "default_provider": "team", "telegram": true, "irc_server": "irc.libera.chat", "irc_port": 6697, "irc_use_tls": true, "irc_nickname": "vbot", "irc_channels": "#veles", "acl": "curated", "acl_owner": "irc:wrath", "locale": "de", "skipped": []}, "current_error": null}}
::vsetup::open
ok "re-entry says the answers start from the file" \
    {[string match "*starts from it*" [wtext .setup_wizard.body.c.inner.f.state]]}
ok "…and names WHICH file (the gateway may be another box's)" \
    {[string match "*/cfg/config.toml*" [wtext .setup_wizard.body.c.inner.f.path]]}
ok "the answers are prefilled from the file" \
    {$::vsetup::v(provider) eq "openai_compatible" && $::vsetup::v(provider_name) eq "box" \
     && $::vsetup::v(model) eq "qwen3" && $::vsetup::v(telegram) == 1 \
     && $::vsetup::v(irc_nick) eq "vbot" && $::vsetup::v(acl_idx) == 1 \
     && $::vsetup::v(acl_owner) eq "irc:wrath" && $::vsetup::v(locale) eq "de"}
ok "a file routing through another entry keeps that routing" \
    {$::vsetup::v(default_provider) eq "team" && $::vsetup::v(dp_touched) == 1}
ok "prefilled fields are not re-suggested over" {$::vsetup::v(base_url) eq "http://box:8000/v1"}
set ::vapi::preview_json {{"ok": true, "mode": "edit", "base_sig": "abc123", "diff": "--- config.toml (now)\n+++ config.toml (after)\n-default = \"qwen3\"\n+default = \"qwen3-max\"\n", "config_toml": "WHOLE FILE TEXT", "secrets": [], "notes": []}}
for {set i 0} {$i < 6} {incr i} { ::vsetup::next }
set pv [.setup_wizard.body.c.inner.f.pv.preview get 1.0 end]
ok "the summary says edit-in-place, with a .bak" \
    {[winfo exists .setup_wizard.body.c.inner.f.warn] \
     && [string match "*edits it in place*config.toml.bak*" [wtext .setup_wizard.body.c.inner.f.warn]]}
ok "an edit previews as the diff, not the whole file" \
    {[string match "*changes to config.toml*+default = \"qwen3-max\"*" $pv] \
     && ![string match "*WHOLE FILE TEXT*" $pv]}
::vsetup::next    ;# Write it
ok "apply carries the base the preview was computed from" \
    {[dict get [last_body /api/setup/apply] expect_base] eq "abc123"}
# The engine takes `confirm: bool`; the string "true" is a 422, and
# "Write it" failed on every real gateway while this mock accepted it.
# json2dict reads both shapes back as `true`, so check the wire text.
ok "apply confirms with a JSON boolean, not the string \"true\"" \
    {[regexp {"confirm":\s*true\M} [lindex $::vapi::bodies [lindex [lsearch -exact -all $::vapi::posts /api/setup/apply] end]]]}

# ── 5. a dead gateway still opens, and says so ──────────────────────
puts "── dead gateway ──"

reset
set ::vapi::fail_state 1
set w [::vsetup::open]
ok "the window opens even with the API down" {[winfo exists .setup_wizard]}
ok "the welcome line names the failure" \
    {[string match "*cannot reach the gateway*" [wtext .setup_wizard.body.c.inner.f.state]]}

# ── 6. provider Skip greys the fields; a pick re-seeds defaults ─────
puts "── provider skip ──"

reset
::vsetup::open
::vsetup::next   ;# provider step
set F .setup_wizard.body.c.inner.f
$F.cb current [expr {[llength [::vsetup::presets]]}]   ;# the Skip row
::vsetup::_provider_picked $F
ok "Skip disables the provider fields" \
    {$::vsetup::v(provider) eq "skip" \
     && [lsearch -exact [$F.e20 state] disabled] >= 0 \
     && [lsearch -exact [$F.e22 state] disabled] >= 0}
$F.cb current 2   ;# Other OpenAI-compatible server
::vsetup::_provider_picked $F
ok "a custom server re-arms them and re-suggests its defaults" \
    {[lsearch -exact [$F.e20 state] disabled] < 0 \
     && $::vsetup::v(provider) eq "openai_compatible" \
     && $::vsetup::v(provider_name) eq "local" \
     && $::vsetup::v(base_url) eq "http://localhost:8000/v1" \
     && $::vsetup::v(model) eq ""}
# A touched field is not clobbered by a provider flip.
set ::vsetup::v(model) "my-model"
set ::vsetup::v(model_touched) 1
$F.cb current 0
::vsetup::_provider_picked $F
ok "a typed model survives the provider flip" {$::vsetup::v(model) eq "my-model"}

# A preset with no model to suggest: the model step asks the provider
# for its list by itself, with the typed key, and offers the answer.
reset
::vsetup::open
::vsetup::next   ;# provider step
$F.cb current 1   ;# OpenAI: no suggested model
::vsetup::_provider_picked $F
set ::vsetup::secret(key) "sk-typed-0007"
set ::vapi::state_json [string map [list @P@ $::PRESETS_JSON] {{"first_run": true, "current": null, "accepts_secrets": true, "presets": @P@}}]
set ::vsetup::state [::json::json2dict $::vapi::state_json]
::vsetup::next   ;# model step
update
ok "the model step fetched the provider's list by itself" \
    {[lindex $::vapi::posts end] eq "/api/setup/models" \
     && [string match {*"key": "sk-typed-0007"*} [lindex $::vapi::bodies end]] \
     && [string match {*api.openai.com*} [lindex $::vapi::bodies end]]}
ok "…and offers it in the model field" \
    {[$F.e0 cget -values] eq {gpt-a gpt-b} \
     && [string match "2 models*" [wtext $F.ms]]}
::vsetup::next
ok "Next refuses an empty model on its own screen" \
    {[wm title .setup_wizard] eq "Setup — default model" \
     && [string match "*choose a model*" [lindex $::vstatus::msgs end]]}
# A failed list says so and leaves the field free text.
set ::vapi::models_reply {ok false models {} error {HTTP 401}}
::vsetup::_list_models $F
update
ok "a failed list says why and asks for a typed name" \
    {[string match "*HTTP 401*" [wtext $F.ms]]}

# ── 7. cancel asks first ─────────────────────────────────────────────
puts "── cancel ──"

reset
::vsetup::open
set ::mb_answer no
::vsetup::cancel
ok "cancel asks" {[llength $::mb_calls] == 1}
ok "…and 'no' keeps the window" {[winfo exists .setup_wizard]}
set ::mb_answer yes
::vsetup::cancel
ok "…and 'yes' closes it" {![winfo exists .setup_wizard]}

# Escape is bound to the same confirm-cancel (the binding exists and
# names the proc — firing it would open the stubbed dialog again).
reset
::vsetup::open
ok "Escape cancels with a confirm" \
    {[string match "*::vsetup::cancel*" [bind .setup_wizard <Escape>]]}
ok "C-g cancels too (emacs parity, the ask-dialog precedent)" \
    {[string match "*::vsetup::cancel*" [bind .setup_wizard <Control-g>]]}

# ── 8. THE SWAP: a language change re-texts an OPEN wizard ──────────
puts "── live language swap ──"

reset
::vsetup::open
::vsetup::next            ;# provider step, the one with the most text
set ::vsetup::v(model) "typed-before-the-swap"
set ::vsetup::v(model_touched) 1
set en_next [wtext .setup_wizard.nav.next]
ok "sanity: English nav text before the swap" {$en_next eq "Next"}
::vi18n::use de
update idletasks
ok "the window still exists after the swap" {[winfo exists .setup_wizard]}
set de_next [wtext .setup_wizard.nav.next]
ok "the nav re-texted into German" {$de_next ne "Next" && $de_next eq [::vi18n::mc {Next}]}
ok "the title re-texted" {[wm title .setup_wizard] eq [::vi18n::mc {Setup — LLM provider}]}
ok "the step indicator re-formatted" \
    {[string match "*2*7*" [wtext .setup_wizard.head.step]]}
ok "the answers survived the rebuild" {$::vsetup::v(model) eq "typed-before-the-swap"}
# Walk one step in German and back: the widgets are rebuilt, not cached.
::vsetup::next
ok "the model step is German too" {[wm title .setup_wizard] eq [::vi18n::mc {Setup — default model}]}
ok "and still holds the typed value" {$::vsetup::v(model) eq "typed-before-the-swap"}
::vi18n::use en
ok "swapping back restores English" {[wtext .setup_wizard.nav.next] eq "Next"}
# The hook is removed on close — a swap with no window must not error.
::vsetup::_close
set rc [catch { ::vi18n::use de ; ::vi18n::use en } err]
ok "a swap after close is clean (the hook was unregistered)" {$rc == 0}

# THE HOOK'S UNIQUE JOB. Everything above (nav, title, step indicator)
# is ALSO re-texted by ::vi18n::use's built-in reverse-sweep, so those
# pass with the locale hook removed — they prove the catalog, not this
# window's re-paint. The sweep deliberately SKIPS `text` widgets
# (is_runtime_text_widget), and the summary preview's section headers
# ("notes from the engine", "secrets to fill into .env") live in exactly
# one. Only the retext hook re-renders that content on a swap, so this
# is the assertion that reddens when the hook is dropped.
reset
::vsetup::open
for {set i 0} {$i < 6} {incr i} { ::vsetup::next }   ;# to the summary
set en_prev [.setup_wizard.body.c.inner.f.pv.preview get 1.0 end]
ok "sanity: the preview header is English before the swap" \
    {[string match "*notes from the engine*" $en_prev]}
::vi18n::use de
update idletasks
set de_prev [.setup_wizard.body.c.inner.f.pv.preview get 1.0 end]
ok "the preview TEXT content re-rendered into German on the swap" \
    {[string match "*Hinweise von der Engine*" $de_prev] \
     && ![string match "*notes from the engine*" $de_prev]}
::vi18n::use en
::vsetup::_close

# ── 9. the finish screen's restart line ─────────────────────────────
puts "── finish report ──"

reset
::vsetup::open
for {set i 0} {$i < 6} {incr i} { ::vsetup::next }
set ::vapi::apply_json {{"ok": true, "mode": "edit", "config_path": "/cfg/config.toml", "config_backup": "/cfg/config.toml.bak", "env_path": "/cfg/.env", "env_written": false, "secrets_needed": ["TELEGRAM_BOT_TOKEN"], "notes": ["writing the first [acl] owner flips this box into CURATED mode"], "reloaded": true}}
::vsetup::next
set report [.setup_wizard.body.report get 1.0 end]
ok "the backup is named" {[string match "*config.toml.bak*" $report]}
ok "a secret still unset is on the to-do, with where to set it" \
    {[string match "*Still to set: TELEGRAM_BOT_TOKEN*veles secret set*" $report]}
ok "the curated-mode note reaches the operator" {[string match "*CURATED*" $report]}
ok "the restart line is there" {[string match "*veles gateway restart*" $report]}
ok "a reload that happened is reported as one" {[string match "*gateway reloaded the config*" $report]}
::vsetup::_run_again
ok "Run the wizard again returns to the welcome step" \
    {[wm title .setup_wizard] eq "Setup — welcome" && $::vsetup::applied eq ""}

# ── 10. boot wiring: the wizard is NOT gated on being attached ──────
#
# A fresh `veles gui` on a first-run box boots UNATTACHED (no registry
# entry, no VELES_API) but has the loopback gateway it just spawned —
# ::vapi falls back to base 127.0.0.1:8642 and /api/setup/state answers
# from it. That is EXACTLY when the wizard must open. The first cut of
# main.tcl wrapped maybe_first_run in `if {[::vapi::attached] ne ""}`,
# which suppressed it in the one case it exists for. maybe_first_run
# already catches a failed fetch (returns 0), so the guard bought
# nothing and cost the first run. Pin the wiring statically: the call
# must be reachable unconditionally, and must NOT be nested in an
# `attached` test.
puts "── boot wiring ──"
set _main [file join $_dir .. main.tcl]
set _msrc ""
if {[file exists $_main]} {
    set _fh [open $_main r]; fconfigure $_fh -encoding utf-8
    set _msrc [read $_fh]; close $_fh
}
set _sfh [open [file join $_dir setup.tcl] r]; fconfigure $_sfh -encoding utf-8
set _setupsrc [read $_sfh]; close $_sfh
ok "main.tcl selects the installer or ordinary first-run boot" \
    {[string match "*::vsetup::boot*" $_msrc] && [string match "*proc ::vsetup::boot*" $_setupsrc] && [string match "*maybe_first_run*" $_setupsrc]}
# The regression shape: maybe_first_run reachable only inside an
# `attached`-gated block. Assert the boot call is NOT wrapped in one.
set _gated 0
foreach _line [split $_msrc \n] {
    # A line that both tests attach state and calls the wizard is the
    # bug; so is an attach-gated if-block whose body is the call. Scan
    # line by line first, then the block form below.
    if {[string match "*attached*" $_line] && [string match "*maybe_first_run*" $_line]} {
        set _gated 1
    }
}
# And the block form: an attach test on one line followed within a
# couple of lines by the wizard call.
if {[regexp {attached[^\n]*\n[^\n]*\n?[^\n]*maybe_first_run} $_msrc]} { set _gated 1 }
ok "the first-run wizard is not gated on being attached" {$_gated == 0}

# ── 11. REVIEW 2026-09-27: each of these was red on the merged branch ─
puts "── review: Skip, routing, language, port, stale base, reload ──"

# Skip SKIPS: the section goes out as `skipped`, whatever its fields say.
# (Skip used to be Next, so a filled-in step was written anyway.)
reset
::vsetup::open
for {set i 0} {$i < 3} {incr i} { ::vsetup::next }   ;# telegram step
set ::vsetup::v(telegram) 1
::vsetup::next 1                                      ;# Skip
ok "Skip moves on" {[wm title .setup_wizard] eq "Setup — IRC"}
ok "…and records the section as skipped" {"telegram" in $::vsetup::skipped}
::vsetup::back
::vsetup::next                                        ;# Next this time
ok "answering the step again un-skips it" {"telegram" ni $::vsetup::skipped}
::vsetup::next 1                                      ;# Skip IRC
for {set i 0} {$i < 2} {incr i} { ::vsetup::next }   ;# acl → summary
ok "the skip reaches the wire" \
    {[dict get [last_body /api/setup/preview] skipped] eq {irc}}

# The [model] provider follows a renamed entry until the operator
# changes it on the model step. Seeded once and never updated, a rename
# routed the default model through a name no entry had.
reset
::vsetup::open
::vsetup::next                                        ;# provider step
set e .setup_wizard.body.c.inner.f.e20
focus -force $e ; update
$e delete 0 end ; $e insert 0 mine
event generate $e <KeyRelease> -keysym e
update
ok "a renamed entry carries the \[model\] provider with it" \
    {$::vsetup::v(default_provider) eq "mine"}
::vsetup::next                                        ;# model step
set e2 .setup_wizard.body.c.inner.f.e2
focus -force $e2 ; update
event generate $e2 <KeyPress> -keysym x
update
set ::vsetup::v(default_provider) other-entry
::vsetup::back
set e .setup_wizard.body.c.inner.f.e20
$e insert end 2
event generate $e <KeyRelease> -keysym 2
update
ok "…until the operator names another one" {$::vsetup::v(default_provider) eq "other-entry"}

# "Make default" is an answer, written with the rest — not a POST to
# /api/config/set before the summary (which, on a first run, created
# config.toml while Cancel still said nothing had been written).
reset
::vsetup::open
set l .setup_wizard.body.c.inner.f.lang
$l selection clear 0 end
set di [lsearch -glob [$l get 0 end] "*·  de"]
$l selection set $di
::vsetup::_lang_default $l
ok "Make default writes nothing yet" {[lsearch -exact $::vapi::posts /api/config/set] < 0}
ok "…it sets the locale answer" {$::vsetup::v(locale) eq "de"}
for {set i 0} {$i < 6} {incr i} { ::vsetup::next }
ok "…which rides in the body" {[dict get [last_body /api/setup/preview] locale] eq "de"}

# A port that is not one stops on its own screen, with the reason.
reset
::vsetup::open
for {set i 0} {$i < 4} {incr i} { ::vsetup::next }   ;# irc step
set ::vsetup::v(irc_server) irc.libera.chat
set ::vsetup::v(irc_port) 66977
::vsetup::next
ok "a bad port keeps the IRC step up" {[wm title .setup_wizard] eq "Setup — IRC"}
ok "…and says why" {[string match "*1 to 65535*" [lindex $::vstatus::msgs end]]}
set ::vsetup::v(irc_port) 6697
::vsetup::next
ok "a good one moves on" {[wm title .setup_wizard] eq "Setup — access control"}

# A stale preview (the file changed after it was read): the engine
# answers 409, nothing is written, the preview refreshes, and the reason
# is said — no finish screen.
reset
::vsetup::open
for {set i 0} {$i < 6} {incr i} { ::vsetup::next }
set ::vapi::apply_code 409
set ::vapi::apply_json {{"ok": false, "error": "config.toml changed since the preview — refresh the preview and check it again"}}
set ::vapi::posts {}
::vsetup::next                                        ;# Write it
ok "a 409 does not reach the finish screen" {[wm title .setup_wizard] eq "Setup — summary"}
ok "…it re-previews" {[lsearch -exact $::vapi::posts /api/setup/preview] >= 0}
ok "…and says the engine's reason, not raw JSON" \
    {[string match "config.toml changed since the preview*" [lindex $::vstatus::msgs end]]}

# A reload that FAILED is said as one. The first cut printed "the live
# config handle was already reloaded" whatever apply returned.
reset
::vsetup::open
for {set i 0} {$i < 6} {incr i} { ::vsetup::next }
set ::vapi::apply_json {{"ok": true, "mode": "fresh", "config_path": "/cfg/config.toml", "config_backup": null, "env_path": "/cfg/.env", "env_written": true, "secrets_needed": [], "notes": [], "reloaded": false}}
::vsetup::next
set report [.setup_wizard.body.report get 1.0 end]
ok "a failed reload is reported, with the restart to do" \
    {[string match "*could not reload*veles gateway restart*" $report] \
     && ![string match "*gateway reloaded the config*" $report]}
ok "no backup line when there was nothing to back up" {![string match "*backed up*" $report]}

# `secrets_needed` is what is STILL unset anywhere after the write (the
# vault included), so the finish screen lists all of it as the to-do,
# with where to set each — not "uncomment .env", which is no longer how
# a key gets in.
reset
::vsetup::open
for {set i 0} {$i < 6} {incr i} { ::vsetup::next }
set ::vapi::apply_json {{"ok": true, "mode": "fresh", "config_path": "/cfg/config.toml", "config_backup": null, "env_path": "/cfg/.env", "env_written": true, "env_added": ["NEW_ONLY"], "secrets_needed": ["OLD_ONE", "NEW_ONLY"], "notes": [], "reloaded": true}}
::vsetup::next
set report [.setup_wizard.body.report get 1.0 end]
ok "the finish screen lists what is still to set, and how" \
    {[string match "*Still to set: OLD_ONE, NEW_ONLY*/secret set NAME*" $report] \
     && ![string match "*uncomment*" $report]}

# The open door is said on the screens that open it.
reset
::vsetup::open
for {set i 0} {$i < 3} {incr i} { ::vsetup::next }
ok "the Telegram step names its allowlist" \
    {[string match "*TELEGRAM_ALLOWED_USERS*owner*" [wtext .setup_wizard.body.c.inner.f.door]]}
::vsetup::next
ok "the IRC step names its allowlist" \
    {[string match "*IRC_ALLOWED_USERS*owner*" [wtext .setup_wizard.body.c.inner.f.door]]}
::vsetup::next
ok "the owner hint no longer suggests an irc: mask" \
    {![string match "*irc:nick!*" [wtext .setup_wizard.body.c.inner.f.lo]] \
     && [string match "*mask*" [wtext .setup_wizard.body.c.inner.f.lo]]}

# ── secrets: masked fields, values only in apply, never kept ─────────
puts "── secrets ──"

set LOCAL_STATE [string map [list @P@ $::PRESETS_JSON] {{"first_run": true, "config_exists": false, "config_path": "/cfg/config.toml", "env_path": "/cfg/.env", "env_exists": false, "current": null, "current_error": null, "stored": [], "accepts_secrets": true, "vault_path": "/cfg/vault", "presets": @P@}}]
set KEYV "sk-typed-in-the-wizard-0123"

reset
set ::vapi::state_json $LOCAL_STATE
set ::vapi::preview_json {{"ok": true, "mode": "fresh", "base_sig": "", "diff": "", "config_toml": "# c\n", "env_text": "", "secrets": ["OPENROUTER_API_KEY", "TELEGRAM_BOT_TOKEN"], "stored": [], "notes": []}}
::vsetup::open
::vsetup::next
ok "the provider step has a masked key field" \
    {[winfo exists .setup_wizard.body.c.inner.f.e25] && [.setup_wizard.body.c.inner.f.e25 cget -show] eq "•"}
ok "…named for a screen reader without the value" \
    {[dict get $::va11y::names .setup_wizard.body.c.inner.f.e25] eq "secret value, hidden while typed"}
set ::vsetup::secret(key) $KEYV
ok "a typed key is a value to store, under its env var's name" \
    {[::vsetup::secret_values] eq [list OPENROUTER_API_KEY $KEYV]}
ok "the answers signature names it, never holds it" \
    {[string match "*OPENROUTER_API_KEY*" [::vsetup::answers_sig]] \
     && ![string match "*$KEYV*" [::vsetup::answers_sig]]}
::vsetup::next ; ::vsetup::next
ok "the Telegram step has a masked token field" \
    {[winfo exists .setup_wizard.body.c.inner.f.e2] && [.setup_wizard.body.c.inner.f.e2 cget -show] eq "•"}
set ::vsetup::secret(bot) "123:typed-bot-token-value"
ok "a token for a Telegram that is OFF is not stored" \
    {"TELEGRAM_BOT_TOKEN" ni [::vsetup::secret_values]}
set ::vsetup::v(telegram) 1
ok "…and is once it is on" {"TELEGRAM_BOT_TOKEN" in [::vsetup::secret_values]}
::vsetup::next
ok "the IRC step has a SASL account and a masked password" \
    {[winfo exists .setup_wizard.body.c.inner.f.e10] && [.setup_wizard.body.c.inner.f.e12 cget -show] eq "•"}
set ::vsetup::secret(sasl) "sasl-typed-password"
ok "a SASL password with no server or account is not stored" \
    {"IRC_SASL_PASSWORD" ni [::vsetup::secret_values]}
::vsetup::next ; ::vsetup::next
set pv [.setup_wizard.body.c.inner.f.pv.preview get 1.0 end]
ok "the summary says which secrets will be stored — names only" \
    {[string match "*OPENROUTER_API_KEY: will be stored in the encrypted vault*" $pv] \
     && ![string match "*$KEYV*" $pv]}
ok "no preview body ever carried a value" \
    {![string match "*$KEYV*" [join $::vapi::bodies]] && ![string match "*typed-bot*" [join $::vapi::bodies]]}
set ::vapi::apply_json {{"ok": true, "mode": "fresh", "config_path": "/cfg/config.toml", "config_backup": null, "env_path": "/cfg/.env", "env_written": false, "secrets_needed": [], "secrets_stored": ["OPENROUTER_API_KEY", "TELEGRAM_BOT_TOKEN"], "vault_path": "/cfg/vault", "notes": [], "reloaded": true}}
::vsetup::next
set ab [last_body /api/setup/apply]
ok "apply carries the values in its one secrets field" \
    {[dict get $ab secrets OPENROUTER_API_KEY] eq $KEYV \
     && [dict get $ab secrets TELEGRAM_BOT_TOKEN] eq "123:typed-bot-token-value" \
     && ![dict exists $ab secrets IRC_SASL_PASSWORD]}
ok "the values leave the wizard's memory once sent" \
    {$::vsetup::secret(key) eq "" && $::vsetup::secret(bot) eq ""}
set report [.setup_wizard.body.report get 1.0 end]
ok "the finish screen says what went into the vault, by name" \
    {[string match "*stored in the encrypted vault (/cfg/vault): OPENROUTER_API_KEY, TELEGRAM_BOT_TOKEN*" $report] \
     && ![string match "*$KEYV*" $report]}

# A skipped provider step stores no key, whatever was typed.
reset
set ::vapi::state_json $LOCAL_STATE
::vsetup::open
::vsetup::next
set ::vsetup::secret(key) $KEYV
::vsetup::next 1
ok "Skip on the provider step drops its typed key" {[::vsetup::secret_values] eq {}}

# Closing forgets typed values.
reset
set ::vapi::state_json $LOCAL_STATE
::vsetup::open
set ::vsetup::secret(key) $KEYV
::vsetup::_close
ok "closing the wizard forgets a typed key" {$::vsetup::secret(key) eq ""}

# A stored key is said, and an empty field keeps it.
reset
set ::vapi::state_json [string map [list @P@ $::PRESETS_JSON] {{"first_run": false, "config_exists": true, "config_path": "/cfg/config.toml", "env_path": "/cfg/.env", "env_exists": true, "current": null, "current_error": null, "stored": ["OPENROUTER_API_KEY"], "accepts_secrets": true, "presets": @P@}}]
::vsetup::open
::vsetup::next
ok "a stored key is said beside the empty field" \
    {[string match "*A value is stored for OPENROUTER_API_KEY*" [wtext .setup_wizard.body.c.inner.f.k25]]}

# A gateway that takes no values from here (another machine, a network
# it does not trust): no field at all, and where to type it.
reset
set ::vapi::state_json [string map [list @P@ $::PRESETS_JSON] {{"first_run": true, "config_exists": false, "config_path": "/cfg/config.toml", "env_path": "/cfg/.env", "env_exists": false, "current": null, "current_error": null, "stored": [], "accepts_secrets": false, "presets": @P@}}]
::vsetup::open
::vsetup::next
ok "a gateway that takes no values from here gets no key field" {![winfo exists .setup_wizard.body.c.inner.f.e25]}
ok "…and the line that says where to enter it, or what to allow" \
    {[string match "*veles secret set OPENROUTER_API_KEY*trusted_networks*" [wtext .setup_wizard.body.c.inner.f.s25]]}
set ::vsetup::secret(key) $KEYV
ok "…and nothing typed is sent to it" {[::vsetup::secret_values] eq {}}

::vsetup::_close
update idletasks

puts "\nsetuptest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
