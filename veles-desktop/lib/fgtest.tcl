# veles-desktop/lib/fgtest.tcl — the failover blueprint's logic.
#
# The editor's value is that the picture cannot disagree with what the
# engine does: first-match-wins rule selection, a `stop` node ending the
# chain, and TOML that means what the boxes showed. All of that is pure,
# so it is tested here without a canvas.
#
# Run: scripts/tcl_tests.sh

set _dir [file dirname [file normalize [info script]]]
source [file join $_dir paths.tcl]
# failgraph.tcl is i18n-wrapped (its strings go through ::vi18n::mc), and
# a suite that sources it bare has no such command: `mc` aborted the
# whole file on the first label. Stub it the way the other suites do —
# identity + format, which is exactly what a missing catalog entry does
# at runtime.
if {![llength [info commands ::vi18n::mc]]} {
    namespace eval ::vi18n {
        proc mc {s args} {
            if {[llength $args]} { return [format $s {*}$args] }
            return $s
        }
    }
}
source [file join $_dir failgraph.tcl]

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

# A config as /api/config returns it.
proc cfg {} {
    return [dict create \
        default_provider "openrouter" \
        default_model "claude" \
        fallback_providers [list \
            [dict create provider "deepseek" model "chat"] \
            [dict create provider "local" model "qwen"]] \
        failover [list \
            [dict create provider "deepseek" model "chat" mode "auto" \
                 aux_fallback_provider "cheapo" aux_fallback_model "flash"] \
            [dict create provider "*" model "*" mode "ask" max_hops 3]]]
}

puts "model from config"
set m [::vfg::from_config [cfg]]
set chain [dict get $m chain]
ok "the PRIMARY route is the first node" \
    {[dict get [lindex $chain 0] provider] eq "openrouter"}
ok "…and is marked primary" {[dict get [lindex $chain 0] primary] == 1}
ok "fallbacks follow, in order" \
    {[dict get [lindex $chain 1] provider] eq "deepseek"
     && [dict get [lindex $chain 2] provider] eq "local"}
ok "the whole chain is three routes" {[llength $chain] == 3}
ok "rules are carried over" {[llength [dict get $m rules]] == 2}

puts "\nrule matching is first-match-wins, like the engine"
set r [::vfg::rule_for $m "deepseek" "chat"]
ok "an exact scope wins over the wildcard" {[dict get $r mode] eq "auto"}
ok "…and brings its aux fallback" {[dict get $r aux_provider] eq "cheapo"}
set r2 [::vfg::rule_for $m "openrouter" "claude"]
ok "an unmatched route falls to the wildcard" {[dict get $r2 mode] eq "ask"}
ok "…with its hop cap" {[dict get $r2 max_hops] == 3}

# A model with no rules at all must still describe the engine's default.
set bare [::vfg::from_config [dict create default_provider "p" default_model "m"]]
ok "with no rules, the default policy is ask" \
    {[dict get [::vfg::rule_for $bare "p" "m"] mode] eq "ask"}

puts "\nedge labels say what the mode DOES"
ok "auto advances silently" \
    {[string match "*silently*" [::vfg::edge_label auto 0]]}
ok "ask asks first" {[string match "*ask you*" [::vfg::edge_label ask 0]]}
ok "stop ends the turn" {[string match "*stop the turn*" [::vfg::edge_label stop 0]]}
ok "script can veto" {[string match "*veto*" [::vfg::edge_label script 0]]}
ok "the last node has nowhere to advance to" \
    {[string match "*give up*" [::vfg::edge_label auto 1]]}

puts "\na stop node ends the chain"
ok "no stop: every route is reachable" {[::vfg::reachable_count $m] == 3}
set stopped [::vfg::set_field $m 1 mode stop]
ok "a stop on route 2 makes route 3 unreachable" \
    {[::vfg::reachable_count $stopped] == 2}
set lay [::vfg::layout $stopped]
ok "…and the layout marks it so it can be greyed out" \
    {[dict get [lindex $lay 2] unreachable] == 1}
ok "…while the reachable ones are not marked" \
    {[dict get [lindex $lay 0] unreachable] == 0}

puts "\nlayout"
set lay [::vfg::layout $m]
ok "one box per route" {[llength $lay] == 3}
ok "boxes descend, and do not overlap" \
    {[dict get [lindex $lay 1] y] > [expr {[dict get [lindex $lay 0] y] + [dict get [lindex $lay 0] h]}]}
ok "each box carries its route" {[dict get [lindex $lay 1] provider] eq "deepseek"}
ok "…and the mode that governs it" {[dict get [lindex $lay 1] mode] eq "auto"}
ok "…and its aux diversion" {[dict get [lindex $lay 1] aux_provider] eq "cheapo"}
ok "an empty model lays out to nothing, not an error" \
    {[llength [::vfg::layout [dict create chain {} rules {}]]] == 0}

puts "\nediting one route does not retune the others"
# Route 1 (openrouter) is covered only by the "*" wildcard. Changing it
# must create a SCOPED rule, not edit the wildcard every other route
# depends on.
set edited [::vfg::set_field $m 0 mode stop]
ok "the wildcard still says ask" \
    {[dict get [::vfg::rule_for $edited "local" "qwen"] mode] eq "ask"}
ok "…while the edited route says stop" \
    {[dict get [::vfg::rule_for $edited "openrouter" "claude"] mode] eq "stop"}
ok "a new scoped rule was inserted BEFORE the wildcard" \
    {[dict get [lindex [dict get $edited rules] 0] provider] eq "openrouter"}
# Editing a route that already has a scoped rule updates it in place.
set e2 [::vfg::set_field $m 1 max_hops 9]
ok "an existing scoped rule is updated, not duplicated" \
    {[llength [dict get $e2 rules]] == 2}
ok "…with the new value" \
    {[dict get [::vfg::rule_for $e2 "deepseek" "chat"] max_hops] == 9}
ok "…and its other fields survive" \
    {[dict get [::vfg::rule_for $e2 "deepseek" "chat"] mode] eq "auto"}
ok "an out-of-range index is a no-op" \
    {[::vfg::set_field $m 99 mode stop] eq $m}

puts "\nreordering"
set mv [::vfg::move_node $m 1 -1]
ok "a route can move up" \
    {[dict get [lindex [dict get $mv chain] 0] provider] eq "deepseek"}
ok "…and the one it passed moves down" \
    {[dict get [lindex [dict get $mv chain] 1] provider] eq "openrouter"}
ok "moving off the top is a no-op" {[::vfg::move_node $m 0 -1] eq $m}
ok "moving off the bottom is a no-op" {[::vfg::move_node $m 2 1] eq $m}

puts "\nTOML emission"
set toml [::vfg::to_toml $m]
ok "the scoped rule is emitted" {[string match "*\\\[\\\[failover\\\]\\\]*" $toml]}
ok "…with its provider" {[string match "*provider = \"deepseek\"*" $toml]}
ok "…its mode" {[string match "*mode = \"auto\"*" $toml]}
ok "…and its aux fallback" {[string match "*aux_fallback_provider = \"cheapo\"*" $toml]}
ok "integers are emitted unquoted" {[string match "*max_hops = 3*" $toml]}
ok "it says where to paste it" {[string match "*config.toml*" $toml]}

# A model whose rules are all defaults has nothing worth writing.
set empty [dict create chain {} rules [list \
    [dict create provider "p" model "m" mode ask max_hops "" \
         wait_default_secs "" aux_provider "" aux_model ""]]]
ok "an all-defaults rule is not emitted as noise" \
    {![string match "*provider = \"p\"*" [::vfg::to_toml $empty]]}
ok "…and it says so rather than emitting an empty file" \
    {[string match "*nothing to write*" [::vfg::to_toml $empty]]}

# Quotes in a provider name must not break the file.
set q [dict create chain {} rules [list \
    [dict create provider "we\"ird" model "m" mode stop max_hops "" \
         wait_default_secs "" aux_provider "" aux_model ""]]]
ok "a quote in a name is escaped" \
    {[string match "*we\\\\\"ird*" [::vfg::to_toml $q]]}

puts "\nadding and removing routes"
set m2 [::vfg::add_node $m 0 "newprov" "newmodel"]
ok "a route can be added after the primary" \
    {[dict get [lindex [dict get $m2 chain] 1] provider] eq "newprov"}
ok "…and the rest shift down" \
    {[dict get [lindex [dict get $m2 chain] 2] provider] eq "deepseek"}
ok "…the chain grew by one" {[llength [dict get $m2 chain]] == 4}
ok "…and it is a fallback, never a second primary" \
    {[dict get [lindex [dict get $m2 chain] 1] primary] == 0}

# A new route can never land before the primary — the chain would then
# start at the wrong place.
set m3 [::vfg::add_node $m -5 "x" "y"]
ok "adding before the primary clamps to just after it" \
    {[dict get [lindex [dict get $m3 chain] 0] primary] == 1
     && [dict get [lindex [dict get $m3 chain] 1] provider] eq "x"}

set m4 [::vfg::remove_node $m 1]
ok "a fallback can be removed" {[llength [dict get $m4 chain]] == 2}
ok "…the right one went" \
    {[dict get [lindex [dict get $m4 chain] 1] provider] eq "local"}
ok "the PRIMARY cannot be removed" {[::vfg::remove_node $m 0] eq $m}
ok "…and can_remove says so before you try" {![::vfg::can_remove $m 0]}
ok "…while a fallback can" {[::vfg::can_remove $m 1]}
ok "an out-of-range remove is a no-op" {[::vfg::remove_node $m 99] eq $m}

puts "\nthe chain in the TOML"
set toml [::vfg::to_toml $m]
ok "fallback_providers is emitted" {[string match "*fallback_providers = *" $toml]}
ok "…with each fallback" \
    {[string match "*provider = \"deepseek\"*" $toml]
     && [string match "*provider = \"local\"*" $toml]}
ok "…and their models" {[string match "*model = \"qwen\"*" $toml]}
# The primary is [model] provider/default, NOT a fallback. Emitting it
# into fallback_providers would try it twice.
# Same trap: a `{` inside a quoted pattern is still counted by Tcl's
# brace parser when the whole expr is braced. Build it outside.
set primary_entry_pat "*[format %c 123] provider = \"openrouter\"*"
ok "the primary is a comment, not a fallback entry" \
    {[string match "*# primary route*" $toml]
     && ![string match $primary_entry_pat $toml]}

set only_primary [dict create chain [list [dict create provider "p" model "m" primary 1]] rules {}]
# `string first`, not `string match`: a glob treats [] as a character
# class, so "fallback_providers = []" can never be matched as a pattern.
ok "a chain with no fallbacks says so explicitly" \
    {[string first "fallback_providers = \[\]" [::vfg::to_toml $only_primary]] >= 0}


# ── the hook script behind `script` mode ────────────────────────────
#
# USER: "where/how do you see/edit the tcl script when set to script?"
# The mode was offered with nothing behind it. These cover the two
# things that make the editor honest: it says whether the plugin would
# actually run, and Save sends the buffer to the endpoint that checks it
# before installing it.
puts "\nthe failover hook script"
ok "a disabled plugin says so" \
    {[string match "*fire nothing*" [::vfg::script_status 0 1]]}
ok "…even when a script exists" \
    {[string match "*fire nothing*" [::vfg::script_status 0 0]]}
ok "a missing script is called a template" \
    {[string match "*template*" [::vfg::script_status 1 0]]}
ok "a live one says what it does" \
    {[string match "*failover::handle*" [::vfg::script_status 1 1]]}

if {[info exists ::env(XVFB_RUNNING)]
    || ([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    source [file join $_dir theme.tcl]
    ::vt::palette_synthwave ::vt::T
    namespace eval ::vstatus { proc set_msg {m} { set ::vstatus::last $m } }
    set ::vstatus::last ""
    namespace eval ::vtitle { proc attach {args} {} }
    namespace eval ::vtext  { proc restyle_all {w} {} }
    namespace eval ::vemacs { proc install {w} {} }
    namespace eval ::vsyn   {
        proc highlight {w} { set ::syn_calls 1 }
        proc diagnostics {w rows} {}
        proc hide_tip {} {}
    }
    namespace eval ::vlsp {
        proc running {} { return 0 }
        proc start {path text cb} { return 0 }
        proc sync {text} {}
    }
    namespace eval ::json { proc json2dict {s} { return [dict create] } }
    namespace eval ::vpanes { variable R .fgdevroot }
    # The REAL editor apparatus, not a stand-in: the point of the
    # exercise is that the script editor gets the Dev pane's bindings.
    source [file join $_dir dev.tcl]

    set ::posted {}
    set ::reply {}
    namespace eval ::vapi {
        proc get_json {path args} {
            return [dict create \
                path "/scratch/plugins/failover/plugin.tcl" \
                exists 0 loaded 1 \
                source "# TEMPLATE\n::veles::hook failover failover::handle\n" \
                template "# TEMPLATE\n::veles::hook failover failover::handle\n"]
        }
        proc post_json {path body} {
            set ::posted [list $path $body]
            return $::reply
        }
        # The marker contract (api.tcl) — failgraph types its free-text
        # bodies now (RM3 #10), and the assertions unwrap them.
        proc json_string {s} {
            set out ""
            foreach ch [split $s ""] {
                set d -1
                scan $ch %d d
                if {$ch eq "\""} { append out {\"} ; continue }
                if {$ch eq "\\"} { append out {\\} ; continue }
                if {$ch eq "\n"} { append out {\n} ; continue }
                if {$ch eq "\r"} { append out {\r} ; continue }
                if {$ch eq "\t"} { append out {\t} ; continue }
                if {$d >= 0 && $d < 32} { append out [format {\u%04x} $d] ; continue }
                append out $ch
            }
            return [list ::vapi::__json__ "\"$out\""]
        }
        proc json_unwrap {v} {
            if {[llength $v] == 2 && [lindex $v 0] eq "::vapi::__json__"} {
                set j [lindex $v 1]
                if {[string index $j 0] ne "\""} { return $j }
                set s [string range $j 1 end-1]
                set map [list {\\n} \n {\\r} \r {\\t} \t {\\b} \b {\\f} \f {\\\\} \\ {\"} \"]
                set s [string map $map $s]
                while {[regexp {\\u([0-9a-fA-F]{4})} $s -> hex]} {
                    set s [string map [list "\\u$hex" [format %c [scan $hex %x]]] $s]
                }
                return $s
            }
            return $v
        }
    }

    catch { destroy .vfgscript }
    ::vfg::edit_script
    update
    ok "the editor opens"            {[winfo exists .vfgscript]}
    ok "…with the script in it"      {[string match "*::veles::hook failover*" \
                                        [.vfgscript.body get 1.0 end-1c]]}
    ok "…and the path on show"       {[string match "*plugin.tcl*" \
                                        [.vfgscript.head.path cget -text]]}
    ok "…saying it is a template"    {[string match "*template*" \
                                        [.vfgscript.head.state cget -text]]}
    ok "…highlighted as Tcl"         {[info exists ::syn_calls]}
    # USER: "hopefully the new script editor shares all the conveniences
    # of the repl/dev pane". These are the Dev pane's bindings, on this
    # widget, because both go through ::vdev::attach.
    ok "…with go-to-definition"      {[bind .vfgscript.body <Alt-period>] ne ""}
    ok "…find references"            {[bind .vfgscript.body <Alt-question>] ne ""}
    ok "…Tab completion"             {[bind .vfgscript.body <Tab>] ne ""}
    ok "…and hover docs"             {[bind .vfgscript.body <Motion>] ne ""}
    ok "…and it knows its own path"  {[::vdev::editor_path] eq \
                                        "/scratch/plugins/failover/plugin.tcl"}
    # C-x C-s must go through the endpoint that sources the script, not
    # write the buffer to disk behind it.
    ok "…and C-x C-s saves the veles way" \
        {[info exists ::vdev::editor_save_cmd(.vfgscript.body)]}

    # Save sends what is in the buffer, to the endpoint that sources it
    # before installing it.
    .vfgscript.body delete 1.0 end
    .vfgscript.body insert end "proc failover::handle {} { return \"\" }"
    # Let the <<Modified>> event drain first — in real use it fires as
    # you type, long before you reach for C-x C-s.
    update
    set ::reply [dict create saved "/scratch/plugin.tcl" reloaded 1]
    ::vfg::_script_save
    update
    ok "Save posts to the script endpoint" \
        {[lindex $::posted 0] eq "/api/failover/script"}
    ok "…carrying the edited buffer" \
        {[string match "*proc failover::handle*" \
             [::vapi::json_unwrap [dict get [lindex $::posted 1] source]]]}
    ok "…and reports that it loaded" \
        {[string match "*loaded*" [.vfgscript.msg cget -text]]}

    # The endpoint refuses a script it cannot source. That message is
    # the interpreter's own, and it is the only useful thing to show.
    set ::reply [dict create error "the script did not load: missing close-brace"]
    ::vfg::_script_save
    update
    ok "a refused save shows the interpreter's complaint" \
        {[string match "*missing close-brace*" [.vfgscript.msg cget -text]]}

    # An empty buffer is not a save.
    set ::posted {}
    .vfgscript.body delete 1.0 end
    ::vfg::_script_save
    ok "an empty buffer is not posted" {$::posted eq ""}

    ::vfg::_script_template
    update
    ok "Reset restores the template" \
        {[string match "*TEMPLATE*" [.vfgscript.body get 1.0 end-1c]]}
    ok "…without saving it"          {$::posted eq ""}
    catch { destroy .vfgscript }
}

puts "\nfgtest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
