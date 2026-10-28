# veles-desktop/lib/overlays.tcl — toplevel overlays (hermes route-overlays
# parity, one keystroke each via the palette): models/settings, cron,
# skills, memory, plugins, usage. Each is a plain toplevel with a list
# or text view + actions; closing returns to chat (no nav stack).

namespace eval ::voverlay {
    # Last payload per manager, keyed by name — the trees show a summary
    # and every action needs the full row back.
    variable plugins_data {}
    variable hooks_data {}
    variable skills_data {}
    variable mech_data {}
    # Hooks the engine can fire but nothing has claimed: useful when
    # writing a plugin, noise otherwise.
    variable hooks_show_all 0
    # Which layer the skills manager writes to. Session first: it is the
    # reversible one, and the one the user asked for.
    variable skills_scope session
    # The memory editor edits a list in memory and writes it on Save,
    # so the list has to outlive each repaint.
    variable memory_entries {}
    variable memory_store memory
    # The Files view: the listing as the API returned it, the file open
    # in the editor, and the sha it was opened at ("" = not saved yet —
    # a new file, or one just deleted).
    variable memory_files {}
    variable mfile_path ""
    variable mfile_sha ""
    variable mfile_hist {}
    # The cron list is a tagged text widget, so "selected" is whichever
    # row was last clicked, and the toolbar needs the full job back.
    variable cron_jobs {}
    variable cron_selected ""
    # The trigger overlay's twin of the cron pair: rows keyed by NAME
    # (the REST path segment), so the selection carries the name.
    variable trigger_rows {}
    variable trigger_selected ""
    # name -> its [mcp_servers.<name>] table, as config.toml has it.
    variable mcp_servers {}
    # Which layer the mechanisms manager writes to.
    variable mech_scope session
    variable tasks_after ""
    # One Usage & Quota request generation, with both HTTP answers pending.
    # The click returns before either answer arrives; closing or refreshing
    # the window invalidates late answers through usage_generation.
    variable usage_fill 0
    variable usage_generation 0
    variable usage_pending
    array set usage_pending {}
}


proc ::voverlay::open {which} {
    set w .ov_$which
    if {[winfo exists $w]} {
        wm deiconify $w
        raise $w
        focus $w
        return
    }
    switch -- $which {
        models  { _models $w }
        cron    { _cron $w }
        triggers { _triggers $w }
        tasks   { _tasks $w }
        skills  { _skills $w }
        memory  { _memory $w }
        plugins { _plugins $w }
        usage   { _usage $w }
        checkpoints { _checkpoints $w }
        log     { _log $w }
        mechanisms { _mechanisms $w }
        hooks   { _hooks $w }
        tools   { _tools $w }
        acl     { ::vacl::build $w }
        sessions { ::vsessions::build $w }
        sandbox { ::vsandbox::build $w }
        phone   { ::vphone::build $w }
        settings { ::vsettings::build $w }
        apps    { ::vapps::launcher $w }
    }
}

# ── log (engine + status history) ────────────────────────────────────
# The status bar shows one line and forgets it. This shows the gateway's
# own tracing ring (GET /api/logs — the same buffer the TUI's F2 pane
# reads) plus every status line this client has shown.
proc ::voverlay::_log {w} {
    toplevel $w
    wm title $w [::vi18n::mc {Log}]
    wm geometry $w 860x520
    wm minsize $w 430 260
    _header $w "Log"
    ttk::frame $w.tabs
    pack $w.tabs -fill x -padx 8
    ttk::button $w.tabs.engine -text [::vi18n::mc {engine}] \
        -command [list ::voverlay::_log_show $w engine]
    ttk::button $w.tabs.client -text [::vi18n::mc {this window}] \
        -command [list ::voverlay::_log_show $w client]
    ttk::button $w.tabs.refresh -text "↻" \
        -command [list ::voverlay::_log_show $w engine]
    ::va11y::name $w.tabs.refresh [::vi18n::mc {refresh the log}]
    pack $w.tabs.engine $w.tabs.client -side left -padx {0 6}
    pack $w.tabs.refresh -side right
    text $w.body -wrap none -relief flat -padx 8 -pady 6 \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -yscrollcommand [list $w.sb set]
    catch { ::va11y::name $w.body [::vi18n::mc {engine and client log}] }
    ttk::scrollbar $w.sb -orient vertical -command [list $w.body yview]
    pack $w.sb -side right -fill y
    pack $w.body -fill both -expand 1 -padx 8 -pady {4 8}
    $w.body tag configure lvl_ERROR -foreground $::vt::T(red)
    $w.body tag configure lvl_WARN  -foreground $::vt::T(gold)
    $w.body tag configure lvl_INFO  -foreground $::vt::T(fg)
    $w.body tag configure lvl_DEBUG -foreground $::vt::T(fg_dim)
    $w.body tag configure meta      -foreground $::vt::T(fg_dim)
    _log_show $w engine
}

proc ::voverlay::_log_show {w which} {
    $w.body configure -state normal
    $w.body delete 1.0 end
    if {$which eq "client"} {
        foreach row [::vstatus::history] {
            lassign $row stamp msg
            $w.body insert end "$stamp " meta
            $w.body insert end "$msg\n" lvl_INFO
        }
        if {[llength [::vstatus::history]] == 0} {
            $w.body insert end "no status messages yet" meta
        }
    } else {
        if {[catch {::vapi::get_json /api/logs?limit=400} d]} {
            $w.body insert end "engine log unavailable: $d\n" lvl_ERROR
            $w.body insert end "(is `veles gateway run` up?)" meta
        } else {
            set rows [dict get $d logs]
            if {[llength $rows] == 0} {
                $w.body insert end "engine log is empty" meta
            }
            foreach l $rows {
                set lvl [dict get $l level]
                set tag lvl_INFO
                if {[lsearch -exact {ERROR WARN INFO DEBUG} $lvl] >= 0} {
                    set tag lvl_$lvl
                }
                $w.body insert end "[dict get $l time] " meta
                $w.body insert end [format "%-5s " $lvl] $tag
                $w.body insert end "[dict get $l target] " meta
                $w.body insert end "[dict get $l msg]\n" $tag
            }
            catch { ::vstatus::set_msg [format [::vi18n::mc {log: %s}] [dict get $d path]] }
        }
    }
    $w.body see end
    ::vtext::readonly $w.body
}

# ── checkpoints (M13) ────────────────────────────────────────────────
# Undo points for the agent's file edits. Restoring is itself
# checkpointed server side, so an unwanted restore can be undone too.
proc ::voverlay::_checkpoints {w} {
    toplevel $w
    wm title $w [::vi18n::mc {Checkpoints}]
    wm geometry $w 720x460
    wm minsize $w 360 240
    _header $w "Checkpoints — undo file edits"
    ttk::treeview $w.tv -columns {id tool path when} -show headings -height 16
    foreach {c t width} {id # 60 tool tool 90 path file 400 when when 150} {
        $w.tv heading $c -text $t
        $w.tv column $c -width $width -anchor w
    }
    pack $w.tv -fill both -expand 1 -padx 8 -pady 4
    ttk::frame $w.act
    ttk::button $w.act.restore -text [::vi18n::mc {Restore selected}] \
        -command [list ::voverlay::_checkpoint_restore $w]
    ttk::label $w.act.msg -text ""
    pack $w.act.restore -side left -padx 8
    pack $w.act.msg -side left -padx 8
    pack $w.act -fill x -pady {0 8}

    if {[catch {::vapi::get_json /api/checkpoints} d]} {
        ::vi18n::label $w.act.msg {unavailable: %s} $d
        return
    }
    foreach c [dict get $d checkpoints] {
        $w.tv insert {} end -values [list \
            [dict get $c id] [dict get $c tool] [dict get $c path] \
            [dict get $c created_at]]
    }
}

proc ::voverlay::_checkpoint_restore {w} {
    set sel [$w.tv selection]
    if {$sel eq ""} {
        $w.act.msg configure -text [::vi18n::mc {select a checkpoint first}]
        return
    }
    set id [lindex [$w.tv item $sel -values] 0]
    if {[catch {::vapi::post /api/checkpoints/$id/restore {{}}} raw]} {
        ::vi18n::label $w.act.msg {restore failed: %s} $raw
        return
    }
    set res [::json::json2dict $raw]
    $w.act.msg configure -text [dict get $res message]
}

# Every overlay gets the full control strip — drag, minimise, maximise,
# close — not just a close button, so the native decoration can be
# switched off without the window becoming unmovable.
# Every overlay's title strip — and the one place all of them pass
# through, which is why the coach sweep lives here.
#
# `after idle`: _header runs near the TOP of each builder, before the
# buttons exist. By the time the event loop is idle the window is built,
# so one line here coaches every overlay in the app, including ones
# added later.
proc ::voverlay::_header {w title} {
    after idle [list catch [list ::vcoach::attach_all $w]]
    # Translate AT THE HEADER, not at the 40-odd call sites: this is the
    # one place an overlay title becomes visible text, and the callers
    # pass a mix of literals ("Log") and strings the ENGINE localized
    # (::vmanage::window passes mc() results through). Resolving here
    # covers both — an engine-rendered string is not a catalog key, so
    # mc() returns it unchanged, which is the right answer for it.
    ::vtitle::attach $w [::vi18n::mc $title] -close [list destroy $w]
}

# ── models & settings ────────────────────────────────────────────────
proc ::voverlay::_models {w} {
    ::vmanage::window $w [::vi18n::mc {Models & settings}] 700x640 {420 320}

    if {[catch {::vapi::get_json /api/config} cfg]} {
        ttk::label $w.err
        ::vi18n::label $w.err {config unavailable: %s} [::vmanage::_reason $cfg]
        pack $w.err -padx 8 -pady 8
        return
    }

    ttk::frame $w.def
    ttk::label $w.def.l -text [::vi18n::mc {Default:}]
    ttk::label $w.def.v -foreground $::vt::T(fg_dim)
    ::vi18n::label $w.def.v {%1$s / %2$s  ·  reasoning %3$s} \
        [dict get $cfg default_provider] [dict get $cfg default_model] \
        [dict get $cfg reasoning_effort]
    # The DEFAULT, as opposed to the session override below it. It was
    # a label; the only way to change it was editing config.toml.
    ttk::button $w.def.edit -text [::vi18n::mc {Change…}] \
        -command [list ::voverlay::_models_default $w]
    catch { ::va11y::name $w.def.edit [::vi18n::mc {change the default provider and model}] }
    catch { ::vcoach::attach $w.def.edit }
    pack $w.def.l $w.def.v -side left -padx 4
    pack $w.def.edit -side right -padx 4
    pack $w.def -fill x -padx 8 -pady {4 2}

    ttk::labelframe $w.aliases -text [::vi18n::mc {Aliases (session override target)}]
    ttk::frame $w.aliases.bar
    ttk::button $w.aliases.bar.add -text [::vi18n::mc {Add…}] \
        -command [list ::voverlay::_alias_edit $w ""]
    ttk::button $w.aliases.bar.edit -text [::vi18n::mc {Edit…}] \
        -command [list ::voverlay::_alias_edit_selected $w]
    ttk::button $w.aliases.bar.del -text [::vi18n::mc {Remove}] \
        -command [list ::voverlay::_alias_remove $w]
    foreach b {add edit del} { catch { ::vcoach::attach $w.aliases.bar.$b } }
    pack $w.aliases.bar.add $w.aliases.bar.edit $w.aliases.bar.del -side left -padx {0 6}
    pack $w.aliases.bar -fill x -padx 4 -pady {4 0}
    ttk::treeview $w.aliases.t -columns {name target} -show headings -height 5 \
        -selectmode browse
    $w.aliases.t heading name -text [::vi18n::mc {alias}]
    $w.aliases.t heading target -text [::vi18n::mc {provider / model}]
    catch { ::va11y::name $w.aliases.t [::vi18n::mc {model aliases}] }
    foreach a [dict get $cfg aliases] {
        $w.aliases.t insert {} end -values [list [dict get $a name] \
            "[dict get $a provider]/[dict get $a model]"]
    }
    pack $w.aliases.t -fill both -expand 1 -padx 4 -pady 4
    pack $w.aliases -fill both -expand 1 -padx 8 -pady 4

    # Session override. This was three BARE entries in a row under one
    # "Apply to current session:" label — no field names, no current
    # values, and no indication of what any of them would accept. Now
    # each is labelled, prefilled with what is actually in effect, and
    # offers the values the engine knows about.
    ttk::labelframe $w.apply -text [::vi18n::mc {Override for THIS session (leave blank to inherit the default)}]
    set row 0
    foreach {field label} {model "Model" provider "Provider" reasoning "Reasoning"} {
        ttk::label $w.apply.l$field
        ::vi18n::label $w.apply.l$field {%s:} $label
        ttk::combobox $w.apply.$field -width 34
        grid $w.apply.l$field -row $row -column 0 -sticky e -padx {6 4} -pady 2
        grid $w.apply.$field  -row $row -column 1 -sticky ew -padx {0 6} -pady 2
        incr row
    }
    grid columnconfigure $w.apply 1 -weight 1

    # Offer what the engine actually has, so a typo cannot become a
    # 404 "Model not exist." at turn time.
    set model_values {}
    foreach a [dict get $cfg aliases] { lappend model_values [dict get $a name] }
    foreach p [dict get $cfg providers] {
        foreach m [dict get $p models] {
            if {[lsearch -exact $model_values $m] < 0} { lappend model_values $m }
        }
    }
    set provider_values {}
    foreach p [dict get $cfg providers] { lappend provider_values [dict get $p name] }
    $w.apply.model configure -values $model_values
    $w.apply.provider configure -values $provider_values
    $w.apply.reasoning configure -values {none low medium high xhigh}

    # Fuzzy search on every dropdown here — typing narrows as you go,
    # and Return (or Ctrl-Space) opens the full ranked picker when you
    # want to browse. USER: "The dropdown selection fields everywhere
    # should have fuzzy search, especially model selector".
    foreach {field values} [list model $model_values provider $provider_values \
                                 reasoning {none low medium high xhigh}] {
        set cb $w.apply.$field
        bind $cb <Return> [list ::voverlay::_pick_field $w $field $values]
        bind $cb <Control-space> [list ::voverlay::_pick_field $w $field $values]
        bind $cb <KeyRelease> [list ::voverlay::_combobox_filter $cb $values %K]
    }

    ttk::frame $w.apply.btns
    ttk::button $w.apply.btns.go -text [::vi18n::mc {Apply}] \
        -command [list ::voverlay::_apply_override $w]
    ttk::button $w.apply.btns.clear -text [::vi18n::mc {Clear override}] \
        -command [list ::voverlay::_clear_override $w]
    # USER: "we should be able to save the values as new defaults to the
    # config." These three boxes were session-only, so a route you had
    # settled on had to be re-entered in every new session or edited
    # into config.toml by hand.
    ttk::button $w.apply.btns.save -text [::vi18n::mc {Save as default}] \
        -command [list ::voverlay::_save_override_as_default $w]
    catch { ::va11y::name $w.apply.btns.save \
        [::vi18n::mc {write these values into config.toml as the defaults}] }
    ttk::label $w.apply.btns.state -text "" -foreground $::vt::T(fg_dim)
    pack $w.apply.btns.go $w.apply.btns.clear $w.apply.btns.save \
        -side left -padx {6 4} -pady {2 6}
    pack $w.apply.btns.state -side left -padx 8
    grid $w.apply.btns -row $row -column 0 -columnspan 2 -sticky w
    pack $w.apply -fill x -padx 8 -pady {2 8}
    ::voverlay::_show_override $w

    ttk::labelframe $w.providers -text [::vi18n::mc {Providers}]
    ttk::frame $w.providers.bar
    # A provider entry carries a base_url, an api_mode, a key_env and a
    # models table — too much shape to guess at in a form, and getting
    # one field wrong makes every route through it fail at turn time. So
    # the button opens the config editor pointed at the right key rather
    # than pretending this is a two-field dialog.
    ttk::button $w.providers.bar.edit -text [::vi18n::mc {Edit in config…}] \
        -command { ::voverlay::_config_editor custom_providers \
            [::vi18n::mc {Providers are a list of tables: custom_providers.0.base_url, .0.api_mode, .0.key_env … Index from the order shown below.}] }
    catch { ::vcoach::attach $w.providers.bar.edit }
    pack $w.providers.bar.edit -side left
    pack $w.providers.bar -fill x -padx 4 -pady {4 0}
    text $w.providers.t -height 8 -wrap word -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -highlightthickness 0 -relief flat
    set i 0
    foreach p [dict get $cfg providers] {
        set ms [dict get $p models]
        $w.providers.t insert end "\[$i\] [dict get $p name]: [join $ms ", "]\n"
        incr i
    }
    catch { ::vtext::readonly $w.providers.t }
    pack $w.providers.t -fill both -expand 1 -padx 4 -pady 4
    pack $w.providers -fill both -expand 1 -padx 8 -pady {4 8}
}

# ── the config half of the models overlay ───────────────────────────
#
# Everything above the session-override block used to be disabled text:
# the default route, the alias table and the provider list were all
# read-only, and the overlay's own title said "Models & settings".

proc ::voverlay::_models_reopen {w} {
    catch { destroy $w }
    ::voverlay::open models
}

proc ::voverlay::_models_default {w} {
    # Fuzzy pickers instead of bare text asks — USER: "The dropdown
    # selection fields everywhere should have fuzzy search, especially
    # model selector". The default route is the one everyone changes,
    # so it goes first.
    if {[catch {::vapi::get_json /api/config} cfg]} {
        ::vmanage::say $w [::vi18n::mc {config unavailable}]
        return
    }
    set provs {}
    foreach p [dict get $cfg providers] { lappend provs [dict get $p name] }
    set cur_p ""
    set cur_m ""
    catch { set cur_p [dict get $cfg default_provider] }
    catch { set cur_m [dict get $cfg default_model] }
    set p [::vedit::fuzzy_pick [::vi18n::mc {Default provider}] $provs $cur_p]
    if {[string trim $p] eq ""} { return }
    # Offer this provider's models, and everything else — a default can
    # legitimately point at another provider's model, and fuzzy search
    # means the extra rows cost nothing to skip past.
    set models {}
    foreach pr [dict get $cfg providers] {
        foreach m [dict get $pr models] {
            if {[lsearch -exact $models $m] < 0} { lappend models $m }
        }
    }
    set m [::vedit::fuzzy_pick [::vi18n::mc {Default model}] $models $cur_m]
    if {[string trim $m] eq ""} { return }
    ::voverlay::_defaults $w "model $m@$p"
    _models_reopen $w
}

# Write a default route through the engine's own `/defaults` — the same
# resolver and the same one write as the REPL's — and show its answer.
proc ::voverlay::_defaults {w cmd} {
    set r [::vmanage::post $w /api/defaults \
        [dict create command [::vapi::json_string $cmd]]]
    if {$r eq ""} { return "" }
    set text ""
    catch { set text [dict get $r text] }
    # The last line is the effect ("applies to new sessions"); the
    # status bar keeps it after the overlay reopens.
    set line [lindex [split [string trim $text] "\n"] end]
    ::vmanage::say $w $line ok
    catch { ::vstatus::set_msg $line }
    return $r
}

proc ::voverlay::_alias_selected {w} {
    set sel [lindex [$w.aliases.t selection] 0]
    if {$sel eq ""} {
        ::vmanage::say $w [::vi18n::mc {select an alias first}]
        return ""
    }
    return [lindex [$w.aliases.t item $sel -values] 0]
}

proc ::voverlay::_alias_edit_selected {w} {
    set name [_alias_selected $w]
    if {$name eq ""} { return }
    _alias_edit $w $name
}

# An alias is exactly {provider, model}, so this one IS a two-field
# dialog — unlike a provider entry.
proc ::voverlay::_alias_edit {w name} {
    set new $name
    if {$name eq ""} {
        set new [::vmanage::ask $w [::vi18n::mc {New alias}] [::vi18n::mc {alias name}] "" \
            [::vi18n::mc {The short name you will type in the session override box, e.g. "fast".}]]
        if {[string trim $new] eq ""} { return }
    }
    set p [::vmanage::ask $w [format [::vi18n::mc {Alias %s}] $new] [::vi18n::mc {provider}] ""]
    if {[string trim $p] eq ""} { return }
    set m [::vmanage::ask $w [format [::vi18n::mc {Alias %s}] $new] [::vi18n::mc {model}] ""]
    if {[string trim $m] eq ""} { return }
    set r [::vmanage::post $w /api/config/set \
        [dict create path "model_aliases.$new.provider" value [::vapi::json_string $p]]]
    if {$r eq ""} { return }
    set r [::vmanage::post $w /api/config/set \
        [dict create path "model_aliases.$new.model" value [::vapi::json_string $m]]]
    if {$r eq ""} { return }
    ::vmanage::effect $w $r "$new -> $p/$m —"
    _models_reopen $w
}

proc ::voverlay::_alias_remove {w} {
    set name [_alias_selected $w]
    if {$name eq ""} { return }
    if {![::vmanage::confirm $w [format [::vi18n::mc {Remove alias %s?}] $name] \
        [::vi18n::mc {Sessions currently overridden to this alias keep the route they resolved; new overrides will not find the name.}]]} {
        return
    }
    set r [::vmanage::post $w /api/config/set \
        [dict create path "model_aliases.$name" value null]]
    if {$r eq ""} { return }
    _models_reopen $w
}

# Promote whatever is in the override boxes to the config defaults.
#
# Three keys, and only the ones actually filled in: a blank box means
# "inherit", and writing "" over a default would be a different, worse
# thing than leaving it alone. An alias is resolved to its provider and
# model first — `model.default` wants a real model name, and storing the
# alias would work only for as long as the alias did.
proc ::voverlay::_save_override_as_default {w} {
    set model    [string trim [$w.apply.model get]]
    set provider [string trim [$w.apply.provider get]]
    set reason   [string trim [$w.apply.reasoning get]]
    if {$model eq "" && $provider eq "" && $reason eq ""} {
        $w.apply.btns.state configure -text \
            [::vi18n::mc {nothing to save — fill in a value first}]
        return
    }

    # Resolve an alias the same way the engine would.
    if {$model ne "" && ![catch {::vapi::get_json /api/config} cfg]} {
        catch {
            foreach a [dict get $cfg aliases] {
                if {[dict get $a name] ne $model} { continue }
                set model [dict get $a model]
                if {$provider eq ""} { set provider [dict get $a provider] }
                break
            }
        }
    }

    # Through `/defaults`: `model m@p` writes both halves of [model] in
    # one edit (a provider alone is `/defaults provider`), and the
    # reasoning effort is `/defaults reasoning`.
    set cmds {}
    if {$model ne ""} {
        lappend cmds [expr {$provider ne "" ? "model $model@$provider" : "model $model"}]
    } elseif {$provider ne ""} {
        lappend cmds "provider $provider"
    }
    if {$reason ne ""} { lappend cmds "reasoning $reason" }
    set last ""
    foreach c $cmds {
        set last [::voverlay::_defaults $w $c]
        if {$last eq ""} {
            $w.apply.btns.state configure -text [::vi18n::mc {save refused}]
            return
        }
    }
    catch { ::vstatus::set_msg [format "%s %s" [::vi18n::mc {saved as the default —}] [join $cmds {; }]] }
    $w.apply.btns.state configure -text [::vi18n::mc {saved}]
    # The Default line at the top is now stale.
    _models_reopen $w
}

# A combobox being typed into: replace its dropdown VALUES with the
# fuzzy-ranked matches for the current text (the widget's own entry
# keeps the text; only the arrow list narrows). Navigation keys are
# ignored — they must keep moving the popup's own selection.
proc ::voverlay::_combobox_filter {cb values key} {
    if {$key in {Return Up Down Left Right Tab Escape}} { return }
    set q [$cb get]
    set matched [::vedit::fuzzy_sort $q $values]
    if {[llength $matched] == 0} { set matched $values }
    $cb configure -values $matched
}

# Return / Ctrl-Space inside an override field: open the ranked picker
# (preselecting the current value) and write the choice back.
proc ::voverlay::_pick_field {w field values} {
    set cur [$w.apply.$field get]
    set title ""
    switch -- $field {
        model     { set title [::vi18n::mc {Model for this session}] }
        provider  { set title [::vi18n::mc {Provider for this session}] }
        reasoning { set title [::vi18n::mc {Reasoning effort}] }
    }
    set pick [::vedit::fuzzy_pick $title $values $cur]
    if {$pick eq ""} { return }
    $w.apply.$field set $pick
}

proc ::voverlay::_apply_override {w} {
    set sid $::vchat::session
    if {$sid eq ""} {
        ::vstatus::set_msg [::vi18n::mc {no session open — open one first}]
        return
    }
    set body "{"
    set parts {}
    foreach {field key} {model model provider provider reasoning reasoning} {
        set v [$w.apply.$field get]
        if {$v ne ""} {
            lappend parts "\"$key\": \"[::vapi::json_escape $v]\""
        }
    }
    append body [join $parts ","] "}"
    if {[catch {::vapi::post "/api/sessions/$sid/override" $body} r]} {
        ::vstatus::set_msg [format [::vi18n::mc {override failed: %s}] $r]
        return
    }
    ::vstatus::set_msg [format [::vi18n::mc {override applied: %s}] $r]
    # The status bar's route label must follow the change immediately —
    # otherwise the overlay says one thing and the bar another.
    if {[catch { ::vchat::refresh_route } rr_err]} {
        puts stderr "veles-desktop: route refresh failed after override: $rr_err"
    }
    ::voverlay::_show_override $w
}

# Drop the session override so the config default applies again.
proc ::voverlay::_clear_override {w} {
    set sid $::vchat::session
    if {$sid eq ""} {
        ::vstatus::set_msg [::vi18n::mc {no session open — open one first}]
        return
    }
    foreach f {model provider reasoning} { $w.apply.$f set "" }
    if {[catch {::vapi::post "/api/sessions/$sid/override" \
        "{\"model\":\"\",\"provider\":\"\",\"reasoning\":\"\"}"} r]} {
        ::vstatus::set_msg [format [::vi18n::mc {clear failed: %s}] $r]
        return
    }
    ::vstatus::set_msg [::vi18n::mc {override cleared — the default applies}]
    if {[catch { ::vchat::refresh_route } rr_err]} {
        puts stderr "veles-desktop: route refresh failed after override clear: $rr_err"
    }
    ::voverlay::_show_override $w
}

# Say what is ACTUALLY in effect for this session, next to the controls
# that change it.
#
# This used to fetch /api/status and the session list, compute a title,
# discard it, and print "session: <id>" — so a setting that failed to
# apply was indistinguishable from one that worked. Now it reads back
# the effective route.
proc ::voverlay::_show_override {w} {
    if {![winfo exists $w.apply.btns.state]} { return }
    set sid $::vchat::session
    if {$sid eq ""} {
        $w.apply.btns.state configure -text [::vi18n::mc {no session open}]
        return
    }
    if {[catch {::vapi::get_json "/api/sessions/$sid/override"} ov]} {
        $w.apply.btns.state configure -text [::vi18n::mc {could not read the current route}]
        return
    }
    set txt ""
    if {[dict exists $ov live] && [dict get $ov live]} {
        set prov [expr {[dict exists $ov provider] ? [dict get $ov provider] : "?"}]
        set mod  [expr {[dict exists $ov model]    ? [dict get $ov model]    : "?"}]
        set txt "in effect: $prov / $mod"
        if {[dict exists $ov reasoning] && [dict get $ov reasoning] ne ""} {
            append txt " (reasoning [dict get $ov reasoning])"
        }
        if {[dict exists $ov overridden] && [dict get $ov overridden]} {
            append txt "  — session override"
        } else {
            append txt "  — config default"
        }
    } elseif {[::vapi::field $ov pending] ne ""} {
        # Parked, not applied: say so rather than implying it is running.
        set txt "pending (applies when this session next runs): [::vapi::field $ov pending]"
    } else {
        set txt "not started — the config default applies"
    }
    $w.apply.btns.state configure -text $txt
}

# ── guards ───────────────────────────────────────────────────────────
#
# USER: "I don't know how to trigger the guards to test them. Maybe I
# already have but they're not visible. I also don't see an M-x command
# to view the guards".
#
# Guards ANNOUNCE themselves when they fire (NoteKind::Guard, inline in
# the transcript). What was missing is the other half: what they are,
# where they stand RIGHT NOW, and what would make each one fire. A
# mechanism you cannot inspect is one you have to take on faith, which
# is the thing this whole pass exists to end.

proc ::voverlay::_mechanisms {w} {
    ::vmanage::window $w \
        [::vi18n::mc {Mechanisms — what the engine does on its own, and how to stop it}] \
        860x820 {560 460}
    ::vmanage::button $w.bar reload [::vi18n::mc {Reload}] \
        [list ::voverlay::_mechanisms_fill $w] [::vi18n::mc {reload the mechanisms}]
    ttk::label $w.hint -foreground $::vt::T(fg_dim) -wraplength 820 -justify left \
        -text [::vi18n::mc {on/off silences the mechanism's note in the transcript — for this session, or as the default, per the scope below. It governs the NOTE; what each mechanism DOES is set in the section named on its row, and Configure… opens it. The guard knobs are editable in place.}]
    pack $w.hint -fill x -padx 8 -pady {2 4}

    # Which layer on/off writes to. Session first: it is the reversible
    # one. Same shape as the Skills manager, deliberately.
    ttk::frame $w.scope
    pack $w.scope -fill x -padx 8 -pady {0 4}
    ttk::label $w.scope.l -text [::vi18n::mc {apply to:}]
    ttk::radiobutton $w.scope.session -text [::vi18n::mc {this session}] \
        -variable ::voverlay::mech_scope -value session \
        -command [list ::voverlay::_mechanisms_fill $w]
    ttk::radiobutton $w.scope.global -text [::vi18n::mc {the default (config.toml)}] \
        -variable ::voverlay::mech_scope -value global \
        -command [list ::voverlay::_mechanisms_fill $w]
    pack $w.scope.l $w.scope.session $w.scope.global -side left -padx {0 8}

    # A vertical split, and the top half SCROLLS.
    #
    # Everything used to be packed top-down in one window: eight
    # two-line mechanism rows, five guard knobs and the guards report.
    # Packed widgets do not scroll, so on any window shorter than the
    # content Tk clipped from the bottom and the knobs became
    # unreachable — no scrollbar, no wheel, no keyboard. USER: "the
    # mechanisms window doesn't have a scrollbar and you can't actually
    # see the bottom options".
    ttk::panedwindow $w.split -orient vertical
    pack $w.split -fill both -expand 1 -padx 8 -pady {0 8}

    set top [::vmanage::scrollframe $w.split top]
    $w.split add $w.split.top -weight 3

    ttk::frame $top.mechs
    pack $top.mechs -fill x
    ttk::separator $top.sep -orient horizontal
    pack $top.sep -fill x -pady 4
    ttk::label $top.gtitle -text [::vi18n::mc {Guards — what the harness stops, and where it stands now}] \
        -font {TkDefaultFont 10 bold}
    pack $top.gtitle -anchor w
    ttk::frame $top.knobs
    pack $top.knobs -fill x -pady {2 6}

    ttk::frame $w.split.main
    text $w.split.main.t -wrap word -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -highlightthickness 0 -relief flat -font {TkDefaultFont 10}
    ttk::scrollbar $w.split.main.sb -orient vertical -command [list $w.split.main.t yview]
    $w.split.main.t configure -yscrollcommand [list $w.split.main.sb set]
    pack $w.split.main.sb -side right -fill y
    pack $w.split.main.t -side left -fill both -expand 1
    catch { ::vtext::readonly $w.split.main.t }
    $w.split add $w.split.main -weight 2

    # The scrolling half keeps at least 220px however small the window
    # gets — see ::voverlay::_split_keep for why -weight is not enough.
    ::voverlay::_split_keep $w.split 0.62 220
    bind $w <Escape> [list destroy $w]
    _mechanisms_fill $w
}

# The three moving parts, named once so the fill procs do not each
# re-derive the paths the split introduced.
proc ::voverlay::_mech_rows {w}  { return $w.split.top.c.inner.mechs }
proc ::voverlay::_mech_knobs {w} { return $w.split.top.c.inner.knobs }
proc ::voverlay::_mech_text {w}  { return $w.split.main.t }

# Rebuild the mechanism rows from the live catalogue + overrides, then
# the guards detail below them. Called on open, on Reload, and after any
# on/off/default button is pressed.
proc ::voverlay::_mechanisms_fill {w} {
    set rows [_mech_rows $w]
    if {[winfo exists $rows]} {
        foreach c [winfo children $rows] { destroy $c }
    }
    if {$::vchat::session eq ""} {
        ttk::label $rows.empty \
            -text [::vi18n::mc {no session open — the catalogue needs one; the default scope still works from config.toml}] \
            -foreground $::vt::T(fg_dim)
        pack $rows.empty -anchor w
    } elseif {[catch {::vapi::get_json "/api/sessions/$::vchat::session/mechanism"} d]} {
        # This is where "mechanisms unreachable: HTTP 405" came from: the
        # route was registered POST-only while this read is a GET. Fixed
        # in desktop_api.rs; the message keeps the reason visible if it
        # ever regresses.
        ttk::label $rows.empty -foreground $::vt::T(gold)
        ::vi18n::label $rows.empty {mechanisms unreachable: %s} [::vmanage::_reason $d]
        pack $rows.empty -anchor w
    } else {
        set omap {}
        set overrides {}
        catch { set overrides [dict get $d overrides] }
        foreach o $overrides { dict set omap [dict get $o key] [dict get $o value] }
        set i 0
        foreach m [dict get $d mechanisms] {
            incr i
            set key [dict get $m key]
            # Both layers, so the row can say WHICH one is deciding.
            set silenced 0
            catch { set silenced [dict get $m silenced_by_default] }
            set ov ""
            if {[dict exists $omap $key]} { set ov [dict get $omap $key] }
            _mech_row $rows $i [dict get $m glyph] $key [dict get $m config_key] \
                [dict get $m help] $ov $silenced
        }
        if {![llength [dict get $d mechanisms]]} {
            ttk::label $rows.empty -text [::vi18n::mc {no mechanisms returned}] \
                -foreground $::vt::T(fg_dim)
            pack $rows.empty -anchor w
        }
    }
    _guards_fill $w
    _guard_knobs $w
    # Wheel events land on the widget under the pointer, and these rows
    # know nothing about the canvas holding them.
    catch { ::vmanage::scroll_children $w.split.top.c.inner }
}

# One row of the mechanism list: name, current scope, and the inline
# control that changes it. `on`/`off`/`default` hit the same endpoint the
# /mechanisms slash command uses, so the two surfaces cannot disagree.
# One mechanism: what it is, which layer is deciding its note, and the
# controls for the layer the scope selector points at.
proc ::voverlay::_mech_row {parent i glyph key cfg help ov silenced} {
    variable mech_scope
    set f $parent.m$i
    ttk::frame $f -padding {0 4}
    # Say which layer decided, not just the answer. "off" with no
    # explanation is the thing this overlay exists to stop.
    if {$ov ne ""} {
        set state_lbl [format [::vi18n::mc {%s (this session)}] $ov]
        set state_fg $::vt::T(gold)
    } elseif {$silenced} {
        set state_lbl [::vi18n::mc {off (the default)}]
        set state_fg $::vt::T(gold)
    } else {
        set state_lbl [::vi18n::mc {on (the default)}]
        set state_fg $::vt::T(fg_dim)
    }
    ttk::label $f.name -font {TkDefaultFont 10 bold}
    ::vi18n::label $f.name {%1$s %2$s} $glyph $key
    ttk::label $f.state -text $state_lbl -foreground $state_fg
    ttk::frame $f.buttons
    ttk::button $f.buttons.on  -text [::vi18n::mc {on}]  -width 4 \
        -command [list ::voverlay::_mech_set $f.buttons.on $key on]
    ttk::button $f.buttons.off -text [::vi18n::mc {off}] -width 4 \
        -command [list ::voverlay::_mech_set $f.buttons.off $key off]
    ttk::button $f.buttons.def -text [::vi18n::mc {default}] \
        -command [list ::voverlay::_mech_set $f.buttons.def $key default]
    # The other half of the answer. on/off is about the NOTE; this is
    # about the behaviour, and it used to be a sentence telling you to go
    # and edit a file.
    ttk::button $f.buttons.cfg -text [::vi18n::mc {Configure…}] \
        -command [list ::voverlay::_mech_configure $key $cfg $help]
    # "default" only means something in the session scope: it drops the
    # override so the config layer decides again. In the config scope
    # there is nothing below to fall through to, so it would be a button
    # that does nothing — and one of those is worse than none.
    set where [expr {$mech_scope eq "global"
        ? [::vi18n::mc {as the default}]
        : [::vi18n::mc {for this session}]}]
    ::vi18n::aname $f.buttons.on  {turn %1$s on %2$s} $key $where
    ::vi18n::aname $f.buttons.off {turn %1$s off %2$s} $key $where
    ::vi18n::aname $f.buttons.def {drop the %s session override} $key
    ::vi18n::aname $f.buttons.cfg {configure what %s does} $key
    foreach b {on off def cfg} { catch { ::vcoach::attach $f.buttons.$b } }
    pack $f.buttons.on $f.buttons.off -side left
    if {$mech_scope ne "global"} {
        pack $f.buttons.def -side left -padx {6 0}
    } else {
        destroy $f.buttons.def
    }
    pack $f.buttons.cfg -side left -padx {6 0}
    ttk::label $f.help -foreground $::vt::T(fg_dim)
    ::vi18n::label $f.help {%1$s   ·  %2$s} $help $cfg
    grid $f.name  -row 0 -column 0 -sticky w
    grid $f.state -row 0 -column 1 -sticky w -padx 10
    grid $f.buttons -row 0 -column 2 -sticky e
    grid $f.help   -row 1 -column 0 -columnspan 3 -sticky nw
    grid columnconfigure $f 1 -weight 1
    pack $f -fill x
}

# Where a mechanism's BEHAVIOUR is set.
#
# Each mechanism names a config section (notes.rs's catalogue), and some
# of them already have a purpose-built editor in this app. Send the user
# there rather than to a text editor; only fall back to the generic
# key editor when nothing better exists.
proc ::voverlay::_mech_configure {key cfg help} {
    switch -glob -- $cfg {
        agent.guards { ::voverlay::open mechanisms ; return }
        failover     {
            if {[catch { ::vfg::open } fg_err]} {
                ::vstatus::set_msg [format [::vi18n::mc {failover graph unavailable: %1$s}] $fg_err]
            }
            return
        }
        plugins      { ::voverlay::open plugins ; return }
        memory       { ::voverlay::open memory ; return }
        cron         { ::voverlay::open cron ; return }
    }
    ::voverlay::_config_editor $cfg $help
}

# Set one mechanism, in whichever layer the scope selector points at,
# then rebuild so the new state is visible immediately.
#
# USER: "we should be able to save the values as new defaults to the
# config." on/off used to write the session override and nothing else,
# so "always keep this quiet" had to be re-stated in every new session.
# The default layer is `[mechanisms] silenced` in config.toml, which
# ::vagent's mechanism_enabled consults when a session says nothing.
proc ::voverlay::_mech_set {btn key value} {
    variable mech_scope
    set w [winfo toplevel $btn]

    if {$mech_scope eq "global"} {
        # A config array, so on/off are remove/add. There is no third
        # state: the config layer IS the bottom of the stack.
        set body [dict create path mechanisms.silenced]
        if {$value eq "off"} {
            dict set body add $key
        } else {
            dict set body remove $key
        }
        set resp [::vmanage::post $w /api/config/set $body]
        if {$resp eq ""} { return }
        ::vmanage::effect $w $resp "$key $value —"
        ::voverlay::_mechanisms_fill $w
        return
    }

    if {$::vchat::session eq ""} {
        ::vmanage::say $w [::vi18n::mc {no session open — switch the scope to the default, or open a session}] warn
        return
    }
    set body [dict create name [::vapi::json_string $key] value [::vapi::json_string $value]]
    if {[catch {::vapi::post_json "/api/sessions/$::vchat::session/mechanism" $body} r]} {
        ::vmanage::say $w [format [::vi18n::mc {mechanism %1$s failed: %2$s}] \
            $key [::vmanage::_reason $r]] warn
        return
    }
    set msg ""
    catch { set msg [dict get $r message] }
    ::vmanage::say $w $msg ok
    ::voverlay::_mechanisms_fill $w
}

# The guard knobs, editable.
#
# They used to be three lines of prose each ending "knob:
# agent.guards.max_iterations in config.toml" — the exact placeholder
# the user objected to. The explanation is still worth having (a guard
# you cannot see the trigger for is one you disable in frustration), so
# it stays in the read-only panel below; what moves up here is a control
# per knob that writes config.toml through /api/config/set.
#
# The effect badge is not decoration: `agent.*` is read when an agent is
# CONSTRUCTED, so a change reaches new sessions and leaves open ones on
# the value they started with. Saying so is the difference between a
# settings pane you trust and one you restart the gateway after.
# ── the generic config-key editor ────────────────────────────────────
#
# Not every setting deserves a bespoke pane, and the ones that do not
# used to get a sentence naming the file to go and edit. This is the
# fallback: name a dotted key, see what config.toml currently says
# (including whether it says anything at all), set it or clear it, and
# be told honestly whether the change is live, applies to new sessions,
# or needs a gateway restart.
#
# It writes through /api/config/set, so it inherits every guarantee the
# engine's editor gives: comments and ordering survive, the previous
# file is kept as config.toml.bak, and an edit that would make the file
# unreadable is refused before anything is written.
#
# It deliberately does NOT dump config.toml. The file can carry inline
# platform tokens, and a settings pane is not a place to spill them.

proc ::voverlay::_config_editor {section {help ""}} {
    set d .config_editor
    catch { destroy $d }
    toplevel $d
    wm title $d [format [::vi18n::mc {Configure %s}] $section]
    ::vplace::centered $d 640 380
    bind $d <Escape> [list destroy $d]
    catch { ::vtitle::attach $d [::vi18n::mc {Configure}] -close [list destroy $d] }

    ttk::frame $d.f
    pack $d.f -fill both -expand 1 -padx 12 -pady 12
    if {$help ne ""} {
        ttk::label $d.f.help -text $help -foreground $::vt::T(fg_dim) \
            -wraplength 600 -justify left
        pack $d.f.help -anchor w -pady {0 8}
    }

    ttk::label $d.f.kl -text [::vi18n::mc {key (dotted path in config.toml)}]
    ttk::entry $d.f.key
    $d.f.key insert 0 "$section."
    catch { ::va11y::name $d.f.key [::vi18n::mc {config key} ] }
    pack $d.f.kl -anchor w
    pack $d.f.key -fill x -pady {2 6}

    ttk::label $d.f.vl -text [::vi18n::mc {value (TOML: 5, true, "text", ["a","b"])}]
    ttk::entry $d.f.value
    catch { ::va11y::name $d.f.value [::vi18n::mc {config value}] }
    pack $d.f.vl -anchor w
    pack $d.f.value -fill x -pady {2 6}

    ttk::label $d.f.status -text "" -foreground $::vt::T(fg_dim) \
        -wraplength 600 -justify left
    pack $d.f.status -anchor w -pady {4 8}

    ttk::frame $d.f.b
    pack $d.f.b -fill x
    ttk::button $d.f.b.read -text [::vi18n::mc {Read}] \
        -command [list ::voverlay::_config_editor_read $d]
    ttk::button $d.f.b.set -text [::vi18n::mc {Set}] \
        -command [list ::voverlay::_config_editor_write $d set]
    ttk::button $d.f.b.clear -text [::vi18n::mc {Clear}] \
        -command [list ::voverlay::_config_editor_write $d clear]
    ttk::button $d.f.b.close -text [::vi18n::mc {Close}] -command [list destroy $d]
    pack $d.f.b.read $d.f.b.set $d.f.b.clear $d.f.b.close -side left -padx {0 6}
    foreach b {read set clear} { catch { ::vcoach::attach $d.f.b.$b } }

    bind $d.f.key <Return> [list ::voverlay::_config_editor_read $d]
    bind $d.f.value <Return> [list ::voverlay::_config_editor_write $d set]
    focus $d.f.key
    return $d
}

proc ::voverlay::_config_editor_say {d msg {kind info}} {
    if {![winfo exists $d.f.status]} { return }
    set fg $::vt::T(fg_dim)
    switch -- $kind {
        ok   { set fg $::vt::T(accent) }
        warn { set fg $::vt::T(gold) }
    }
    $d.f.status configure -text $msg -foreground $fg
}

proc ::voverlay::_config_editor_read {d} {
    set key [string trim [$d.f.key get] " ."]
    if {$key eq ""} { return }
    $d.f.key delete 0 end
    $d.f.key insert 0 $key
    if {[catch { ::vapi::get_json "/api/config/get?path=$key" } resp]} {
        _config_editor_say $d [format [::vi18n::mc {unreachable: %s}] \
            [::vmanage::_reason $resp]] warn
        return
    }
    set is_set 0
    catch { set is_set [dict get $resp set] }
    if {$is_set} {
        # Round-trip through the JSON encoder so what lands in the box
        # is valid TOML to send straight back. A value it cannot render
        # (a nested table) is worth saying: an empty box would read as
        # "not set", which is a different and wrong answer.
        set raw ""
        if {[catch { set raw [::vapi::_json_value [dict get $resp value]] } e]} {
            _config_editor_say $d [format \
                [::vi18n::mc {%1$s is set to something this box cannot edit (%2$s)}] \
                $key $e] warn
            return
        }
        $d.f.value delete 0 end
        $d.f.value insert 0 $raw
        _config_editor_say $d [format [::vi18n::mc {set. %s}] [dict get $resp note]]
    } else {
        $d.f.value delete 0 end
        # "Not set" is a real answer, distinct from "set to the default":
        # only the first means removing it changes nothing.
        _config_editor_say $d [format \
            [::vi18n::mc {not set in config.toml — the built-in default applies. %s}] \
            [dict get $resp note]]
    }
}

proc ::voverlay::_config_editor_write {d mode} {
    set key [string trim [$d.f.key get] " ."]
    if {$key eq ""} {
        _config_editor_say $d [::vi18n::mc {name a key first}] warn
        return
    }
    set body [dict create path $key]
    if {$mode eq "clear"} {
        dict set body value null
    } else {
        set raw [string trim [$d.f.value get]]
        if {$raw eq ""} {
            _config_editor_say $d [::vi18n::mc {give a value, or use Clear to remove the key}] warn
            return
        }
        # The value box takes TOML-ish literals so a number stays a
        # number: sending everything as a string would write
        # `max_iterations = "5"` and the config would stop parsing.
        dict set body raw_value $raw
    }
    if {[catch { ::vapi::post /api/config/set [::voverlay::_config_body $body] } resp]} {
        _config_editor_say $d [format [::vi18n::mc {refused: %s}] \
            [::vmanage::_reason $resp]] warn
        return
    }
    set r [::json::json2dict $resp]
    set note ""
    catch { set note [dict get $r note] }
    set kind ok
    if {[catch { dict get $r effect } eff]} { set eff "" }
    if {$eff eq "restart_gateway"} { set kind warn }
    _config_editor_say $d [string trim "[::vi18n::mc {saved.}] $note"] $kind
}

# Build the /api/config/set body, turning the box's TOML-ish literal into
# the right JSON type. `raw_value` is our own marker, never sent.
proc ::voverlay::_config_body {body} {
    set path [dict get $body path]
    if {![dict exists $body raw_value]} {
        return "{\"path\":\"[::vapi::json_escape $path]\",\"value\":null}"
    }
    set raw [dict get $body raw_value]
    return "{\"path\":\"[::vapi::json_escape $path]\",\"value\":[::voverlay::_toml_literal_to_json $raw]}"
}

# A small literal reader: bare true/false, integers, floats, quoted
# strings, and simple lists. Anything else is sent as a string, which is
# the safe reading of "the user typed a word".
proc ::voverlay::_toml_literal_to_json {raw} {
    set s [string trim $raw]
    if {$s in {true false}} { return $s }
    if {[string is integer -strict $s]} { return $s }
    if {[string is double -strict $s]} { return $s }
    if {[string index $s 0] eq "\[" && [string index $s end] eq "\]"} {
        set inner [string trim [string range $s 1 end-1]]
        if {$inner eq ""} { return "\[\]" }
        set out {}
        foreach part [split $inner ,] {
            lappend out [_toml_literal_to_json $part]
        }
        return "\[[join $out ,]\]"
    }
    # Strip one layer of quotes if the user wrote them; JSON-escape either way.
    if {[string length $s] >= 2 && [string index $s 0] eq "\"" && [string index $s end] eq "\""} {
        set s [string range $s 1 end-1]
    }
    return "\"[::vapi::json_escape $s]\""
}
# The guard knobs, editable — see the comment on ::voverlay::_guard_save.
proc ::voverlay::_guard_knobs {w} {
    set f [_mech_knobs $w]
    if {![winfo exists $f]} { return }
    foreach c [winfo children $f] { destroy $c }

    set cfg {}
    if {![catch { ::vapi::get_json /api/guards } g]} {
        catch { set cfg [dict get $g config] }
    }
    if {$cfg eq ""} {
        ttk::label $f.err -text [::vi18n::mc {guards unreachable}] -foreground $::vt::T(gold)
        grid $f.err -row 0 -column 0 -sticky w
        return
    }

    set row 0
    foreach {key label kind} {
        max_iterations            {tool rounds per turn}        int
        guardrail_warn_at         {warn after N identical calls} int
        guardrail_halt_at         {halt after N identical calls} int
        verification_max_attempts {nudges per turn}              int
        verification_nudge        {verification nudge}           bool
    } {
        set cur ""
        catch { set cur [dict get $cfg $key] }
        ttk::label $f.l$row -text [::vi18n::mc $label]
        grid $f.l$row -row $row -column 0 -sticky e -padx {0 6} -pady 1
        if {$kind eq "bool"} {
            set ::voverlay::guard_$key [expr {$cur in {1 true}}]
            ttk::checkbutton $f.c$row -variable ::voverlay::guard_$key \
                -command [list ::voverlay::_guard_save $w $key bool]
            catch { ::va11y::name $f.c$row [::vi18n::mc $label] }
            grid $f.c$row -row $row -column 1 -sticky w
        } else {
            ttk::spinbox $f.c$row -from 1 -to 100000 -width 8 -increment 1
            $f.c$row set $cur
            catch { ::va11y::name $f.c$row [::vi18n::mc $label] }
            grid $f.c$row -row $row -column 1 -sticky w
            ttk::button $f.b$row -text [::vi18n::mc {Set}] \
                -command [list ::voverlay::_guard_save $w $key $f.c$row]
            catch { ::vcoach::attach $f.b$row }
            grid $f.b$row -row $row -column 2 -sticky w -padx {6 0}
        }
        ttk::label $f.k$row -text "agent.guards.$key" -foreground $::vt::T(fg_dim) ;# i18n-exempt: config key path
        grid $f.k$row -row $row -column 3 -sticky w -padx {12 0}
        incr row
    }
    grid columnconfigure $f 3 -weight 1
}

proc ::voverlay::_guard_save {w key src} {
    if {$src eq "bool"} {
        set value [::vapi::json_bool [set ::voverlay::guard_$key]]
    } else {
        set raw [string trim [$src get]]
        if {![string is integer -strict $raw] || $raw < 1} {
            ::vmanage::say $w [format [::vi18n::mc {%s must be a positive whole number}] $key] warn
            return
        }
        set value $raw
    }
    set resp [::vmanage::post $w /api/config/set \
        [dict create path "agent.guards.$key" value $value]]
    if {$resp eq ""} { return }
    ::vmanage::effect $w $resp "$key = $value —"
    _guards_fill $w
}
# The guards report: what each guard is, what makes it fire, and where
# it stands in the sessions that have run a turn this process.
proc ::voverlay::_guards_fill {w} {
    set t [_mech_text $w]
    if {![winfo exists $t]} { return }
    ::vchat::_writable $t { $t delete 1.0 end }
    $t tag configure gh -foreground $::vt::T(accent) -font {TkDefaultFont 10 bold}
    $t tag configure gdim -foreground $::vt::T(fg_dim)
    $t tag configure gwarn -foreground $::vt::T(gold)

    if {[catch { ::vapi::get_json /api/guards } g]} {
        ::vchat::_writable $t { $t insert end "guards unreachable: $g\n" gwarn }
        return
    }
    set cfg {}
    catch { set cfg [dict get $g config] }

    proc ::voverlay::_gval {d k {dflt "?"}} {
        if {[dict exists $d $k]} { return [dict get $d $k] }
        return $dflt
    }

    ::vchat::_writable $t {
        $t insert end "ITERATION CAP\n" gh
        $t insert end "  a turn may run at most [_gval $cfg max_iterations] tool rounds.\n"
        $t insert end "  fires when: the model keeps calling tools without finishing.\n" gdim
        $t insert end "  to see it:  ask for something that loops, e.g. \"keep listing files\n" gdim
        $t insert end "              until you have seen every one\" with the cap set low.\n" gdim
        $t insert end "  knob:       agent.guards.max_iterations in config.toml\n\n" gdim

        $t insert end "REPEAT GUARDRAIL\n" gh
        $t insert end "  warns after [_gval $cfg guardrail_warn_at] identical calls,"
        $t insert end " halts after [_gval $cfg guardrail_halt_at].\n"
        $t insert end "  fires when: the SAME tool is called with the SAME arguments\n" gdim
        $t insert end "              over and over — a stuck loop.\n" gdim
        $t insert end "  to see it:  ask it to read one file repeatedly.\n" gdim
        $t insert end "  knob:       agent.guards.guardrail_warn_at / _halt_at\n\n" gdim

        $t insert end "VERIFICATION NUDGE\n" gh
        set vn [_gval $cfg verification_nudge]
        $t insert end "  [expr {$vn in {1 true} ? "on" : "off"}];"
        $t insert end " at most [_gval $cfg verification_max_attempts] nudge(s) per turn.\n"
        $t insert end "  fires when: the turn edited files but ran nothing to check them.\n" gdim
        $t insert end "  to see it:  ask for an edit and tell it not to run the tests.\n" gdim
        $t insert end "  knob:       agent.guards.verification_nudge\n\n" gdim

        $t insert end "WHERE THEY STAND NOW\n" gh
        set sessions {}
        catch { set sessions [dict get $g active_sessions] }
        if {![llength $sessions]} {
            $t insert end "  no session has run a turn yet this process — state appears\n" gdim
            $t insert end "  once a turn has started.\n" gdim
        }
        foreach s $sessions {
            set sid [_gval $s session]
            set st [_gval $s state]
            $t insert end "  session $sid\n"
            if {![catch {dict size $st}]} {
                set it {}
                catch { set it [dict get $st iterations] }
                if {$it ne ""} {
                    set used [_gval $it used 0]
                    set cap [_gval $it cap 0]
                    set tag [expr {$cap ne 0 && $used >= $cap ? "gwarn" : "gdim"}]
                    $t insert end "    tool rounds used: $used / $cap\n" $tag
                }
                set v {}
                catch { set v [dict get $st verification] }
                if {$v ne ""} {
                    $t insert end "    edited files: [_gval $v edits_seen false]" gdim
                    $t insert end "  ·  evidence since the edit: [_gval $v evidence_after_edit 0]\n" gdim
                    $t insert end "    nudges this turn: [_gval $v nudges_this_turn 0]\n" gdim
                    set paths {}
                    catch { set paths [dict get $v edited_paths] }
                    foreach p $paths { $t insert end "      edited: $p\n" gdim }
                }
            } else {
                $t insert end "    $st\n" gdim
            }
        }

        $t insert end "\nWHEN A GUARD FIRES\n" gh
        $t insert end "  it writes a ⚠ line into the transcript at the point it acted,\n" gdim
        $t insert end "  with the knob attached. Nothing happens silently.\n" gdim
        $t insert end "  /mechanisms lists every automatic mechanism and its state.\n" gdim
    }
}

# ── cron ─────────────────────────────────────────────────────────────
# ── Tasks: running subagents, background shells and monitors ──────────
#
# The 2026-09-02 audit found this state was tracked and displayed
# NOWHERE: the registry was written on every delegate call and read by
# nothing. This panel is the desktop half of fixing that, and it follows
# the cron overlay exactly — a tagged text widget with inline [action]
# links per row, because each row needs its own verbs.
proc ::voverlay::_tasks {w} {
    ::vmanage::window $w [::vi18n::mc {Running tasks}] 820x520 {420 300}
    ::vmanage::button $w.bar reload [::vi18n::mc {Reload}] \
        [list ::voverlay::_tasks_fill $w] [::vi18n::mc {refresh the list}]

    ttk::frame $w.main
    text $w.main.t -wrap word -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -highlightthickness 0 -relief flat -font {monospace 10}
    # Section headers (subagents / shells / monitors): with the taxonomy
    # split, the KIND is the primary fact, so headers get the accent +
    # bold treatment the mechanism panel's `gh` uses.
    $w.main.t tag configure thead -foreground $::vt::T(accent) -font {TkDefaultFont 10 bold}
    ttk::scrollbar $w.main.sb -orient vertical -command [list $w.main.t yview]
    $w.main.t configure -yscrollcommand [list $w.main.sb set]
    pack $w.main.sb -side right -fill y
    pack $w.main.t -side left -fill both -expand 1
    pack $w.main -fill both -expand 1 -padx 8 -pady {0 8}

    _tasks_fill $w
    # Live state goes stale while you look at it; poll only while the
    # window is up, and stop on destroy (the instances pane's rule).
    _tasks_poll $w
    bind $w <Destroy> [list ::voverlay::_tasks_stop %W $w]
}

proc ::voverlay::_tasks_stop {which w} {
    variable tasks_after
    if {$which ne $w} { return }
    if {[info exists tasks_after] && $tasks_after ne ""} {
        after cancel $tasks_after
        set tasks_after ""
    }
}

proc ::voverlay::_tasks_poll {w} {
    variable tasks_after
    if {![winfo exists $w]} { set tasks_after ""; return }
    catch { ::voverlay::_tasks_fill $w }
    set tasks_after [after 3000 [list ::voverlay::_tasks_poll $w]]
}

proc ::voverlay::_tasks_fill {w} {
    if {![winfo exists $w.main.t]} { return }
    set t $w.main.t
    # Keep the scroll position: a 3s poll that jumps you to the top
    # while you are reading is worse than no refresh.
    set yview [lindex [$t yview] 0]
    $t configure -state normal
    $t delete 1.0 end
    if {[catch {::vapi::get_json /api/tasks} resp]} {
        $t insert end "tasks unavailable: [::vmanage::_reason $resp]\n"
        $t configure -state disabled
        return
    }
    set tasks [dict get $resp tasks]
    # Keep only what is running, bucketed by kind. The USER taxonomy
    # (2026-09-17) made these three things SEPARATE concepts, so the
    # panel shows them as sections — "subagent" vs "shell" vs "monitor"
    # is the first thing you want to see, not a column to scan.
    set by_kind {subagents {} shells {} monitors {}}
    set live 0
    foreach task $tasks {
        if {[dict get $task status] ne "running"} { continue }
        incr live
        set k [dict get $task kind]
        set key [_tasks_kind_key $k]
        dict set by_kind $key [list {*}[dict get $by_kind $key] $task]
    }
    if {!$live} {
        $t insert end [::vi18n::mc {no running tasks}]
        $t insert end "\n"
    } else {
        foreach {key tasks_of_kind} $by_kind {
            if {![llength $tasks_of_kind]} { continue }
            $t insert end "[_tasks_kind_title $key]\n" thead
            foreach task $tasks_of_kind { _tasks_row $t $task }
            $t insert end "\n"
        }
    }
    $t configure -state disabled
    catch { $t yview moveto $yview }
}

# Registry kind → section key. The registry says "subagent" (singular);
# the sections read as plural group names. Unknown kinds land in
# "shells" — a conservative default: they get [cancel] and, if the
# engine ever gives them output, nothing breaks.
proc ::voverlay::_tasks_kind_key {kind} {
    switch -- $kind {
        subagent { return subagents }
        monitor  { return monitors }
        default  { return shells }
    }
}

# Translated section header. The `mc` calls carry LITERAL keys (not a
# variable) on purpose: uitest scans the sources for every mc key and
# cross-checks en.msg in both directions, and a computed key is
# invisible to it.
proc ::voverlay::_tasks_kind_title {key} {
    switch -- $key {
        subagents { return [::vi18n::mc {subagents}] }
        monitors  { return [::vi18n::mc {monitors}] }
        default   { return [::vi18n::mc {shells}] }
    }
}

proc ::voverlay::_tasks_row {t task} {
    set id [dict get $task id]
    set kind [dict get $task kind]
    set label [dict get $task label]
    set secs [dict get $task elapsed_s]
    # The section already names the kind; repeating it per row is noise.
    $t insert end [format "  \[%s\] %ss  %s" $id $secs $label]
    # A shell is the only kind with captured output to show.
    if {$kind eq "shell"} {
        _tasks_link $t $id output
    }
    # A subagent IS a session (USER taxonomy, 2026-09-17): attach
    # means switch the chat to the child session and review it.
    set csession [::vapi::field $task child_session]
    if {$csession ne ""} {
        _tasks_link $t $id "attach:$csession"
    }
    _tasks_link $t $id cancel
    $t insert end "\n"
}

proc ::voverlay::_tasks_link {t id action} {
    set tag "btn_${id}_${action}"
    # An action can carry an argument as "verb:arg" (attach:<session>);
    # the label is the verb alone, the whole string is what the click
    # handler receives.
    set verb [lindex [split $action ":"] 0]
    $t insert end "  \[$verb\]" $tag
    $t tag configure $tag -foreground $::vt::T(accent) -underline 1
    $t tag bind $tag <Button-1> [list ::voverlay::_tasks_action $t $id $action]
}

proc ::voverlay::_tasks_action {t id action} {
    set w [winfo toplevel $t]
    set verb [lindex [split $action ":"] 0]
    set arg  [lindex [split $action ":"] 1]
    switch -- $verb {
        attach {
            # Switch the chat to the child session and close the overlay:
            # "attach and review" is a navigation act, not a listing one.
            if {$arg eq ""} { return }
            catch { destroy $w }
            if {[catch { ::vchat::open_session $arg } attach_err]} {
                ::vstatus::set_msg [format [::vi18n::mc {attach failed: %1$s}] $attach_err]
                return
            }
            ::vstatus::set_msg [format [::vi18n::mc {attached to child session %1$s}] $arg]
            return
        }
        cancel {
            if {[catch {::vapi::post "/api/tasks/$id/cancel" ""} e]} {
                ::vstatus::set_msg [format [::vi18n::mc {cancel failed: %1$s}] [::vmanage::_reason $e]]
                return
            }
            ::vstatus::set_msg [format [::vi18n::mc {cancelled task %1$s}] $id]
        }
        output {
            if {[catch {::vapi::get_json "/api/tasks/$id/output"} r]} {
                ::vstatus::set_msg [format [::vi18n::mc {no output for task %1$s}] $id]
                return
            }
            $t configure -state normal
            $t insert end "\n--- task $id output ---\n[dict get $r output]\n"
            $t configure -state disabled
        }
    }
    after 400 [list catch [list ::voverlay::_tasks_fill $w]]
}

proc ::voverlay::_cron {w} {
    ::vmanage::window $w [::vi18n::mc {Cron jobs}] 820x520 {420 300}
    ::vmanage::button $w.bar new [::vi18n::mc {New…}] \
        [list ::voverlay::_cron_new $w] [::vi18n::mc {create a cron job}]
    ::vmanage::button $w.bar edit [::vi18n::mc {Edit…}] \
        [list ::voverlay::_cron_edit $w] [::vi18n::mc {edit the selected job}]
    ::vmanage::button $w.bar del [::vi18n::mc {Delete}] \
        [list ::voverlay::_cron_delete $w] [::vi18n::mc {delete the selected job}]
    ::vmanage::separator $w.bar sep1
    ::vmanage::button $w.bar reload [::vi18n::mc {Reload}] \
        [list ::voverlay::_cron_fill $w]

    ttk::frame $w.main
    text $w.main.t -wrap word -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -highlightthickness 0 -relief flat -font {monospace 10}
    ttk::scrollbar $w.main.sb -orient vertical -command [list $w.main.t yview]
    $w.main.t configure -yscrollcommand [list $w.main.sb set]
    pack $w.main.sb -side right -fill y
    pack $w.main.t -side left -fill both -expand 1
    pack $w.main -fill both -expand 1 -padx 8 -pady {0 8}

    _cron_fill $w
}

proc ::voverlay::_cron_fill {w} {
    variable cron_jobs
    variable cron_selected
    set t $w.main.t
    $t configure -state normal
    $t delete 1.0 end
    if {[catch {::vapi::get_json /api/cron} resp]} {
        $t insert end "cron unavailable: [::vmanage::_reason $resp]\n"
        $t configure -state disabled
        return
    }
    set jobs [dict get $resp jobs]
    set cron_jobs {}
    if {![llength $jobs]} {
        $t insert end [::vi18n::mc {no cron jobs — New… creates one}]
        $t insert end "\n"
    }
    foreach j $jobs {
        set id [dict get $j id]
        dict set cron_jobs $id $j
        # name/schedule_human/session are nullable — read them through
        # ::vapi::field so a JSON null reads as absence, not the word.
        set sched [dict get $j schedule]
        set enabled [dict get $j enabled]
        set next [dict get $j next_run_at]
        set last [dict get $j last_status]
        set state [expr {$enabled ? "●" : "○"}]
        set label [::vapi::field $j name $id]
        # The engine's own words for the schedule, next to the raw
        # string: `every:3600` is exact and tells a human nothing.
        set sched_txt $sched
        set human [::vapi::field $j schedule_human]
        if {$human ne ""} { set sched_txt "$sched ($human)" }
        # Stage 5: a wakeup job says WHICH session it wakes, right on
        # the row — the difference between "runs somewhere anonymous"
        # and "resumes my session" must not need the edit dialog.
        set sess [::vapi::field $j session]
        set sess_txt [expr {$sess ne "" ? " · wakes $sess" : ""}]
        set line "$state $label · $sched_txt$sess_txt · next: $next · last: $last\n"
        set start [$t index "end-1c"]
        $t insert end $line
        $t tag add "job_$id" $start "end-1c"
        # Clicking the row is what "selected" means for the toolbar —
        # this list is a tagged text widget, not a tree.
        $t tag bind "job_$id" <Button-1> [list ::voverlay::_cron_select $w $id]
        foreach a [list pause resume run history edit delete] {
            $t insert end "  \[$a\]" "btn_$id$a"
            $t tag configure "btn_$id$a" -foreground $::vt::T(accent) -underline 1
            $t tag bind "btn_$id$a" <Button-1> [list ::voverlay::_cron_action $w $id $a]
        }
        $t insert end "\n\n"
    }
    $t configure -state disabled
    if {[info exists cron_selected] && $cron_selected ne ""
        && ![dict exists $cron_jobs $cron_selected]} {
        set cron_selected ""
    }
    _cron_say_selection $w
}

proc ::voverlay::_cron_select {w id} {
    variable cron_selected
    set cron_selected $id
    _cron_say_selection $w
}

proc ::voverlay::_cron_say_selection {w} {
    variable cron_selected
    variable cron_jobs
    if {![info exists cron_selected] || $cron_selected eq ""} {
        ::vmanage::say $w [::vi18n::mc {click a job to select it}]
        return
    }
    # A pure local dict read — no API call, no catch needed (and the
    # uitest gate rightly flags ::vapi::* inside a silent catch).
    set label $cron_selected
    if {[dict exists $cron_jobs $cron_selected]} {
        set label [::vapi::field [dict get $cron_jobs $cron_selected] name $cron_selected]
    }
    ::vmanage::say $w [format [::vi18n::mc {selected: %s}] $label]
}

proc ::voverlay::_cron_action {w id action} {
    variable cron_selected
    set cron_selected $id
    if {$action eq "edit"}   { _cron_dialog $w $id ; return }
    if {$action eq "delete"} { _cron_delete $w ; return }
    switch -- $action {
        pause - resume {
            # These two used to swallow the error AND say nothing on
            # success: clicking Pause looked identical to clicking
            # nothing, whether the job paused or the POST 500'd.
            if {[catch {::vapi::post "/api/cron/$id/$action" ""} e]} {
                ::vstatus::set_msg [format [::vi18n::mc {%1$s failed: %2$s}] $action $e]
                return
            }
            ::vstatus::set_msg [format [::vi18n::mc {cron %1$s %2$sd}] $id $action]
            catch { ::voverlay::_cron_fill $w }
        }
        run {
            if {[catch {::vapi::post "/api/cron/$id/run" ""} r]} {
                ::vstatus::set_msg [format [::vi18n::mc {run failed: %s}] $r]
                return
            }
            ::vstatus::set_msg [::vi18n::mc {cron run started (check history)}]
        }
        history {
            if {[catch {::vapi::get_json "/api/cron/$id/history"} h]} {
                ::vstatus::set_msg [format [::vi18n::mc {history failed: %s}] $h]
                return
            }
            set msg "runs for $id:\n"
            foreach r [dict get $h runs] {
                append msg "[dict get $r finished_at] [dict get $r status] [dict get $r detail]\n"
            }
            tk_messageBox -title [::vi18n::mc {Cron history}] -message $msg
        }
    }
    after 400 [list ::voverlay::_cron_fill $w]
}
# ── skills ───────────────────────────────────────────────────────────
#
# USER: "The skills manager is also not really a manager in the desktop
# ui? It just lists them. I want to control which are injected/visible
# per session, add/remove them - i.e. MANAGE them."
#
# So: a state column and an on/off/default control per skill, a scope
# switch (this session vs the config default), and New / Install /
# Edit / Delete. The viewer stays — with 134 skills installed, reading
# one is still the common case, and the sash keeps the split adjustable.
#
# TWO LAYERS OF CHOICE, and the row says which one is deciding:
#
#   default   `[skills] disabled` in config.toml, every session
#   session   store meta `skill:<id>:<name>`, this session only, wins
#
# A session toggle REBUILDS that session's system prompt, so it takes
# effect on the next turn rather than the next session — the skills
# index is a frozen snapshot and would otherwise ignore the switch.
#
# hermes-imported skills are read-only: that library belongs to another
# tool. The row says so and Edit explains rather than failing silently.

proc ::voverlay::_skills {w} {
    ::vmanage::window $w [::vi18n::mc {Skills}] 980x620
    ::vmanage::button $w.bar on [::vi18n::mc {On}] \
        [list ::voverlay::_skill_set $w on] [::vi18n::mc {switch the selected skill on}]
    ::vmanage::button $w.bar off [::vi18n::mc {Off}] \
        [list ::voverlay::_skill_set $w off] [::vi18n::mc {switch the selected skill off}]
    ::vmanage::button $w.bar def [::vi18n::mc {Default}] \
        [list ::voverlay::_skill_set $w default] [::vi18n::mc {drop the override for the selected skill}]
    ::vmanage::separator $w.bar sep1
    ::vmanage::button $w.bar new [::vi18n::mc {New…}] \
        [list ::voverlay::_skill_new $w] [::vi18n::mc {create a skill}]
    ::vmanage::button $w.bar install [::vi18n::mc {Install…}] \
        [list ::voverlay::_skill_install $w] [::vi18n::mc {install a skill from a git URL}]
    ::vmanage::button $w.bar edit [::vi18n::mc {Edit}] \
        [list ::voverlay::_skill_edit $w] [::vi18n::mc {open the selected skill in the Dev editor}]
    ::vmanage::button $w.bar remove [::vi18n::mc {Delete}] \
        [list ::voverlay::_skill_delete $w] [::vi18n::mc {delete the selected skill}]

    # Scope: which layer the on/off buttons write to. Session first —
    # it is the reversible one, and the one the user asked for.
    ttk::frame $w.scope
    pack $w.scope -fill x -padx 8 -pady {4 0}
    ttk::label $w.scope.l -text [::vi18n::mc {apply to:}]
    ttk::radiobutton $w.scope.session -text [::vi18n::mc {this session}] \
        -variable ::voverlay::skills_scope -value session \
        -command [list ::voverlay::_skills_reload $w]
    ttk::radiobutton $w.scope.global -text [::vi18n::mc {the default (config.toml)}] \
        -variable ::voverlay::skills_scope -value global \
        -command [list ::voverlay::_skills_reload $w]
    ttk::label $w.scope.filterl -text [::vi18n::mc {filter}]
    ttk::entry $w.scope.filter
    ttk::label $w.scope.count -text "" -foreground $::vt::T(fg_dim)
    catch { ::va11y::name $w.scope.filter [::vi18n::mc {filter skills by name}] }
    pack $w.scope.l $w.scope.session $w.scope.global -side left -padx {0 8}
    pack $w.scope.count -side right -padx 6
    pack $w.scope.filterl -side left -padx {12 4}
    pack $w.scope.filter -side left -fill x -expand 1

    ttk::panedwindow $w.main -orient horizontal
    pack $w.main -fill both -expand 1 -padx 8 -pady 6
    set t [::vmanage::tree $w.main list {name state scope where} \
        [list [::vi18n::mc {skill}] [::vi18n::mc {state}] [::vi18n::mc {set by}] [::vi18n::mc {library}]] \
        {260 70 110 80}]
    $w.main add $t -weight 2
    catch { ::va11y::name $t.t [::vi18n::mc {skill list}] }

    ttk::frame $w.main.right
    text $w.main.right.view -wrap word -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -highlightthickness 0 -relief flat \
        -font {monospace 10} -yscrollcommand [list $w.main.right.sb set]
    ttk::scrollbar $w.main.right.sb -orient vertical \
        -command [list $w.main.right.view yview]
    grid $w.main.right.view -row 0 -column 0 -sticky nsew
    grid $w.main.right.sb   -row 0 -column 1 -sticky ns
    grid rowconfigure    $w.main.right 0 -weight 1
    grid columnconfigure $w.main.right 0 -weight 1
    catch { ::vtext::readonly $w.main.right.view }
    $w.main add $w.main.right -weight 3
    bind $w.main <Map> [list ::voverlay::_split_once %W 0.45]

    bind $w.scope.filter <KeyRelease> [list ::voverlay::_skills_paint $w]
    bind $w.scope.filter <Down> [list focus $t.t]
    bind $t.t <<TreeviewSelect>> [list ::voverlay::_skills_view $w]
    # Double-click cycles the state: the fastest way to try a session
    # without a skill, which is the whole point of the session scope.
    bind $t.t <Double-1> [list ::voverlay::_skill_cycle $w]
    focus $w.scope.filter
    _skills_reload $w
}

proc ::voverlay::_skills_tree {w} { return $w.main.list.t }

# Fetch, then paint. Separated because the filter repaints without
# re-fetching 134 rows.
proc ::voverlay::_skills_reload {w} {
    variable skills_data
    variable skills_scope
    set path /api/skills
    if {$skills_scope eq "session" && $::vchat::session ne ""} {
        append path "?session=$::vchat::session"
    }
    set resp [::vmanage::get $w $path]
    if {$resp eq ""} { return }
    set skills_data [dict get $resp skills]
    if {$skills_scope eq "session" && $::vchat::session eq ""} {
        ::vmanage::say $w [::vi18n::mc {no session open — session overrides need one; showing the defaults}] warn
    } else {
        ::vmanage::say $w [format [::vi18n::mc {%1$s of %2$s skills reach the prompt}] \
            [dict get $resp enabled_count] [dict get $resp count]]
    }
    _skills_paint $w
}

proc ::voverlay::_skills_paint {w} {
    variable skills_data
    set t [_skills_tree $w]
    if {![winfo exists $t]} { return }
    set keep [lindex [$t selection] 0]
    $t delete [$t children {}]
    set fl [string tolower [$w.scope.filter get]]
    set shown 0
    foreach s $skills_data {
        set name [dict get $s name]
        set cat [dict get $s category]
        set label [expr {$cat ne "" ? "$cat/$name" : $name}]
        if {$fl ne "" && ![string match *[string map {* \\*} $fl]* [string tolower $label]]} {
            continue
        }
        # WHICH layer decided, not just the answer. "off" with no
        # explanation is the thing this overlay exists to stop.
        if {[::vapi::field $s session_override] ne ""} {
            set by [::vi18n::mc {session}]
        } elseif {[dict get $s disabled_by_default]} {
            set by [::vi18n::mc {config}]
        } else {
            set by ""
        }
        $t insert {} end -id $name -values [list $label \
            [expr {[dict get $s enabled] ? [::vi18n::mc {on}] : [::vi18n::mc {off}]}] \
            $by [dict get $s root]]
        incr shown
    }
    ::vi18n::label $w.scope.count {%d shown} $shown
    if {$keep ne "" && [$t exists $keep]} {
        $t selection set $keep
    }
}

proc ::voverlay::_skill_selected {w} {
    variable skills_data
    set sel [lindex [[_skills_tree $w] selection] 0]
    if {$sel eq ""} {
        ::vmanage::say $w [::vi18n::mc {select a skill first}]
        return ""
    }
    foreach s $skills_data {
        if {[dict get $s name] eq $sel} { return $s }
    }
    return ""
}

proc ::voverlay::_skills_view {w} {
    set s [_skill_selected $w]
    if {$s eq ""} { return }
    set body [::vi18n::mc {loading…}]
    if {[catch {::vapi::get_json "/api/skills/[dict get $s name]"} resp]} {
        set body [format [::vi18n::mc {unavailable: %s}] $resp]
    } else {
        set body [dict get $resp body]
    }
    if {![dict get $s editable]} {
        set body "# [format [::vi18n::mc {read-only: this skill comes from the %s library}] [dict get $s root]]\n\n$body"
    }
    ::vtext::set_text $w.main.right.view $body
}

# on | off | default, written to whichever scope the radio says.
proc ::voverlay::_skill_set {w value} {
    variable skills_scope
    set s [_skill_selected $w]
    if {$s eq ""} { return }
    set name [dict get $s name]

    if {$skills_scope eq "session"} {
        if {$::vchat::session eq ""} {
            ::vmanage::say $w [::vi18n::mc {no session open — open one, or switch the scope to the default}] warn
            return
        }
        set resp [::vmanage::post $w "/api/sessions/$::vchat::session/skill" \
            [dict create name $name value $value]]
        if {$resp eq ""} { return }
        set msg ""
        catch { set msg [dict get $resp message] }
        ::vmanage::say $w $msg ok
        _skills_reload $w
        return
    }

    # The default layer is a config array, so on/off are add/remove and
    # "default" means the same as "on" — there is no third state to drop
    # to below the config default itself.
    set body [dict create path skills.disabled]
    if {$value eq "off"} {
        dict set body add $name
    } else {
        dict set body remove $name
    }
    set resp [::vmanage::post $w /api/config/set $body]
    if {$resp eq ""} { return }
    ::vmanage::effect $w $resp "$name:"
    _skills_reload $w
}

proc ::voverlay::_skill_cycle {w} {
    set s [_skill_selected $w]
    if {$s eq ""} { return }
    _skill_set $w [expr {[dict get $s enabled] ? "off" : "on"}]
}

proc ::voverlay::_skill_new {w} {
    set name [::vmanage::ask $w [::vi18n::mc {New skill}] [::vi18n::mc {name}] "" \
        [format [::vi18n::mc {Creates %s/<name>/SKILL.md from a template and opens it. The frontmatter description is the only part that reaches the prompt index, so make it say WHEN to use the skill.}] [::vpaths::skills_dir]]]
    if {[string trim $name] eq ""} { return }
    set resp [::vmanage::post $w /api/skills/new [dict create name $name description ""]]
    if {$resp eq ""} { return }
    _skills_reload $w
    if {[catch { ::voverlay::_open_in_dev [dict get $resp path] } od_err]} {
        puts stderr "veles-desktop: open-in-dev failed for new skill $name: $od_err"
    }
    ::vmanage::say $w [format [::vi18n::mc {created %s — editing it now}] $name] ok
}

proc ::voverlay::_skill_install {w} {
    set url [::vmanage::ask $w [::vi18n::mc {Install skill}] [::vi18n::mc {git URL}] "" \
        [format [::vi18n::mc {The repository needs a SKILL.md at its root or one category deep. It lands in %s.}] [::vpaths::skills_dir]]]
    if {[string trim $url] eq ""} { return }
    ::vmanage::say $w [::vi18n::mc {cloning…}]
    set resp [::vmanage::post $w /api/skills/install [dict create url $url]]
    if {$resp eq ""} { return }
    set msg ""
    catch { set msg [dict get $resp message] }
    _skills_reload $w
    ::vmanage::say $w $msg ok
}

proc ::voverlay::_skill_edit {w} {
    set s [_skill_selected $w]
    if {$s eq ""} { return }
    if {![dict get $s editable]} {
        ::vmanage::say $w [format \
            [::vi18n::mc {%1$s belongs to the %2$s library and is read-only — copy it with New… to change it}] \
            [dict get $s name] [dict get $s root]] warn
        return
    }
    ::voverlay::_open_in_dev [dict get $s path]
    ::vmanage::say $w [format [::vi18n::mc {opened %s in the Dev pane}] [dict get $s name]] ok
}

proc ::voverlay::_skill_delete {w} {
    set s [_skill_selected $w]
    if {$s eq ""} { return }
    set name [dict get $s name]
    if {![dict get $s editable]} {
        ::vmanage::say $w [format \
            [::vi18n::mc {%1$s belongs to the %2$s library — veles never deletes another tool's skills}] \
            $name [dict get $s root]] warn
        return
    }
    if {![::vmanage::confirm $w [format [::vi18n::mc {Delete skill %s?}] $name] \
        [::vi18n::mc {The directory and everything in it is removed. This one is not undoable — switch it off instead if you only want it out of the prompt.}]]} {
        return
    }
    set resp [::vmanage::del $w "/api/skills/$name"]
    if {$resp eq ""} { return }
    _skills_reload $w
    ::vmanage::say $w [format [::vi18n::mc {deleted %s}] $name] ok
}



# Put a paned window's sash at `frac` of its width, once. Re-running on
# every <Map> would fight the user every time the overlay is unmapped
# and remapped, so the binding removes itself.
proc ::voverlay::_split_once {pw frac} {
    bind $pw <Map> {}
    after idle [list ::voverlay::_split_apply $pw $frac]
}

# Put the sash at `frac` along the panedwindow's OWN axis.
#
# ttk's `-weight` only shares out EXTRA space; the INITIAL split comes
# from each pane's requested size, and ttk::panedwindow has no per-pane
# -minsize to stop one collapsing. In the Mechanisms overlay the guards
# report asks for a tall widget, so it took the whole window and left
# the scrolling half a five-pixel slot — a scrollbar that is technically
# present and practically useless.
proc ::voverlay::_split_apply {pw frac} {
    if {![winfo exists $pw]} { return }
    set vertical 0
    catch { set vertical [expr {[$pw cget -orient] eq "vertical"}] }
    set extent [expr {$vertical ? [winfo height $pw] : [winfo width $pw]}]
    if {$extent <= 1} { return }
    catch { $pw sashpos 0 [expr {int($extent * $frac)}] }
}

# Re-apply on resize, with a floor.
#
# One-shot placement is right for the Skills overlay, whose split is a
# preference the user then owns. It is wrong here: shrinking the window
# squeezes the top pane back to nothing, and the pane that SCROLLS is
# the one that has to keep a usable height. `floor` is in pixels.
proc ::voverlay::_split_keep {pw frac floor} {
    bind $pw <Configure> [list ::voverlay::_split_enforce $pw $frac $floor]
    after idle [list ::voverlay::_split_enforce $pw $frac $floor]
}

proc ::voverlay::_split_enforce {pw frac floor} {
    if {![winfo exists $pw]} { return }
    set h [winfo height $pw]
    if {$h <= 1} { return }
    set want [expr {int($h * $frac)}]
    if {$want < $floor} { set want [expr {$h < $floor ? $h : $floor}] }
    set now 0
    catch { set now [$pw sashpos 0] }
    # Only correct a sash squeezed BELOW the floor — otherwise every
    # resize would undo a split the user had dragged.
    if {$now >= $floor || $now >= $want} { return }
    catch { $pw sashpos 0 $want }
}


# ── memory ───────────────────────────────────────────────────────────
#
# The header used to say "Memory (read-only view)" and it meant it: the
# entries rendered into a disabled text widget and there was no way to
# add, change or remove one. MemoryStore has had the full write API all
# along (write / write_file / apply / rollback / log) — it was reachable
# only from the agent's `memory` tool and the CLI.
#
# Each entry is one paragraph, edited in place. Save writes the whole
# list through /api/memory, which goes through MemoryStore, so every
# save is a REVISION in the memory history (state.db): History shows
# them and Undo rolls one back. That is why editing here is not
# frightening.
#
# The character budget is enforced on the way in rather than discovered
# at the next prompt build — going over it is what makes the engine
# start refusing the agent's own writes.
#
# ALL FILES (2026-09-26). The two resident stores are two of many memory
# files; the rest — reference/, projects/, the dream lessons, the curated
# topics — were not reachable here at all, and on disk only as memory.d,
# which is gone: the database is the store. The third view lists every
# file, opens one in the editor and saves it WITH THE SHA IT WAS OPENED
# AT, so a file the agent rewrote meanwhile is refused (409) rather than
# overwritten. New, Rename, Delete (a revision — History can restore it)
# and a per-file History with diffs complete it.

proc ::voverlay::_memory {w} {
    ::vmanage::window $w [::vi18n::mc {Memory}] 900x640
    # One toolbar per view; _memory_mode packs the one that applies.
    ttk::frame $w.bar.ent
    ttk::frame $w.bar.fil
    ::vmanage::button $w.bar.ent add [::vi18n::mc {Add}] \
        [list ::voverlay::_memory_add $w] [::vi18n::mc {add an entry}]
    ::vmanage::button $w.bar.ent del [::vi18n::mc {Delete}] \
        [list ::voverlay::_memory_delete $w] [::vi18n::mc {delete the selected entry}]
    ::vmanage::button $w.bar.ent save [::vi18n::mc {Save}] \
        [list ::voverlay::_memory_save $w] [::vi18n::mc {write the entries back}]
    ::vmanage::separator $w.bar.ent sep1
    ::vmanage::button $w.bar.ent hist [::vi18n::mc {History…}] \
        [list ::voverlay::_memory_history $w] [::vi18n::mc {revision history}]
    ::vmanage::button $w.bar.ent undo [::vi18n::mc {Undo last}] \
        [list ::voverlay::_memory_rollback $w] [::vi18n::mc {roll back one revision}]
    ::vmanage::button $w.bar.ent refresh [::vi18n::mc {Reload}] \
        [list ::voverlay::_memory_fill $w]

    ::vmanage::button $w.bar.fil new [::vi18n::mc {New…}] \
        [list ::voverlay::_mfile_new $w] [::vi18n::mc {create a memory file}]
    ::vmanage::button $w.bar.fil save [::vi18n::mc {Save}] \
        [list ::voverlay::_mfile_save $w] [::vi18n::mc {save the open file}]
    ::vmanage::button $w.bar.fil ren [::vi18n::mc {Rename…}] \
        [list ::voverlay::_mfile_rename $w] [::vi18n::mc {rename the open file}]
    ::vmanage::button $w.bar.fil del [::vi18n::mc {Delete}] \
        [list ::voverlay::_mfile_delete $w] \
        [::vi18n::mc {delete the open file (History can restore it)}]
    ::vmanage::separator $w.bar.fil sep1
    ::vmanage::button $w.bar.fil hist [::vi18n::mc {History…}] \
        [list ::voverlay::_mfile_history $w] \
        [::vi18n::mc {this file's revisions, their diffs, and restore}]
    ::vmanage::button $w.bar.fil refresh [::vi18n::mc {Reload}] \
        [list ::voverlay::_mfile_reload $w]

    ttk::frame $w.which
    pack $w.which -fill x -padx 8 -pady {4 0}
    ttk::label $w.which.l -text [::vi18n::mc {view}]
    ttk::radiobutton $w.which.mem -text [::vi18n::mc {core notes}] \
        -variable ::voverlay::memory_store -value memory \
        -command [list ::voverlay::_memory_mode $w]
    ttk::radiobutton $w.which.usr -text [::vi18n::mc {user profile}] \
        -variable ::voverlay::memory_store -value user \
        -command [list ::voverlay::_memory_mode $w]
    ttk::radiobutton $w.which.fil -text [::vi18n::mc {all files}] \
        -variable ::voverlay::memory_store -value files \
        -command [list ::voverlay::_memory_mode $w]
    ttk::label $w.which.usage -text "" -foreground $::vt::T(fg_dim)
    pack $w.which.l $w.which.mem $w.which.usr $w.which.fil -side left -padx {0 8}
    pack $w.which.usage -side right -padx 6

    # The entries view (core notes / user profile).
    ttk::panedwindow $w.split -orient horizontal
    set t [::vmanage::tree $w.split list {entry} [list [::vi18n::mc {entry}]] {420}]
    $w.split add $t -weight 2
    catch { ::va11y::name $t.t [::vi18n::mc {memory entries}] }

    ttk::frame $w.split.edit
    ttk::label $w.split.edit.l -text [::vi18n::mc {the selected entry (edit, then Save)}] \
        -foreground $::vt::T(fg_dim)
    text $w.split.edit.t -wrap word -relief flat -padx 8 -pady 6 \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg)
    pack $w.split.edit.l -anchor w -padx 4
    pack $w.split.edit.t -fill both -expand 1
    $w.split add $w.split.edit -weight 3
    catch { ::va11y::name $w.split.edit.t [::vi18n::mc {entry text}] }
    bind $t.t <<TreeviewSelect>> [list ::voverlay::_memory_show $w]

    # The files view.
    ttk::panedwindow $w.files -orient horizontal
    set ft [::vmanage::tree $w.files list {path chars changed} \
        [list [::vi18n::mc {file}] [::vi18n::mc {chars}] [::vi18n::mc {changed}]] {320 70 150}]
    $w.files add $ft -weight 2
    catch { ::va11y::name $ft.t [::vi18n::mc {memory files}] }
    ttk::frame $w.files.edit
    ttk::label $w.files.edit.l -text [::vi18n::mc {select a file, or New… to create one}] \
        -foreground $::vt::T(fg_dim)
    text $w.files.edit.t -wrap word -undo 1 -relief flat -padx 8 -pady 6 \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg)
    pack $w.files.edit.l -anchor w -padx 4
    pack $w.files.edit.t -fill both -expand 1
    $w.files add $w.files.edit -weight 3
    catch { ::va11y::name $w.files.edit.t [::vi18n::mc {file text}] }
    bind $ft.t <<TreeviewSelect>> [list ::voverlay::_mfile_select $w]

    _memory_mode $w
}

# Show the view the selector names. Leaving the files view with unsaved
# edits asks first, and stays put on "no".
proc ::voverlay::_memory_mode {w} {
    variable memory_store
    if {$memory_store eq "files"} {
        pack forget $w.bar.ent $w.split
        pack $w.bar.fil -side left
        pack $w.files -fill both -expand 1 -padx 8 -pady 6
        $w.which.usage configure -text ""
        _mfile_fill $w
        return
    }
    if {[winfo ismapped $w.files] || [winfo manager $w.files] ne ""} {
        if {![_mfile_guard $w]} {
            set memory_store files
            return
        }
    }
    pack forget $w.bar.fil $w.files
    pack $w.bar.ent -side left
    pack $w.split -fill both -expand 1 -padx 8 -pady 6
    _memory_fill $w
}

proc ::voverlay::_memory_tree {w} { return $w.split.list.t }

proc ::voverlay::_memory_fill {w} {
    variable memory_entries
    variable memory_store
    set resp [::vmanage::get $w /api/memory]
    if {$resp eq ""} { return }
    set memory_entries [dict get $resp $memory_store]
    set used [dict get [dict get $resp usage] $memory_store]
    set lim  [dict get [dict get $resp limits] $memory_store]
    ::vi18n::label $w.which.usage {%1$s of %2$s characters} $used $lim
    _memory_paint $w
    ::vmanage::say $w [format [::vi18n::mc {%d entries}] [llength $memory_entries]]
}

proc ::voverlay::_memory_paint {w} {
    variable memory_entries
    set t [_memory_tree $w]
    $t delete [$t children {}]
    set i 0
    foreach e $memory_entries {
        # One line in the list, the whole thing in the editor: entries
        # are paragraphs and a tree row is not the place to read one.
        set first [lindex [split $e "\n"] 0]
        if {[string length $first] > 110} { set first "[string range $first 0 107]…" }
        $t insert {} end -id e$i -values [list $first]
        incr i
    }
    ::vtext::set_text $w.split.edit.t ""
}

proc ::voverlay::_memory_index {w} {
    set sel [lindex [[_memory_tree $w] selection] 0]
    if {$sel eq ""} { return -1 }
    if {![regexp {^e(\d+)$} $sel -> i]} { return -1 }
    return $i
}

proc ::voverlay::_memory_show {w} {
    variable memory_entries
    set i [_memory_index $w]
    if {$i < 0} { return }
    ::vtext::set_text $w.split.edit.t [lindex $memory_entries $i]
}

# Pull whatever is in the editor back into the list before any action
# that would otherwise discard it.
proc ::voverlay::_memory_capture {w} {
    variable memory_entries
    set i [_memory_index $w]
    if {$i < 0} { return }
    set text [string trim [$w.split.edit.t get 1.0 end]]
    set memory_entries [lreplace $memory_entries $i $i $text]
}

proc ::voverlay::_memory_add {w} {
    variable memory_entries
    _memory_capture $w
    lappend memory_entries [::vi18n::mc {new entry — replace this text, then Save}]
    _memory_paint $w
    set t [_memory_tree $w]
    set last e[expr {[llength $memory_entries] - 1}]
    if {[$t exists $last]} {
        $t selection set $last
        $t see $last
    }
    _memory_show $w
    focus $w.split.edit.t
}

proc ::voverlay::_memory_delete {w} {
    variable memory_entries
    set i [_memory_index $w]
    if {$i < 0} {
        ::vmanage::say $w [::vi18n::mc {select an entry first}]
        return
    }
    set memory_entries [lreplace $memory_entries $i $i]
    _memory_paint $w
    ::vmanage::say $w [::vi18n::mc {removed — press Save to write it}] warn
}

proc ::voverlay::_memory_save {w} {
    variable memory_entries
    variable memory_store
    _memory_capture $w
    set clean {}
    foreach e $memory_entries {
        if {[string trim $e] ne ""} { lappend clean [string trim $e] }
    }
    set resp [::vmanage::post $w /api/memory \
        [dict create store $memory_store entries [::vapi::json_strings $clean]]]
    if {$resp eq ""} { return }
    _memory_fill $w
    ::vmanage::say $w [format [::vi18n::mc {saved %1$s entries (%2$s characters) — Undo last reverts it}] \
        [dict get $resp written] [dict get $resp chars]] ok
}

proc ::voverlay::_memory_history {w} {
    set resp [::vmanage::get $w /api/memory/log]
    if {$resp eq ""} { return }
    set d .memory_history
    catch { destroy $d }
    toplevel $d
    wm title $d [::vi18n::mc {Memory history}]
    wm transient $d $w
    ::vplace::centered $d 760 420
    bind $d <Escape> [list destroy $d]
    text $d.t -wrap none -relief flat -padx 8 -pady 6 \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg) -font {monospace 9}
    pack $d.t -fill both -expand 1 -padx 10 -pady 10
    ::vtext::set_text $d.t [join [dict get $resp log] "\n"]
    catch { ::vtext::readonly $d.t }
}

proc ::voverlay::_memory_rollback {w} {
    variable memory_store
    if {![::vmanage::confirm $w [::vi18n::mc {Undo the last memory revision?}] \
        [::vi18n::mc {The store goes back one revision. The rollback is itself a revision, so it can be undone in turn.}]]} {
        return
    }
    set resp [::vmanage::post $w /api/memory/rollback \
        [dict create store $memory_store n 1]]
    if {$resp eq ""} { return }
    _memory_fill $w
    set msg ""
    catch { set msg [dict get $resp message] }
    ::vmanage::say $w $msg ok
}

# ── memory: all files ────────────────────────────────────────────────

proc ::voverlay::_mfile_tree {w} { return $w.files.list.t }

# Unsaved edits to the open file: 1 = fine to throw them away.
proc ::voverlay::_mfile_guard {w} {
    variable mfile_path
    if {$mfile_path eq "" || ![$w.files.edit.t edit modified]} { return 1 }
    return [::vmanage::confirm $w \
        [format [::vi18n::mc {Discard your changes to %s?}] $mfile_path] \
        [::vi18n::mc {They have not been saved.}]]
}

# List every file; re-select `select` (default: the open one) so the
# editor keeps its place across a save or a rename.
proc ::voverlay::_mfile_fill {w {select ""}} {
    variable memory_files
    variable mfile_path
    set resp [::vmanage::get $w /api/memory/files]
    if {$resp eq ""} { return }
    set memory_files [dict get $resp files]
    set t [_mfile_tree $w]
    $t delete [$t children {}]
    set i 0
    set total 0
    foreach f $memory_files {
        $t insert {} end -id f$i -values \
            [list [dict get $f path] [dict get $f chars] [dict get $f at]]
        incr total [dict get $f chars]
        incr i
    }
    ::vmanage::say $w [format [::vi18n::mc {%1$d files, %2$d characters}] \
        [llength $memory_files] $total]
    if {$select eq ""} { set select $mfile_path }
    set i 0
    foreach f $memory_files {
        if {[dict get $f path] eq $select} {
            $t selection set f$i
            $t see f$i
            break
        }
        incr i
    }
}

proc ::voverlay::_mfile_select {w} {
    variable memory_files
    variable mfile_path
    set sel [lindex [[_mfile_tree $w] selection] 0]
    if {![regexp {^f(\d+)$} $sel -> i]} { return }
    set path [dict get [lindex $memory_files $i] path]
    if {$path eq $mfile_path} { return }
    if {![_mfile_guard $w]} {
        # Put the selection back on the file that is still open.
        _mfile_fill $w $mfile_path
        return
    }
    _mfile_open $w $path
}

proc ::voverlay::_mfile_open {w path} {
    variable mfile_path
    variable mfile_sha
    set resp [::vmanage::get $w "/api/memory/file?path=[::vapi::urlencode $path]"]
    if {$resp eq ""} { return }
    set mfile_path $path
    set mfile_sha [dict get $resp sha256]
    ::vtext::set_text $w.files.edit.t [dict get $resp content]
    $w.files.edit.t edit reset
    $w.files.edit.t edit modified 0
    ::vi18n::label $w.files.edit.l {%1$s — revision r%2$s} $path [dict get $resp rev]
}

# Save against the sha the file was opened at. A 409 means the file moved
# on (the agent wrote it): nothing was overwritten, and the text stays in
# the editor so it is not lost either.
proc ::voverlay::_mfile_save {w} {
    variable mfile_path
    variable mfile_sha
    if {$mfile_path eq ""} {
        ::vmanage::say $w [::vi18n::mc {select a file, or New… to create one}] warn
        return
    }
    set text [$w.files.edit.t get 1.0 end-1c]
    if {$mfile_sha eq ""} {
        set base [::vapi::json_raw null]
    } else {
        set base [::vapi::json_string $mfile_sha]
    }
    set body [dict create path [::vapi::json_string $mfile_path] \
        content [::vapi::json_string $text] base_sha $base]
    if {[catch { ::vapi::post_json /api/memory/file $body } resp]} {
        if {[string match "HTTP 409:*" $resp] && $mfile_sha eq ""} {
            ::vmanage::say $w [format [::vi18n::mc {%s already exists — open it from the list}] \
                $mfile_path] warn
        } elseif {[string match "HTTP 409:*" $resp]} {
            ::vmanage::say $w [format [::vi18n::mc {%s changed since you opened it — nothing was overwritten; copy your text out, then Reload}] \
                $mfile_path] warn
        } else {
            ::vmanage::say $w [format [::vi18n::mc {failed: %s}] [::vmanage::_reason $resp]] warn
        }
        return
    }
    set mfile_sha [dict get $resp sha256]
    $w.files.edit.t edit modified 0
    _mfile_fill $w $mfile_path
    ::vmanage::say $w [format [::vi18n::mc {saved %s}] $mfile_path] ok
}

proc ::voverlay::_mfile_new {w} {
    variable mfile_path
    variable mfile_sha
    if {![_mfile_guard $w]} { return }
    set p [string trim [::vmanage::ask $w [::vi18n::mc {New memory file}] \
        [::vi18n::mc {path, e.g. reference/topic.md}] reference/]]
    if {$p eq "" || [string index $p end] eq "/"} { return }
    set mfile_path $p
    set mfile_sha ""
    [_mfile_tree $w] selection set {}
    ::vtext::set_text $w.files.edit.t ""
    $w.files.edit.t edit reset
    $w.files.edit.t edit modified 0
    ::vi18n::label $w.files.edit.l {%s — new: type, then Save} $p
    focus $w.files.edit.t
}

proc ::voverlay::_mfile_rename {w} {
    variable mfile_path
    variable mfile_sha
    if {$mfile_path eq "" || $mfile_sha eq ""} {
        ::vmanage::say $w [::vi18n::mc {open a saved file first}] warn
        return
    }
    if {[$w.files.edit.t edit modified]} {
        ::vmanage::say $w [::vi18n::mc {save or reload the file before renaming it}] warn
        return
    }
    set to [string trim [::vmanage::ask $w [::vi18n::mc {Rename memory file}] \
        [::vi18n::mc {new path}] $mfile_path]]
    if {$to eq "" || $to eq $mfile_path} { return }
    set resp [::vmanage::post $w /api/memory/file/move \
        [dict create from [::vapi::json_string $mfile_path] to [::vapi::json_string $to]]]
    if {$resp eq ""} { return }
    set from $mfile_path
    set mfile_path ""
    _mfile_fill $w $to
    ::vmanage::say $w [format [::vi18n::mc {renamed %1$s to %2$s}] $from $to] ok
}

# Delete is a revision: the file leaves memory, its history stays. The
# path stays OPEN (empty, unsaved) so History… can bring it back.
proc ::voverlay::_mfile_delete {w} {
    variable mfile_path
    variable mfile_sha
    if {$mfile_path eq "" || $mfile_sha eq ""} {
        ::vmanage::say $w [::vi18n::mc {open a saved file first}] warn
        return
    }
    if {![::vmanage::confirm $w [format [::vi18n::mc {Delete %s?}] $mfile_path] \
        [::vi18n::mc {It leaves memory now. Its history keeps every revision, and History… can restore it.}]]} {
        return
    }
    set resp [::vmanage::post $w /api/memory/file/delete \
        [dict create path [::vapi::json_string $mfile_path]]]
    if {$resp eq ""} { return }
    set mfile_sha ""
    ::vtext::set_text $w.files.edit.t ""
    $w.files.edit.t edit modified 0
    ::vi18n::label $w.files.edit.l {%s — deleted; History… can restore it} $mfile_path
    _mfile_fill $w
    ::vmanage::say $w [format [::vi18n::mc {deleted %s}] $mfile_path] ok
}

proc ::voverlay::_mfile_reload {w} {
    variable mfile_path
    if {![_mfile_guard $w]} { return }
    set p $mfile_path
    $w.files.edit.t edit modified 0
    set mfile_path ""
    _mfile_fill $w $p
}

# The open file's revisions, newest first; selecting one shows what it
# changed, and Restore makes it current again (as a new revision).
proc ::voverlay::_mfile_history {w} {
    variable mfile_path
    variable mfile_hist
    if {$mfile_path eq ""} {
        ::vmanage::say $w [::vi18n::mc {select a file, or New… to create one}] warn
        return
    }
    set resp [::vmanage::get $w \
        "/api/memory/log?limit=200&path=[::vapi::urlencode $mfile_path]"]
    if {$resp eq ""} { return }
    set mfile_hist [dict get $resp rows]
    set d .memory_file_history
    catch { destroy $d }
    toplevel $d
    wm title $d [format [::vi18n::mc {History — %s}] $mfile_path]
    wm transient $d $w
    ::vplace::centered $d 860 520
    bind $d <Escape> [list destroy $d]
    ttk::frame $d.bar
    pack $d.bar -fill x -padx 10 -pady {8 0}
    ttk::button $d.bar.restore -text [::vi18n::mc {Restore this revision}] \
        -command [list ::voverlay::_mfile_restore $w $d]
    pack $d.bar.restore -side left
    ttk::panedwindow $d.split -orient vertical
    pack $d.split -fill both -expand 1 -padx 10 -pady 8
    set t [::vmanage::tree $d.split list {rev at source note} \
        [list [::vi18n::mc {revision}] [::vi18n::mc {when}] [::vi18n::mc {by}] [::vi18n::mc {note}]] \
        {80 150 90 320}]
    $d.split add $t -weight 1
    text $d.split.diff -wrap none -relief flat -padx 8 -pady 6 -font {monospace 9} \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg)
    $d.split add $d.split.diff -weight 2
    catch { ::vtext::readonly $d.split.diff }
    set i 0
    foreach r $mfile_hist {
        set note [dict get $r note]
        if {[string is true -strict [dict get $r deleted]]} {
            append note " " [::vi18n::mc {(deleted)}]
        }
        $t.t insert {} end -id h$i -values \
            [list r[dict get $r rev] [dict get $r at] [dict get $r source] $note]
        incr i
    }
    bind $t.t <<TreeviewSelect>> [list ::voverlay::_mfile_hist_show $w $d]
    if {$i > 0} { $t.t selection set h0 }
}

proc ::voverlay::_mfile_hist_rev {d} {
    variable mfile_hist
    set sel [lindex [$d.split.list.t selection] 0]
    if {![regexp {^h(\d+)$} $sel -> i]} { return "" }
    return [dict get [lindex $mfile_hist $i] rev]
}

proc ::voverlay::_mfile_hist_show {w d} {
    variable mfile_path
    set rev [_mfile_hist_rev $d]
    if {$rev eq ""} { return }
    set resp [::vmanage::get $w \
        "/api/memory/diff?path=[::vapi::urlencode $mfile_path]&new=$rev"]
    if {$resp eq ""} { return }
    ::vtext::set_text $d.split.diff [dict get $resp diff]
}

proc ::voverlay::_mfile_restore {w d} {
    variable mfile_path
    set rev [_mfile_hist_rev $d]
    if {$rev eq ""} { return }
    if {![::vmanage::confirm $w [format [::vi18n::mc {Restore revision r%1$s of %2$s?}] $rev $mfile_path] \
        [::vi18n::mc {It becomes the current content, as a new revision — so the restore can be undone too.}]]} {
        return
    }
    set resp [::vmanage::post $w /api/memory/restore \
        [dict create path [::vapi::json_string $mfile_path] rev $rev]]
    if {$resp eq ""} { return }
    catch { destroy $d }
    set p $mfile_path
    $w.files.edit.t edit modified 0
    set mfile_path ""
    _mfile_fill $w $p
    ::vmanage::say $w [format [::vi18n::mc {restored r%1$s of %2$s}] $rev $p] ok
}

# ── hooks ────────────────────────────────────────────────────────────
#
# Hooks had no surface at all. They exist only as an array inside the
# gateway's Tcl interpreter, so the single place you could see one was
# the Dev pane's "gw hooks" button — which sent `dict keys` at a Tcl
# ARRAY and therefore threw on every press.
#
# This lists every registered handler in DISPATCH ORDER (which matters:
# the first non-empty return wins), switches individual handlers off
# without disabling the plugin that registered them, and shows the hooks
# the engine can fire that nothing has claimed yet — that last part is
# what someone writing a plugin actually needs.
#
# A toggle writes twice: the interpreter, so it takes effect on the very
# next fire, and `[hooks] disabled` in config.toml, so it survives the
# reload that re-sources every plugin and re-registers its handlers.

proc ::voverlay::_hooks {w} {
    ::vmanage::window $w [::vi18n::mc {Hooks}] 900x560
    ::vmanage::button $w.bar toggle [::vi18n::mc {Enable / disable}] \
        [list ::voverlay::_hook_toggle $w] [::vi18n::mc {switch the selected handler on or off}]
    ::vmanage::button $w.bar open [::vi18n::mc {Open plugin}] \
        [list ::voverlay::_hook_open $w] [::vi18n::mc {edit the plugin that declares this hook}]
    ::vmanage::separator $w.bar sep1
    ::vmanage::button $w.bar refresh [::vi18n::mc {Refresh}] \
        [list ::voverlay::_hooks_fill $w]
    ttk::checkbutton $w.bar.unclaimed -text [::vi18n::mc {show unclaimed}] \
        -variable ::voverlay::hooks_show_all \
        -command [list ::voverlay::_hooks_fill $w]
    catch { ::va11y::name $w.bar.unclaimed [::vi18n::mc {also list hooks with no handler}] }
    pack $w.bar.unclaimed -side left -padx {6 0}

    set t [::vmanage::tree $w list {hook handler state plugin} \
        [list [::vi18n::mc {hook}] [::vi18n::mc {handler}] [::vi18n::mc {state}] [::vi18n::mc {declared by}]] \
        {220 260 90 200}]
    pack $t -fill both -expand 1 -padx 8 -pady 6
    catch { ::va11y::name $t.t [::vi18n::mc {hook handler list}] }
    bind $t.t <Double-1> [list ::voverlay::_hook_toggle $w]

    ttk::label $w.hint -foreground $::vt::T(fg_dim) -wraplength 860 -justify left \
        -text [::vi18n::mc {Handlers run in the order shown; the first one to return a non-empty value wins. Switching one off skips it without unregistering it, and the choice is kept in config.toml so a plugin reload cannot quietly turn it back on.}]
    pack $w.hint -fill x -padx 8 -pady {0 8}
    _hooks_fill $w
}

proc ::voverlay::_hooks_tree {w} { return $w.list.t }

proc ::voverlay::_hooks_fill {w} {
    variable hooks_data
    variable hooks_show_all
    set t [_hooks_tree $w]
    if {![winfo exists $t]} { return }
    set keep [lindex [$t selection] 0]
    $t delete [$t children {}]
    set hooks_data {}
    set resp [::vmanage::get $w /api/hooks]
    if {$resp eq ""} { return }

    set has_tcl 1
    catch { set has_tcl [dict get $resp has_tcl] }
    if {!$has_tcl} {
        set note [::vi18n::mc {this gateway has no Tcl interpreter}]
        catch { set note [dict get $resp note] }
        ::vmanage::say $w $note warn
        return
    }

    set claimed {}
    set n 0
    foreach h [dict get $resp hooks] {
        set name [dict get $h name]
        lappend claimed $name
        set plugin ""
        set path ""
        catch {
            foreach p [dict get $h provided_by] {
                set plugin [dict get $p plugin]
                set path [dict get $p path]
                break
            }
        }
        foreach handler [dict get $h handlers] {
            set hproc [dict get $handler proc]
            set on [dict get $handler enabled]
            # A SYNTHETIC id, not "$name $hproc". A treeview item id
            # containing a space is stored fine but read back as a LIST,
            # so `selection set` on it looks for two items and fails with
            # "Item post_llm_call not found". The real identity lives in
            # hooks_data.
            set id h$n
            dict set hooks_data $id [dict create hook $name handler $hproc enabled $on path $path]
            $t insert {} end -id $id -values [list $name $hproc \
                [expr {$on ? [::vi18n::mc {on}] : [::vi18n::mc {off}]}] $plugin]
            incr n
        }
    }

    # The hooks nothing has registered for. Off by default — the list is
    # long and mostly noise until you are writing a plugin, which is
    # exactly when the checkbox is worth ticking.
    if {[info exists hooks_show_all] && $hooks_show_all} {
        catch {
            foreach k [dict get $resp known] {
                if {[lsearch -exact $claimed $k] >= 0} { continue }
                $t insert {} end -values [list $k "-" [::vi18n::mc {no handler}] ""]
            }
        }
    }

    if {$keep ne "" && [$t exists $keep]} { $t selection set $keep }
    if {$n == 0} {
        ::vmanage::say $w [::vi18n::mc {no plugin has registered a hook}]
    } else {
        ::vmanage::say $w [format [::vi18n::mc {%d handler(s) registered}] $n]
    }
}

proc ::voverlay::_hook_selected {w} {
    variable hooks_data
    set sel [lindex [[_hooks_tree $w] selection] 0]
    if {$sel eq "" || ![dict exists $hooks_data $sel]} {
        ::vmanage::say $w [::vi18n::mc {select a handler first}]
        return ""
    }
    return [dict get $hooks_data $sel]
}

proc ::voverlay::_hook_toggle {w} {
    set h [_hook_selected $w]
    if {$h eq ""} { return }
    set want [expr {[dict get $h enabled] ? 0 : 1}]
    set resp [::vmanage::post $w /api/hooks/toggle [dict create \
        hook [dict get $h hook] handler [dict get $h handler] \
        enabled [::vapi::json_bool $want]]]
    if {$resp eq ""} { return }
    set verb [expr {$want ? [::vi18n::mc {on}] : [::vi18n::mc {off}]}]
    ::vmanage::effect $w $resp "[dict get $h handler] $verb:"
    _hooks_fill $w
}

proc ::voverlay::_hook_open {w} {
    set h [_hook_selected $w]
    if {$h eq ""} { return }
    set path [dict get $h path]
    if {$path eq ""} {
        ::vmanage::say $w [::vi18n::mc {no plugin declares this hook in its manifest, so there is no file to open}] warn
        return
    }
    ::voverlay::_open_in_dev $path
    ::vmanage::say $w [format [::vi18n::mc {opened %s in the Dev pane}] $path] ok
}

# ── plugins ──────────────────────────────────────────────────────────
#
# This was a disabled text widget listing what `discover()` returned —
# which FILTERS OUT the disabled plugins, so the ●/○ dot it drew could
# never be ○ and a plugin you switched off simply vanished with no row
# left to switch it back on. USER: "There is only placeholder bullshit
# that tells you to edit config.toml manually."
#
# It is a manager now: enable/disable (config.toml + a live reload, so
# the change takes effect without a restart), new/install/remove, open
# the entry script in the Dev editor, and the plugin directory's git
# history — which is the undo for every destructive action here, since
# the runtime commits that tree on every change.

proc ::voverlay::_plugins {w} {
    ::vmanage::window $w [::vi18n::mc {Plugins}] 900x540
    ::vmanage::button $w.bar toggle  [::vi18n::mc {Enable / disable}] \
        [list ::voverlay::_plugin_toggle $w] [::vi18n::mc {switch the selected plugin on or off}]
    ::vmanage::button $w.bar open    [::vi18n::mc {Open in Dev}] \
        [list ::voverlay::_plugin_open $w] [::vi18n::mc {edit the selected plugin's script}]
    ::vmanage::separator $w.bar sep1
    ::vmanage::button $w.bar new     [::vi18n::mc {New…}] \
        [list ::voverlay::_plugin_new $w] [::vi18n::mc {scaffold a new plugin}]
    ::vmanage::button $w.bar install [::vi18n::mc {Install…}] \
        [list ::voverlay::_plugin_install $w] [::vi18n::mc {install a plugin from a git URL}]
    ::vmanage::button $w.bar remove  [::vi18n::mc {Remove}] \
        [list ::voverlay::_plugin_remove $w] [::vi18n::mc {delete the selected plugin}]
    ::vmanage::separator $w.bar sep2
    ::vmanage::button $w.bar reload  [::vi18n::mc {Reload}] \
        [list ::voverlay::_plugin_reload $w] [::vi18n::mc {re-source every plugin}]
    ::vmanage::button $w.bar history [::vi18n::mc {History…}] \
        [list ::voverlay::_plugin_history $w] [::vi18n::mc {plugin directory git history}]
    ::vmanage::button $w.bar refresh [::vi18n::mc {Refresh}] \
        [list ::voverlay::_plugins_fill $w]

    ttk::panedwindow $w.split -orient vertical
    pack $w.split -fill both -expand 1 -padx 8 -pady 6
    set t [::vmanage::tree $w.split list \
        {name state kind hooks entry} \
        [list [::vi18n::mc {plugin}] [::vi18n::mc {state}] [::vi18n::mc {kind}] \
              [::vi18n::mc {hooks}] [::vi18n::mc {entry}]] \
        {200 130 60 220 220}]
    $w.split add $t -weight 3
    catch { ::va11y::name $t.t [::vi18n::mc {plugin list}] }

    ttk::frame $w.split.detail
    text $w.split.detail.t -wrap word -height 6 -relief flat -padx 8 -pady 6 \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg)
    pack $w.split.detail.t -fill both -expand 1
    catch { ::vtext::readonly $w.split.detail.t }
    $w.split add $w.split.detail -weight 1
    $w.split.detail.t tag configure dim -foreground $::vt::T(fg_dim)

    bind $t.t <<TreeviewSelect>> [list ::voverlay::_plugin_detail $w]
    bind $t.t <Double-1> [list ::voverlay::_plugin_toggle $w]
    _plugins_fill $w
}

proc ::voverlay::_plugins_tree {w} { return $w.split.list.t }

proc ::voverlay::_plugins_fill {w} {
    variable plugins_data
    set t [_plugins_tree $w]
    if {![winfo exists $t]} { return }
    set keep [lindex [$t selection] 0]
    $t delete [$t children {}]
    set resp [::vmanage::get $w /api/plugins]
    if {$resp eq ""} { return }
    set plugins_data {}
    set allowlist 0
    catch { set allowlist [dict get $resp allowlist_active] }
    foreach p [dict get $resp plugins] {
        set name [dict get $p name]
        dict set plugins_data $name $p
        # WHY it is off matters: "you switched this off" and "an
        # allowlist is in force and this is not on it" are different
        # problems with different fixes.
        if {[dict get $p enabled]} {
            set state [::vi18n::mc {on}]
        } elseif {[dict get $p explicitly_disabled]} {
            set state [::vi18n::mc {off}]
        } else {
            set state [::vi18n::mc {not allowlisted}]
        }
        $t insert {} end -id $name -values [list $name $state [dict get $p kind] \
            [join [dict get $p hooks] ", "] [dict get $p entry]]
    }
    if {$keep ne "" && [$t exists $keep]} {
        $t selection set $keep
    }
    set n [llength [dict get $resp plugins]]
    if {$n == 0} {
        ::vmanage::say $w [::vi18n::mc {no plugins yet — New… scaffolds one}]
    } elseif {$allowlist} {
        ::vmanage::say $w [::vi18n::mc {an allowlist is in force: a plugin not on it stays off}] warn
    } else {
        ::vmanage::say $w [format [::vi18n::mc {%d plugin(s)}] $n]
    }
    _plugin_detail $w
}

proc ::voverlay::_plugin_selected {w} {
    variable plugins_data
    set vals [::vmanage::selected $w [_plugins_tree $w] [::vi18n::mc {a plugin}]]
    if {$vals eq ""} { return "" }
    set name [lindex $vals 0]
    if {![dict exists $plugins_data $name]} { return "" }
    return [dict get $plugins_data $name]
}

proc ::voverlay::_plugin_detail {w} {
    variable plugins_data
    set t $w.split.detail.t
    if {![winfo exists $t]} { return }
    ::vtext::set_text $t ""
    set sel [lindex [[_plugins_tree $w] selection] 0]
    if {$sel eq "" || ![dict exists $plugins_data $sel]} { return }
    set p [dict get $plugins_data $sel]
    set lines {}
    lappend lines [dict get $p description]
    lappend lines ""
    lappend lines "[::vi18n::mc {script}]: [dict get $p path]"
    if {[llength [dict get $p hooks]]} {
        lappend lines "[::vi18n::mc {declares hooks}]: [join [dict get $p hooks] ", "]"
    }
    ::vtext::set_text $t [join $lines "\n"]
}

# Enable ⇄ disable. The API writes config.toml and reloads the plugin
# interpreter, so the change is live — the reply's `note` says so.
proc ::voverlay::_plugin_toggle {w} {
    set p [_plugin_selected $w]
    if {$p eq ""} { return }
    set name [dict get $p name]
    set action [expr {[dict get $p enabled] ? "disable" : "enable"}]
    set resp [::vmanage::post $w "/api/plugins/$name/$action" {}]
    if {$resp eq ""} { return }
    ::vmanage::effect $w $resp "$name $action:"
    _plugins_fill $w
}

proc ::voverlay::_plugin_open {w} {
    set p [_plugin_selected $w]
    if {$p eq ""} { return }
    ::voverlay::_open_in_dev [dict get $p path]
    ::vmanage::say $w [format [::vi18n::mc {opened %s in the Dev pane}] [dict get $p name]] ok
}

proc ::voverlay::_plugin_new {w} {
    set name [::vmanage::ask $w [::vi18n::mc {New plugin}] [::vi18n::mc {name}] "" \
        [::vi18n::mc {Letters, digits, - and _. Creates plugin.toml and a plugin.tcl that registers a post_llm_call hook, then opens it in the Dev pane.}]]
    if {[string trim $name] eq ""} { return }
    set resp [::vmanage::post $w /api/plugins/new [dict create name $name description ""]]
    if {$resp eq ""} { return }
    _plugins_fill $w
    if {[catch { ::voverlay::_open_in_dev [dict get $resp path] } od_err]} {
        puts stderr "veles-desktop: open-in-dev failed for new plugin $name: $od_err"
    }
    ::vmanage::say $w [format [::vi18n::mc {created %s — editing it now}] $name] ok
}

proc ::voverlay::_plugin_install {w} {
    set url [::vmanage::ask $w [::vi18n::mc {Install plugin}] [::vi18n::mc {git URL}] "" \
        [format [::vi18n::mc {The repository root must contain plugin.toml. It is cloned into %s under the manifest's name.}] [::vpaths::plugins_dir]]]
    if {[string trim $url] eq ""} { return }
    ::vmanage::say $w [::vi18n::mc {cloning…}]
    set resp [::vmanage::post $w /api/plugins/install [dict create url $url]]
    if {$resp eq ""} { return }
    _plugins_fill $w
    ::vmanage::say $w [format [::vi18n::mc {installed to %s}] [dict get $resp installed]] ok
}

proc ::voverlay::_plugin_remove {w} {
    set p [_plugin_selected $w]
    if {$p eq ""} { return }
    set name [dict get $p name]
    if {![::vmanage::confirm $w [format [::vi18n::mc {Remove plugin %s?}] $name] \
        [::vi18n::mc {The directory is deleted. The plugin folder is a git repository and a snapshot is committed first, so History… can bring it back.}]]} {
        return
    }
    set resp [::vmanage::del $w "/api/plugins/$name"]
    if {$resp eq ""} { return }
    _plugins_fill $w
    ::vmanage::say $w [format [::vi18n::mc {removed %s — recoverable from History…}] $name] ok
}

proc ::voverlay::_plugin_reload {w} {
    set resp [::vmanage::post $w /api/tcl/reload {}]
    if {$resp eq ""} { return }
    set msg ""
    catch { set msg [dict get $resp result] }
    ::vmanage::say $w [expr {$msg eq "" ? [::vi18n::mc {reloaded}] : $msg}] ok
    _plugins_fill $w
}

# The plugin directory's git log, and a rollback for the selected commit.
proc ::voverlay::_plugin_history {w} {
    set d .plugin_history
    catch { destroy $d }
    set resp [::vmanage::get $w /api/plugins/history]
    if {$resp eq ""} { return }
    toplevel $d
    wm title $d [::vi18n::mc {Plugin history}]
    wm transient $d $w
    ::vplace::centered $d 760 420
    bind $d <Escape> [list destroy $d]
    ttk::label $d.hint -foreground $::vt::T(fg_dim) -wraplength 720 -justify left
    ::vi18n::label $d.hint {Every change to %s is committed. Restoring puts the whole directory back to that commit — and snapshots the current state first, so it is not a one-way door.} [::vpaths::plugins_dir]
    pack $d.hint -fill x -padx 10 -pady {10 4}
    set t [::vmanage::tree $d list {commit when subject} \
        [list [::vi18n::mc {commit}] [::vi18n::mc {when}] [::vi18n::mc {what}]] {90 200 400}]
    pack $t -fill both -expand 1 -padx 10 -pady 4
    foreach c [dict get $resp commits] {
        $t.t insert {} end -values [list [string range [dict get $c commit] 0 8] \
            [dict get $c when] [dict get $c subject]]
    }
    if {![llength [dict get $resp commits]]} {
        ttk::label $d.empty -text [::vi18n::mc {no history yet}] -foreground $::vt::T(fg_dim)
        pack $d.empty -padx 10 -pady 4
    }
    ttk::frame $d.b
    pack $d.b -fill x -padx 10 -pady {4 10}
    ttk::button $d.b.restore -text [::vi18n::mc {Restore this commit}] \
        -command [list ::voverlay::_plugin_rollback $w $d $t.t]
    ttk::button $d.b.close -text [::vi18n::mc {Close}] -command [list destroy $d]
    pack $d.b.restore $d.b.close -side left -padx {0 6}
    catch { ::vcoach::attach $d.b.restore }
}

proc ::voverlay::_plugin_rollback {w d tree} {
    set vals [::vmanage::selected $w $tree [::vi18n::mc {a commit}]]
    if {$vals eq ""} { return }
    set commit [lindex $vals 0]
    if {![::vmanage::confirm $d [format [::vi18n::mc {Restore plugins to %s?}] $commit] \
        [::vi18n::mc {Every plugin file goes back to that commit. The current state is committed first.}]]} {
        return
    }
    set resp [::vmanage::post $w /api/plugins/rollback [dict create commit $commit]]
    if {$resp eq ""} { return }
    destroy $d
    _plugins_fill $w
    ::vmanage::say $w [format [::vi18n::mc {rolled back to %s}] $commit] ok
}

# Open a file in the Dev pane's editor — the app already has a
# multi-buffer editor with an LSP; a manager that made you find the file
# yourself would be sending you to a second tool.
#
# ::vdev::buf_new CREATES A BUFFER AND RECORDS THE PATH; it never reads
# the file. Opening a plugin this way gave you an empty editor titled
# plugin.tcl, which looks exactly like a plugin whose script is empty.
# ::vdev::editor_open is the real thing — it fetches the whole file
# through /api/fs/file, fills the buffer and starts the LSP on it, and
# reuses an existing tab for a file already open rather than stacking a
# second copy that would race it on save.
#
# `after idle`: ::vpanes::show builds the Dev pane lazily, and
# editor_open needs a buffer to open into.
proc ::voverlay::_open_in_dev {path} {
    catch { ::vpanes::show dev }
    after idle [list ::voverlay::_open_in_dev_now $path]
}

proc ::voverlay::_open_in_dev_now {path} {
    if {[catch {
        set ::vdev::current_file $path
        ::vdev::editor_open
    } e]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {could not open %1\$s: %2\$s}] $path $e] }
    }
}

# ── usage ────────────────────────────────────────────────────────────
# ── usage ────────────────────────────────────────────────────────────
#
# This overlay HAS NEVER OPENED. It read `messages`, and the endpoint
# answers `calls` — renamed on the Rust side when the status bar was
# found reporting the call count as the prompt total (see `usage` in
# desktop_api.rs, which says so). `dict get` on a missing key throws, so
# ::voverlay::open usage left a half-built toplevel and an error every
# single time, on every gateway.
#
# Read through a helper that tolerates an absent key, and show the
# per-session figures the endpoint already returns.
# A count the payload does not carry reads as an em dash, never as a
# crash — which is precisely what `dict get` on a missing key did to
# this overlay for its whole life.
proc ::voverlay::_uval {d k} {
    if {[dict exists $d $k]} { return [dict get $d $k] }
    return "\u2014"
}

proc ::voverlay::detach_async {} {
    variable usage_fill
    variable usage_generation
    variable usage_pending
    set usage_fill 0
    incr usage_generation
    array unset usage_pending
    catch { destroy .ov_usage }
}

proc ::voverlay::_usage {w} {
    ::vmanage::window $w [::vi18n::mc {Usage & Quota}] 560x560 {380 300}
    ::vmanage::button $w.bar refresh [::vi18n::mc {Refresh}] \
        [list ::voverlay::_usage_fill $w 1]
    # Closing invalidates both callbacks and releases the guard. Merely
    # testing winfo exists would accept the old answer into a reopened
    # window at the same Tk path.
    bind $w <Destroy> [list ::voverlay::_usage_closed %W $w]
    _usage_scroll $w
    _usage_fill $w
}

proc ::voverlay::_usage_closed {which w} {
    variable usage_generation
    variable usage_fill
    variable usage_pending
    if {$which ne $w} { return }
    incr usage_generation
    set usage_fill 0
    catch { array unset usage_pending }
}

# ── the Usage window's scrolling (USER 2026-09-23) ────────────────────
#
# "I need to be able to scroll the window because there's so much stuff
# in there now it barely fits." The window grids the ledger split, four
# plans' pools and the reading's age into one frame; past the window's
# height Tk simply clipped the bottom — no scrollbar, no wheel, no
# keyboard. That is the Mechanisms overlay's exact history one window
# over (see ::vmanage::scrollframe for the write-up), and the fix is
# the same Tk idiom: a canvas, because a canvas is the only widget with
# a scrollregion, with the content frame as its window item.
#
# The content frame keeps the name `$w.body` — every row-builder in
# this file grids into `$w.body.<name>` — so the scroll wraps the
# existing fill code instead of renaming forty widget paths, and a
# suite that drives `_quota_fill` against a hand-built `$w.body` keeps
# working unchanged.
proc ::voverlay::_usage_scroll {w} {
    ttk::frame $w.scroll
    # `-takefocus 1`: the key bindings below live on the TOPLEVEL, so
    # they fire whenever the focus is anywhere inside the window — but
    # when the window opens nothing inside it has the focus yet, and a
    # canvas is not in the default Tab traversal, so without this the
    # focus -force below would land nowhere and every key would go to
    # whatever toplevel still held it.
    canvas $w.scroll.c -highlightthickness 0 -borderwidth 0 -takefocus 1 \
        -background $::vt::T(bg_panel) -yscrollincrement 14
    ttk::scrollbar $w.scroll.sb -orient vertical \
        -command [list $w.scroll.c yview]
    $w.scroll.c configure -yscrollcommand [list $w.scroll.sb set]
    grid $w.scroll.c  -row 0 -column 0 -sticky nsew
    grid $w.scroll.sb -row 0 -column 1 -sticky ns
    grid rowconfigure    $w.scroll 0 -weight 1
    grid columnconfigure $w.scroll 0 -weight 1
    pack $w.scroll -fill both -expand 1 -padx 8 -pady 8

    ttk::frame $w.body
    set win [$w.scroll.c create window 0 0 -anchor nw -window $w.body]
    # Two bindings, two jobs: the content changing height resets the
    # scrollregion, and the canvas changing width re-widths the content
    # so a `-sticky ew` grid lays out against the real window width.
    bind $w.body     <Configure> [list ::voverlay::_uscr_settle $w]
    bind $w.scroll.c <Configure> [list ::voverlay::_uscr_width $w $win]

    # The wheel, bound ON THE TOPLEVEL: wheel events follow bindtags, so
    # one binding here covers the canvas, every row and the scrollbar —
    # and unlike per-widget bindings it survives the fill re-creating
    # the rows on every Refresh.
    foreach ev {<Button-4> <Button-5> <MouseWheel>} {
        bind $w $ev [list ::voverlay::_uscr_wheel $w %D %b]
    }
    # Keyboard, same surface — a manager window that only answers the
    # mouse is the thing this app's keymap doctrine exists to avoid.
    bind $w <Up>    [list ::voverlay::_uscr_key $w -2]
    bind $w <Down>  [list ::voverlay::_uscr_key $w  2]
    bind $w <Prior> [list ::voverlay::_uscr_key $w page-]
    bind $w <Next>  [list ::voverlay::_uscr_key $w page+]
    bind $w <Home>  [list ::voverlay::_uscr_key $w top]
    bind $w <End>   [list ::voverlay::_uscr_key $w bottom]
    catch { focus $w.scroll.c }
    return $w.body
}

proc ::voverlay::_uscr_width {w win} {
    catch { $w.scroll.c itemconfigure $win -width [winfo width $w.scroll.c] }
    ::voverlay::_uscr_settle $w
}

# The scrollregion is the content's own bbox. Called from the content
# frame's <Configure> and at the tail of every fill, because a fill
# that ends without a geometry change still needs the region to cover
# what it just painted.
#
# NOT a position keeper: measured 2026-09-23 under Xvfb, Tk preserves
# the canvas's yview across a destroy-and-rebuild of the window item's
# contents (the fraction is recomputed against the new scrollregion on
# the next `yview`), so a set of remember/restore procs that read the
# view before the fill and wrote it back after were DEAD CODE — the
# mutation harness ran them emptied-out and every scroll assertion
# stayed green. They are deleted rather than kept as decoration; the
# pin for "a Refresh keeps the reader's place" below is the honest one
# and it holds without them.
proc ::voverlay::_uscr_settle {w} {
    set c $w.scroll.c
    if {![winfo exists $c]} { return }
    catch { $c configure -scrollregion [$c bbox all] }
}

proc ::voverlay::_uscr_wheel {w D b} {
    set c $w.scroll.c
    if {![winfo exists $c]} { return }
    # X11 delivers Button-4/5 with no %D; everything else sends %D (the
    # same spelling ::vmanage::_sf_wheel uses).
    if {$b eq "4"} { catch { $c yview scroll -3 units } return }
    if {$b eq "5"} { catch { $c yview scroll  3 units } return }
    if {$D eq "" || $D eq "??"} { return }
    catch { $c yview scroll [expr {$D > 0 ? -3 : 3}] units }
}

# One unit is the canvas's yscrollincrement (14px); a page is the
# canvas's own page, which is what `-1 pages` asks for.
proc ::voverlay::_uscr_key {w what} {
    set c $w.scroll.c
    if {![winfo exists $c]} { return }
    switch -- $what {
        top    { catch { $c yview moveto 0.0 } }
        bottom { catch { $c yview moveto 1.0 } }
        page-  { catch { $c yview scroll -1 pages } }
        page+  { catch { $c yview scroll  1 pages } }
        default { catch { $c yview scroll $what units } }
    }
}

# One fill at a time. The window opens first and BOTH reads use the
# asynchronous HTTP helper: a nested vwait inside a click is still the
# click's stack, and admits unrelated UI commands into half-done work.
proc ::voverlay::_usage_fill {w {force 0}} {
    variable usage_fill
    variable usage_generation
    variable usage_pending
    variable usage_again
    if {$usage_fill} {
        # Coalesce a Refresh or a session switch while this pair is out.
        # The later pair starts after this one settles; it never waits
        # inside a click or replaces an in-flight request's state.
        set usage_again 1
        ::vmanage::say $w [::vi18n::mc {already refreshing…}]
        return
    }
    if {![winfo exists $w.body]} { return }
    set usage_fill 1
    set usage_again 0
    catch { array unset usage_pending }
    set generation [incr usage_generation]
    set sid ""
    catch { set sid $::vchat::session }
    set usage_pending($generation) [dict create sid $sid usage {} quota {}]
    set path /api/usage
    if {$sid ne ""} { append path "?session=$sid" }
    foreach {kind url} [list usage $path quota [_quota_path $force]] {
        if {[catch {
            ::vapi::get_json_async $url \
                [list ::voverlay::_usage_answer $w $generation $kind]
        } err]} {
            # An immediate transport error still finishes from the event
            # loop. Invalidate the other request if it was already sent.
            set generation [incr usage_generation]
            set usage_pending($generation) \
                [dict create sid $sid usage [list 0 $err] quota [list 0 $err]]
            after idle [list ::voverlay::_usage_answer \
                $w $generation finish 0 $err]
            return
        }
    }
}

# An answer belongs only to the exact window generation that requested it.
# Both answers are collected before any row is destroyed or rebuilt, so a
# close/reopen or duplicate click cannot interleave two fills.
proc ::voverlay::_usage_answer {w generation kind ok value} {
    variable usage_fill
    variable usage_generation
    variable usage_pending
    variable usage_again
    if {$generation != $usage_generation} { return }
    if {![winfo exists $w.body]} {
        set usage_fill 0
        catch { unset usage_pending($generation) }
        return
    }
    if {![info exists usage_pending($generation)]} { return }
    set pending $usage_pending($generation)
    dict set pending $kind [list $ok $value]
    set usage_pending($generation) $pending
    if {[dict get $pending usage] eq "" || [dict get $pending quota] eq ""} {
        return
    }
    unset usage_pending($generation)
    set usage_fill 0
    set sid [dict get $pending sid]
    set current ""
    catch { set current $::vchat::session }
    if {$current ne $sid} {
        # A request can outlive the session that created it. Its figures
        # must not appear in the new session; get a fresh pair instead.
        _usage_fill $w
        return
    }
    lassign [dict get $pending usage] usage_ok u
    lassign [dict get $pending quota] quota_ok q
    set rc [catch {
        # A failed read is not an empty ledger. Keep the last good rows;
        # the first failure still clears a window that has none to keep.
        # This is an if/else, NOT `return` in catch: Tcl propagates that
        # as catch code 2, which must not masquerade as a failed fill or
        # skip a refresh queued while the requests were outstanding.
        if {!$usage_ok} {
            if {![winfo exists $w.body.h1]} {
                foreach c [winfo children $w.body] { destroy $c }
            }
            ::vmanage::say $w [format [::vi18n::mc {unreachable: %s}] \
                [::vmanage::_reason $u]] warn
        } else {
            foreach c [winfo children $w.body] { destroy $c }
            set row [_usage_paint $w $u]
            _quota_paint $w $row $quota_ok $q
            grid columnconfigure $w.body 1 -weight 1
            _uscr_settle $w
        }
    } err opts]
    if {$rc} {
        puts stderr "veles-desktop: usage fill failed: $err"
        if {[dict exists $opts -errorinfo]} {
            puts stderr [dict get $opts -errorinfo]
        }
    }
    if {$usage_again} {
        set usage_again 0
        _usage_fill $w
    }
}

# The fetched local ledger. No HTTP or event-loop wait belongs here.
proc ::voverlay::_usage_paint {w u} {
    set row 0
    # THE PLAN SPLIT FIRST. It is the answer to "what did we spend",
    # split by the pools that actually exist; the flattened totals
    # below it are the same numbers added up, kept because they are
    # what the status bar abbreviates.
    set by_plan {}
    catch { set by_plan [dict get $u by_plan] }
    set row [::voverlay::_uplan_fill $w $row $by_plan allplan \
        [::vi18n::mc {by plan — all sessions}]]
    # THIS SESSION's split: which pools has the conversation on screen
    # actually drawn down. Shown only when it has spent something —
    # an empty block would read as "no usage" rather than "not yet".
    set sess_plan {}
    catch { set sess_plan [dict get $u session_by_plan] }
    set row [::voverlay::_uplan_fill $w $row $sess_plan sessplan \
        [::vi18n::mc {by plan — this session}]]

    ttk::label $w.body.h1 -text [::vi18n::mc {all sessions}] -font {TkDefaultFont 10 bold}
    grid $w.body.h1 -row $row -column 0 -columnspan 2 -sticky w -pady {8 2}
    incr row
    foreach {key label} {
        prompt_tokens     {prompt tokens}
        completion_tokens {completion tokens}
        calls             {API calls}
    } {
        ttk::label $w.body.l$row -text [::vi18n::mc $label]
        ttk::label $w.body.v$row -text [_uval $u $key] -foreground $::vt::T(fg_dim)
        grid $w.body.l$row -row $row -column 0 -sticky w -padx {8 0} -pady 1
        grid $w.body.v$row -row $row -column 1 -sticky e -padx {12 8} -pady 1
        incr row
    }

    set sess [::vapi::field $u session]
    if {$sess ne "" && ![catch { dict size $sess }]} {
        ttk::label $w.body.h2 -text [::vi18n::mc {this session}] -font {TkDefaultFont 10 bold}
        grid $w.body.h2 -row $row -column 0 -columnspan 2 -sticky w -pady {8 2}
        incr row
        foreach {key label} {
            prompt_tokens     {prompt tokens}
            completion_tokens {completion tokens}
            calls             {API calls}
        } {
            ttk::label $w.body.l$row -text [::vi18n::mc $label]
            ttk::label $w.body.v$row -text [_uval $sess $key] -foreground $::vt::T(fg_dim)
            grid $w.body.l$row -row $row -column 0 -sticky w -padx {8 0} -pady 1
            grid $w.body.v$row -row $row -column 1 -sticky e -padx {12 8} -pady 1
            incr row
        }
    }
    return $row
}

# ── usage / plan, the two scopes ──────────────────────────────────────
#
# `/api/usage` answers what WE spent by our own counting; `/api/quota`
# answers what the UPSTREAM plan says is used. They are different
# sources with different failure modes, and the widget shows both —
# that adjacency is the point (see _quota_fill's header on why they
# share one window).
#
# THE PLAN SPLIT (USER 2026-09-20: "the existing usage is confusing, it
# conflates the personal and team alibaba tokens"). A single total over
# every provider cannot be acted on: the personal plan, the team plan,
# z.ai and Kimi are four disjoint pools behind four credentials. The
# engine serves `by_plan` rows and this widget renders them — the
# mapping is not re-derived here, because a second implementation of
# "which pool is this" is exactly how the desktop and the engine would
# silently disagree.

# 111650.3 -> "111,650"; 1870018956 -> "1.9G". Tokens need the coarse
# form — a ten-digit count in a 520px window is noise — while credits
# are grouped exactly, as the text surfaces do.
proc ::voverlay::_utok {n} {
    if {$n eq "" || $n eq "null" || ![string is integer -strict $n]} { return "—" }
    if {$n >= 1000000000} { return [format %.1fG [expr {$n / 1000000000.0}]] }
    if {$n >= 1000000}    { return [format %.1fM [expr {$n / 1000000.0}]] }
    if {$n >= 1000}       { return [format %.1fk [expr {$n / 1000.0}]] }
    return $n
}

# One `by_plan` row as a line: "team   1.9G tok · 3,503 calls".
proc ::voverlay::_uplan_line {r} {
    set name ""
    catch { set name [dict get $r plan] }
    if {$name eq "" || $name eq "null"} {
        # No plan claims these providers. Naming them is the point —
        # "other" would hide the fact that something is configured
        # which no quota reader covers.
        set provs {}
        catch { set provs [dict get $r providers] }
        set name [join $provs ", "]
    }
    set tot ""
    catch { set tot [dict get $r total_tokens] }
    set calls ""
    catch { set calls [dict get $r calls] }
    return "$name — [::voverlay::_utok $tot] tok · [::voverlay::_utok $calls] calls"
}

# The plan split block for one scope. Takes the ROWS (already fetched by
# the caller) and the ALREADY-TRANSLATED heading — the mc call happens
# at the call site with a LITERAL key, because uitest scans the sources
# for literal mc keys and a computed one is invisible to it (which
# means a translation for it could never be checked, and a missing one
# never noticed). Returns the next grid row.
proc ::voverlay::_uplan_fill {w row rows tag heading} {
    if {$rows eq "" || $rows eq "null" || [llength $rows] == 0} { return $row }
    # `$tag` distinguishes the two headings' widget NAMES; it is not a
    # container. `$w.body.$tag.h` would name a label `h` INSIDE a widget
    # called `$tag`, which does not exist — "bad window path name".
    ttk::label $w.body.${tag}_h -text $heading -font {TkDefaultFont 10 bold}
    grid $w.body.${tag}_h -row $row -column 0 -columnspan 2 -sticky w -pady {8 2}
    incr row
    foreach r $rows {
        ttk::label $w.body.${tag}_l$row -text [::voverlay::_uplan_line $r] \
            -foreground $::vt::T(fg_dim)
        grid $w.body.${tag}_l$row -row $row -column 0 -columnspan 2 -sticky w \
            -padx {8 0} -pady 1
        incr row
        # Which provider entries fed the row. `ali-responses` sitting on
        # the team pool is exactly the kind of claim a reader should be
        # able to check rather than take on faith — and it is the one
        # the old provider-substring mapping got wrong.
        set provs {}
        catch { set provs [dict get $r providers] }
        if {[llength $provs] > 1} {
            ttk::label $w.body.${tag}_p$row -text [join $provs ", "] \
                -foreground $::vt::T(fg_dim) -font {TkDefaultFont 8}
            grid $w.body.${tag}_p$row -row $row -column 0 -columnspan 2 -sticky w \
                -padx {24 0}
            incr row
        }
    }
    return $row
}
# Every figure the engine served as `null` is a figure the API did not
# report, and it renders as "not reported" here for the same reason
# `format_quota_report` does it on the text surfaces: an invented 0% on
# a quota display is the exact lie this feature exists to avoid.

# A plan's HEADER row: its name, whether it is this session's, and the
# tightest fraction the engine resolved for that pool.
#
# THE POINT (USER 2026-09-20: "please clearly label the plans
# separately"). Every row below this used to arrive bare — "weekly",
# "credits", "5-hour" — and two plans both have a "weekly" and two both
# have a "5-hour". A column of unlabelled window names is exactly the
# conflation being complained about: nothing on screen said which bill
# a figure belonged to. Every plan now opens with its own named header,
# so no figure is ambiguous about whose it is.
#
# `frac` is the fraction the ENGINE resolved (`""` = not reported, and
# deliberately NOT 0).
proc ::voverlay::_qplan_head {w row name {active 0} {frac ""}} {
    # `name` is a plan IDENTIFIER, not prose — deliberately no `mc` on
    # it. It is computed (uitest scans the sources for LITERAL mc keys
    # and is blind to computed ones, so a translation could never be
    # checked), and these are proper nouns that mean the same thing in
    # every locale. The prose around them is translated; the identifier
    # passes through.
    set txt $name
    if {$active} { append txt "  ← [::vi18n::mc {active}]" }
    if {$frac ne ""} {
        append txt "  ·  [format %.1f%% [expr {$frac * 100.0}]] [::vi18n::mc {used}]"
    } else {
        append txt "  ·  [::vi18n::mc {not reported}]"
    }
    ttk::label $w.body.hp$row -text $txt -font {TkDefaultFont 9 bold} \
        -foreground [expr {$active ? $::vt::T(gold) : $::vt::T(fg_dim)}]
    grid $w.body.hp$row -row $row -column 0 -columnspan 2 -sticky w -pady {6 1}
    return [expr {$row + 1}]
}

# The fraction the engine resolved for one plan, or "" when the plan
# has no reading. Reads the engine's own `plan_fractions` — the mapping
# is never re-derived here from a provider name, because a second
# implementation of "which pool is this" is precisely how this desktop
# and the engine came to disagree (see QuotaQuery.provider on the Rust
# side).
proc ::voverlay::_qplan_frac {q name} {
    set pf ""
    catch { set pf [dict get $q plan_fractions] }
    if {$pf eq "" || $pf eq "null"} { return "" }
    return [::voverlay::_qnum $pf $name]
}

# ── the upstream plan's quota, beside the local ledger ───────────────
#
# Deliberately the SAME overlay as Usage rather than a window of its
# own. The two answer the same question from opposite ends — "what did
# we spend by our own count" and "what does the account say is left" —
# and the adjacency is what makes the distinction legible. It also
# costs no new keybinding, palette row or overlay-switch arm.

# A float from a dict key, or "" when the engine said null / omitted it.
proc ::voverlay::_qnum {d k} {
    if {![dict exists $d $k]} { return "" }
    set v [dict get $d $k]
    if {$v eq "" || $v eq "null"} { return "" }
    if {![string is double -strict $v]} { return "" }
    return $v
}

# A fraction (0..1) as a percentage, or the honest absence.
proc ::voverlay::_qpct {d k} {
    set v [::voverlay::_qnum $d $k]
    if {$v eq ""} { return [::vi18n::mc {not reported}] }
    return [format %.1f%% [expr {$v * 100.0}]]
}

# 111650.3 -> "111,650". Same treatment the text report gives credits:
# nobody reads a quota to the hundredth of a credit.
proc ::voverlay::_qgroup {v} {
    if {$v eq ""} { return [::vi18n::mc {not reported}] }
    return [::vstatus::_group [expr {round($v)}]]
}

# "2026-10-03T09:00:00+00:00" -> "2026-10-03 09:00 UTC".
proc ::voverlay::_qwhen {d k} {
    if {![dict exists $d $k]} { return "" }
    set v [dict get $d $k]
    if {$v eq "" || $v eq "null"} { return "" }
    if {[catch { clock scan [string range $v 0 18] -format {%Y-%m-%dT%H:%M:%S} -timezone :UTC } t]} {
        return $v
    }
    return [clock format $t -format {%Y-%m-%d %H:%M UTC} -timezone :UTC]
}

proc ::voverlay::_qrow {w row label value} {
    ttk::label $w.body.l$row -text $label
    ttk::label $w.body.v$row -text $value -foreground $::vt::T(fg_dim)
    grid $w.body.l$row -row $row -column 0 -sticky w -padx {8 0} -pady 1
    grid $w.body.v$row -row $row -column 1 -sticky e -padx {12 8} -pady 1
    return [expr {$row + 1}]
}

# Paint the quota half. Returns the next free grid row.
#
# CACHE-ONLY unless `force`: opening the overlay must never cost a round
# trip to Singapore, which is why `/api/quota` answers from the engine's
# cache and only the Refresh button passes `?refresh=1`.
proc ::voverlay::_quota_path {{force 0}} {
    set path /api/quota
    set prov [::voverlay::_qroute_prov]
    if {$prov ne ""} { append path "?provider=[::vapi::urlencode $prov]" }
    if {$force} {
        append path [expr {$prov ne "" ? "&" : "?"}] "refresh=1"
    }
    return $path
}

# Standalone quota-view compatibility seam for tests and embedders.
# The real Usage window fetches asynchronously in _usage_fill instead.
proc ::voverlay::_quota_fill {w row {force 0}} {
    if {[catch { ::vapi::get_json [_quota_path $force] } q]} {
        _quota_paint $w $row 0 $q
    } else {
        _quota_paint $w $row 1 $q
    }
}

# Paint an already-fetched quota answer; failed reads retain their own
# error instead of suppressing a valid local-ledger answer.
proc ::voverlay::_quota_paint {w row ok q} {
    set prov [::voverlay::_qroute_prov]
    if {!$ok} {
        set row [_qrow $w $row [::vi18n::mc {plan quota}] [::vi18n::mc {unavailable}]]
        ::vmanage::say $w "quota: $q" warn
        return $row
    }

    ttk::label $w.body.hq -text [::vi18n::mc {upstream plan quota}] \
        -font {TkDefaultFont 10 bold}
    grid $w.body.hq -row $row -column 0 -columnspan 2 -sticky w -pady {8 2}
    incr row
    # WHICH PLAN THIS SESSION IS ON, said once at the top. The sections
    # below are all labelled, but the answer to "what am I spending" is
    # one line and burying it inside four labelled blocks is how it got
    # missed in the first place.
    set active ""
    catch { set active [dict get $q active_plan] }
    if {$active ne "" && $active ne "null"} {
        # The plan name is an identifier (see _qplan_head); only the
        # frame around it is translated.
        set row [_qrow $w $row [::vi18n::mc {this session}] \
            "[::vi18n::mc {on the}] $active [::vi18n::mc {plan}]"]
    } elseif {$prov ne ""} {
        set row [_qrow $w $row [::vi18n::mc {this session}] \
            "[::vi18n::mc {on no plan}] ($prov)"]
    }

    if {[dict exists $q enabled] && ![string is true -strict [dict get $q enabled]]} {
        set why [::vi18n::mc {not configured}]
        catch { set why [dict get $q reason] }
        return [_qrow $w $row [::vi18n::mc {status}] $why]
    }
    # Nothing warm yet: say so rather than showing blanks that read as
    # zero. The background warm fills it, or Refresh does.
    if {[dict exists $q cold] && [string is true -strict [dict get $q cold]]} {
        return [_qrow $w $row [::vi18n::mc {status}] \
            [::vi18n::mc {no reading yet — press Refresh}]]
    }

    # ── team ──
    #
    # Each plan opens with a NAMED header carrying its own fraction, and
    # the rows under it read "credits", "weekly", …., which are only
    # interpretable in that context. Gold + an arrow marks the session's
    # own pool.
    set is_team [expr {$active eq "team"}]
    set terr [::vapi::field $q team_error]
    set row [::voverlay::_qplan_head $w $row team $is_team [::voverlay::_qplan_frac $q team]]
    if {$terr ne ""} {
        set row [_qrow $w $row [::vi18n::mc {status}] $terr]
    } elseif {[dict exists $q team]} {
        set t [dict get $q team]
        set seats {}
        catch { set seats [dict get $t seats] }
        foreach s $seats {
            set rem [::voverlay::_qnum $s credits_remaining]
            set tot [::voverlay::_qnum $s credits_total]
            if {$rem eq "" && $tot eq ""} {
                set row [_qrow $w $row [::vi18n::mc {credits}] [::vi18n::mc {not reported}]]
            } else {
                set row [_qrow $w $row [::vi18n::mc {credits}] \
                    "[::voverlay::_qgroup $rem] / [::voverlay::_qgroup $tot] · [::voverlay::_qpct $s used_fraction] used"]
            }
            set until [::voverlay::_qwhen $s cycle_end]
            if {$until ne ""} {
                set row [_qrow $w $row [::vi18n::mc {cycle ends}] $until]
            }
        }
        set spend [::vapi::field $t spend]
        if {$spend ne "" && ![catch { dict size $spend }]} {
            set row [_qrow $w $row [::vi18n::mc {spend}] \
                "[dict get $spend bill_month] [dict get $spend currency] [dict get $spend total]"]
        }
    }

    # ── personal ──
    set row [::voverlay::_qplan_head $w $row personal \
        [expr {$active eq "personal"}] [::voverlay::_qplan_frac $q personal]]
    set perr [::vapi::field $q personal_error]
    if {$perr ne ""} {
        set row [_qrow $w $row [::vi18n::mc {status}] $perr]
    } elseif {[dict exists $q personal]} {
        set p [dict get $q personal]
        set wins {}
        catch { set wins [dict get $p windows] }
        foreach win $wins {
            set label [dict get $win label]
            set txt [::voverlay::_qpct $win used_fraction]
            set reset [::voverlay::_qwhen $win resets_at]
            if {$reset ne ""} { append txt " · resets $reset" }
            set row [_qrow $w $row $label $txt]
        }
    }

    # ── z.ai — a disjoint pool from another vendor (2026-09-16) ──
    set row [::voverlay::_qplan_head $w $row z.ai \
        [expr {$active eq "zai"}] [::voverlay::_qplan_frac $q zai]]
    set zerr [::vapi::field $q zai_error]
    if {$zerr ne ""} {
        set row [_qrow $w $row [::vi18n::mc {status}] $zerr]
    } elseif {[dict exists $q zai]} {
        set z [dict get $q zai]
        set lvl [::vapi::field $z level]
        if {$lvl ne ""} {
            set row [_qrow $w $row [::vi18n::mc {tier}] $lvl]
        }
        foreach key {window_5h weekly} {
            set win ""
            catch { set win [dict get $z $key] }
            if {$win eq "" || $win eq "null"} { continue }
            set label ""
            catch { set label [dict get $win label] }
            if {$label eq ""} {
                set label [expr {$key eq "window_5h" ? "5-hour" : "weekly"}]
            }
            set txt [::voverlay::_qpct $win used_fraction]
            set reset [::voverlay::_qwhen $win resets_at]
            if {$reset ne ""} { append txt " · resets $reset" }
            set row [_qrow $w $row $label $txt]
        }
        set tools [::vapi::field $z tools]
        if {$tools ne "" && ![catch { dict size $tools }]} {
            set used [::voverlay::_qnum $tools used]
            set lim [::voverlay::_qnum $tools limit]
            if {$used eq "" && $lim eq ""} {
                set row [_qrow $w $row [::vi18n::mc {tools}] [::vi18n::mc {not reported}]]
            } else {
                set txt "[::voverlay::_qgroup $used] / [::voverlay::_qgroup $lim] · [::voverlay::_qpct $tools used_fraction] used"
                set reset [::voverlay::_qwhen $tools resets_at]
                if {$reset ne ""} { append txt " · resets $reset" }
                set row [_qrow $w $row [::vi18n::mc {tools}] $txt]
            }
            set det {}
            catch { set det [dict get $tools details] }
            if {[llength $det] > 0} {
                set parts {}
                foreach d $det {
                    if {[catch { dict get $d model } m]} { set m "?" }
                    if {[catch { dict get $d usage } u]} { set u "" }
                    lappend parts "$m [::voverlay::_qgroup $u]"
                }
                set row [_qrow $w $row [::vi18n::mc {by tool}] [join $parts " · "]]
            }
        }
    }

    # ── kimi — a fourth pool, another vendor again (2026-09-20) ──
    #
    # The window labels come off the payload's own descriptors, so what
    # appears here is whatever the plan calls its windows ("5-hour",
    # "month", "month · code"). The month rows exist only as ratios in
    # the payload — the same half that read 0 through an observed 403 on
    # the 5-hour window — so they are shown, and the module's header
    # records why they are the weakest numbers on this screen.
    set row [::voverlay::_qplan_head $w $row kimi \
        [expr {$active eq "kimi"}] [::voverlay::_qplan_frac $q kimi]]
    set kerr [::vapi::field $q kimi_error]
    if {$kerr ne ""} {
        set row [_qrow $w $row [::vi18n::mc {status}] $kerr]
    } elseif {[dict exists $q kimi]} {
        set k [dict get $q kimi]
        set kwins {}
        catch { set kwins [dict get $k windows] }
        if {[llength $kwins] == 0} {
            set row [_qrow $w $row [::vi18n::mc {status}] [::vi18n::mc {nothing reported}]]
        }
        foreach win $kwins {
            set label ""
            catch { set label [dict get $win label] }
            set txt [::voverlay::_qpct $win used_fraction]
            set reset [::voverlay::_qwhen $win resets_at]
            if {$reset ne ""} { append txt " · resets $reset" }
            set row [_qrow $w $row $label $txt]
        }
    }

    # Nothing implicit: the reader is told how old this is and how often
    # it refreshes itself.
    set age 0
    catch { set age [dict get $q age_secs] }
    set poll 0
    catch { set poll [dict get $q poll_secs] }
    set note [::vi18n::mc {plan figures read}]
    append note " [::voverlay::_qage $age]"
    # Past the TTL the engine serves the reading anyway rather than
    # claiming it has none — so say which of the two this is, here as
    # well as in the status bar's tooltip.
    set stale 0
    catch { set stale [string is true -strict [dict get $q stale]] }
    if {$stale} { append note ", [::vi18n::mc {past its ttl}]" }
    if {$poll > 0} {
        append note ", [::vi18n::mc {refreshed every}] [expr {$poll / 60}]m"
    } else {
        append note ", [::vi18n::mc {no background refresh}]"
    }
    ::vmanage::say $w $note
    return $row
}

# The provider half of the session's current route, or "" — read from
# the one variable that owns it (::vstatus::route_prov, set by
# set_route), so the overlay and the status bar cannot disagree about
# which provider the session is on.
proc ::voverlay::_qroute_prov {} {
    if {[info exists ::vstatus::route_prov]} { return $::vstatus::route_prov }
    return ""
}

proc ::voverlay::_qage {secs} {
    if {![string is integer -strict $secs]} { return [::vi18n::mc {just now}] }
    if {$secs < 5}    { return [::vi18n::mc {just now}] }
    if {$secs < 60}   { return "${secs}s ago" }
    if {$secs < 3600} { return "[expr {$secs / 60}]m ago" }
    return "[expr {$secs / 3600}]h ago"
}

# ── cron: authoring, not just pausing ───────────────────────────────
#
# The overlay could pause, resume, run and show history for jobs that
# already existed — and there was no way to create one, so every job had
# to be authored by asking the agent to call its `cronjob` tool. The
# store has had cron_upsert / cron_update / cron_delete all along.
#
# The schedule is the part that goes wrong quietly: `every 1h`,
# `cron:0 9 * * 1` and `once:<rfc3339>` are all easy to mistype into
# something that parses as nothing and simply never fires. The dialog
# echoes the engine's own reading of the string as you type it, and the
# create is refused outright if it will not normalize.

proc ::voverlay::_cron_new {w} { _cron_dialog $w "" }

proc ::voverlay::_cron_edit {w} {
    set id [_cron_selected_id $w]
    if {$id eq ""} { return }
    _cron_dialog $w $id
}

proc ::voverlay::_cron_delete {w} {
    set id [_cron_selected_id $w]
    if {$id eq ""} { return }
    if {![::vmanage::confirm $w [format [::vi18n::mc {Delete job %s?}] $id] \
        [::vi18n::mc {The job and its run history are removed. This is not undoable — pause it instead if you only want it to stop.}]]} {
        return
    }
    set resp [::vmanage::del $w "/api/cron/$id"]
    if {$resp eq ""} { return }
    _cron_fill $w
    ::vmanage::say $w [format [::vi18n::mc {deleted %s}] $id] ok
}

# The cron list is a text widget with clickable tags rather than a tree,
# so the "selection" is whichever job the user last acted on.
proc ::voverlay::_cron_selected_id {w} {
    variable cron_selected
    if {![info exists cron_selected] || $cron_selected eq ""} {
        ::vmanage::say $w [::vi18n::mc {click a job in the list first}]
        return ""
    }
    return $cron_selected
}

proc ::voverlay::_cron_dialog {w {id ""}} {
    variable cron_jobs
    set d .cron_edit
    catch { destroy $d }
    toplevel $d
    wm title $d [expr {$id eq "" ? [::vi18n::mc {New cron job}] : [::vi18n::mc {Edit cron job}]}]
    wm transient $d $w
    ::vplace::centered $d 660 480
    bind $d <Escape> [list destroy $d]

    set job {}
    if {$id ne "" && [dict exists $cron_jobs $id]} { set job [dict get $cron_jobs $id] }

    ttk::frame $d.f
    pack $d.f -fill both -expand 1 -padx 12 -pady 12
    set row 0
    # `session` (Stage 5): leave empty for a fresh-session run; an id
    # (or unique prefix) turns the job into a WAKEUP that resumes that
    # session with its history as context. The engine resolves and
    # validates at save — an unresolvable name comes back as an error
    # on the status line, not as a job that fails at 03:00.
    foreach {field label} {name {name} schedule {schedule} deliver {deliver to} session {wake session}} {
        ttk::label $d.f.l$field -text [::vi18n::mc $label]
        ttk::entry $d.f.$field
        if {$job ne "" && [dict exists $job $field]} {
            set v [::vapi::field $job $field]
            if {$v ne ""} { $d.f.$field insert 0 $v }
        }
        catch { ::va11y::name $d.f.$field [::vi18n::mc $label] }
        grid $d.f.l$field -row $row -column 0 -sticky e -padx {0 6} -pady 2
        grid $d.f.$field  -row $row -column 1 -sticky ew -pady 2
        incr row
    }
    grid columnconfigure $d.f 1 -weight 1

    # The engine's reading of the schedule, live. Getting this wrong is
    # the failure mode that costs a day, because nothing happens.
    ttk::label $d.f.sched_note -text "" -foreground $::vt::T(fg_dim) -wraplength 480 -justify left
    grid $d.f.sched_note -row $row -column 1 -sticky w -pady {0 6}
    incr row
    bind $d.f.schedule <KeyRelease> [list ::voverlay::_cron_describe $d]

    ttk::label $d.f.lprompt -text [::vi18n::mc {prompt}]
    text $d.f.prompt -height 8 -wrap word -relief flat -padx 6 -pady 4 \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg)
    if {$job ne "" && [dict exists $job prompt]} {
        $d.f.prompt insert end [dict get $job prompt]
    }
    catch { ::va11y::name $d.f.prompt [::vi18n::mc {prompt}] }
    grid $d.f.lprompt -row $row -column 0 -sticky ne -padx {0 6} -pady 2
    grid $d.f.prompt  -row $row -column 1 -sticky nsew -pady 2
    grid rowconfigure $d.f $row -weight 1
    incr row

    ttk::label $d.f.hint -foreground $::vt::T(fg_dim) -wraplength 480 -justify left \
        -text [::vi18n::mc {Schedule: an interval ("30m", "every 2h"), a cron pattern ("0 9 * * 1"), or an ISO timestamp for a one-off. The line above shows what the engine reads it as. Deliver: "local" keeps the output in the log — the only honest default from the desktop, which has no chat platform behind it — or "platform:chat_id". Wake session: leave empty to run in a fresh session; set a session id (or unique prefix) to resume that session with its full history — a busy session takes the prompt at its next turn boundary instead.}]
    grid $d.f.hint -row $row -column 1 -sticky w -pady {6 6}
    incr row

    ttk::label $d.f.status -text "" -foreground $::vt::T(gold) -wraplength 480 -justify left
    grid $d.f.status -row $row -column 1 -sticky w
    incr row

    ttk::frame $d.f.b
    grid $d.f.b -row $row -column 1 -sticky w -pady {8 0}
    ttk::button $d.f.b.save -text [::vi18n::mc {Save}] \
        -command [list ::voverlay::_cron_dialog_save $w $d $id]
    ttk::button $d.f.b.cancel -text [::vi18n::mc {Cancel}] -command [list destroy $d]
    pack $d.f.b.save $d.f.b.cancel -side left -padx {0 6}
    catch { ::vcoach::attach $d.f.b.save }

    if {$job eq ""} { $d.f.deliver insert 0 "local" }
    _cron_describe $d
    focus $d.f.name
}

proc ::voverlay::_cron_describe {d} {
    if {![winfo exists $d.f.sched_note]} { return }
    set raw [string trim [$d.f.schedule get]]
    if {$raw eq ""} {
        $d.f.sched_note configure -text ""
        return
    }
    if {[catch { ::vapi::get_json "/api/cron/describe?schedule=[::vapi::urlencode $raw]" } resp]} {
        puts stderr "veles-desktop: cron describe failed: $resp"
        return
    }
    set valid 0
    catch { set valid [dict get $resp valid] }
    if {$valid} {
        $d.f.sched_note configure -text [dict get $resp human] -foreground $::vt::T(accent)
    } else {
        set why ""
        catch { set why [dict get $resp error] }
        $d.f.sched_note configure -text $why -foreground $::vt::T(gold)
    }
}

proc ::voverlay::_cron_dialog_save {w d id} {
    # Free text throughout (a cron name can be "nightly cleanup", a
    # schedule has spaces, the prompt is a Text widget's contents) —
    # every field typed, or the shape inference serializes them as
    # arrays/objects (RM3 #10 class).
    set body [dict create \
        name     [::vapi::json_string [string trim [$d.f.name get]]] \
        schedule [::vapi::json_string [string trim [$d.f.schedule get]]] \
        deliver  [::vapi::json_string [string trim [$d.f.deliver get]]] \
        session  [::vapi::json_string [string trim [$d.f.session get]]] \
        prompt   [::vapi::json_string [string trim [$d.f.prompt get 1.0 end]]]]
    if {[dict get $body prompt] eq ""} {
        $d.f.status configure -text [::vi18n::mc {a job needs a prompt}]
        return
    }
    set path [expr {$id eq "" ? "/api/cron/new" : "/api/cron/$id"}]
    if {[catch { ::vapi::post_json $path $body } resp]} {
        $d.f.status configure -text [::vmanage::_reason $resp]
        return
    }
    destroy $d
    _cron_fill $w
    ::vmanage::say $w [expr {$id eq ""
        ? [::vi18n::mc {job created}]
        : [::vi18n::mc {job updated}]}] ok
}

# ── triggers — the channel-event reaction table ─────────────────────
#
# Same family as _cron, keyed by NAME instead of id (the REST path
# segment is /api/triggers/<name>). The row renders the engine's
# meaning, and the dialog's bounded fields (event, match, action,
# min-role) are readonly comboboxes so a typo cannot become a row that
# never fires.

proc ::voverlay::_triggers {w} {
    ::vmanage::window $w [::vi18n::mc {Triggers}] 820x520 {420 300}
    ::vmanage::button $w.bar new [::vi18n::mc {New…}] \
        [list ::voverlay::_trigger_new $w] [::vi18n::mc {create a trigger}]
    ::vmanage::button $w.bar edit [::vi18n::mc {Edit…}] \
        [list ::voverlay::_trigger_edit $w] [::vi18n::mc {edit the selected trigger}]
    ::vmanage::button $w.bar del [::vi18n::mc {Delete}] \
        [list ::voverlay::_trigger_delete $w] [::vi18n::mc {delete the selected trigger}]
    ::vmanage::separator $w.bar sep1
    ::vmanage::button $w.bar reload [::vi18n::mc {Reload}] \
        [list ::voverlay::_trigger_fill $w]

    ttk::frame $w.main
    text $w.main.t -wrap word -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -highlightthickness 0 -relief flat -font {monospace 10}
    ttk::scrollbar $w.main.sb -orient vertical -command [list $w.main.t yview]
    $w.main.t configure -yscrollcommand [list $w.main.sb set]
    pack $w.main.sb -side right -fill y
    pack $w.main.t -side left -fill both -expand 1
    pack $w.main -fill both -expand 1 -padx 8 -pady {0 8}

    _trigger_fill $w
}

proc ::voverlay::_trigger_fill {w} {
    variable trigger_rows
    variable trigger_selected
    set t $w.main.t
    $t configure -state normal
    $t delete 1.0 end
    if {[catch {::vapi::get_json /api/triggers} resp]} {
        $t insert end "triggers unavailable: [::vmanage::_reason $resp]\n"
        $t configure -state disabled
        return
    }
    set rows [dict get $resp triggers]
    set trigger_rows {}
    if {![llength $rows]} {
        $t insert end [::vi18n::mc {no triggers — New… creates one}]
        $t insert end "\n"
    }
    foreach tr $rows {
        set name [dict get $tr name]
        dict set trigger_rows $name $tr
        set en [dict get $tr enabled]
        set event [dict get $tr event]
        set match [dict get $tr match_kind]
        set action [dict get $tr action]
        set scope ""
        catch {
            set net [dict get $tr network]
            if {$net eq "null" || $net eq ""} { set net "*" }
            set chan [dict get $tr channel]
            if {$chan eq "null" || $chan eq ""} { set chan "*" }
            set scope "$net/$chan"
        }
        set state [expr {$en ? "●" : "○"}]
        set line "$state $name · $event · $scope · match=$match · action=$action\n"
        set last [::vapi::field $tr last_status]
        if {$last ne ""} {
            append line "    last: $last (fires: [dict get $tr fire_count])\n"
        }
        set start [$t index "end-1c"]
        $t insert end $line
        $t tag add "trig_$name" $start "end-1c"
        $t tag bind "trig_$name" <Button-1> [list ::voverlay::_trigger_select $w $name]
        foreach a [list enable disable history edit delete] {
            $t insert end "  \[$a\]" "trigbtn_${name}_$a"
            $t tag configure "trigbtn_${name}_$a" -foreground $::vt::T(accent) -underline 1
            $t tag bind "trigbtn_${name}_$a" <Button-1> \
                [list ::voverlay::_trigger_action $w $name $a]
        }
        $t insert end "\n\n"
    }
    $t configure -state disabled
    if {[info exists trigger_selected] && $trigger_selected ne ""
        && ![dict exists $trigger_rows $trigger_selected]} {
        set trigger_selected ""
    }
    _trigger_say_selection $w
}

proc ::voverlay::_trigger_select {w name} {
    variable trigger_selected
    set trigger_selected $name
    _trigger_say_selection $w
}

proc ::voverlay::_trigger_say_selection {w} {
    variable trigger_selected
    if {![info exists trigger_selected] || $trigger_selected eq ""} {
        ::vmanage::say $w [::vi18n::mc {click a trigger to select it}]
        return
    }
    ::vmanage::say $w [format [::vi18n::mc {selected: %s}] $trigger_selected]
}

proc ::voverlay::_trigger_selected_name {w} {
    variable trigger_selected
    if {![info exists trigger_selected] || $trigger_selected eq ""} {
        ::vmanage::say $w [::vi18n::mc {click a trigger in the list first}]
        return ""
    }
    return $trigger_selected
}

proc ::voverlay::_trigger_new {w} { _trigger_dialog $w "" }

proc ::voverlay::_trigger_edit {w} {
    set name [_trigger_selected_name $w]
    if {$name eq ""} { return }
    _trigger_dialog $w $name
}

proc ::voverlay::_trigger_delete {w} {
    set name [_trigger_selected_name $w]
    if {$name eq ""} { return }
    if {![::vmanage::confirm $w [format [::vi18n::mc {Delete trigger %s?}] $name] \
        [::vi18n::mc {The row and its fire history are removed. This is not undoable — disable it instead if you only want it to stop.}]]} {
        return
    }
    set resp [::vmanage::del $w "/api/triggers/$name"]
    if {$resp eq ""} { return }
    _trigger_fill $w
    ::vmanage::say $w [format [::vi18n::mc {deleted %s}] $name] ok
}

proc ::voverlay::_trigger_action {w name action} {
    variable trigger_selected
    set trigger_selected $name
    if {$action eq "edit"}   { _trigger_dialog $w $name ; return }
    if {$action eq "delete"} { _trigger_delete $w ; return }
    switch -- $action {
        enable - disable {
            if {[catch {::vapi::post "/api/triggers/$name/$action" ""} e]} {
                ::vstatus::set_msg [format [::vi18n::mc {%1$s failed: %2$s}] $action $e]
                return
            }
            ::vstatus::set_msg [format [::vi18n::mc {trigger %1$s %2$sd}] $name $action]
            catch { ::voverlay::_trigger_fill $w }
        }
        history {
            if {[catch {::vapi::get_json "/api/triggers/$name/history"} h]} {
                ::vstatus::set_msg [format [::vi18n::mc {history failed: %s}] $h]
                return
            }
            set msg "fires for $name:\n"
            set n 0
            foreach f [dict get $h fires] {
                incr n
                append msg "[dict get $f at] [dict get $f channel] [dict get $f status]"
                set detail [::vapi::field $f detail]
                if {$detail ne ""} { append msg " — $detail" }
                append msg "\n"
            }
            if {!$n} { set msg "[::vi18n::mc {fires for %s:} $name]\n[::vi18n::mc {none recorded}]\n" }
            tk_messageBox -title [::vi18n::mc {Trigger history}] -message $msg
        }
    }
    after 400 [list ::voverlay::_trigger_fill $w]
}

proc ::voverlay::_trigger_dialog {w {name ""}} {
    variable trigger_rows
    set d .trigger_edit
    catch { destroy $d }
    toplevel $d
    wm title $d [expr {$name eq "" ? [::vi18n::mc {New trigger}] : [::vi18n::mc {Edit trigger}]}]
    wm transient $d $w
    ::vplace::centered $d 660 560
    bind $d <Escape> [list destroy $d]

    set tr {}
    if {$name ne "" && [dict exists $trigger_rows $name]} {
        set tr [dict get $trigger_rows $name]
    }

    ttk::frame $d.f
    pack $d.f -fill both -expand 1 -padx 12 -pady 12
    set row 0
    foreach {field label} {
        name {name} network {network} channel {channel} pattern {pattern}
        arg {argument} cooldown_secs {cooldown (seconds)} per_hour_cap {fires per hour}
    } {
        ttk::label $d.f.l$field -text [::vi18n::mc $label]
        ttk::entry $d.f.$field
        if {$tr ne "" && [dict exists $tr $field]} {
            set v [::vapi::field $tr $field]
            if {$v ne ""} { $d.f.$field insert 0 $v }
        }
        catch { ::va11y::name $d.f.$field [::vi18n::mc $label] }
        grid $d.f.l$field -row $row -column 0 -sticky e -padx {0 6} -pady 2
        grid $d.f.$field  -row $row -column 1 -sticky ew -pady 2
        incr row
    }
    # The bounded fields — a typo must not become a silently-dead row.
    # The choices are the engine's TOKENS, shown as typed, so they live
    # outside the label table: a braced cell in a foreach table is read
    # as a translatable string.
    set choices {
        event {TEXT JOIN PART QUIT KICK NICK}
        match_kind {always contains regex url imageurl bluesky}
        action {slopdrop vision bluesky prompt command}
        min_role {{} guest user trusted admin owner}
    }
    foreach {field label} {
        event {event}
        match_kind {match kind}
        action {action}
        min_role {minimum role}
    } {
        set values [dict get $choices $field]
        ttk::label $d.f.l$field -text [::vi18n::mc $label]
        ttk::combobox $d.f.$field -state readonly -width 20 -values $values
        if {$tr ne "" && [dict exists $tr $field]} {
            set v [::vapi::field $tr $field]
            if {$v ne ""} { $d.f.$field set $v }
        }
        if {$field eq "event"}   { $d.f.$field set TEXT }
        if {$field eq "match_kind"} { $d.f.$field set always }
        if {$field eq "action"}  { $d.f.$field set slopdrop }
        catch { ::va11y::name $d.f.$field [::vi18n::mc $label] }
        grid $d.f.l$field -row $row -column 0 -sticky e -padx {0 6} -pady 2
        grid $d.f.$field  -row $row -column 1 -sticky ew -pady 2
        incr row
    }
    grid columnconfigure $d.f 1 -weight 1

    ttk::label $d.f.hint -foreground $::vt::T(fg_dim) -wraplength 480 -justify left \
        -text [::vi18n::mc {Triggers react to channel events without a turn. slopdrop forwards the event to the bridge and relays the reply; vision describes an image URL inside the cooldown and hourly cap. The row only fires where the [triggers] config switch AND the derived mask allow it — #coven forwards nothing under the live slopdrop bindings.}]
    grid $d.f.hint -row $row -column 1 -sticky w -pady {6 6}
    incr row

    ttk::label $d.f.status -text "" -foreground $::vt::T(gold) -wraplength 480 -justify left
    grid $d.f.status -row $row -column 1 -sticky w
    incr row

    ttk::frame $d.f.b
    grid $d.f.b -row $row -column 1 -sticky w -pady {8 0}
    ttk::button $d.f.b.save -text [::vi18n::mc {Save}] \
        -command [list ::voverlay::_trigger_dialog_save $w $d $name]
    ttk::button $d.f.b.cancel -text [::vi18n::mc {Cancel}] -command [list destroy $d]
    pack $d.f.b.save $d.f.b.cancel -side left -padx {0 6}
    catch { ::vcoach::attach $d.f.b.save }

    focus $d.f.name
}

proc ::voverlay::_trigger_dialog_save {w d name} {
    # Every field may be empty except the name: the engine reads empty
    # scope fields as "any". The bounded fields always carry a value
    # (the combobox started on the row's, or the default).
    #
    # CLEARING is the half this used to drop. Blanking a field and
    # saving simply skipped it, and "absent" means "leave alone" to
    # `POST /api/triggers/:name` — so the dialog could widen a row's
    # scope or set a min-role but never take one back off, and the
    # field sat there looking empty while the stored row kept its old
    # value. The REST surface has always accepted `null` for exactly
    # this; only an EDIT sends it (on a create there is nothing to
    # clear, and a null would just be noise).
    set editing [expr {$name ne ""}]
    set nullable {network channel pattern arg min_role}
    set body [dict create]
    foreach field {
        name network channel pattern arg event match_kind action min_role
    } {
        set v [string trim [$d.f.$field get]]
        if {$v eq ""} {
            if {$editing && [lsearch -exact $nullable $field] >= 0} {
                dict set body $field [::vapi::json_raw null]
            }
            continue
        }
        dict set body $field [::vapi::json_string $v]
    }
    if {![dict exists $body name]} {
        $d.f.status configure -text [::vi18n::mc {a trigger needs a name}]
        return
    }
    foreach field {cooldown_secs per_hour_cap} {
        set v [string trim [$d.f.$field get]]
        if {$v eq ""} { continue }
        if {![string is integer -strict $v]} {
            $d.f.status configure -text \
                [format [::vi18n::mc {%1$s is not a whole number}] $field]
            return
        }
        dict set body $field $v
    }
    set path [expr {$name eq "" ? "/api/triggers/new" : "/api/triggers/$name"}]
    if {[catch { ::vapi::post_json $path $body } resp]} {
        $d.f.status configure -text [::vmanage::_reason $resp]
        return
    }
    destroy $d
    _trigger_fill $w
    ::vmanage::say $w [expr {$name eq ""
        ? [::vi18n::mc {trigger created}]
        : [::vi18n::mc {trigger updated}]}] ok
}

# ── tools & MCP servers ─────────────────────────────────────────────
#
# The only surface for either was `/tools`, a slash command that printed
# a list into the transcript. MCP servers had no surface at all: they are
# a `[mcp_servers.<name>]` table in config.toml, edited by hand, and
# nothing in the app could tell you which ones had actually connected.
#
# This lists what the engine has REGISTERED (so a server that failed to
# connect is visible by its absence, with the configured name still on
# the left), and manages the config side: add a server, edit its fields,
# remove it. `[mcp_servers]` is re-read per use, so a change is live for
# the next tool lookup — the effect line says so per write.

proc ::voverlay::_tools {w} {
    ::vmanage::window $w [::vi18n::mc {Tools & MCP servers}] 900x600
    ::vmanage::button $w.bar add [::vi18n::mc {Add server…}] \
        [list ::voverlay::_mcp_add $w] [::vi18n::mc {add an MCP server}]
    ::vmanage::button $w.bar edit [::vi18n::mc {Edit…}] \
        [list ::voverlay::_mcp_edit $w] [::vi18n::mc {edit the selected server}]
    ::vmanage::button $w.bar del [::vi18n::mc {Remove}] \
        [list ::voverlay::_mcp_remove $w] [::vi18n::mc {remove the selected server}]
    ::vmanage::separator $w.bar sep1
    ::vmanage::button $w.bar refresh [::vi18n::mc {Refresh}] \
        [list ::voverlay::_tools_fill $w]

    ttk::panedwindow $w.split -orient vertical
    pack $w.split -fill both -expand 1 -padx 8 -pady 6

    ttk::frame $w.split.top
    ttk::label $w.split.top.l -text [::vi18n::mc {MCP servers (from config.toml)}] \
        -font {TkDefaultFont 10 bold}
    pack $w.split.top.l -anchor w
    set st [::vmanage::tree $w.split.top list {name detail} \
        [list [::vi18n::mc {server}] [::vi18n::mc {configuration}]] {180 560}]
    pack $st -fill both -expand 1
    catch { ::va11y::name $st.t [::vi18n::mc {MCP server list}] }
    $w.split add $w.split.top -weight 1

    ttk::frame $w.split.bot
    ttk::label $w.split.bot.l -text [::vi18n::mc {tools the engine has registered}] \
        -font {TkDefaultFont 10 bold}
    pack $w.split.bot.l -anchor w
    text $w.split.bot.t -wrap word -relief flat -padx 8 -pady 6 \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -yscrollcommand [list $w.split.bot.sb set]
    ttk::scrollbar $w.split.bot.sb -orient vertical -command [list $w.split.bot.t yview]
    pack $w.split.bot.sb -side right -fill y
    pack $w.split.bot.t -side left -fill both -expand 1
    catch { ::vtext::readonly $w.split.bot.t }
    $w.split add $w.split.bot -weight 2

    bind $st.t <Double-1> [list ::voverlay::_mcp_edit $w]
    _tools_fill $w
}

proc ::voverlay::_tools_tree {w} { return $w.split.top.list.t }

proc ::voverlay::_tools_fill {w} {
    variable mcp_servers
    set t [_tools_tree $w]
    if {![winfo exists $t]} { return }
    $t delete [$t children {}]
    set mcp_servers {}

    set cfg [::vmanage::get $w "/api/config/get?path=mcp_servers"]
    if {$cfg ne "" && [dict exists $cfg value]} {
        set v [::vapi::field $cfg value]
        if {$v ne "" && ![catch { dict size $v }]} {
            foreach {name spec} $v {
                dict set mcp_servers $name $spec
                set parts {}
                catch {
                    foreach {k val} $spec { lappend parts "$k=$val" }
                }
                $t insert {} end -id $name -values [list $name [join $parts "  "]]
            }
        }
    }
    if {![llength [dict keys $mcp_servers]]} {
        ::vmanage::say $w [::vi18n::mc {no MCP servers configured — Add server… writes one into config.toml}]
    } else {
        ::vmanage::say $w [format [::vi18n::mc {%d MCP server(s) configured}] \
            [llength [dict keys $mcp_servers]]]
    }

    # The live registry. A configured server whose tools do NOT appear
    # here failed to connect, and that difference is the whole reason to
    # show both halves in one window.
    set body [::vi18n::mc {tools unavailable}]
    if {![catch { ::vapi::get_json /api/tools } tools]} {
        set lines {}
        foreach section {builtin deferred} {
            if {![dict exists $tools $section]} { continue }
            set names [dict get $tools $section]
            lappend lines "$section ([llength $names]):"
            lappend lines "  [join $names {, }]"
            lappend lines ""
        }
        set body [join $lines "\n"]
    }
    ::vtext::set_text $w.split.bot.t $body
}

proc ::voverlay::_mcp_selected {w} {
    set vals [::vmanage::selected $w [_tools_tree $w] [::vi18n::mc {a server}]]
    if {$vals eq ""} { return "" }
    return [lindex $vals 0]
}

# One dialog for add and edit: an MCP server entry is a small table and
# the honest editor for it is its TOML, not a form guessing at fields
# that differ per transport (command/args for stdio, url for http).
proc ::voverlay::_mcp_add {w} { _mcp_dialog $w "" }

proc ::voverlay::_mcp_edit {w} {
    set name [_mcp_selected $w]
    if {$name eq ""} { return }
    _mcp_dialog $w $name
}

proc ::voverlay::_mcp_dialog {w name} {
    variable mcp_servers
    set d .mcp_edit
    catch { destroy $d }
    toplevel $d
    wm title $d [expr {$name eq "" ? [::vi18n::mc {Add MCP server}] : [::vi18n::mc {Edit MCP server}]}]
    wm transient $d $w
    ::vplace::centered $d 620 380
    bind $d <Escape> [list destroy $d]

    ttk::frame $d.f
    pack $d.f -fill both -expand 1 -padx 12 -pady 12
    ttk::label $d.f.ln -text [::vi18n::mc {name}]
    ttk::entry $d.f.name
    $d.f.name insert 0 $name
    if {$name ne ""} { $d.f.name configure -state readonly }
    pack $d.f.ln -anchor w
    pack $d.f.name -fill x -pady {2 8}

    ttk::label $d.f.lv -text [::vi18n::mc {settings, one key = value per line}]
    text $d.f.body -height 10 -wrap none -relief flat -padx 6 -pady 4 \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg) -font {monospace 10}
    pack $d.f.lv -anchor w
    pack $d.f.body -fill both -expand 1 -pady {2 6}
    if {$name ne "" && [dict exists $mcp_servers $name]} {
        set lines {}
        catch {
            foreach {k v} [dict get $mcp_servers $name] { lappend lines "$k = $v" }
        }
        $d.f.body insert end [join $lines "\n"]
    } else {
        $d.f.body insert end "command = \"npx\"\nargs = \[\"-y\", \"@modelcontextprotocol/server-filesystem\", \"/tmp\"\]"
    }

    ttk::label $d.f.hint -foreground $::vt::T(fg_dim) -wraplength 560 -justify left \
        -text [::vi18n::mc {Values are TOML: quoted strings, numbers, or lists. Written into [mcp_servers.<name>] — your comments and every other section are left untouched.}]
    pack $d.f.hint -anchor w -pady {0 6}
    ttk::label $d.f.status -text "" -foreground $::vt::T(gold) -wraplength 560 -justify left
    pack $d.f.status -anchor w -pady {0 6}

    ttk::frame $d.f.b
    pack $d.f.b -fill x
    ttk::button $d.f.b.save -text [::vi18n::mc {Save}] \
        -command [list ::voverlay::_mcp_save $w $d]
    ttk::button $d.f.b.cancel -text [::vi18n::mc {Cancel}] -command [list destroy $d]
    pack $d.f.b.save $d.f.b.cancel -side left -padx {0 6}
    catch { ::vcoach::attach $d.f.b.save }
    focus [expr {$name eq "" ? "$d.f.name" : "$d.f.body"}]
}

proc ::voverlay::_mcp_save {w d} {
    set name [string trim [$d.f.name get]]
    if {$name eq ""} {
        $d.f.status configure -text [::vi18n::mc {name the server first}]
        return
    }
    set ok 1
    foreach line [split [$d.f.body get 1.0 end] "\n"] {
        set line [string trim $line]
        if {$line eq "" || [string index $line 0] eq "#"} { continue }
        set idx [string first "=" $line]
        if {$idx < 1} {
            ::vi18n::label $d.f.status {not a key = value line: %s} $line
            return
        }
        set k [string trim [string range $line 0 [expr {$idx - 1}]]]
        set v [string trim [string range $line [expr {$idx + 1}] end]]
        set resp [::vmanage::post $w /api/config/set \
            [::voverlay::_config_body [dict create path "mcp_servers.$name.$k" raw_value $v]]]
        if {$resp eq ""} { set ok 0 ; break }
    }
    if {!$ok} {
        $d.f.status configure -text [::vi18n::mc {the write was refused — see the manager's status line}]
        return
    }
    destroy $d
    _tools_fill $w
    ::vmanage::say $w [format [::vi18n::mc {saved %s — in effect for the next tool lookup}] $name] ok
}

proc ::voverlay::_mcp_remove {w} {
    set name [_mcp_selected $w]
    if {$name eq ""} { return }
    if {![::vmanage::confirm $w [format [::vi18n::mc {Remove MCP server %s?}] $name] \
        [::vi18n::mc {The [mcp_servers.<name>] table is deleted from config.toml. The previous file is kept as config.toml.bak.}]]} {
        return
    }
    set resp [::vmanage::post $w /api/config/set \
        [dict create path "mcp_servers.$name" value null]]
    if {$resp eq ""} { return }
    _tools_fill $w
    ::vmanage::effect $w $resp "$name:"
}
