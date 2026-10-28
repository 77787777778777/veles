# veles-desktop/lib/setup.tcl — the first-run setup wizard (Tk).
#
# PLAN-setup-wizard-tk.md, decision 1(a): Tk owns the FLOW, core owns
# the WRITES. Every screen here just collects answers into ::vsetup::v;
# the summary screen POSTs them to /api/setup/preview and shows the
# engine's own bytes back, and "Write it" POSTs to /api/setup/apply.
# The block builders, the config emitter, the write rule and the 0600
# rule all live in veles_core::setup — the SAME code `veles setup`
# drives — so the two wizards cannot emit different TOML (the drift
# that produced api_key_env and [platforms.ircx], 60786dd).
#
# THE FLOW: welcome+language FIRST (so the rest of the wizard renders
# in the chosen language and ::vi18n is exercised before any config
# exists), then provider, model, telegram, IRC, ACL posture, summary
# (preview-gates-apply, the ACL promote pattern), finish.
#
# I18N DISCIPLINE (the reason this file looks verbose): EVERY
# translatable string is a literal brace group DIRECTLY inside a
# ::vi18n::mc / ::vi18n::label / ::va11y::name … [::vi18n::mc {…}] call.
# The key extractor (scripts/i18n-keys.tcl) walks exactly those three
# shapes; a key routed through a helper variable (`_head {Title}`, a
# `foreach {idx key} {…}` data list) is INVISIBLE to it and would ship
# English-only in all 46 locales while every catalog reported complete
# — the symbolic-key blind spot that hid seven /tasks strings. So the
# helpers below take ALREADY-TRANSLATED text, and each widget spells its
# own key inline. The window is fully rebuilt on a locale change
# (see `retext`), so baking the text at build time loses nothing.
#
# LANGUAGE SWAP ON AN OPEN WIZARD: answers live in ::vsetup::v, never
# only in widgets, and a locale-change hook re-paints the whole window
# from that state — so a swap re-texts an open wizard instead of
# leaving stale labels on screen (the reverse-sweep covers registered
# widgets; a step-swap that cached text would not).
#
# RE-ENTRY: palette row "Setup wizard…" / `/setup` / C-S-W. Over an
# existing config.toml the answers start from what the FILE says
# (/api/setup/state `current`) and the engine EDITS the file in place:
# only the sections answered change, and a walk that changes nothing
# writes nothing (USER 2026-09-27; the version this replaced swapped the
# live config for the wizard's minimal one). Skip means "leave this
# section as it is" — it is sent as `skipped`, never as Next. First-run
# boot detection lives in main.tcl: it fires only when config.toml is
# ABSENT and the desktop's own setup_done marker (desktop_state.json —
# NOT config.toml, which must stay copyable between machines) is unset.
#
# KEYBOARD: declared Tab order per step (::va11y::tab_order), Return
# advances from any entry, Escape cancels with a confirm, and C-g
# cancels too (a toplevel binding, which beats editline's `bind all
# <Control-g>` ladder — the ask-dialog arm is the precedent).

namespace eval ::vsetup {
    variable win ""
    variable cur 0          ;# current step index
    variable state {}       ;# /api/setup/state payload (first_run, current)
    variable api_err ""     ;# why the last API call failed (shown, not swallowed)
    variable preview_sig "" ;# answers the on-screen preview was built from
    variable preview_base "";# base_sig of the config.toml that preview saw
    variable preview_plan "";# exact combined setup + component review
    variable applied {}     ;# the /api/setup/apply report, on the finish step
    variable skipped {}     ;# section ids the operator Skipped (see _section_for)
    # R11 component installer. Component previews never include the
    # existing config text; only these typed, non-secret answers return
    # to apply together with the engine's base/plan signatures.
    variable install_state {}
    variable install_manifest {}
    variable install_options {}
    variable install_plan {}
    variable install_preview {}
    variable install_busy 0
    variable install_profile desktop
    variable install_system 0
    variable install_user_unit 0
    variable install_container_storage ""
    variable install_container_gateway veles
    variable install_container_sandbox veles-sandbox
    variable install_container_network velesnet
    variable install_container_image ""
    variable install_container_digest ""
    variable install_container_name gateway
    variable install_core ""
    variable install_pds 0
    variable install_pds_url https://pds.example
    variable install_pds_domain pds.example
    variable install_pds_email acme@example
    variable install_pds_image ghcr.io/jazware/vlpds:1.2.2
    variable install_pds_digest ""
    variable install_pds_s3_endpoint https://s3.example
    variable install_pds_s3_region auto
    variable install_pds_s3_bucket pds-bucket
    variable install_pds_s3_prefix pds
    variable install_pds_network veles-net
    variable install_pds_backup 0
    variable install_container_file ""
    variable install_container_options {}
    variable install_container_json ""
    variable install_mode 0
    variable profile_mode 0
    variable install_instance ""
    # The model step's list (from /api/setup/models) and the request
    # generation that drops a late reply for an older fetch.
    variable models {}
    variable models_gen 0
    variable install_apply_pending 0
    # The answers. Widgets -variable into these, so a re-paint after a
    # language swap rebuilds every field FROM here — the values survive
    # the rebuild because they never lived in the widgets alone. The
    # *_touched flags mark a field the operator typed into, so flipping
    # the provider choice re-suggests defaults without clobbering input.
    variable v
    array set v {
        provider openrouter     base_url {}       key_env {}
        api_mode {}             provider_name {}  model {}
        default_provider {}     model_touched 0   name_touched 0
        keyenv_touched 0        base_touched 0    dp_touched 0
        telegram 0              irc_server {}     irc_port 6697
        irc_tls 1               irc_nick {}       irc_channels {}
        acl_idx 0               acl_owner {}      locale {}
        irc_sasl {}
    }
    # Secret VALUES the operator typed (masked fields). Kept apart from
    # `v` on purpose: nothing that snapshots or signs the answers ever
    # sees them — answers_sig takes only whether each is filled — and
    # they are cleared the moment they are sent, and on every close.
    variable secret
    array set secret { key {} bot {} sasl {} }
}

# Forget every typed secret value (after a write, on close).
proc ::vsetup::_forget_secrets {} {
    variable secret
    foreach k [array names secret] { set secret($k) "" }
}

# The gateway's word on whether THIS desktop may send it secret values:
# its own machine, or a network in its [vault] trusted_networks (a
# WireGuard subnet). An older gateway that does not say is treated as
# "no" — no field is offered that the write would then refuse.
proc ::vsetup::_secrets_local {} {
    variable state
    return [expr {[dict exists $state accepts_secrets] && [dict get $state accepts_secrets] eq "true"}]
}

# Is a value for NAME already stored somewhere the gateway can read?
proc ::vsetup::_stored {name} {
    variable state
    set st {}
    catch { set st [dict get $state stored] }
    return [expr {$name ne "" && $name in $st}]
}

# A masked field for one secret, or — against a gateway that takes no
# values from this machine — the line that says where to enter it instead. `slot` is the ::vsetup::secret
# element; `name` the variable it is stored as.
proc ::vsetup::_secret_field {p row label slot name} {
    if {![_secrets_local]} {
        pack [_note $p.s$row [format [::vi18n::mc {This gateway does not take secret values from this machine: enter it there with veles secret set %s, or list this network in its [vault] trusted_networks}] $name]] \
            -anchor w -fill x -pady {6 0}
        return ""
    }
    ttk::label $p.l$row -text $label
    ttk::entry $p.e$row -textvariable ::vsetup::secret($slot) -width 46 -show •
    pack $p.l$row -anchor w -pady {6 0}
    pack $p.e$row -fill x
    ::va11y::name $p.e$row [::vi18n::mc {secret value, hidden while typed}]
    bind $p.e$row <Return> [list ::vsetup::next]
    if {[_stored $name]} {
        pack [_note $p.k$row [format [::vi18n::mc {A value is stored for %s — leave this empty to keep it.}] $name]] \
            -anchor w -fill x
    }
    return $p.e$row
}

# The provider step's rows: /api/setup/state `presets`, which is
# veles_core::setup::PROVIDER_PRESETS — dicts with choice, label,
# entry_name, base_url, key_env, api_mode, model, key_url. A gateway too
# old to send them gets the one row every gateway understands.
proc ::vsetup::presets {} {
    variable state
    set p {}
    catch { set p [dict get $state presets] }
    if {[llength $p] == 0} {
        set p [list [dict create choice openai_compatible \
            label "OpenAI-compatible server" entry_name local \
            base_url http://localhost:8000/v1 key_env {} \
            api_mode chat_completions model {} key_url {}]]
    }
    return $p
}

# The preset row for a choice id, or "" (skip, or unknown).
proc ::vsetup::preset {choice} {
    foreach p [presets] {
        if {[dict get $p choice] eq $choice} { return $p }
    }
    return ""
}

# Step ids + the English TITLE of each (the title is a key; it is
# rendered through ::vi18n::mc at paint time, so it re-texts). The id is
# what _build_step dispatches on; the title key is a literal in mc below.
proc ::vsetup::_step_ids {} {
    variable install_mode
    if {$install_mode} { return {profile components install_options summary} }
    return {welcome provider model telegram irc acl summary}
}

# Ordinary provider setup stays the original seven-step walk. Component
# installation is an explicit mode, never mandatory first-run discovery.
proc ::vsetup::open_install {} {
    variable install_mode
    variable profile_mode
    variable install_manifest
    variable install_preview
    variable install_apply_pending
    set install_mode 1
    set install_manifest {}
    set install_preview {}
    set install_apply_pending 0
    set profile_mode 0
    return [open]
}

proc ::vsetup::open_profiles {} {
    variable install_mode
    variable profile_mode
    variable install_manifest
    variable install_preview
    set install_mode 1
    set profile_mode 1
    set install_manifest {}
    set install_preview {}
    return [open]
}

proc ::vsetup::step_count {} {
    return [llength [_step_ids]]
}

proc ::vsetup::step_id {n} {
    return [lindex [_step_ids] $n]
}

# The config.toml section a step owns — the id the engine takes in
# `skipped`. The welcome step owns the default language; the summary
# owns nothing.
proc ::vsetup::_section_for {id} {
    switch -- $id {
        profile - components - install_options { return "" }
        welcome { return locale }
        provider - model - telegram - irc - acl { return $id }
    }
    return ""
}

# The window title for a step, translated. One switch, each arm an
# inline mc literal so the extractor sees all seven.
proc ::vsetup::_title_for {id} {
    switch -- $id {
        profile  { return [::vi18n::mc {Setup — installation profile}] }
        components { return [::vi18n::mc {Setup — components}] }
        install_options { return [::vi18n::mc {Setup — component options}] }
        welcome  { return [::vi18n::mc {Setup — welcome}] }
        provider { return [::vi18n::mc {Setup — LLM provider}] }
        model    { return [::vi18n::mc {Setup — default model}] }
        telegram { return [::vi18n::mc {Setup — Telegram}] }
        irc      { return [::vi18n::mc {Setup — IRC}] }
        acl      { return [::vi18n::mc {Setup — access control}] }
        summary  { return [::vi18n::mc {Setup — summary}] }
    }
    return [::vi18n::mc {Setup}]
}

# ── open / close ─────────────────────────────────────────────────────

# Open the wizard. Safe to call twice (raises the existing window).
# Fetches /api/setup/state first so the welcome screen can say honestly
# whether this is a first run or a re-entry, and so a re-entry starts
# from what the file already says.
proc ::vsetup::open {} {
    variable win
    variable state
    variable api_err
    variable cur
    variable applied
    variable skipped
    variable preview_sig
    variable install_apply_pending
    variable install_mode
    variable profile_mode
    if {$win ne "" && [winfo exists $win]} {
        wm deiconify $win ; raise $win ; focus $win
        return $win
    }
    set applied {}
    set ::vsetup::models {}
    set api_err ""
    set state {}
    set skipped {}
    if {$install_mode} { set skipped {provider model telegram irc acl locale} }
    set preview_sig ""
    set install_apply_pending 0
    if {!$install_mode} { set profile_mode 0 }
    if {[catch { set state [::vapi::get_json /api/setup/state] } err]} {
        # A dead gateway is not a reason to refuse the window: the steps
        # still collect answers, and the summary says the API is gone
        # rather than pretending a preview is fresh.
        set api_err [::vapi::reason $err]
        set state {}
    }
    _prefill
    set cur 0
    _suggest_provider 0 ;# fill what the file left empty for the initial choice
    set win .setup_wizard
    catch { destroy $win }
    toplevel $win
    wm minsize $win 620 480
    ::vplace::centered $win 680 560
    # The re-text hook: a live language swap re-paints the OPEN wizard
    # from ::vsetup::v. Registered here, removed in _close — a hook left
    # behind would repaint a destroyed window.
    ::vi18n::on_locale_change [list ::vsetup::retext]
    # Escape and C-g both cancel with a confirm. A toplevel binding
    # outranks editline's `bind all <Control-g>` (bindtags: widget →
    # class → toplevel → all), so C-g inside the wizard means "cancel
    # the wizard", never the generic "destroy the focused toplevel".
    bind $win <Escape>    [list ::vsetup::cancel]
    bind $win <Control-g> [list ::vsetup::cancel]
    wm protocol $win WM_DELETE_WINDOW [list ::vsetup::cancel]
    _paint
    return $win
}

# Seed the answers from /api/setup/state `current` — the answers an
# existing config.toml already gives. Prefilled fields are NOT marked
# touched: they stand while the provider stays the file's, and a
# deliberate provider change re-suggests that provider's defaults.
# No `current` (first run, or a file that does not load) leaves the
# answers as they are.
proc ::vsetup::_prefill {} {
    variable state
    variable v
    if {![dict exists $state current]} { return }
    set c [dict get $state current]
    if {$c eq "" || $c eq "null" || [catch { dict size $c }]} { return }
    set g [list apply {{c k {d ""}} {
        expr {[dict exists $c $k] ? [dict get $c $k] : $d}
    }}]
    set v(provider) [{*}$g $c provider openai_compatible]
    if {$v(provider) ne "skip" && [preset $v(provider)] eq ""} { set v(provider) openai_compatible }
    foreach {field key} {
        base_url base_url  key_env key_env  api_mode api_mode
        provider_name provider_name  model model
        default_provider default_provider  irc_server irc_server
        irc_nick irc_nickname  irc_channels irc_channels
        acl_owner acl_owner  locale locale
    } {
        set v($field) [{*}$g $c $key]
    }
    set v(irc_sasl) [{*}$g $c irc_sasl_username]
    set v(irc_port) [{*}$g $c irc_port 6697]
    set v(telegram) [expr {[{*}$g $c telegram false] eq "true" ? 1 : 0}]
    set v(irc_tls) [expr {[{*}$g $c irc_use_tls true] eq "true" ? 1 : 0}]
    set v(acl_idx) [expr {[{*}$g $c acl legacy_open] eq "curated" ? 1 : 0}]
    # The [model] provider follows the entry name only while the two
    # agree; a file that routes through another entry keeps that.
    set v(dp_touched) [expr {$v(default_provider) ne $v(provider_name)}]
}

# Cancel with a confirm — Escape, C-g and the ✕ all land here. Nothing
# has been written until the summary's "Write it", so the question is
# only about losing the answers.
proc ::vsetup::cancel {} {
    variable win
    if {$win eq "" || ![winfo exists $win]} { return }
    set answer [tk_messageBox -parent $win -type yesno -icon question \
        -title [::vi18n::mc {Setup}] \
        -message [::vi18n::mc {Close the setup wizard? Nothing has been written yet.}]]
    if {$answer eq "yes"} { _close }
}

proc ::vsetup::_close {} {
    variable win
    # Invalidate component callbacks before destroying the widgets they
    # paint. An in-flight HTTP request cannot make an old selection or
    # reviewed preview live again on re-entry.
    ::vinstall::reset
    _forget_secrets
    ::vi18n::off_locale_change [list ::vsetup::retext]
    if {$win ne "" && [winfo exists $win]} { destroy $win }
    set win ""
}

# Re-paint after a language swap. The whole window is rebuilt from
# state — chrome and current step — so no label can survive in the old
# language. Registered with ::vi18n::on_locale_change.
proc ::vsetup::retext {} {
    variable win
    if {$win eq "" || ![winfo exists $win]} { return }
    _paint
}

# ── the window: chrome + content + nav ──────────────────────────────

proc ::vsetup::_paint {} {
    variable win
    variable cur
    variable applied
    variable profile_mode
    if {$win eq "" || ![winfo exists $win]} { return }
    set id [step_id $cur]
    wm title $win [expr {$applied ne "" && $profile_mode ? [::vi18n::mc {Setup — profile result}] : [_title_for $id]}]
    ::vtitle::attach $win [::vi18n::mc {Setup wizard}] -close [list ::vsetup::cancel]

    # Step indicator — rebuilt every paint so a swap re-formats it.
    catch { destroy $win.head }
    ttk::frame $win.head
    pack $win.head -fill x -padx 12 -pady {6 0}
    ttk::label $win.head.step -foreground $::vt::T(fg_dim)
    ::vi18n::label $win.head.step {Step %1$d of %2$d} \
        [expr {$cur + 1}] [step_count]
    pack $win.head.step -side left

    # Content frame — one per paint, so a step never shows a widget
    # built under an older locale. It scrolls: a step can be taller than
    # the window, as the provider and IRC steps are on a phone, and with
    # the keyboard up the field being typed into was out of sight.
    catch { destroy $win.body }
    set p [::vmanage::scrollframe $win body]
    $win.body.c configure -background [ttk::style lookup TFrame -background]
    pack $win.body -fill both -expand 1 -padx 12 -pady 8

    # The finish screen replaces both nav and body; draw it and stop.
    if {[llength $applied]} {
        _paint_finish
        return
    }

    # Nav bar. The summary's Next IS "Write it".
    #
    # Destroyed like head and body above: _paint rebuilds the WHOLE
    # window on every step change and every language swap, and a frame
    # it forgets to tear down makes the next ttk::frame throw "window
    # name nav already exists" — which is the wizard being unable to
    # leave its first screen (found by setuptest's forward walk).
    catch { destroy $win.nav }
    ttk::frame $win.nav
    # From the bottom, ahead of the body: the way on stays on the window
    # whatever a step needs (on a phone the language step pushed Next off
    # the screen).
    pack $win.nav -side bottom -fill x -padx 12 -pady {0 10} -before $win.body
    ttk::button $win.nav.cancel -command [list ::vsetup::cancel]
    ::vi18n::label $win.nav.cancel {Cancel}
    ttk::button $win.nav.skip -command [list ::vsetup::next 1]
    ::vi18n::label $win.nav.skip {Skip}
    ttk::button $win.nav.back -command [list ::vsetup::back]
    ::vi18n::label $win.nav.back {Back}
    ttk::button $win.nav.next -command [list ::vsetup::next 0]
    if {$cur == [expr {[step_count] - 1}]} {
        ::vi18n::label $win.nav.next {Write it}
    } else {
        ::vi18n::label $win.nav.next {Next}
    }
    pack $win.nav.cancel -side left
    pack $win.nav.next $win.nav.back $win.nav.skip -side right -padx {6 0}
    if {$cur == 0} { $win.nav.back state disabled }
    # Skip on the summary would mean "write nothing" — that is Cancel's
    # job, and a button that says Skip while writing a config is worse
    # than no button.
    if {$cur == [expr {[step_count] - 1}]} { $win.nav.skip state disabled }
    # Four buttons are wider than a phone, where Skip got no room at all.
    if {[llength [info commands ::vandroid::flow]]} { ::vandroid::flow $win.nav -from end }

    _build_step $p $id
    ::vmanage::scroll_children $p
    ::va11y::tab_order [list $win.nav.back $win.nav.next $win.nav.skip $win.nav.cancel]
}

proc ::vsetup::back {} {
    variable cur
    if {$cur > 0} { incr cur -1 ; _paint }
}

# next — Next answers the step; Skip leaves its section out of a new
# config and exactly as it is in an existing one. They used to be the
# same motion, so "Skip" on a filled-in step wrote it anyway. On the
# last step, Next applies.
proc ::vsetup::next {{skip 0}} {
    variable cur
    variable skipped
    variable v
    variable install_mode
    variable install_manifest
    variable install_preview
    variable install_apply_pending
    set id [step_id $cur]
    set sec [_section_for $id]
    if {$sec ne ""} {
        set skipped [lsearch -all -inline -not -exact $skipped $sec]
        if {$skip} { lappend skipped $sec }
    }
    # A port that is not one is said here, on its own screen — not
    # turned into 6697 on the way out.
    if {$id eq "irc" && !$skip && [string trim $v(irc_server)] ne ""
        && ![_port_ok $v(irc_port)]} {
        _say [::vi18n::mc {the IRC port must be a number from 1 to 65535}]
        return
    }
    # A provider with no model writes a config that cannot answer; the
    # engine refuses it at preview, but by then the person is four
    # screens past the field. Say it where the field is.
    if {$id eq "model" && !$skip && $v(provider) ne "skip"
        && [string trim $v(model)] eq ""} {
        _say [::vi18n::mc {choose a model: pick one from the list, or type its name}]
        return
    }
    if {$install_mode && $id in {components install_options}} {
        if {$install_apply_pending && $id eq "components" && ![dict size $install_manifest]} {
            _say [::vi18n::mc {The component manifest is still loading. Wait for it or choose another component.}]
            return
        }
        if {$id eq "install_options" && $install_apply_pending} {
            _say [::vi18n::mc {The component preview is still loading. Wait for the reply before reviewing the combined changes.}]
            return
        }
        if {[dict size $install_manifest] && !$skip} {
            if {!$install_apply_pending && ![dict size $install_preview]} {
                ::vinstall::refresh_preview
                _say [::vi18n::mc {Review the component options on the summary screen before applying.}]
                if {$id eq "install_options"} { return }
            }
            if {[dict size $install_preview] && [dict get $install_preview answers] ne [::vinstall::answers]} {
                ::vinstall::refresh_preview
                _say [::vi18n::mc {Component options changed. Review the new preview before applying.}]
                return
            }
        } elseif {$skip} {
            ::vinstall::reset
            set install_manifest {}
        }
    }
    if {$cur < [expr {[step_count] - 1}]} {
        incr cur
        _paint
        return
    }
    if {$install_mode && $install_apply_pending} {
        _say [::vi18n::mc {The component preview is not ready yet. Refresh it and review the result before applying.}]
        return
    }
    if {$install_mode && [dict size $install_manifest] && ![dict size $install_preview]} {
        _say [::vi18n::mc {Review the component preview before applying. No component configuration has been written.}]
        return
    }
    if {$install_mode} { set skipped {provider model telegram irc acl locale} }
    _apply
}

proc ::vsetup::_port_ok {p} {
    return [expr {[string is integer -strict $p] && $p >= 1 && $p <= 65535}]
}

# Dispatch one step's body builder.
proc ::vsetup::_build_step {p id} {
    switch -- $id {
        profile  { ::vinstall::step_profile $p }
        components { ::vinstall::step_components $p }
        install_options { ::vinstall::step_options $p }
        welcome  { _step_welcome  $p }
        provider { _step_provider $p }
        model    { _step_model    $p }
        telegram { _step_telegram $p }
        irc      { _step_irc      $p }
        acl      { _step_acl      $p }
        summary  { _step_summary  $p }
    }
}

# Shared shape for a step: heading + hint + a form frame. Takes
# ALREADY-TRANSLATED heading/hint (the caller inlines the mc), returns
# the form frame for the builder to fill.
proc ::vsetup::_step_head {p heading hint} {
    ttk::label $p.h -text $heading -font {TkDefaultFont 12 bold}
    pack $p.h -anchor w
    if {$hint ne ""} {
        ttk::label $p.hint -text $hint -wraplength 620 \
            -justify left -foreground $::vt::T(fg_dim)
        pack $p.hint -anchor w -fill x -pady {2 8}
    }
    ttk::frame $p.f
    pack $p.f -fill both -expand 1
    return $p.f
}

# An entry row: label above, entry below. Takes ALREADY-TRANSLATED label
# and a11y name (caller inlines both mc's, so the extractor sees them).
# Returns the entry widget. The -textvariable binds the answer, so a
# re-paint rebuilds the widget around the same value. PACKED, not
# gridded: the steps mix radio buttons and checkbuttons (packed) with
# fields, and pack+grid under one master fight over every <Configure>.
proc ::vsetup::_field {p row label varname aname {width 46}} {
    ttk::label $p.l$row -text $label
    ttk::entry $p.e$row -textvariable ::vsetup::v($varname) -width $width
    pack $p.l$row -anchor w -pady {6 0}
    pack $p.e$row -fill x
    ::va11y::name $p.e$row $aname ;# i18n-exempt: caller-supplied label, already through ::vi18n::mc
    bind $p.e$row <Return> [list ::vsetup::next]
    return $p.e$row
}

# A dim, wrapped explanatory line (already-translated text).
proc ::vsetup::_note {w text {color fg_dim}} {
    ttk::label $w -text $text -wraplength 600 -justify left \
        -foreground $::vt::T($color)
    return $w
}

# ── step: welcome + language ────────────────────────────────────────

proc ::vsetup::_step_welcome {p} {
    variable state
    variable api_err
    set f [_step_head $p [::vi18n::mc {Welcome to veles}] \
        [::vi18n::mc {This wizard writes config.toml, and puts the keys you give it into the encrypted vault. Every step can be skipped: a skipped step is left out of a new config, and left exactly as it is in an existing one. An existing config.toml is edited in place — only the sections you answer change, and everything else in it, comments included, stays.}]]

    # Honest state line: first run, or a re-entry over an existing
    # config. From /api/setup/state, not from a guess.
    ttk::label $f.state -wraplength 620 -justify left -foreground $::vt::T(gold)
    if {$api_err ne ""} {
        ::vi18n::label $f.state {cannot reach the gateway: %s} $api_err
    } elseif {[dict exists $state first_run] && [dict get $state first_run]} {
        ::vi18n::label $f.state {First run: no config.toml yet.}
    } else {
        ::vi18n::label $f.state {config.toml already exists — every answer below starts from it, and only the sections you answer change.}
    }
    pack $f.state -anchor w -fill x -pady {0 2}
    # WHICH file: the desktop may be attached to another box's gateway,
    # and this writes that box's config.
    if {[dict exists $state config_path]} {
        ttk::label $f.path -foreground $::vt::T(fg_dim)
        ::vi18n::label $f.path {config file: %s} [dict get $state config_path]
        pack $f.path -anchor w -pady {0 8}
    }

    # Language FIRST: the rest of the wizard renders in whatever is
    # picked here, live — no restart, no second window.
    ttk::label $f.ll -text [::vi18n::mc {Language}]
    pack $f.ll -anchor w -pady {4 0}
    ttk::label $f.lhint -text [::vi18n::mc {pick a language (↑↓ Enter) — the rest of the wizard follows immediately}] \
        -foreground $::vt::T(fg_dim)
    pack $f.lhint -anchor w

    listbox $f.lang -height 7 -background $::vt::T(bg_panel) \
        -foreground $::vt::T(fg) -selectbackground $::vt::T(select_bg) \
        -selectforeground $::vt::T(fg) -highlightthickness 0 \
        -relief flat -bd 0 -activestyle none
    foreach id [::vi18n::available] {
        set name [::vi18n::locale_name $id]
        $f.lang insert end [expr {$name eq $id ? $id : "$name  ·  $id"}]
        if {$id eq [::vi18n::current]} {
            $f.lang selection clear 0 end
            $f.lang selection set end
            $f.lang see end
        }
    }
    pack $f.lang -fill x -pady 4
    ::va11y::name $f.lang [::vi18n::mc {wizard language}]
    bind $f.lang <Double-Button-1> [list ::vsetup::_lang_use $f.lang]
    bind $f.lang <Return> [list ::vsetup::_lang_use $f.lang]

    ttk::frame $f.lb
    ttk::button $f.lb.use -command [list ::vsetup::_lang_use $f.lang]
    ::vi18n::label $f.lb.use {Use now}
    ttk::button $f.lb.def -command [list ::vsetup::_lang_default $f.lang]
    ::vi18n::label $f.lb.def {Make default (config.toml)}
    pack $f.lb -anchor w -pady {2 4}
    pack $f.lb.use $f.lb.def -side left -padx {0 6}
    # Wider than a small phone, where "(config.toml)" was cut off: there
    # the two wrap into rows (lib/android.tcl).
    if {[llength [info commands ::vandroid::flow]]} { ::vandroid::flow $f.lb }
    ::va11y::tab_order [list $f.lang $f.lb.use $f.lb.def]
    focus $f.lang
}

# The id under the listbox cursor (rows end with the canonical id).
proc ::vsetup::_lang_selected {listw} {
    set sel [lindex [$listw curselection] 0]
    if {$sel eq ""} { return "" }
    set row [$listw get $sel]
    set idx [string last "  ·  " $row]
    if {$idx < 0} { return [string trim $row] }
    return [string trim [string range $row [expr {$idx + 5}] end]]
}

proc ::vsetup::_lang_use {listw} {
    set id [_lang_selected $listw]
    if {$id eq ""} { return }
    if {[catch { ::vi18n::use $id } err]} {
        _say $err
        return
    }
    # `use` re-texts registered widgets and fires the locale hooks —
    # this window's hook re-paints it, so the language list itself comes
    # back with the selection intact (it is rebuilt from state).
    _say [format [::vi18n::mc {wizard language: %s}] $id]
}

# The default language is an ANSWER like the others: `tui.locale`,
# written with the rest at "Write it". It used to POST /api/config/set
# on the spot — which on a first run created config.toml before the
# summary (while Cancel still said "Nothing has been written yet"), and
# which the whole-file write that followed then threw away.
proc ::vsetup::_lang_default {listw} {
    variable v
    set id [_lang_selected $listw]
    if {$id eq ""} { return }
    set v(locale) $id
    _say [format [::vi18n::mc {default language %s — written to config.toml with the other answers}] $id]
}

# ── step: provider ──────────────────────────────────────────────────

proc ::vsetup::_step_provider {p} {
    variable v
    set f [_step_head $p [::vi18n::mc {Which provider should veles talk to?}] \
        [::vi18n::mc {Paste the API key below: it goes into the encrypted vault on the gateway's machine, never into config.toml, and the agent can use it without ever seeing it.}]]

    # One readonly combobox over the presets (product names, from the
    # engine) plus Skip. The selection is v(provider), a choice id.
    set labels {}
    foreach row [presets] { lappend labels [dict get $row label] }
    lappend labels [::vi18n::mc {Skip — configure a provider manually later}]
    ttk::label $f.lp -text [::vi18n::mc {Provider}]
    ttk::combobox $f.cb -state readonly -values $labels -width 44
    $f.cb current [_provider_index]
    pack $f.lp -anchor w -pady {4 0}
    pack $f.cb -anchor w
    ::va11y::name $f.cb [::vi18n::mc {LLM provider}]
    bind $f.cb <<ComboboxSelected>> [list ::vsetup::_provider_picked $f]
    ttk::label $f.hk -wraplength 600 -justify left -foreground $::vt::T(fg_dim)
    pack $f.hk -anchor w -padx {2 0} -pady {2 0}

    _field $f 20 [::vi18n::mc {Provider entry name}] provider_name [::vi18n::mc {provider entry name}]
    _field $f 22 [::vi18n::mc {Base URL}] base_url [::vi18n::mc {provider base URL}]
    _field $f 24 [::vi18n::mc {API key env var name}] key_env [::vi18n::mc {name of the env var holding the API key}]
    set key_entry [_secret_field $f 25 [::vi18n::mc {API key (hidden)}] key $v(key_env)]
    _field $f 26 [::vi18n::mc {Wire protocol (chat_completions or responses)}] api_mode [::vi18n::mc {wire protocol}]
    # Mark typed-into fields so a provider change re-suggests defaults
    # without clobbering what the operator wrote.
    bind $f.e20 <KeyPress> {+ set ::vsetup::v(name_touched) 1 }
    bind $f.e22 <KeyPress> {+ set ::vsetup::v(base_touched) 1 }
    bind $f.e24 <KeyPress> {+ set ::vsetup::v(keyenv_touched) 1 }
    # The [model] provider names this entry until the operator says
    # otherwise on the model step: seeded once and never updated, a
    # renamed entry left the default model routed through a name no
    # entry had — a config that parsed, and failed every turn.
    bind $f.e20 <KeyRelease> {+ ::vsetup::_follow_entry_name }

    _sync_provider_fields $f
    ::va11y::tab_order [concat [list $f.cb $f.e20 $f.e22 $f.e24] $key_entry [list $f.e26]]
    focus $f.cb
}

# v(provider)'s row in the combobox; Skip (and anything unknown) is last.
proc ::vsetup::_provider_index {} {
    variable v
    set i 0
    foreach row [presets] {
        if {[dict get $row choice] eq $v(provider)} { return $i }
        incr i
    }
    return $i
}

# The operator picked a row: that provider's defaults, over every field
# not typed into since the wizard opened.
proc ::vsetup::_provider_picked {f} {
    variable v
    set rows [presets]
    set i [$f.cb current]
    set v(provider) [expr {$i >= 0 && $i < [llength $rows]
        ? [dict get [lindex $rows $i] choice] : "skip"}]
    _provider_changed $f
}

proc ::vsetup::_follow_entry_name {} {
    variable v
    if {!$v(dp_touched)} { set v(default_provider) $v(provider_name) }
}

# Seed the visible defaults for the current provider — shown so the
# operator sees what "accept defaults" means. With `force` 0 (the wizard
# opening over an existing file) only EMPTY fields are filled, so the
# file's values stand; with `force` 1 (the operator changed provider)
# every field not typed into takes the new provider's default. Typed
# fields (the touched flags) are never overwritten.
proc ::vsetup::_suggest_provider {{force 1}} {
    variable v
    set row [preset $v(provider)]
    if {$row ne ""} {
        foreach {field key flag} {
            provider_name entry_name name_touched
            base_url      base_url   base_touched
            key_env       key_env    keyenv_touched
            model         model      model_touched
        } {
            if {!$v($flag) && ($force || $v($field) eq "")} {
                set v($field) [dict get $row $key]
            }
        }
        if {$force || $v(api_mode) eq ""} { set v(api_mode) [dict get $row api_mode] }
    }
    _follow_entry_name
}

# The provider changed: re-suggest defaults, grey the fields when
# skipped, and say where the key comes from.
proc ::vsetup::_provider_changed {f} {
    # A list fetched for the previous provider names its models, not these.
    set ::vsetup::models {}
    _suggest_provider
    _sync_provider_fields $f
}

proc ::vsetup::_sync_provider_fields {f} {
    variable v
    set skip [expr {$v(provider) eq "skip"}]
    set st [expr {$skip ? "disabled" : "normal"}]
    foreach row {20 22 24 25 26} {
        catch { $f.e$row configure -state $st }
        catch { $f.l$row configure -foreground \
            [expr {$st eq "disabled" ? $::vt::T(fg_dim) : $::vt::T(fg)}] }
    }
    set row [preset $v(provider)]
    if {$skip} {
        set hint [::vi18n::mc {Writes no provider. The gateway will not route a turn until a [[custom_providers]] entry and the [model] pair exist.}]
    } elseif {$row ne "" && [dict get $row key_url] ne ""} {
        set hint [format [::vi18n::mc {Get a key at %s}] [dict get $row key_url]]
    } elseif {$row ne "" && [dict get $row key_env] eq ""} {
        set hint [::vi18n::mc {A local server: no key is needed unless you set one up.}]
    } else {
        set hint [::vi18n::mc {Any server speaking the OpenAI wire. Give its base URL and the env var that will hold the key.}]
    }
    catch { $f.hk configure -text $hint }
}

# ── step: model ─────────────────────────────────────────────────────

proc ::vsetup::_step_model {p} {
    variable v
    variable models
    set f [_step_head $p [::vi18n::mc {Default model}] \
        [::vi18n::mc {The route new sessions start on. /model and the models overlay change it later, per session or as the new default.}]]
    # An editable combobox: the provider's own list once fetched, and
    # still free text for a model the list does not show.
    ttk::label $f.l0 -text [::vi18n::mc {Model}]
    ttk::combobox $f.e0 -textvariable ::vsetup::v(model) -width 46 -values $models
    pack $f.l0 -anchor w -pady {6 0}
    pack $f.e0 -fill x
    ::va11y::name $f.e0 [::vi18n::mc {default model name}]
    bind $f.e0 <Return> [list ::vsetup::next]
    ttk::button $f.list -text [::vi18n::mc {List the provider's models}] \
        -command [list ::vsetup::_list_models $f]
    pack $f.list -anchor w -pady {6 0}
    ttk::label $f.ms -wraplength 600 -justify left -foreground $::vt::T(fg_dim)
    pack $f.ms -anchor w -fill x
    _field $f 2 [::vi18n::mc {[model] provider}] default_provider [::vi18n::mc {provider that routes the default model}]
    ttk::label $f.hint2 -wraplength 600 -justify left -foreground $::vt::T(fg_dim) \
        -text [::vi18n::mc {Usually the same as the provider entry name. Leave both empty on the Skip choice — a half-set pair does not route.}]
    pack $f.hint2 -anchor w -fill x -pady {8 0}
    ::va11y::tab_order [list $f.e0 $f.list $f.e2]
    focus $f.e0
    bind $f.e0 <KeyPress> {+ set ::vsetup::v(model_touched) 1 }
    bind $f.e0 <<ComboboxSelected>> {+ set ::vsetup::v(model_touched) 1 }
    bind $f.e2 <KeyPress> {+ set ::vsetup::v(dp_touched) 1 }
    # Nothing suggested (OpenAI, Anthropic, Ollama…): ask the provider
    # straight away rather than leave an empty field to guess into.
    if {$v(model) eq "" && $v(provider) ne "skip" && [llength $models] == 0} {
        _list_models $f
    }
}

# Fetch the provider's /models through the gateway (/api/setup/models),
# with the key typed on the provider step when this desktop may send it,
# else the one stored under its name. Asynchronous: a slow provider never
# freezes the wizard, and a reply for an older request is dropped.
proc ::vsetup::_list_models {f} {
    variable v
    variable secret
    variable models_gen
    incr models_gen
    set key [expr {[_secrets_local] ? $secret(key) : ""}]
    set body [dict create \
        base_url [::vapi::json_string $v(base_url)] \
        key_env [::vapi::json_string $v(key_env)] \
        key [::vapi::json_string $key]]
    ::vi18n::label $f.ms {Asking the provider for its models…}
    if {[catch {
        ::vapi::post_json_async /api/setup/models $body \
            [list ::vsetup::_models_err $f $models_gen] \
            [list ::vsetup::_models_ok $f $models_gen]
    } err]} {
        _models_err $f $models_gen [::vapi::reason $err]
    }
}

proc ::vsetup::_models_ok {f gen reply} {
    variable models_gen
    variable models
    variable v
    if {$gen != $models_gen || ![winfo exists $f.e0]} { return }
    set got {}
    catch { set got [dict get $reply models] }
    if {[llength $got] == 0} {
        set why ""
        catch { set why [dict get $reply error] }
        _models_err $f $gen $why
        return
    }
    set models $got
    $f.e0 configure -values $models
    ::vi18n::label $f.ms {%d models — pick one from the list or type a name.} [llength $models]
}

proc ::vsetup::_models_err {f gen why} {
    variable models_gen
    if {$gen != $models_gen || ![winfo exists $f.ms]} { return }
    ::vi18n::label $f.ms {Could not list the models (%s) — type the model's name.} $why
}

# ── step: telegram ──────────────────────────────────────────────────

proc ::vsetup::_step_telegram {p} {
    set f [_step_head $p [::vi18n::mc {Telegram}] \
        [::vi18n::mc {Create the bot with @BotFather, then paste its token below: it goes into the encrypted vault, never into config.toml.}]]
    ttk::checkbutton $f.on -variable ::vsetup::v(telegram) \
        -text [::vi18n::mc {Enable the Telegram adapter}]
    pack $f.on -anchor w -pady {6 2}
    ::va11y::name $f.on [::vi18n::mc {enable the Telegram adapter}]
    set tok [_secret_field $f 2 [::vi18n::mc {Bot token (hidden)}] bot TELEGRAM_BOT_TOKEN]
    # The open door, said where the door is opened (the engine says it
    # again on the summary when the answers leave it open).
    pack [_note $f.door [::vi18n::mc {Until TELEGRAM_ALLOWED_USERS in .env names your user id, anyone who finds the bot can talk to it — and with no [acl] section, as owner, with every tool.}] gold] \
        -anchor w -fill x -pady {6 0}
    ::va11y::tab_order [concat [list $f.on] $tok]
    focus $f.on
}

# ── step: IRC ───────────────────────────────────────────────────────

proc ::vsetup::_step_irc {p} {
    set f [_step_head $p [::vi18n::mc {IRC}] \
        [::vi18n::mc {Optional: the IRC network to join. List channels separated by spaces or commas; IRC_CHANNELS in .env, when set, replaces this list.}]]
    _field $f 0 [::vi18n::mc {Server (empty = no IRC)}] irc_server [::vi18n::mc {IRC server hostname}]
    _field $f 2 [::vi18n::mc {Port}] irc_port [::vi18n::mc {IRC server port}] 10
    ttk::checkbutton $f.tls -variable ::vsetup::v(irc_tls) -text [::vi18n::mc {Use TLS}]
    pack $f.tls -anchor w -pady {6 0}
    ::va11y::name $f.tls [::vi18n::mc {connect over TLS}]
    _field $f 6 [::vi18n::mc {Nickname}] irc_nick [::vi18n::mc {IRC nickname}]
    _field $f 8 [::vi18n::mc {Channels (e.g. #veles #dev)}] irc_channels [::vi18n::mc {channels to join, a space or comma separated string}]
    _field $f 10 [::vi18n::mc {SASL account (optional)}] irc_sasl [::vi18n::mc {SASL account (optional)}]
    set pw [_secret_field $f 12 [::vi18n::mc {SASL password (hidden)}] sasl IRC_SASL_PASSWORD]
    pack [_note $f.door [::vi18n::mc {Until IRC_ALLOWED_USERS in .env names your account, anyone on the network can talk to the bot — and with no [acl] section, as owner, with every tool.}] gold] \
        -anchor w -fill x -pady {6 0}
    ::va11y::tab_order [concat [list $f.e0 $f.e2 $f.tls $f.e6 $f.e8 $f.e10] $pw]
    focus $f.e0
}

# ── step: ACL posture ───────────────────────────────────────────────

proc ::vsetup::_step_acl {p} {
    set f [_step_head $p [::vi18n::mc {Access control}] \
        [::vi18n::mc {Who may talk to the bot. This choice decides whether config.toml carries an [acl] section at all — and that flips the whole posture.}]]
    ttk::radiobutton $f.r0 -value 0 -variable ::vsetup::v(acl_idx) \
        -text [::vi18n::mc {Legacy open — no [acl] section}] \
        -command [list ::vsetup::_acl_changed $f]
    pack $f.r0 -anchor w -pady {6 0}
    ttk::label $f.h0 -wraplength 600 -justify left -foreground $::vt::T(fg_dim) \
        -text [::vi18n::mc {The adapter allowlists (IRC_ALLOWED_USERS, TELEGRAM_ALLOWED_USERS) are the trust boundary, and every allowed sender resolves as owner. This is the pre-ACL posture.}]
    pack $f.h0 -anchor w -padx {24 0}
    ttk::radiobutton $f.r1 -value 1 -variable ::vsetup::v(acl_idx) \
        -text [::vi18n::mc {Curated — write the first owner entry}] \
        -command [list ::vsetup::_acl_changed $f]
    pack $f.r1 -anchor w -pady {8 0}
    ttk::label $f.h1 -wraplength 600 -justify left -foreground $::vt::T(gold) \
        -text [::vi18n::mc {Writing ANY [acl] key switches this gateway into curated mode: the default role becomes guest (fail-closed) and the allowlists stop being the boundary. Only pick this with an owner identity you trust.}]
    pack $f.h1 -anchor w -padx {24 0}
    # `[acl] owner` compares identities (telegram:<id>, irc:<services
    # account>); a nick!user@host mask goes in as an owner entry instead.
    # The label used to suggest `irc:nick!user@host` — which, as an owner
    # id, matches nobody, so following it locked the operator out.
    ttk::label $f.lo -text [::vi18n::mc {Owner identity (telegram:123456, irc:account, or a nick!user@host mask)}]
    pack $f.lo -anchor w -padx {24 0} -pady {4 0}
    ttk::entry $f.owner -textvariable ::vsetup::v(acl_owner) -width 46
    pack $f.owner -anchor w -padx {24 0}
    ::va11y::name $f.owner [::vi18n::mc {the first ACL owner identity}]
    bind $f.owner <Return> [list ::vsetup::next]
    _acl_sync $f
    ::va11y::tab_order [list $f.r0 $f.r1 $f.owner]
    focus $f.r0
}

proc ::vsetup::_acl_changed {f} { _acl_sync $f }

proc ::vsetup::_acl_sync {f} {
    variable v
    set st [expr {$v(acl_idx) == 1 ? "normal" : "disabled"}]
    catch { $f.owner configure -state $st }
    catch { $f.lo configure -foreground \
        [expr {$st eq "disabled" ? $::vt::T(fg_dim) : $::vt::T(fg)}] }
}

# ── step: summary (preview gates apply) ─────────────────────────────

# The answers as the API body. Every free-text value goes through
# json_string (the dict heuristic would turn "a b" into an object),
# booleans through json_bool (a serde bool answers 422 to a JSON
# number), `skipped` through json_strings (an array even when it holds
# one id), the port as a real integer — or 0 when the field is not a
# port, which the engine refuses by name rather than this guessing 6697.
# desktop_api.rs's TK_SETUP_BODY pins this exact shape from the Rust
# side.
proc ::vsetup::answers_body {} {
    variable v
    variable skipped
    set port [expr {[_port_ok $v(irc_port)] ? $v(irc_port) : 0}]
    return [dict create \
        provider [::vapi::json_string $v(provider)] \
        base_url [::vapi::json_string $v(base_url)] \
        key_env [::vapi::json_string $v(key_env)] \
        api_mode [::vapi::json_string $v(api_mode)] \
        provider_name [::vapi::json_string $v(provider_name)] \
        model [::vapi::json_string $v(model)] \
        default_provider [::vapi::json_string $v(default_provider)] \
        telegram [::vapi::json_bool $v(telegram)] \
        irc_server [::vapi::json_string $v(irc_server)] \
        irc_port $port \
        irc_use_tls [::vapi::json_bool $v(irc_tls)] \
        irc_nickname [::vapi::json_string $v(irc_nick)] \
        irc_channels [::vapi::json_string $v(irc_channels)] \
        irc_sasl_username [::vapi::json_string $v(irc_sasl)] \
        acl [::vapi::json_string [expr {$v(acl_idx) == 1 ? "curated" : "legacy_open"}]] \
        acl_owner [::vapi::json_string $v(acl_owner)] \
        locale [::vapi::json_string $v(locale)] \
        skipped [::vapi::json_strings [lsort $skipped]]]
}

# The secret values these answers would store, as {NAME value …}: only
# a filled field, only for a step that is on and not skipped — the
# engine refuses a name the answers do not use, and a key typed before
# the operator switched Telegram off must not be stored anyway.
proc ::vsetup::secret_values {} {
    variable v
    variable secret
    variable skipped
    if {![_secrets_local]} { return {} }
    set out {}
    if {$v(provider) ne "skip" && "provider" ni $skipped
        && [regexp {^[A-Za-z_][A-Za-z0-9_]*$} $v(key_env)] && $secret(key) ne ""} {
        lappend out $v(key_env) $secret(key)
    }
    if {$v(telegram) && "telegram" ni $skipped && $secret(bot) ne ""} {
        lappend out TELEGRAM_BOT_TOKEN $secret(bot)
    }
    if {[string trim $v(irc_server)] ne "" && "irc" ni $skipped
        && [string trim $v(irc_sasl)] ne "" && $secret(sasl) ne ""} {
        lappend out IRC_SASL_PASSWORD $secret(sasl)
    }
    return $out
}

# A signature of the answers, so a stale preview cannot be applied:
# changing anything after the preview invalidates "Write it" until the
# preview is refreshed (the ACL promote overlay's promote_seen pattern).
# Secrets count by NAME only — which ones will be stored, never a value:
# this string lives in a variable, and a value must not.
proc ::vsetup::answers_sig {} {
    variable v
    variable skipped
    return [join [list $v(provider) $v(base_url) $v(key_env) $v(api_mode) \
        $v(provider_name) $v(model) $v(default_provider) $v(telegram) \
        $v(irc_server) $v(irc_port) $v(irc_tls) $v(irc_nick) \
        $v(irc_channels) $v(irc_sasl) $v(acl_idx) $v(acl_owner) $v(locale) \
        [lsort $skipped] [dict keys [secret_values]]] \x1f]
}

proc ::vsetup::_step_summary {p} {
    variable state
    set f [_step_head $p [::vi18n::mc {Summary}] \
        [::vi18n::mc {This is exactly what will be written — the engine's own bytes, fetched live. Nothing has touched the disk yet.}]]
    if {[dict exists $state first_run] && ![dict get $state first_run]} {
        ttk::label $f.warn -wraplength 620 -justify left -foreground $::vt::T(gold) \
            -text [::vi18n::mc {config.toml already exists — this edits it in place, and the previous version is kept as config.toml.bak.}]
        pack $f.warn -anchor w -fill x -pady {0 6}
    }
    # The text+scrollbar pair wants grid; the warn label above is packed.
    # Grid lives inside its OWN frame so the two managers never share a
    # master (pack and grid in one container fight over every resize).
    ttk::frame $f.pv
    pack $f.pv -fill both -expand 1
    text $f.pv.preview -height 16 -wrap none -state disabled \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -highlightthickness 0 -yscrollcommand [list $f.pv.sb set] \
        -xscrollcommand [list $f.pv.sbx set]
    ttk::scrollbar $f.pv.sb -orient vertical -command [list $f.pv.preview yview]
    ttk::scrollbar $f.pv.sbx -orient horizontal -command [list $f.pv.preview xview]
    grid $f.pv.preview -row 0 -column 0 -sticky nsew
    grid $f.pv.sb -row 0 -column 1 -sticky ns
    grid $f.pv.sbx -row 1 -column 0 -sticky ew
    grid columnconfigure $f.pv 0 -weight 1
    grid rowconfigure $f.pv 0 -weight 1
    ::va11y::name $f.pv.preview [::vi18n::mc {the config.toml that will be written}]

    ttk::frame $f.b
    pack $f.b -fill x -pady {8 0}
    ttk::button $f.b.refresh -command [list ::vsetup::preview_refresh]
    ::vi18n::label $f.b.refresh {Refresh preview}
    pack $f.b.refresh -side left
    ::va11y::tab_order [list $f.pv.preview $f.b.refresh]
    focus $f.b.refresh
    preview_refresh
}

# Fetch the preview for the CURRENT answers and paint it. Returns 1 on
# success. The signature is recorded only on success — a failed fetch
# leaves "Write it" stale rather than armed. A refusal arrives as HTTP
# 400 and `post` THROWS it; ::vapi::reason takes the engine's own words
# out of it (the branch this came from read `ok` off a returned dict, a
# path the real wire never takes, and showed raw JSON instead).
proc ::vsetup::preview_refresh {} {
    variable profile_mode
    if {$profile_mode} { return [::vinstall::profile_review] }
    variable preview_sig
    variable preview_base
    variable install_mode
    variable install_manifest
    variable install_preview
    variable install_instance
    set body [answers_body]
    variable skipped
    if {$install_mode} { dict set body skipped [::vapi::json_strings {provider model telegram irc acl locale}] }
    variable preview_plan
    set preview_plan ""
    if {$install_mode && [dict size $install_manifest]} {
        if {![dict size $install_preview]} {
            _show_preview [::vi18n::mc {Refresh the component preview before reviewing the combined changes.}]
            set preview_sig ""
            return 0
        }
        dict set body component_name [::vapi::json_string [dict get $install_manifest name]]
        dict set body instance [::vapi::json_string $install_instance]
        dict set body options [dict get $install_preview answers options]
    }
    if {[catch { set resp [::vapi::post_json /api/setup/preview $body] } err]} {
        set preview_sig ""
        set why [::vapi::reason $err]
        if {[string match "HTTP 4*" $err]} {
            _show_preview [format [::vi18n::mc {the engine refused these answers: %s}] $why]
        } else {
            _show_preview [format [::vi18n::mc {preview failed: %s}] $why]
        }
        return 0
    }
    set text ""
    catch { set text [dict get $resp config_toml] }
    set mode fresh
    catch { set mode [dict get $resp mode] }
    set diff ""
    catch { set diff [dict get $resp diff] }
    set notes {}
    catch { set notes [dict get $resp notes] }
    set secrets {}
    catch { set secrets [dict get $resp secrets] }
    set component_out ""
    if {$install_mode && [dict exists $install_preview review]} {
        set component_out "--- [::vi18n::mc {component options and declared actions}] ---\n"
        set review [dict get $install_preview review]
        if {[dict exists $review actions]} {
            foreach line [dict get $review actions] { append component_out "$line\n" }
        }
        if {[dict exists $review note]} { append component_out "[dict get $review note]\n" }
        append component_out "\n"
    }
    if {$mode eq "edit"} {
        # Over an existing file the whole text would bury the lines that
        # move; the diff IS the write (apply refuses if the file changes).
        # Keep the separately reviewed component actions visible too.
        if {$diff eq ""} {
            set out "${component_out}--- [::vi18n::mc {no change to config.toml}] ---\n"
        } else {
            set out "${component_out}--- [::vi18n::mc {changes to config.toml}] ---\n$diff"
        }
    } else {
        set out "${component_out}--- config.toml ---\n$text"
    }
    if {[llength $notes]} {
        append out "\n--- [::vi18n::mc {notes from the engine}] ---\n"
        foreach n $notes { append out "  · $n\n" }
    }
    if {[llength $secrets]} {
        set stored {}
        catch { set stored [dict get $resp stored] }
        set giving [dict keys [secret_values]]
        append out "\n--- [::vi18n::mc {secrets (names only — values are never shown)}] ---\n"
        foreach s $secrets {
            if {$s in $giving} {
                set st [::vi18n::mc {will be stored in the encrypted vault}]
            } elseif {$s in $stored} {
                set st [::vi18n::mc {already stored}]
            } else {
                set st [::vi18n::mc {not set yet — /secret set it later}]
            }
            append out "  \$$s: $st\n"
        }
    }
    _show_preview $out
    set preview_base ""
    catch { set preview_base [dict get $resp base_sig] }
    set preview_sig [answers_sig]
    if {$install_mode} { set preview_sig [join [list [answers_sig] $skipped $install_manifest [dict get $install_preview answers]] \x1f] }
    variable preview_plan
    catch { set preview_plan [dict get $resp plan_sig] }
    return 1
}

proc ::vsetup::_show_preview {text} {
    variable win
    if {$win eq "" || ![winfo exists $win.body.c.inner.f.pv.preview]} { return }
    set t $win.body.c.inner.f.pv.preview
    $t configure -state normal
    $t delete 1.0 end
    $t insert end $text
    $t configure -state disabled
}

# Write it — apply, then the finish screen. The preview signature gates
# the write: answers changed since the preview means the operator has
# not read what will land, so refresh instead of writing. The engine
# gates the other half: `expect_base` is the file the preview was
# computed from, and a 409 means it changed since — refreshed, not
# written.
proc ::vsetup::_apply {} {
    variable profile_mode
    if {$profile_mode} { return [::vinstall::apply_profile] }
    variable preview_sig
    variable preview_base
    variable preview_plan
    variable skipped
    variable install_mode
    variable install_manifest
    variable install_preview
    variable install_instance
    variable applied
    if {$install_mode} { set skipped {provider model telegram irc acl locale} }
    set current_sig [answers_sig]
    if {$install_mode} { set current_sig [join [list [answers_sig] $skipped $install_manifest [dict get $install_preview answers]] \x1f] }
    if {$preview_sig ne $current_sig} {
        if {![preview_refresh]} { return }
        # Even after a successful refresh, make the operator read it:
        # the paint happened this instant, "Write it" needs a second press.
        _say [::vi18n::mc {the answers changed, so that is a fresh preview — press “Write it” again to write it}]
        return
    }
    set body [answers_body]
    dict set body expect_base [::vapi::json_string $preview_base]
    if {$preview_plan ne ""} { dict set body plan_sig [::vapi::json_string $preview_plan] }
    dict set body confirm [::vapi::json_bool 1]
    if {$install_mode} { dict set body skipped [::vapi::json_strings {provider model telegram irc acl locale}] }
    if {$install_mode && [dict size $install_manifest]} {
        dict set body component_name [::vapi::json_string [dict get $install_manifest name]]
        dict set body instance [::vapi::json_string $install_instance]
        dict set body options [dict get $install_preview answers options]
    }
    # The values, in the one field that carries them. A dict of
    # json_string pairs becomes the JSON object {"NAME": "value"}.
    set vals {}
    foreach {n val} [secret_values] { dict set vals $n [::vapi::json_string $val] }
    if {[dict size $vals]} { dict set body secrets $vals }
    set rc [catch { set resp [::vapi::post_json /api/setup/apply $body] } err]
    # Sent (or refused): either way the values leave this process's
    # memory now. A refusal asks for them again rather than keeping them.
    unset -nocomplain body vals
    _forget_secrets
    if {$rc} {
        set why [::vapi::reason $err]
        if {[string match "HTTP 409*" $err]} {
            preview_refresh
            _say $why
            return
        }
        _say [format [::vi18n::mc {write failed: %s}] $why]
        return
    }
    set applied $resp
    # The desktop's own "wizard done" marker — desktop_state.json, NOT
    # config.toml (a config copied between machines must not carry
    # machine-local markers; the node.toml lesson).
    if {![::vstate::put setup_done 1]} {
        _say [format [::vi18n::mc {setup_done marker NOT saved: %s}] $::vstate::error]
    }
    _paint
}

# ── finish ──────────────────────────────────────────────────────────

proc ::vsetup::_paint_finish {} {
    variable profile_mode
    if {$profile_mode} { return [::vinstall::paint_result] }
    variable win
    variable applied
    if {$win eq "" || ![winfo exists $win]} { return }
    wm title $win [::vi18n::mc {Setup — done}]
    ::vtitle::attach $win [::vi18n::mc {Setup wizard}] -close [list ::vsetup::_close]
    # The step indicator goes too: "Step 7 of 7" on the done screen is a
    # progress line for a progress that finished.
    catch { destroy $win.head }
    catch { destroy $win.nav }
    catch { destroy $win.body }
    ttk::frame $win.body
    pack $win.body -fill both -expand 1 -padx 12 -pady 8
    set p $win.body
    ttk::label $p.h -text [::vi18n::mc {Done — config written}] -font {TkDefaultFont 12 bold}
    pack $p.h -anchor w

    text $p.report -height 12 -wrap word -state disabled \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -highlightthickness 0
    pack $p.report -fill both -expand 1 -pady 8
    set lines {}
    if {[dict exists $applied config_path]} {
        lappend lines [format [::vi18n::mc {wrote %s}] [dict get $applied config_path]]
    }
    if {[dict exists $applied config_backup]} {
        set bak [dict get $applied config_backup]
        if {$bak ne "" && $bak ne "null"} {
            lappend lines [format [::vi18n::mc {backed up the previous config to %s}] $bak]
        }
    }
    if {[dict exists $applied env_path]} {
        # `secrets_needed` is what is still unset ANYWHERE after the
        # write; `.env` only gained commented reference lines for those.
        set secrets {}
        catch { set secrets [dict get $applied secrets_needed] }
        set stored {}
        catch { set stored [dict get $applied secrets_stored] }
        set vp ""
        catch { set vp [dict get $applied vault_path] }
        if {[llength $stored]} {
            lappend lines [format [::vi18n::mc {stored in the encrypted vault (%1$s): %2$s}] $vp [join $stored {, }]]
        }
        if {[llength $secrets]} {
            lappend lines [format [::vi18n::mc {Still to set: %s — type /secret set NAME here, or run veles secret set NAME on the gateway's machine.}] [join $secrets {, }]]
        }
    }
    set notes {}
    catch { set notes [dict get $applied notes] }
    foreach n $notes { lappend lines "· $n" }
    # What is live now and what waits for a restart — said from the
    # engine's `reloaded`, not assumed: the old line claimed the reload
    # whether or not it had happened.
    set reloaded 0
    catch { set reloaded [expr {[dict get $applied reloaded] eq "true"}] }
    if {$reloaded} {
        lappend lines [::vi18n::mc {The gateway reloaded the config, so new sessions use it now, and a key stored in the vault is used by the next request. Telegram and IRC start on the next gateway restart: veles gateway restart.}]
    } else {
        lappend lines [::vi18n::mc {config.toml was written, but the running gateway could not reload it — restart it: veles gateway restart.}]
    }
    $p.report configure -state normal
    $p.report insert end [join $lines "\n\n"]
    $p.report configure -state disabled
    ::va11y::name $p.report [::vi18n::mc {what the wizard wrote}]

    ttk::frame $p.b
    pack $p.b -fill x
    ttk::button $p.b.close -command [list ::vsetup::_close]
    ::vi18n::label $p.b.close {Close}
    ttk::button $p.b.again -command [list ::vsetup::_run_again]
    ::vi18n::label $p.b.again {Run the wizard again}
    pack $p.b.close -side right
    pack $p.b.again -side right -padx {0 6}
    ::va11y::tab_order [list $p.b.close $p.b.again]
    focus $p.b.close
}

# Re-entry from the finish screen: reset to the welcome step. State is
# re-fetched, so the answers start from the file just written.
proc ::vsetup::_run_again {} {
    variable applied
    variable cur
    variable state
    variable skipped
    variable preview_sig
    set applied {}
    set cur 0
    set skipped {}
    set preview_sig ""
    # Names its error var (uitest's no-silent-catch rule) and keeps the
    # previous state on failure: the wizard itself still works, and the
    # summary's own preview refresh says loudly if the API is down.
    if {![catch { set state [::vapi::get_json /api/setup/state] } _err]} {
        _prefill
    }
    _paint
}

# ── boot wiring (called from main.tcl) ──────────────────────────────

# First-run check: fire ONLY when config.toml is absent (the engine's
# answer, /api/setup/state first_run) AND this desktop has not already
# completed a wizard here (setup_done in desktop_state.json). An
# operator with a hand-rolled config never sees this; a copy of someone
# else's config never sees it either.
proc ::vsetup::boot {} {
    # An explicit installer bootstrap is not a first-run-only wizard:
    # the operator may be selecting a profile after configuring a model.
    if {[info exists ::env(VELES_SETUP_INSTALL)] && $::env(VELES_SETUP_INSTALL) eq "1"} {
        open_profiles
        return 1
    }
    maybe_first_run
}

proc ::vsetup::maybe_first_run {} {
    # Names its error var (uitest's no-silent-catch rule) and stays quiet
    # on purpose: a boot-time probe that fails means "no gateway yet",
    # not "tell the user". The wizard simply does not auto-open; the
    # palette row, C-S-W and /setup all still work once one is up.
    if {[catch { set st [::vapi::get_json /api/setup/state] } _err]} { return 0 }
    if {![dict exists $st first_run] || ![dict get $st first_run]} { return 0 }
    if {[string trim [::vstate::get setup_done 0]] in {1 true}} { return 0 }
    open
    return 1
}

# Status-bar line — every failure and confirmation says itself out
# loud, the nothing-implicit rule.
proc ::vsetup::_say {msg} {
    if {[llength [info commands ::vstatus::set_msg]]} {
        ::vstatus::set_msg $msg
    } else {
        puts stderr "vsetup: $msg"
    }
}
