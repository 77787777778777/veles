# veles-desktop/lib/failgraph.tcl — a blueprint editor for failover.
#
# USER: "please add a flowchart/blueprint editor for failover scripting".
#
# Failover is two things that only make sense together, and config.toml
# shows them a page apart:
#
#   fallback_providers   an ORDERED CHAIN of routes to try
#   [[failover]]         per-scope POLICY: what happens when one fails —
#                        ask / auto / stop / script, a hop cap, and where
#                        aux work goes while that scope is throttled
#
# Reading a chain and its rules out of TOML and holding the resulting
# control flow in your head is the actual difficulty. So: draw it. Each
# route is a node, each failure is an edge, and the edge is LABELLED
# with what that scope's mode does. `script` nodes show where the Tcl
# hook fires and can veto the hop.
#
# ── how it persists ─────────────────────────────────────────────────
#
# Three routes, narrowest first:
#
#   Apply to session   changes the RUNNING session now, through the
#                      same /failover endpoint the slash command uses
#   Copy TOML          emits the exact [[failover]] blocks to paste
#   Save globally      writes them into config.toml (_save_global →
#                      POST /api/config/failover), keeping the previous
#                      file as config.toml.bak
#
# The write is SURGICAL, never a re-serialization: the engine edits the
# failover keys in place and leaves every other line, and every comment,
# byte-identical. Re-emitting a file the user hand-wrote would lose
# their comments and ordering, which is the whole reason it works this
# way. (This comment used to claim "there is no config writer anywhere
# in this codebase" — it went stale the day Save globally shipped.)
#
# The model, layout and TOML emission below are PURE — no canvas, no
# widgets — because that is where the logic worth testing lives.
# lib/fgtest.tcl exercises them directly.

namespace eval ::vfg {
    variable model {}       ;# dict: chain (list) + rules (list)
    variable sel -1         ;# selected node index
    variable node_at        ;# canvas item -> node index
}

# ── the model ───────────────────────────────────────────────────────

# Build the editable model from /api/config.
#
# `chain` is the primary route followed by fallback_providers, because
# that IS the order the engine tries them — showing the fallbacks alone
# would draw a graph that starts in the middle.
proc ::vfg::from_config {cfg} {
    set chain {}
    set dp ""
    set dm ""
    catch { set dp [dict get $cfg default_provider] }
    catch { set dm [dict get $cfg default_model] }
    if {$dp ne "" || $dm ne ""} {
        lappend chain [dict create provider $dp model $dm primary 1]
    }
    if {[dict exists $cfg fallback_providers]} {
        foreach f [dict get $cfg fallback_providers] {
            set fp ""
            set fm ""
            catch { set fp [dict get $f provider] }
            catch { set fm [dict get $f model] }
            if {$fm eq "null"} { set fm "" }
            lappend chain [dict create provider $fp model $fm primary 0]
        }
    }
    set rules {}
    if {[dict exists $cfg failover]} {
        foreach r [dict get $cfg failover] { lappend rules [_norm_rule $r] }
    }
    return [dict create chain $chain rules $rules]
}

proc ::vfg::_norm_rule {r} {
    set out [dict create provider "" model "" mode ask script "" max_hops "" \
                         wait_default_secs "" aux_provider "" aux_model ""]
    foreach {k src} {provider provider model model mode mode script script \
                     max_hops max_hops wait_default_secs wait_default_secs \
                     aux_provider aux_fallback_provider \
                     aux_model aux_fallback_model} {
        if {[dict exists $r $src]} {
            set v [dict get $r $src]
            if {$v ne "null" && $v ne ""} { dict set out $k $v }
        }
    }
    return $out
}

# The rule governing a route. FIRST MATCH WINS, and "*"/empty is a
# wildcard — same precedence the engine uses, so the picture cannot
# disagree with the behaviour.
proc ::vfg::rule_for {model_ provider model} {
    foreach r [dict get $model_ rules] {
        set rp [dict get $r provider]
        set rm [dict get $r model]
        if {$rp ne "" && $rp ne "*" && $rp ne $provider} { continue }
        if {$rm ne "" && $rm ne "*" && $rm ne $model} { continue }
        return $r
    }
    # No rule: the engine's implicit default.
    return [dict create provider "" model "" mode ask script "" max_hops "" \
                        wait_default_secs "" aux_provider "" aux_model ""]
}

# What the edge LEAVING this node means, in words.
proc ::vfg::edge_label {mode is_last} {
    switch -- $mode {
        auto   { return [expr {$is_last ? "fails → give up" : "fails → next, silently"}] }
        ask    { return [expr {$is_last ? "fails → ask, then give up" : "fails → ask you first"}] }
        stop   { return "fails → stop the turn" }
        script { return [expr {$is_last ? "fails → hook, then give up" : "fails → hook may veto"}] }
        default { return "fails → next" }
    }
}

# A `stop` node ends the chain: nothing after it is reachable, and
# saying so is the whole value of drawing this.
proc ::vfg::reachable_count {model_} {
    set chain [dict get $model_ chain]
    set n 0
    foreach node $chain {
        incr n
        set r [rule_for $model_ [dict get $node provider] [dict get $node model]]
        if {[dict get $r mode] eq "stop"} { return $n }
    }
    return $n
}

# ── layout (pure) ───────────────────────────────────────────────────

# Node boxes for the chain, top to bottom. Returns a list of dicts:
# {x y w h index provider model mode edge unreachable}.
proc ::vfg::layout {model_ {x 40} {y 30} {w 300} {h 62} {gap 46}} {
    set out {}
    set chain [dict get $model_ chain]
    set reach [reachable_count $model_]
    set i 0
    foreach node $chain {
        set p [dict get $node provider]
        set m [dict get $node model]
        set r [rule_for $model_ $p $m]
        set is_last [expr {$i == [llength $chain] - 1}]
        lappend out [dict create \
            index $i x $x y [expr {$y + $i * ($h + $gap)}] w $w h $h \
            provider $p model $m primary [dict get $node primary] \
            mode [dict get $r mode] \
            max_hops [dict get $r max_hops] \
            aux_provider [dict get $r aux_provider] \
            aux_model [dict get $r aux_model] \
            edge [edge_label [dict get $r mode] $is_last] \
            unreachable [expr {$i >= $reach}]]
        incr i
    }
    return $out
}

# ── TOML emission (pure) ────────────────────────────────────────────

proc ::vfg::_toml_str {s} {
    return "\"[string map {\\ \\\\ \" \\\"} $s]\""
}

# The [[failover]] blocks for this model, ready to paste. Only rules
# that say something are emitted — a block that is all defaults is
# noise in a config file.
proc ::vfg::to_toml {model_} {
    set lines {}
    lappend lines "# generated by the blueprint editor — paste into"
    # The real path, not a description of it: this line is pasted next to
    # a config file, and "the config dir" sends the operator looking while
    # the desktop knows the answer exactly.
    lappend lines "# [::vpaths::config_file], replacing the existing"
    lappend lines "# fallback_providers and \[\[failover\]\] blocks."
    lappend lines ""

    # The CHAIN. The primary is [model] provider/default, not a fallback,
    # so it is reported as a comment rather than silently emitted as one
    # — pasting it into fallback_providers would try the primary twice.
    set chain [dict get $model_ chain]
    set fallbacks {}
    foreach node $chain {
        if {[dict get $node primary]} {
            lappend lines "# primary route (\[model\] provider/default, not shown here):"
            lappend lines "#   [dict get $node provider] / [dict get $node model]"
            continue
        }
        lappend fallbacks $node
    }
    if {[llength $fallbacks]} {
        lappend lines "fallback_providers = \["
        foreach node $fallbacks {
            set entry "  { provider = [_toml_str [dict get $node provider]]"
            if {[dict get $node model] ne ""} {
                append entry ", model = [_toml_str [dict get $node model]]"
            }
            append entry " },"
            lappend lines $entry
        }
        lappend lines "\]"
    } else {
        lappend lines "fallback_providers = \[\]   # no fallbacks — the primary is the whole chain"
    }
    set any 0
    foreach r [dict get $model_ rules] {
        set body {}
        foreach {key field} {provider provider model model mode mode} {
            set v [dict get $r $field]
            if {$v ne ""} { lappend body "$key = [_toml_str $v]" }
        }
        foreach {key field} {max_hops max_hops wait_default_secs wait_default_secs} {
            set v [dict get $r $field]
            if {$v ne "" && [string is integer -strict $v]} { lappend body "$key = $v" }
        }
        foreach {key field} {aux_fallback_provider aux_provider aux_fallback_model aux_model} {
            set v [dict get $r $field]
            if {$v ne ""} { lappend body "$key = [_toml_str $v]" }
        }
        # A named script only matters in `script` mode; emit it when set.
        # Guarded: rule dicts constructed outside _norm_rule may lack the
        # key, and a missing key must read as "no script", not an error.
        if {[dict exists $r script] && [dict get $r script] ne ""} {
            lappend body "script = [_toml_str [dict get $r script]]"
        }
        # Does this rule SAY anything? provider+model only scope it, and
        # mode=ask is the engine's implicit default — a block carrying
        # just those is noise in a hand-written config.
        set says_something 0
        if {[dict get $r mode] ne "" && [dict get $r mode] ne "ask"} { set says_something 1 }
        foreach f {max_hops wait_default_secs aux_provider aux_model} {
            if {[dict get $r $f] ne ""} { set says_something 1 }
        }
        if {[dict exists $r script] && [dict get $r script] ne ""} { set says_something 1 }
        if {!$says_something} { continue }
        lappend lines ""
        lappend lines "\[\[failover\]\]"
        foreach b $body { lappend lines $b }
        set any 1
    }
    if {!$any} {
        lappend lines ""
        lappend lines "# (nothing to write — every scope is on the default policy)"
    }
    return [join $lines "\n"]
}

# ── model edits (pure) ──────────────────────────────────────────────

# Set a field of the rule governing node $i, creating a scoped rule when
# the node is currently covered by a wildcard or by no rule at all.
# Editing one route must not silently retune every other route that
# happened to share a wildcard.
proc ::vfg::set_field {model_ i field value} {
    set chain [dict get $model_ chain]
    if {$i < 0 || $i >= [llength $chain]} { return $model_ }
    set node [lindex $chain $i]
    set p [dict get $node provider]
    set m [dict get $node model]
    set rules [dict get $model_ rules]

    set found -1
    set j 0
    foreach r $rules {
        if {[dict get $r provider] eq $p && [dict get $r model] eq $m} { set found $j ; break }
        incr j
    }
    if {$found < 0} {
        set r [dict create provider $p model $m mode ask script "" max_hops "" \
                           wait_default_secs "" aux_provider "" aux_model ""]
        dict set r $field $value
        # Scoped rules must come BEFORE wildcards or first-match-wins
        # would never reach them.
        set rules [linsert $rules 0 $r]
    } else {
        set r [lindex $rules $found]
        dict set r $field $value
        lset rules $found $r
    }
    dict set model_ rules $rules
    return $model_
}

# Add a route AFTER index $i (use -1 for "at the front of the
# fallbacks"). New routes are always fallbacks — the PRIMARY is
# `[model] provider/default`, a different setting entirely.
proc ::vfg::add_node {model_ i provider model} {
    set chain [dict get $model_ chain]
    set node [dict create provider $provider model $model primary 0]
    set at [expr {$i + 1}]
    if {$at < 1} { set at 1 }
    if {$at > [llength $chain]} { set at [llength $chain] }
    set chain [linsert $chain $at $node]
    dict set model_ chain $chain
    return $model_
}

# Remove a route. The PRIMARY cannot be removed here: it is not part of
# fallback_providers, it is `[model] provider` / `[model] default`, and
# quietly deleting it would emit a chain that starts at the wrong route.
# Returns the model unchanged when asked to.
proc ::vfg::remove_node {model_ i} {
    set chain [dict get $model_ chain]
    if {$i < 0 || $i >= [llength $chain]} { return $model_ }
    if {[dict get [lindex $chain $i] primary]} { return $model_ }
    set chain [lreplace $chain $i $i]
    dict set model_ chain $chain
    return $model_
}

proc ::vfg::can_remove {model_ i} {
    set chain [dict get $model_ chain]
    if {$i < 0 || $i >= [llength $chain]} { return 0 }
    return [expr {![dict get [lindex $chain $i] primary]}]
}

proc ::vfg::move_node {model_ i delta} {
    set chain [dict get $model_ chain]
    set j [expr {$i + $delta}]
    if {$i < 0 || $i >= [llength $chain] || $j < 0 || $j >= [llength $chain]} {
        return $model_
    }
    set a [lindex $chain $i]
    lset chain $i [lindex $chain $j]
    lset chain $j $a
    dict set model_ chain $chain
    return $model_
}

# ── the editor window ───────────────────────────────────────────────

proc ::vfg::open {} {
    variable model
    variable sel
    set w .vfg
    if {[winfo exists $w]} { raise $w ; return }

    set cfg {}
    if {[catch { ::vapi::get_json /api/config } cfg]} {
        ::vstatus::set_msg [::vi18n::mc {failover blueprint: cannot reach the gateway}]
        return
    }
    set model [from_config $cfg]
    set sel -1

    toplevel $w
    wm title $w [::vi18n::mc {Failover blueprint}]
    wm geometry $w 980x640
    wm minsize $w 700 420
    catch { ::vtitle::attach $w "Failover blueprint" -close [list destroy $w] }

    # A SASH between the drawing and the inspector.
    #
    # USER: "the buttons are now clipped because there's too many of them
    # and the actual blueprint display area can't be resized to fix it".
    # The side panel was a fixed-width frame with `pack propagate 0`, so
    # it could neither grow to fit its buttons nor be traded against the
    # canvas.
    ttk::panedwindow $w.split -orient horizontal
    ttk::frame $w.main
    canvas $w.main.c -background $::vt::T(bg_input) -highlightthickness 0 \
        -yscrollcommand [list $w.main.sb set]
    ttk::scrollbar $w.main.sb -orient vertical -command [list $w.main.c yview]
    pack $w.main.sb -side right -fill y
    pack $w.main.c -side left -fill both -expand 1

    # The inspector edits the SELECTED node's rule.
    ttk::frame $w.side
    ttk::label $w.side.title -text [::vi18n::mc {select a route}] \
        -font {TkDefaultFont 10 bold} -foreground $::vt::T(accent)
    pack $w.side.title -anchor w -padx 10 -pady {10 6}

    ttk::label $w.side.ml -text [::vi18n::mc {on failure}] -foreground $::vt::T(fg_dim)
    pack $w.side.ml -anchor w -padx 10
    ttk::frame $w.side.modes
    foreach {m label} {ask "ask me first" auto "advance silently"
                       stop "stop the turn" script "fire the Tcl hook"} {
        ttk::radiobutton $w.side.modes.$m -value $m \
            -variable ::vfg::ui_mode -command [list ::vfg::_set mode]
        ::vi18n::label $w.side.modes.$m {%1$s — %2$s} $m $label
        pack $w.side.modes.$m -anchor w
    }
    pack $w.side.modes -anchor w -padx 16 -pady {2 8}

    # `script` runs a Tcl hook. Offering the mode without offering the
    # hook is what made it unusable — so the button is always here, next
    # to the radio that needs it.
    ttk::button $w.side.script -text [::vi18n::mc {edit the hook script…}] \
        -command { ::vfg::edit_script }
    pack $w.side.script -anchor w -padx 16 -pady {0 4}

    # A NAMED script for this rule (multiple scripts, not just the one).
    # Empty = the default hook. The "…" button lists the library and
    # opens the script editor on the selection.
    ttk::label $w.side.l_script -text [::vi18n::mc {script (named)}] -foreground $::vt::T(fg_dim)
    pack $w.side.l_script -anchor w -padx 10 -pady {0 0}
    ttk::frame $w.side.srow
    ttk::entry $w.side.e_script -width 18
    ttk::button $w.side.srow.pick -text "…" -width 3 -command ::vfg::pick_script
    ttk::button $w.side.srow.edit -text [::vi18n::mc {edit}] -command { ::vfg::edit_script_named }
    pack $w.side.e_script -side left
    pack $w.side.srow.pick -side left -padx 2
    pack $w.side.srow.edit -side left -padx 2
    pack $w.side.srow -anchor w -padx 10 -pady {0 8}
    bind $w.side.e_script <FocusOut> [list ::vfg::_set script]
    bind $w.side.e_script <Return>   [list ::vfg::_set script]

    foreach {f label hint} {
        max_hops          "max hops"          "give up after this many advances"
        wait_default_secs "wait (seconds)"    "cooldown before retrying a scope"
        aux_provider      "aux provider"      "where aux work goes while throttled"
        aux_model         "aux model"         ""
    } {
        ttk::label $w.side.l_$f -text $label -foreground $::vt::T(fg_dim)
        pack $w.side.l_$f -anchor w -padx 10 -pady {6 0}
        ttk::entry $w.side.e_$f -width 26
        pack $w.side.e_$f -anchor w -padx 10
        bind $w.side.e_$f <FocusOut> [list ::vfg::_set $f]
        bind $w.side.e_$f <Return>   [list ::vfg::_set $f]
        if {$hint ne ""} {
            ttk::label $w.side.h_$f -text $hint -foreground $::vt::T(fg_dim) \
                -font {TkDefaultFont 8}
            pack $w.side.h_$f -anchor w -padx 10
        }
    }

    ttk::frame $w.side.ord
    ttk::button $w.side.ord.up -text [::vi18n::mc {▲ earlier}] -width 10 -command { ::vfg::_move -1 }
    ttk::button $w.side.ord.dn -text [::vi18n::mc {▼ later}]   -width 10 -command { ::vfg::_move 1 }
    grid $w.side.ord.up $w.side.ord.dn -padx 2 -pady 2
    pack $w.side.ord -anchor w -padx 8 -pady {8 0}

    # ADD / REMOVE. add_node/remove_node existed in the model from the
    # start but nothing in the UI ever called them, so the editor could
    # only re-order and re-policy what config.toml already had.
    ttk::label $w.side.addl -text [::vi18n::mc {add a fallback route}] \
        -foreground $::vt::T(fg_dim)
    pack $w.side.addl -anchor w -padx 10 -pady {10 0}
    ttk::frame $w.side.add
    ttk::entry $w.side.add.prov -width 12
    ttk::entry $w.side.add.model -width 12
    pack $w.side.add.prov $w.side.add.model -side left -padx 2
    pack $w.side.add -anchor w -padx 8
    ttk::frame $w.side.addbtn
    ttk::button $w.side.addbtn.add -text [::vi18n::mc {+ add route}] -width 12 -command ::vfg::_add
    ttk::button $w.side.addbtn.del -text [::vi18n::mc {− remove}]    -width 10 -command ::vfg::_remove
    grid $w.side.addbtn.add $w.side.addbtn.del -padx 2 -pady 2
    pack $w.side.addbtn -anchor w -padx 8 -pady {2 0}

    ttk::frame $w.side.act
    ttk::button $w.side.act.apply -text [::vi18n::mc {Apply to session}] -width 16 -command ::vfg::_apply
    ttk::button $w.side.act.toml  -text [::vi18n::mc {Copy TOML}]        -width 16 -command ::vfg::_copy_toml
    ttk::button $w.side.act.save  -text [::vi18n::mc {Save globally}]    -width 16 -command ::vfg::_save_global
    # Stacked, not packed side-by-side: three buttons on one row is what
    # clipped, and a sash cannot rescue a row that never wraps.
    grid $w.side.act.apply -sticky w -pady 1
    grid $w.side.act.toml  -sticky w -pady 1
    grid $w.side.act.save  -sticky w -pady 1
    pack $w.side.act -anchor w -padx 8 -pady {8 4}

    ttk::label $w.side.note -wraplength 280 -justify left \
        -foreground $::vt::T(fg_dim) -font {TkDefaultFont 8} \
        -text [::vi18n::mc {Apply changes the RUNNING session only. Copy TOML gives you the blocks to paste. Save globally splices them into config.toml — only the failover sections are touched, every other line and comment is left alone, and the old file is kept as config.toml.bak.}]
    pack $w.side.note -anchor w -padx 10 -pady {4 10}

    $w.split add $w.main -weight 3
    $w.split add $w.side -weight 1
    pack $w.split -fill both -expand 1

    bind $w <Escape> [list destroy $w]
    catch { ::vtext::restyle_all $w }
    _redraw
}

proc ::vfg::_redraw {} {
    variable model
    variable sel
    variable node_at
    set c .vfg.main.c
    if {![winfo exists $c]} { return }
    $c delete all
    array unset node_at

    set lay [layout $model]
    if {![llength $lay]} {
        # TWO keys, one per LINE. This was a single braced literal with
        # a real newline inside it, and the catalog is line-oriented:
        # the generator wrote the two lines as two separate en.msg
        # entries while the code asked for the joined string, so the
        # lookup MISSED at runtime and the whole empty state fell back
        # to English - one key no translator could ever reach, plus two
        # entries no code used. Asking for each line separately makes
        # the code and the catalog agree.
        $c create text 30 30 -anchor nw -fill $::vt::T(fg_dim) \
            -text "[::vi18n::mc {No routes configured.}]\n[::vi18n::mc {Set [model] provider/default in config.toml.}]"
        return
    }
    foreach box $lay {
        set x [dict get $box x]
        set y [dict get $box y]
        set bw [dict get $box w]
        set bh [dict get $box h]
        set i [dict get $box index]
        set unreach [dict get $box unreachable]

        set outline [expr {$i == $sel ? $::vt::T(accent) : $::vt::T(hairline)}]
        set fill [expr {$unreach ? $::vt::T(bg_panel) : $::vt::T(bg)}]
        set fg [expr {$unreach ? $::vt::T(fg_dim) : $::vt::T(fg)}]

        set r [$c create rectangle $x $y [expr {$x + $bw}] [expr {$y + $bh}] \
            -outline $outline -width [expr {$i == $sel ? 2 : 1}] -fill $fill]
        set node_at($r) $i

        set badge [expr {[dict get $box primary] ? "primary" : "fallback [expr {$i}]"}]
        set t1 [$c create text [expr {$x + 12}] [expr {$y + 10}] -anchor nw -fill $fg \
            -font {TkDefaultFont 10 bold} \
            -text "[dict get $box provider] / [dict get $box model]" ;# i18n-exempt: provider/model identifiers
        set sub "$badge · mode [dict get $box mode]"
        if {[dict get $box max_hops] ne ""} { append sub " · max [dict get $box max_hops] hops" }
        if {[dict get $box aux_provider] ne ""} {
            append sub "\naux → [dict get $box aux_provider]/[dict get $box aux_model]"
        }
        if {$unreach} { append sub "\nunreachable — an earlier route stops the turn" }
        set t2 [$c create text [expr {$x + 12}] [expr {$y + 28}] -anchor nw \
            -fill $::vt::T(fg_dim) -font {TkDefaultFont 9} -text $sub]
        foreach it [list $r $t1 $t2] {
            set node_at($it) $i
            $c bind $it <Button-1> [list ::vfg::_select $i]
        }

        # The edge to the next route, labelled with what the mode does.
        set ny [expr {$y + $bh}]
        set gapend [expr {$ny + 46}]
        set cx [expr {$x + $bw / 2}]
        $c create line $cx $ny $cx [expr {$gapend - 8}] -fill $::vt::T(hairline) -width 2
        $c create polygon $cx $gapend [expr {$cx - 5}] [expr {$gapend - 9}] \
            [expr {$cx + 5}] [expr {$gapend - 9}] -fill $::vt::T(hairline)
        $c create text [expr {$cx + 12}] [expr {$ny + 16}] -anchor nw \
            -fill $::vt::T(fg_dim) -font {TkDefaultFont 9 italic} \
            -text [dict get $box edge]
    }
    # A terminal node, so the chain visibly ENDS rather than trailing off.
    set last [lindex $lay end]
    set ey [expr {[dict get $last y] + [dict get $last h] + 46}]
    set ex [dict get $last x]
    $c create rectangle $ex $ey [expr {$ex + 300}] [expr {$ey + 30}] \
        -outline $::vt::T(hairline) -dash {3 3} -fill ""
    $c create text [expr {$ex + 12}] [expr {$ey + 8}] -anchor nw \
        -fill $::vt::T(fg_dim) -text [::vi18n::mc {turn fails — no routes left}]
    $c configure -scrollregion [$c bbox all]
    _load_inspector
}

proc ::vfg::_select {i} {
    variable sel
    set sel $i
    _redraw
}

proc ::vfg::_load_inspector {} {
    variable model
    variable sel
    set w .vfg
    if {![winfo exists $w]} { return }
    set chain [dict get $model chain]
    if {$sel < 0 || $sel >= [llength $chain]} {
        $w.side.title configure -text [::vi18n::mc {select a route}]
        return
    }
    set node [lindex $chain $sel]
    set p [dict get $node provider]
    set m [dict get $node model]
    $w.side.title configure -text "$p / $m"
    set r [rule_for $model $p $m]
    set ::vfg::ui_mode [dict get $r mode]
    foreach f {max_hops wait_default_secs aux_provider aux_model script} {
        $w.side.e_$f delete 0 end
        $w.side.e_$f insert 0 [dict get $r $f]
    }
}

proc ::vfg::_set {field} {
    variable model
    variable sel
    if {$sel < 0} { return }
    set w .vfg
    if {$field eq "mode"} {
        set v $::vfg::ui_mode
    } else {
        set v [string trim [$w.side.e_$field get]]
    }
    set model [set_field $model $sel $field $v]
    _redraw
}

proc ::vfg::_add {} {
    variable model
    variable sel
    set w .vfg
    set p [string trim [$w.side.add.prov get]]
    set m [string trim [$w.side.add.model get]]
    if {$p eq ""} {
        ::vstatus::set_msg [::vi18n::mc {give the new route a provider first}]
        return
    }
    # After the selection, or at the end when nothing is selected.
    set at [expr {$sel >= 0 ? $sel : [llength [dict get $model chain]] - 1}]
    set model [add_node $model $at $p $m]
    set sel [expr {$at + 1}]
    $w.side.add.prov delete 0 end
    $w.side.add.model delete 0 end
    _redraw
    ::vstatus::set_msg [format [::vi18n::mc {added %1$s/%2$s — Save globally to keep it}] $p $m]
}

proc ::vfg::_remove {} {
    variable model
    variable sel
    if {$sel < 0} { ::vstatus::set_msg [::vi18n::mc {select a route first}] ; return }
    if {![can_remove $model $sel]} {
        ::vstatus::set_msg [::vi18n::mc {the primary route is \[model\] provider/default — change it there, not here}]
        return
    }
    set model [remove_node $model $sel]
    set sel -1
    _redraw
    ::vstatus::set_msg [::vi18n::mc {route removed — Save globally to keep it}]
}

proc ::vfg::_move {delta} {
    variable model
    variable sel
    if {$sel < 0} { return }
    set model [move_node $model $sel $delta]
    set sel [expr {$sel + $delta}]
    _redraw
}

# Apply the selected route's mode to the RUNNING session, through the
# same endpoint /failover uses. Config is untouched.
proc ::vfg::_apply {} {
    variable model
    variable sel
    if {$sel < 0} { ::vstatus::set_msg [::vi18n::mc {select a route first}] ; return }
    set sid ""
    catch { set sid $::vchat::session }
    if {$sid eq ""} { ::vstatus::set_msg [::vi18n::mc {no session open}] ; return }
    set mode $::vfg::ui_mode
    if {[catch { ::vapi::post_json "/api/sessions/$sid/failover" \
                    [dict create mode $mode] } r]} {
        ::vstatus::set_msg [format [::vi18n::mc {apply failed: %s}] $r]
        return
    }
    ::vstatus::set_msg [format [::vi18n::mc {session failover mode → %s (config unchanged)}] $mode]
}

# Write the failover sections into config.toml.
#
# The endpoint SPLICES: it replaces only fallback_providers and the
# [[failover]] blocks, keeps every other line and comment byte-identical,
# refuses to write anything that would not parse, and keeps the previous
# file as config.toml.bak. A running gateway keeps its loaded config
# until restarted, and the reply says so.
proc ::vfg::_save_global {} {
    variable model
    set t [to_toml $model]
    if {[catch { ::vapi::post_json /api/config/failover \
        [dict create toml [::vapi::json_string $t]] } r]} {
        ::vstatus::set_msg [format [::vi18n::mc {save failed: %s}] $r]
        return
    }
    set saved ""
    catch { set saved [dict get $r saved] }
    if {$saved eq ""} {
        set err ""
        catch { set err [dict get $r error] }
        ::vstatus::set_msg [expr {$err ne "" \
            ? [::vi18n::mc {save refused: %s} $err] : [::vi18n::mc {save failed}]}]
        return
    }
    ::vstatus::set_msg [format [::vi18n::mc {saved to %1$s (backup: config.toml.bak) — restart to load it}] $saved]
}

proc ::vfg::_copy_toml {} {
    variable model
    set t [to_toml $model]
    catch { clipboard clear ; clipboard append $t }
    # Through the kill ring too, so C-y reaches it.
    catch { ::vemacs::absorb_external_copy }
    ::vstatus::set_msg [::vi18n::mc {TOML copied — paste it into config.toml}]
}

# ── the script behind `script` mode ─────────────────────────────────
#
# USER: "where/how do you see/edit the tcl script when set to script?"
#
# Nowhere, was the answer. `script` fires the `failover` hook; hooks
# come from the config dir's plugins//<name>/plugin.tcl; nothing in the desktop
# ever showed you one. On a box where no plugin provides that hook the
# mode did nothing at all — `auto` plus a round trip — and the editor
# offered it anyway.
#
# So the mode now opens the thing it runs: the handler itself, with the
# hook's real contract in front of it, saved through an endpoint that
# SOURCES the script before installing it. A plugin.tcl with an
# unbalanced brace is not a bad save, it is a broken hook that fires at
# the worst possible moment.

# What does the editor's title bar say about a mode?
proc ::vfg::script_status {loaded exists} {
    if {!$loaded} { return "this plugin is disabled — script mode will fire nothing" }
    if {!$exists} { return "no handler yet — this is a starting template" }
    return "loaded: every failed route runs failover::handle"
}

# Open the script named in the rule's `script` field (empty = default).
proc ::vfg::edit_script_named {} {
    set name [string trim [.vfg.side.e_script get]]
    ::vfg::edit_script $name
}

# List the named scripts and open one. Typing a fresh name into the field
# and hitting "edit" is how a NEW script starts — it returns the template.
proc ::vfg::pick_script {} {
    if {[catch { ::vapi::get_json /api/failover/scripts } r]} {
        ::vstatus::set_msg [::vi18n::mc {cannot list scripts (gateway offline?)}]
        return
    }
    set names {}
    catch { set names [dict get $r scripts] }
    if {![llength $names]} {
        ::vstatus::set_msg [::vi18n::mc {no named scripts yet — type a name in the field and hit edit}]
        return
    }
    catch { destroy .vfgpick }
    toplevel .vfgpick
    wm title .vfgpick [::vi18n::mc {Choose a failover script}]
    wm geometry .vfgpick 320x320
    listbox .vfgpick.list -exportselection 0 -font {monospace 10} \
        -background $::vt::T(bg_panel) -foreground $::vt::T(fg)
    foreach n $names { .vfgpick.list insert end $n }
    pack .vfgpick.list -fill both -expand 1 -padx 6 -pady 6
    if {[llength $names]} { .vfgpick.list selection set 0 }
    bind .vfgpick.list <Double-Button-1> { ::vfg::_pick_use }
    bind .vfgpick.list <Return> { ::vfg::_pick_use ; break }
    bind .vfgpick <Escape> { destroy .vfgpick }
}

proc ::vfg::_pick_use {} {
    set sel [.vfgpick.list curselection]
    if {$sel eq ""} { destroy .vfgpick ; return }
    set name [.vfgpick.list get [lindex $sel 0]]
    destroy .vfgpick
    # Write the name into the rule AND open it in the editor, so picking
    # is one action, not two.
    set ::vfg::model [::vfg::set_field $::vfg::model $::vfg::sel script $name]
    ::vfg::_redraw
    ::vfg::edit_script $name
}

proc ::vfg::edit_script {{name ""}} {
    variable script_path
    variable script_name
    set w .vfgscript
    if {[winfo exists $w] && ($name eq "" || $script_name eq $name)} {
        raise $w ; focus $w.body ; return
    }
    if {[winfo exists $w]} { destroy $w }

    set query /api/failover/script
    if {$name ne ""} { append query "?name=[::vapi::json_escape $name]" }
    if {[catch { ::vapi::get_json $query } r]} {
        ::vstatus::set_msg [::vi18n::mc {failover script: cannot reach the gateway}]
        return
    }
    set script_name $name
    set err ""
    catch { set err [dict get $r error] }
    if {$err ne ""} { ::vstatus::set_msg $err ; return }
    set src ""
    set path ""
    set exists 0
    set loaded 1
    catch { set src    [dict get $r source] }
    catch { set path   [dict get $r path] }
    catch { set exists [dict get $r exists] }
    catch { set loaded [dict get $r loaded] }
    set script_path $path

    toplevel $w
    wm title $w [expr {$script_name eq "" ? "Failover hook (default)" : "Failover script: $script_name"}]
    wm geometry $w 820x600
    wm minsize $w 560 360
    catch { ::vtitle::attach $w "Failover hook" -close [list destroy $w] }

    ttk::frame $w.head
    ttk::label $w.head.path -text $path -foreground $::vt::T(fg_dim) \
        -font {monospace 9}
    ttk::label $w.head.state -text [script_status $loaded $exists] \
        -foreground [expr {$loaded ? $::vt::T(fg_dim) : $::vt::T(gold)}] \
        -font {TkDefaultFont 9}
    pack $w.head.path -anchor w -padx 10 -pady {8 0}
    pack $w.head.state -anchor w -padx 10
    pack $w.head -fill x

    text $w.body -wrap none -undo 1 \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -font {monospace 10} -highlightthickness 0 -relief flat -bd 0 \
        -yscrollcommand [list $w.sb set]
    ttk::scrollbar $w.sb -orient vertical -command [list $w.body yview]

    ttk::frame $w.act
    ttk::button $w.act.save -text [::vi18n::mc {Save & load}] -command ::vfg::_script_save
    ttk::button $w.act.tmpl -text [::vi18n::mc {Reset to template}] -command ::vfg::_script_template
    ttk::button $w.act.close -text [::vi18n::mc {Close}] -command [list destroy $w]
    pack $w.act.close -side right -padx 4
    pack $w.act.tmpl -side right -padx 4
    pack $w.act.save -side right -padx 4
    pack $w.act -side bottom -fill x -pady 6

    ttk::label $w.msg -text "" -foreground $::vt::T(gold) -wraplength 780 \
        -justify left -font {TkDefaultFont 9}
    pack $w.msg -side bottom -anchor w -padx 10

    pack $w.sb -side right -fill y
    pack $w.body -fill both -expand 1 -padx 6 -pady 6

    $w.body insert end $src
    # USER: "hopefully the new script editor shares all the conveniences
    # of the repl/dev pane". It is the SAME editor: ::vdev::attach
    # installs LSP go-to-definition (M-.), references (M-?), signature
    # help, hover docs, Tab completion, live diagnostics, syntax
    # highlighting and the emacs bindings — all of which used to exist
    # only inside the Dev pane because every helper named its widget
    # directly.
    #
    # -save because this buffer is not a plain file: C-x C-s has to go
    # through the endpoint that sources it before installing it.
    if {[catch { ::vdev::attach $w.body -path $path -status $w.msg \
                     -save ::vfg::_script_save }]} {
        # No Dev pane loaded (tests, a stripped build): keep the editor
        # usable rather than refusing to open it.
        catch { ::vemacs::install $w.body }
        catch { ::vsyn::highlight $w.body }
        bind $w.body <KeyRelease> { ::vfg::_script_rehighlight %W }
    }
    bind $w <Escape> [list destroy $w]
    catch { ::vtext::restyle_all $w }
    focus $w.body
    return
}

proc ::vfg::_script_rehighlight {w} {
    variable rehl_after
    if {[info exists rehl_after]} { catch { after cancel $rehl_after } }
    set rehl_after [after 180 [list catch [list ::vsyn::highlight $w]]]
    return
}

proc ::vfg::_script_msg {text {bad 0}} {
    set w .vfgscript
    if {![winfo exists $w.msg]} { ::vstatus::set_msg $text ; return }
    $w.msg configure -text $text \
        -foreground [expr {$bad ? $::vt::T(red) : $::vt::T(fg_dim)}]
    ::vstatus::set_msg $text
    return
}

proc ::vfg::_script_template {} {
    set w .vfgscript
    if {![winfo exists $w.body]} { return }
    if {[catch { ::vapi::get_json /api/failover/script } r]} {
        _script_msg "cannot reach the gateway" 1
        return
    }
    set t ""
    catch { set t [dict get $r template] }
    if {$t eq ""} { _script_msg "no template came back" 1 ; return }
    $w.body delete 1.0 end
    $w.body insert end $t
    catch { ::vsyn::highlight $w.body }
    _script_msg "template loaded — not saved yet"
    return
}

proc ::vfg::_script_save {} {
    set w .vfgscript
    if {![winfo exists $w.body]} { return }
    set src [$w.body get 1.0 end-1c]
    if {[string trim $src] eq ""} {
        _script_msg "nothing to save" 1
        return
    }
    if {[catch { ::vapi::post_json /api/failover/script \
        [dict create name [::vapi::json_string $::vfg::script_name] \
                       source [::vapi::json_string $src]] } r]} {
        _script_msg "save failed: $r" 1
        return
    }
    set err ""
    catch { set err [dict get $r error] }
    if {$err ne ""} {
        # The endpoint sources the script before installing it, so this
        # is the interpreter's own complaint — the most useful thing the
        # editor can show.
        _script_msg $err 1
        return
    }
    set saved ""
    set reloaded 0
    catch { set saved [dict get $r saved] }
    catch { set reloaded [dict get $r reloaded] }
    # Saved is not modified. Leaving the dirty marker up after a
    # successful save is how an editor teaches you to distrust it.
    catch { ::vdev::set_editor_dirty 0 }
    _script_msg [expr {$reloaded
        ? "saved and loaded — the next failed route runs it"
        : "saved to $saved, but this gateway has no Tcl runtime to load it"}]
    return
}
