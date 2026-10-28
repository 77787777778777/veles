# veles-desktop/lib/acl.tcl — access control management (issue: "where
# is ACL/capabilities management? should have a ui").
#
# The engine half is acl.rs + pairing.rs: roles are resolved per
# identity (platform:principal) against config [acl] pins, then scoped
# entries ([[acl.entries]] — mask patterns, per-network/per-channel
# roles), cross-network users ([[acl.users]]), and runtime grants. This
# is the UI half: an overlay (palette: "Access control (ACL)", /acl)
# that lists every identity with its role, scope and source, renders the
# role/capability catalogs, lists the cross-network users, and manages
# runtime grants/users through /api/acl{,/grant,/revoke,/user}.
#
# IRC identities come in two shapes, and the grant dialog says so:
#   - verified accounts:  irc:friend  telegram:1234 (strong)
#   - masks:  nick!user@host with * wildcards or re: regex PER SEGMENT
#     (weaker — nick/ident are choosable; for servers without accounts
#     and for per-channel trust). Grants can be scoped to one network
#     and/or one channel.
#
# The desktop is an OWNER surface by acl.rs's definition (the human at
# the keyboard IS the owner); the bearer-token gate on the API is the
# boundary, so the overlay needs no extra client-side role check.
#
# Sourced before overlays.tcl in main.tcl; overlays.tcl's `acl` arm
# delegates to ::vacl::build. The IRC tab opens this overlay prefilled
# (::vacl::open_for) — direct management where the people are.

namespace eval ::vacl {
    variable wins {}
    # name -> the full row dict from the last paint (the users tree only
    # shows a summary; edit needs the identities back).
    variable user_data
    # The promote dialog's options. Users default ON: a pane that writes
    # half the store and reports "promoted" is worse than one that writes
    # all of it, and the preview says exactly what it would do.
    variable promote_users 1
    variable promote_expiring 0
    variable promote_prune 0
    # The options the report on screen was produced from, so "Write it"
    # cannot mean something the operator has not read.
    variable promote_seen ""
}

# The overlay window. One builder, called by ::voverlay::open acl.
proc ::vacl::build {w} {
    toplevel $w
    wm title $w [::vi18n::mc {Access control (ACL)}]
    wm geometry $w 980x620
    wm minsize $w 520 340
    ::voverlay::_header $w [::vi18n::mc {Access control}]
    bind $w <Escape> [list destroy $w]

    ttk::frame $w.bar
    pack $w.bar -fill x -padx 8 -pady {4 0}
    ttk::button $w.bar.grant -text [::vi18n::mc {Grant…}] \
        -command [list ::vacl::grant_ask $w]
    ttk::button $w.bar.revoke -text [::vi18n::mc {Revoke}] \
        -command [list ::vacl::revoke_selected $w]
    ttk::button $w.bar.promote -text [::vi18n::mc {Promote…}] \
        -command [list ::vacl::promote_ask $w]
    ttk::separator $w.bar.sep1 -orient vertical
    ttk::button $w.bar.user_new -text [::vi18n::mc {New user…}] \
        -command [list ::vacl::user_ask $w]
    ttk::button $w.bar.user_edit -text [::vi18n::mc {Edit user…}] \
        -command [list ::vacl::user_edit_selected $w]
    ttk::button $w.bar.user_del -text [::vi18n::mc {Delete user}] \
        -command [list ::vacl::user_delete_selected $w]
    ttk::separator $w.bar.sep2 -orient vertical
    ttk::button $w.bar.refresh -text [::vi18n::mc {Refresh}] \
        -command [list ::vacl::load $w]
    ttk::button $w.bar.lock -text [::vi18n::mc {Lock down}] \
        -command [list ::vacl::lockdown_set $w on]
    ttk::label $w.bar.status -text "" -foreground $::vt::T(fg_dim)
    pack $w.bar.grant $w.bar.revoke $w.bar.promote -side left -padx {0 6}
    pack $w.bar.sep1 -side left -fill y -padx 6 -pady 2
    pack $w.bar.user_new $w.bar.user_edit $w.bar.user_del -side left -padx {0 6}
    pack $w.bar.sep2 -side left -fill y -padx 6 -pady 2
    pack $w.bar.refresh $w.bar.lock -side left -padx {0 6}
    pack $w.bar.status -side right -fill x -expand 1 -padx 8

    # ── the emergency brake ──
    #
    # Engaging is reachable from every surface, including a phone over
    # IRC. LIFTING is reachable only from a LOCAL one, and this window
    # is local — so the operator who locked the bot down from the
    # channel comes HERE to undo it, and the banner is the only place
    # in the UI that says the connectors are being refused. A
    # locked-down bot with no visible reason looks broken.
    ttk::frame $w.lock
    ttk::label $w.lock.msg -text "" -foreground $::vt::T(gold) -wraplength 760 \
        -justify left
    ttk::button $w.lock.act -text [::vi18n::mc {Lift lockdown}] \
        -command [list ::vacl::lockdown_set $w off]
    pack $w.lock.act -side right -padx {6 0}
    pack $w.lock.msg -side left -fill x -expand 1

    ttk::panedwindow $w.split -orient horizontal
    pack $w.split -fill both -expand 1 -padx 8 -pady 6

    # Left: identities above, cross-network users below.
    ttk::frame $w.split.list
    ttk::treeview $w.split.list.t -columns {identity role scope source} \
        -show headings -selectmode browse -height 12
    $w.split.list.t heading identity -text [::vi18n::mc {identity}]
    $w.split.list.t heading role     -text [::vi18n::mc {role}]
    $w.split.list.t heading scope    -text [::vi18n::mc {scope}]
    $w.split.list.t heading source   -text [::vi18n::mc {source}]
    $w.split.list.t column identity -width 230
    $w.split.list.t column role     -width 90
    $w.split.list.t column scope    -width 130
    $w.split.list.t column source   -width 70
    ttk::scrollbar $w.split.list.s -orient vertical \
        -command [list $w.split.list.t yview]
    $w.split.list.t configure -yscrollcommand [list $w.split.list.s set]
    pack $w.split.list.s -side right -fill y
    pack $w.split.list.t -fill both -expand 1
    ::va11y::name $w.split.list.t [::vi18n::mc {identities and roles}]

    ttk::label $w.split.list.ulabel \
        -text [::vi18n::mc {Users — one role across networks and platforms}] \
        -foreground $::vt::T(fg_dim)
    ttk::treeview $w.split.list.u -columns {name role identities source} \
        -show headings -selectmode browse -height 5
    $w.split.list.u heading name       -text [::vi18n::mc {user}]
    $w.split.list.u heading role       -text [::vi18n::mc {role}]
    $w.split.list.u heading identities -text [::vi18n::mc {identities}]
    $w.split.list.u heading source     -text [::vi18n::mc {source}]
    $w.split.list.u column name       -width 110
    $w.split.list.u column role       -width 80
    $w.split.list.u column identities -width 220
    $w.split.list.u column source     -width 70
    pack $w.split.list.ulabel -anchor w -pady {6 0}
    pack $w.split.list.u -fill x
    ::va11y::name $w.split.list.u [::vi18n::mc {cross-network users}]

    # Right: the catalogs (roles above, caps below).
    text $w.split.help -wrap word -relief flat -padx 10 -pady 6 \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -yscrollcommand [list $w.split.help.sb set]
    ttk::scrollbar $w.split.help.sb -orient vertical \
        -command [list $w.split.help yview]
    pack $w.split.help.sb -side right -fill y
    pack $w.split.help -fill both -expand 1
    $w.split.help configure -state disabled
    ::va11y::name $w.split.help [::vi18n::mc {roles and capabilities explained}]

    $w.split add $w.split.list -weight 1
    $w.split add $w.split.help -weight 1

    ::va11y::name $w.bar.grant [::vi18n::mc {grant a role to an identity}]
    ::va11y::name $w.bar.revoke [::vi18n::mc {revoke the selected identity's grant}]
    ::va11y::name $w.bar.promote [::vi18n::mc {write the store's runtime rules into config.toml}]
    ::va11y::name $w.bar.user_new [::vi18n::mc {create a cross-network user}]
    ::va11y::name $w.bar.user_edit [::vi18n::mc {edit the selected user}]
    ::va11y::name $w.bar.user_del [::vi18n::mc {delete the selected user}]
    ::va11y::name $w.bar.refresh [::vi18n::mc {refresh identities}]
    ::va11y::name $w.bar.lock [::vi18n::mc {refuse every connector until lifted}]
    ::va11y::name $w.lock.act [::vi18n::mc {lift the lockdown (only a local frontend can)}]
    ::va11y::tab_order [list $w.bar.grant $w.bar.revoke $w.bar.promote \
        $w.bar.user_new $w.bar.user_edit $w.bar.user_del $w.bar.refresh \
        $w.bar.lock $w.lock.act $w.split.list.t $w.split.list.u $w.split.help]

    load $w
    focus $w.bar.grant
    return $w
}

# Open the overlay and drop straight into the grant dialog, prefilled.
# The IRC tab's /acl and roster menu come through here, so managing a
# nick you are LOOKING AT is two clicks, not a hand-typed identity.
proc ::vacl::open_for {{identity ""} {network ""}} {
    ::voverlay::open acl
    grant_ask .ov_acl $identity $network
}

# POST a grant/revoke/user mutation and reload. Errors land in the
# status label — the API answers plain {ok, error}, no silent failure
# tolerated.
proc ::vacl::_post {w path body} {
    if {[catch { ::vapi::post_json $path $body } resp]} {
        ::vi18n::label $w.bar.status {failed: %s} $resp
        return 0
    }
    set ok 0
    catch { set ok [dict get $resp ok] }
    if {!$ok} {
        set why [::vi18n::mc {refused}]
        catch { set why [dict get $resp error] }
        ::vi18n::label $w.bar.status {refused: %s} $why
        return 0
    }
    load $w
    return 1
}

# Engage or lift the brake. `on` is available from every surface; `off`
# is refused everywhere except a local one, and the engine — not this
# window — is what enforces that (`Surface::Desktop`, checked in
# `lockdown_command`). The UI does not get to decide it is trusted.
proc ::vacl::lockdown_set {w action} {
    set body [dict create action $action]
    if {$action eq "on"} {
        dict set body reason [::vi18n::mc {engaged from the desktop}]
    }
    if {[catch { ::vapi::post_json /api/lockdown $body } resp]} {
        ::vi18n::label $w.bar.status {failed: %s} $resp
        return 0
    }
    set text ""
    catch { set text [dict get $resp text] }
    $w.bar.status configure -text $text
    load $w
    return 1
}

proc ::vacl::revoke_selected {w} {
    set sel [$w.split.list.t selection]
    if {![llength $sel]} {
        $w.bar.status configure -text [::vi18n::mc {select an identity first}]
        return 0
    }
    set id [$w.split.list.t item [lindex $sel 0] -values]
    set identity [lindex $id 0]
    # Config pins and config entries live in config.toml; the API only
    # manages runtime grants. Say so rather than POSTing a revoke that
    # cannot change anything (the API would answer {ok:false} and we
    # would blame it).
    set source [lindex $id 3]
    if {$source eq "config" || $source eq "entry"} {
        ::vi18n::label $w.bar.status {%s comes from config.toml [acl] — edit there} $identity
        return 0
    }
    _post $w /api/acl/revoke [dict create identity $identity]
}

# Grant dialog: identity + role + optional network/channel scope.
# Roles include banned — a runtime ban is the management path's
# emergency brake. Owner is never grantable (that is the person running
# the instance).
proc ::vacl::grant_ask {w {prefill_identity ""} {prefill_network ""}} {
    set g .acl_grant
    catch { destroy $g }
    toplevel $g
    wm title $g [::vi18n::mc {Grant access}]
    wm transient $g $w
    ::vplace::centered $g 500 300
    bind $g <Escape> [list destroy $g]

    ttk::frame $g.f
    pack $g.f -fill both -expand 1 -padx 10 -pady 10
    ttk::label $g.f.li -text [::vi18n::mc {identity}]
    ttk::entry $g.f.identity -width 52
    ttk::label $g.f.hint -foreground $::vt::T(fg_dim) -wraplength 460 -justify left -text [::vi18n::mc {verified account: irc:friend or telegram:1234 · mask: nick!user@host with * wildcards or re: regex in each segment (weaker than an account)}]
    ttk::label $g.f.ln -text [::vi18n::mc {network (optional — e.g. libera; empty = any)}]
    ttk::entry $g.f.network -width 52
    ttk::label $g.f.lc -text [::vi18n::mc {channel (optional — e.g. #veles; empty = any)}]
    ttk::entry $g.f.channel -width 52
    ttk::label $g.f.lr -text [::vi18n::mc {role}]
    ttk::combobox $g.f.role -state readonly -width 20 \
        -values {trusted user guest admin banned}
    $g.f.role current 0
    ttk::frame $g.f.b
    ttk::button $g.f.b.ok -text [::vi18n::mc {Grant}] \
        -command [list ::vacl::grant_do $w $g]
    ttk::button $g.f.b.cancel -text [::vi18n::mc {Cancel}] \
        -command [list destroy $g]
    grid $g.f.li -row 0 -column 0 -sticky w -pady {4 2}
    grid $g.f.identity -row 1 -column 0 -sticky ew
    grid $g.f.hint -row 2 -column 0 -sticky ew -pady {2 6}
    grid $g.f.ln -row 3 -column 0 -sticky w -pady {2 2}
    grid $g.f.network -row 4 -column 0 -sticky ew
    grid $g.f.lc -row 5 -column 0 -sticky w -pady {6 2}
    grid $g.f.channel -row 6 -column 0 -sticky ew
    grid $g.f.lr -row 7 -column 0 -sticky w -pady {8 2}
    grid $g.f.role -row 8 -column 0 -sticky w
    grid $g.f.b -row 9 -column 0 -pady {12 0}
    grid $g.f.b.ok $g.f.b.cancel -sticky e
    grid columnconfigure $g.f 0 -weight 1

    if {$prefill_identity ne ""} { $g.f.identity insert 0 $prefill_identity }
    if {$prefill_network ne ""}  { $g.f.network insert 0 $prefill_network }

    ::va11y::name $g.f.identity [::vi18n::mc {identity to grant}]
    ::va11y::name $g.f.network [::vi18n::mc {network to scope the grant to}]
    ::va11y::name $g.f.channel [::vi18n::mc {channel to scope the grant to}]
    ::va11y::name $g.f.role [::vi18n::mc {role to grant}]
    ::va11y::tab_order [list $g.f.identity $g.f.network $g.f.channel \
        $g.f.role $g.f.b.ok $g.f.b.cancel]
    bind $g.f.identity <Return> [list ::vacl::grant_do $w $g]
    if {$prefill_identity ne ""} {
        focus $g.f.b.ok
    } else {
        focus $g.f.identity
    }
}

proc ::vacl::grant_do {w g} {
    set identity [string trim [$g.f.identity get]]
    set role [$g.f.role get]
    if {$identity eq ""} {
        $g.f.identity configure -background $::vt::T(select_bg)
        return
    }
    set body [dict create identity $identity role $role]
    set network [string trim [$g.f.network get]]
    set channel [string trim [$g.f.channel get]]
    if {$network ne ""} { dict set body network $network }
    if {$channel ne ""} { dict set body channel $channel }
    set rc [_post $w /api/acl/grant $body]
    if {$rc} { destroy $g }
}

# ── promote: the store's rules, written into config.toml ────────────
#
# Grant and revoke both write the store, and a store reset takes it with
# them — so this overlay could make a decision but could not keep one. The
# path was `/acl export` and a paste, which is how a config file ends up
# holding a rule nobody can explain afterwards.
#
# The dialog asks the engine for the DRY RUN first and shows the report,
# because the report is the point: it names the rules the file already
# decides, the ones that only overlap a line already there, and the ones it
# refuses to write and why. Nothing here decides what promote MEANS — the
# flags go over and the engine's own sentences come back, so the pane
# cannot drift from the command the way the desktop's hand-written command
# table did.

proc ::vacl::promote_ask {w} {
    variable promote_seen
    set promote_seen ""
    set g .acl_promote
    catch { destroy $g }
    toplevel $g
    wm title $g [::vi18n::mc {Promote into config.toml}]
    wm transient $g $w
    ::vplace::centered $g 660 540
    bind $g <Escape> [list destroy $g]

    ttk::frame $g.f
    pack $g.f -fill both -expand 1 -padx 10 -pady 10
    ttk::label $g.f.why -wraplength 620 -justify left \
        -foreground $::vt::T(fg_dim) -text [::vi18n::mc {Runtime grants and users live in the store, and a store reset takes them. Promote appends the ones the store decides to config.toml — the file's comments and key order kept, a backup written beside it, the store copies dropped — and refuses rather than guessing where the file already has an opinion.}]

    ttk::frame $g.f.opts
    ttk::checkbutton $g.f.opts.users -variable ::vacl::promote_users \
        -text [::vi18n::mc {fold runtime users into [[acl.users]] too}] \
        -command [list ::vacl::promote_preview $w $g]
    ttk::checkbutton $g.f.opts.expiring -variable ::vacl::promote_expiring \
        -text [::vi18n::mc {write grants that still expire (the expiry is dropped — a config line cannot expire)}] \
        -command [list ::vacl::promote_preview $w $g]
    ttk::checkbutton $g.f.opts.prune -variable ::vacl::promote_prune \
        -text [::vi18n::mc {drop the store copies the file already decides}] \
        -command [list ::vacl::promote_preview $w $g]
    pack $g.f.opts.users $g.f.opts.expiring $g.f.opts.prune -anchor w -pady 1

    ttk::label $g.f.lr -text [::vi18n::mc {what the engine says}]
    text $g.f.report -height 14 -width 78 -wrap word -state disabled \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -highlightthickness 0 -yscrollcommand [list $g.f.sb set]
    ttk::scrollbar $g.f.sb -orient vertical -command [list $g.f.report yview]

    ttk::frame $g.f.b
    ttk::button $g.f.b.preview -text [::vi18n::mc {Preview}] \
        -command [list ::vacl::promote_preview $w $g]
    ttk::button $g.f.b.write -text [::vi18n::mc {Write it}] \
        -command [list ::vacl::promote_do $w $g]
    ttk::button $g.f.b.close -text [::vi18n::mc {Close}] \
        -command [list destroy $g]
    pack $g.f.b.preview $g.f.b.write -side left -padx {0 6}
    pack $g.f.b.close -side right

    grid $g.f.why -row 0 -column 0 -columnspan 2 -sticky ew
    grid $g.f.opts -row 1 -column 0 -columnspan 2 -sticky ew -pady {8 0}
    grid $g.f.lr -row 2 -column 0 -columnspan 2 -sticky w -pady {8 2}
    grid $g.f.report -row 3 -column 0 -sticky nsew
    grid $g.f.sb -row 3 -column 1 -sticky ns
    grid $g.f.b -row 4 -column 0 -columnspan 2 -sticky ew -pady {10 0}
    grid columnconfigure $g.f 0 -weight 1
    grid rowconfigure $g.f 3 -weight 1

    ::va11y::name $g.f.report [::vi18n::mc {what promote would do, in the engine's words}]
    ::va11y::name $g.f.opts.users [::vi18n::mc {also write runtime users}]
    ::va11y::name $g.f.opts.expiring [::vi18n::mc {also write grants that still have an expiry}]
    ::va11y::name $g.f.opts.prune [::vi18n::mc {also drop the store copies the file already decides}]
    ::va11y::name $g.f.b.preview [::vi18n::mc {show what would be written, and write nothing}]
    ::va11y::name $g.f.b.write [::vi18n::mc {append these rules to config.toml}]
    ::va11y::tab_order [list $g.f.opts.users $g.f.opts.expiring $g.f.opts.prune \
        $g.f.report $g.f.b.preview $g.f.b.write $g.f.b.close]

    promote_preview $w $g
    focus $g.f.b.write
    return $g
}

# The flags as the engine spells them. Booleans go through `json_bool`:
# `_json_value` infers a JSON NUMBER from 1/0, and a serde `bool` field
# answers 422 to a number — the same class of mistake as the one-identity
# array `json_strings` exists for.
proc ::vacl::promote_body {dry} {
    variable promote_users
    variable promote_expiring
    variable promote_prune
    dict create \
        dry_run [::vapi::json_bool $dry] \
        users [::vapi::json_bool $promote_users] \
        include_expiring [::vapi::json_bool $promote_expiring] \
        prune [::vapi::json_bool $promote_prune]
}

# Which options the report on screen was produced from.
proc ::vacl::promote_sig {} {
    variable promote_users
    variable promote_expiring
    variable promote_prune
    return "$promote_users/$promote_expiring/$promote_prune"
}

proc ::vacl::promote_preview {w g} {
    variable promote_seen
    if {[catch { ::vapi::post_json /api/acl/promote [promote_body 1] } resp]} {
        _show_text $g [format [::vi18n::mc {preview failed: %s}] $resp]
        set promote_seen ""
        return 0
    }
    _show_report $g $resp
    set promote_seen [promote_sig]
    return 1
}

proc ::vacl::promote_do {w g} {
    variable promote_seen
    # The report on screen has to be the one these options produced, or
    # "Write it" means something the operator has not read.
    if {$promote_seen ne [promote_sig]} {
        promote_preview $w $g
        _append_report $g [::vi18n::mc {the options changed, so that is a fresh preview — press “Write it” again to write it}]
        return 0
    }
    if {[catch { ::vapi::post_json /api/acl/promote [promote_body 0] } resp]} {
        _show_text $g [format [::vi18n::mc {write failed: %s}] $resp]
        return 0
    }
    _show_report $g $resp
    set wrote 0
    catch { set wrote [dict get $resp wrote] }
    if {$wrote} {
        # The store copies are gone, so the identities list this overlay
        # paints is now wrong until it is repainted.
        load $w
    }
    return 1
}

# Paint the engine's answer. A refusal is a report too, so both land in the
# text widget instead of one of them going to a status label nobody is
# looking at.
proc ::vacl::_show_report {g resp} {
    set text ""
    catch { set text [dict get $resp text] }
    if {$text eq ""} {
        catch { set text [dict get $resp error] }
    }
    if {$text eq ""} {
        set text [::vi18n::mc {the gateway answered without a report}]
    }
    _show_text $g $text
}

# Paint a report that is ALREADY a string — the failure paths, where the
# request never came back and there is no response dict to read a field
# from. Split out of `_show_report` so those paths do not have to invent a
# dict shaped like an API answer just to say "that failed".
proc ::vacl::_show_text {g text} {
    $g.f.report configure -state normal
    $g.f.report delete 1.0 end
    $g.f.report insert end $text
    $g.f.report configure -state disabled
    $g.f.report see 1.0
}

proc ::vacl::_append_report {g line} {
    $g.f.report configure -state normal
    $g.f.report insert end "\n\n$line"
    $g.f.report configure -state disabled
    $g.f.report see end
}

# ── users (cross-network identities) ────────────────────────────────

proc ::vacl::user_ask {w {name ""} {role "trusted"} {identities {}}} {
    set g .acl_user
    catch { destroy $g }
    toplevel $g
    wm title $g [expr {$name eq ""
        ? [::vi18n::mc {New user}] : [::vi18n::mc {Edit user}]}]
    wm transient $g $w
    ::vplace::centered $g 480 340
    bind $g <Escape> [list destroy $g]

    ttk::frame $g.f
    pack $g.f -fill both -expand 1 -padx 10 -pady 10
    ttk::label $g.f.ln -text [::vi18n::mc {name}]
    ttk::entry $g.f.name -width 40
    ttk::label $g.f.lr -text [::vi18n::mc {role}]
    ttk::combobox $g.f.role -state readonly -width 20 \
        -values {owner admin trusted user guest banned}
    ttk::label $g.f.li -text [::vi18n::mc {identities — one per line}]
    ttk::label $g.f.hint -foreground $::vt::T(fg_dim) -wraplength 440 -justify left -text [::vi18n::mc {irc:account · telegram:1234 · or nick!user@host masks — every line that matches this person resolves to the same role, on any network}]
    text $g.f.ids -height 5 -width 46 -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -highlightthickness 0
    ttk::frame $g.f.b
    ttk::button $g.f.b.ok -text [::vi18n::mc {Save}] \
        -command [list ::vacl::user_do $w $g]
    ttk::button $g.f.b.cancel -text [::vi18n::mc {Cancel}] \
        -command [list destroy $g]
    grid $g.f.ln -row 0 -column 0 -sticky w -pady {4 2}
    grid $g.f.name -row 1 -column 0 -sticky ew
    grid $g.f.lr -row 2 -column 0 -sticky w -pady {6 2}
    grid $g.f.role -row 3 -column 0 -sticky w
    grid $g.f.li -row 4 -column 0 -sticky w -pady {8 2}
    grid $g.f.ids -row 5 -column 0 -sticky ew
    grid $g.f.hint -row 6 -column 0 -sticky ew -pady {2 6}
    grid $g.f.b -row 7 -column 0 -pady {8 0}
    grid $g.f.b.ok $g.f.b.cancel -sticky e
    grid columnconfigure $g.f 0 -weight 1

    $g.f.name insert 0 $name
    set i [lsearch -exact {owner admin trusted user guest banned} $role]
    $g.f.role current [expr {$i < 0 ? 2 : $i}]
    foreach id $identities { $g.f.ids insert end "$id\n" }
    if {$name ne ""} { $g.f.name configure -state disabled }

    ::va11y::name $g.f.name [::vi18n::mc {user name}]
    ::va11y::name $g.f.role [::vi18n::mc {user role}]
    ::va11y::name $g.f.ids [::vi18n::mc {user identities, one per line}]
    ::va11y::tab_order [list $g.f.name $g.f.role $g.f.ids $g.f.b.ok $g.f.b.cancel]
    if {$name eq ""} {
        focus $g.f.name
    } else {
        focus $g.f.ids
    }
}

proc ::vacl::user_do {w g} {
    set name [string trim [$g.f.name get]]
    set role [$g.f.role get]
    if {$name eq ""} {
        $g.f.name configure -background $::vt::T(select_bg)
        return
    }
    set identities {}
    foreach line [split [string trim [$g.f.ids get 1.0 end]] "\n"] {
        set line [string trim $line]
        if {$line ne ""} { lappend identities $line }
    }
    # `json_strings`, not a bare list: `_json_value` infers an ARRAY
    # only from a multi-element list, so a user with exactly ONE
    # identity serialized as a JSON string and the endpoint answered
    # 422 — "adding an identity 422s" (PENDING.md). The identities are
    # also free text (a mask has `!` and `@`, a two-word line would be
    # read as an object), so the no-inference constructor is the right
    # one.
    set rc [_post $w /api/acl/user \
        [dict create name $name role $role \
             identities [::vapi::json_strings $identities]]]
    if {$rc} { destroy $g }
}

proc ::vacl::user_edit_selected {w} {
    variable user_data
    set sel [$w.split.list.u selection]
    if {![llength $sel]} {
        $w.bar.status configure -text [::vi18n::mc {select a user first}]
        return 0
    }
    set name [lindex [$w.split.list.u item [lindex $sel 0] -values] 0]
    if {![info exists user_data($name)]} { return 0 }
    set d $user_data($name)
    if {[dict get $d source] eq "config"} {
        ::vi18n::label $w.bar.status {%s comes from config.toml [acl.users] — edit there} $name
        return 0
    }
    user_ask $w $name [dict get $d role] [dict get $d identities]
    return 1
}

proc ::vacl::user_delete_selected {w} {
    variable user_data
    set sel [$w.split.list.u selection]
    if {![llength $sel]} {
        $w.bar.status configure -text [::vi18n::mc {select a user first}]
        return 0
    }
    set name [lindex [$w.split.list.u item [lindex $sel 0] -values] 0]
    if {[info exists user_data($name)]
        && [dict get $user_data($name) source] eq "config"} {
        ::vi18n::label $w.bar.status {%s comes from config.toml [acl.users] — edit there} $name
        return 0
    }
    _post $w /api/acl/user/delete [dict create name $name]
}

# Fetch /api/acl and paint the tables + catalogs. `$rows` stands in for
# the list-of-dicts payload so the renderer is testable without HTTP.
proc ::vacl::load {w} {
    if {[catch { ::vapi::get_json /api/acl } d]} {
        ::vi18n::label $w.bar.status {unavailable: %s} $d
        return 0
    }
    paint $w $d
    return 1
}

proc ::vacl::_scope_text {row} {
    set net ""
    set chan ""
    catch { set net [dict get $row network] }
    catch { set chan [dict get $row channel] }
    if {$net eq "" && $chan eq ""} { return [::vi18n::mc {everywhere}] }
    if {$net ne "" && $chan ne ""} { return "$net / $chan" }
    if {$net ne ""} { return $net }
    return $chan
}

proc ::vacl::paint {w d} {
    variable user_data
    array unset user_data

    # The brake's banner: shown only while engaged, and the LIFT button
    # is hidden for a config-declared one because no frontend can lift
    # that — offering a button that cannot work is worse than offering
    # none.
    set lock ""
    catch { set lock [dict get $d lockdown] }
    if {$lock eq "" || $lock eq "null"} {
        catch { pack forget $w.lock }
    } else {
        set text [::vi18n::mc {lockdown: ON}]
        catch { set text [dict get $lock text] }
        catch { $w.lock.msg configure -text $text }
        set from_config 0
        catch { set from_config [dict get $lock from_config] }
        if {[string is true -strict $from_config] || $from_config eq "1"} {
            catch { pack forget $w.lock.act }
        } else {
            catch { pack $w.lock.act -side right -padx {6 0} }
        }
        catch { pack $w.lock -fill x -padx 8 -pady {4 0} -after $w.bar }
    }
    set t $w.split.list.t
    $t delete [$t children {}]

    set ids {}
    catch { set ids [dict get $d identities] }
    foreach row $ids {
        set identity [dict get $row identity]
        set role [dict get $row role]
        set source [dict get $row source]
        set eff $role
        catch { set eff [dict get $row effective_role] }
        if {$eff ne $role} {
            set shown [format [::vi18n::mc {%s (%s)}] $role $eff]
        } else {
            set shown $role
        }
        set tag ""
        if {$eff eq "owner"}   { set tag acl_owner }
        if {$eff eq "banned"}  { set tag acl_banned }
        set values [list $identity $shown [_scope_text $row] $source]
        if {$tag eq ""} {
            $t insert {} end -id $identity -values $values
        } else {
            $t insert {} end -id $identity -values $values -tags [list $tag]
        }
    }

    # Users table.
    set u $w.split.list.u
    $u delete [$u children {}]
    set users {}
    catch { set users [dict get $d users] }
    foreach row $users {
        set name [dict get $row name]
        set role [dict get $row role]
        set source [dict get $row source]
        set identities {}
        catch { set identities [dict get $row identities] }
        set user_data($name) $row
        set tag ""
        if {$role eq "owner"}  { set tag acl_owner }
        if {$role eq "banned"} { set tag acl_banned }
        set values [list $name $role [join $identities {, }] $source]
        if {$tag eq ""} {
            $u insert {} end -id $name -values $values
        } else {
            $u insert {} end -id $name -values $values -tags [list $tag]
        }
    }

    set h $w.split.help
    $h configure -state normal
    $h delete 1.0 end
    $h tag configure h2 -foreground $::vt::T(pink) -font {TkDefaultFont 11 bold}
    $h tag configure dim -foreground $::vt::T(fg_dim)
    $h tag configure warn -foreground $::vt::T(gold)

    if {![dict get $d configured]} {
        $h insert end "[::vi18n::mc {acl not configured (legacy mode)}]\n" warn
        $h insert end "[::vi18n::mc {No {[}acl{]} section in config.toml: every sender that reaches the gateway is treated as owner, the pre-ACL single-user behaviour. Granting and revoking still works and takes effect the moment {[}acl{]} exists.}]\n\n" dim
    } else {
        set owners [dict get $d owners]
        if {[llength $owners]} {
            $h insert end [format [::vi18n::mc {owners: %s}] [join $owners {, }]] ""
            $h insert end "\n"
        }
        set def [dict get $d default_role]
        if {$def ne "" && $def ne "null"} {
            $h insert end [format [::vi18n::mc {default role for new identities: %s}] $def] ""
            $h insert end "\n\n"
        }
    }

    $h insert end "[::vi18n::mc {IRC identities}]\n" h2
    $h insert end "[::vi18n::mc {verified account: irc:name (strong) · mask: nick!user@host — * wildcards or re: regex in each segment, scoped per network/channel (weaker: nick and ident are choosable). Users group identities across networks under one role.}]\n" dim
    $h insert end "\n[::vi18n::mc {Roles}]\n" h2
    foreach r [dict get $d role_catalog] {
        $h insert end "[dict get $r name]\t"
        $h insert end "[dict get $r desc] " dim
        $h insert end "\n"
    }
    $h insert end "\n[::vi18n::mc {Capabilities}]\n" h2
    foreach c [dict get $d cap_catalog] {
        $h insert end "[dict get $c name]\t"
        $h insert end "[dict get $c desc] " dim
        $h insert end "\n"
    }
    $h configure -state disabled
    set n [llength $ids]
    set nu [llength $users]
    ::vi18n::label $w.bar.status {%d identities · %d users} $n $nu
}
