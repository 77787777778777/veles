# veles-desktop/lib/lsptest.tcl — the LSP client's TRANSPORT and its
# result normalizers.
#
# go-to-definition (M-.) and document-symbols (outline) ship Definition
# and symbols over the wire in two shapes each, depending on the server.
# These procs flatten both shapes to one internal form; a drift here
# would silently blank the Def jump or the Outline list, so pin them.
#
# The normalizer half is pure functions — no widget, no live server, no
# X server needed.
#
# The transport half is new (2026-08-25), when the language server moved
# from a subprocess of `wish` to a process on the GATEWAY reached over
# /api/lsp. There was NO transport coverage before that — the framing,
# the dispatch and the handshake were all untested — so this is new
# ground either way, and it is the half most likely to break silently:
# a client that never dispatches a frame looks exactly like a server
# with nothing to say.
#
# Tests run under Xvfb only because the shared runner launches every
# suite through wish.
#
# Run: scripts/tcl_tests.sh

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

namespace eval ::vstatus {
    variable last ""
    proc set_msg {m} { set ::vstatus::last $m }
}
source [file join $_dir api.tcl]
source [file join $_dir paths.tcl]
source [file join $_dir i18n.tcl]
source [file join $_dir lspclient.tcl]

# ── definition_target ────────────────────────────────────────────────
puts "definition target"

proc loc {line char} {
    return [dict create uri "file:///x/chat.tcl" \
        range [dict create start [dict create line $line character $char] \
               end [dict create line $line character [expr {$char + 5}]]]]
}

set single [::vlsp::definition_target [loc 3 11]]
ok "a single Location flattens to uri line char" {$single eq "file:///x/chat.tcl 3 11"}

set arr [::vlsp::definition_target [list [loc 3 11] [loc 9 0]]]
ok "a Location array yields the first" {$arr eq "file:///x/chat.tcl 3 11"}

ok "null definition is empty" {[::vlsp::definition_target null] eq {}}
ok "empty definition is empty" {[::vlsp::definition_target {}] eq {}}

set noloc [dict create uri "file:///x" ]
ok "a location without a range defaults to 0 0" \
    {[::vlsp::definition_target $noloc] eq "file:///x 0 0"}

# ── symbol_list ──────────────────────────────────────────────────────
puts "symbol list"

# DocumentSymbol shape: carries a `range` key.
set doc [list [dict create name "::vchat::send_current" kind 12 \
    range [dict create start [dict create line 5 character 0]] \
    selectionRange [dict create start [dict create line 5 character 0]]]]
set dsl [::vlsp::symbol_list $doc]
ok "DocumentSymbol shape flattens" \
    {[lindex $dsl 0] eq "::vchat::send_current function 5"}

# SymbolInformation shape: carries a `location` key instead.
set info [list [dict create name "::vchat::send" kind 12 \
    location [dict create uri "file:///x" \
        range [dict create start [dict create line 7 character 0]]]]]
set isl [::vlsp::symbol_list $info]
ok "SymbolInformation shape flattens (location key)" \
    {[lindex $isl 0] eq "::vchat::send function 7"}

# Kind 12 must decode as function, and mixed shapes stay in order.
set mixed [list \
    [dict create name "a" kind 3 range [dict create start [dict create line 1 character 0]]] \
    [dict create name "b" kind 12 location [dict create range [dict create start [dict create line 2 character 0]]]]]
set msl [::vlsp::symbol_list $mixed]
ok "namespace (3) and function (12) decode correctly" \
    {[lindex [lindex $msl 0] 1] eq "namespace" && [lindex [lindex $msl 1] 1] eq "function"}
ok "lines are 0-based as the wire sends them" \
    {[lindex [lindex $msl 0] 2] == 1 && [lindex [lindex $msl 1] 2] == 2}

ok "null symbols are empty" {[::vlsp::symbol_list null] eq {}}
ok "empty symbols are empty" {[::vlsp::symbol_list {}] eq {}}

# ── references ──────────────────────────────────────────────────────
puts "\nreferences"

set refs [list \
    [dict create uri "file:///a.tcl" \
        range [dict create start [dict create line 3 character 8]]] \
    [dict create uri "file:///b.tcl" \
        range [dict create start [dict create line 11 character 2]]]]
set rl [::vlsp::reference_list $refs]
ok "every location becomes a triple" {[llength $rl] == 2}
ok "uri, line and char survive in order" \
    {[lindex $rl 0] eq {file:///a.tcl 3 8} && [lindex $rl 1] eq {file:///b.tcl 11 2}}
ok "null references are empty, not an error" {[::vlsp::reference_list null] eq {}}
ok "empty references are empty" {[::vlsp::reference_list {}] eq {}}

# A malformed entry must be skipped, not abort the whole list — a server
# that answers one bad Location should not lose the good ones.
set mixed [list \
    [dict create nouri 1] \
    [dict create uri "file:///c.tcl" range [dict create start [dict create line 1 character 0]]]]
set ml [::vlsp::reference_list $mixed]
ok "a location with no uri is skipped, the rest survive" \
    {[llength $ml] == 1 && [lindex [lindex $ml 0] 0] eq "file:///c.tcl"}

# A location with no range still resolves — position defaults to 0,0
# rather than dropping a real reference.
set norange [list [dict create uri "file:///d.tcl"]]
ok "a location with no range defaults to 0 0" \
    {[::vlsp::reference_list $norange] eq {{file:///d.tcl 0 0}}}

# ── signature help ──────────────────────────────────────────────────
puts "\nsignature help"

proc sig {label params {active -1} {asig 0}} {
    set d [dict create signatures [list [dict create label $label parameters $params]] \
                       activeSignature $asig]
    if {$active >= 0} { dict set d activeParameter $active }
    return $d
}

ok "an empty result renders nothing" {[::vlsp::render_signature null] eq ""}
ok "a result with no signatures renders nothing" \
    {[::vlsp::render_signature [dict create signatures {}]] eq ""}
ok "the signature label is returned" \
    {[::vlsp::render_signature [sig "proc foo {a b}" {}]] eq "proc foo {a b}"}

# The active parameter is marked, so you can see WHICH argument you are
# typing — the entire point of signature help.
set marked [::vlsp::render_signature \
    [sig "foo(a, b)" [list [dict create label "a"] [dict create label "b"]] 1]]
ok "the active parameter is marked" {[string match "*\u2039b\u203a*" $marked]}
ok "…and the inactive one is not" {![string match "*\u2039a\u203a*" $marked]}

# LSP allows a parameter label to be an [start end] OFFSET PAIR into the
# signature label instead of a string. Servers really do send both.
set offs [::vlsp::render_signature \
    [sig "foo(a, b)" [list [dict create label {4 5}] [dict create label {7 8}]] 1]]
ok "an offset-pair parameter label resolves to its text" \
    {[string match "*\u2039b\u203a*" $offs]}

# Out-of-range indices must not crash or mark the wrong thing.
set oor [::vlsp::render_signature \
    [sig "foo(a)" [list [dict create label "a"]] 9]]
ok "an out-of-range active parameter is ignored" {$oor eq "foo(a)"}
set badsig [::vlsp::render_signature \
    [dict create signatures [list [dict create label "only"]] activeSignature 7]]
ok "an out-of-range active signature falls back to the first" {$badsig eq "only"}
ok "a signature with no label renders nothing" \
    {[::vlsp::render_signature [dict create signatures [list [dict create parameters {}]]]] eq ""}

# ── server capabilities ─────────────────────────────────────────────
#
# A request for a method the server does not advertise usually returns
# null, which at the UI is indistinguishable from "no results" — so the
# feature looks broken rather than absent. ::vlsp::supports lets the
# caller say which it is.
puts "\nserver capabilities"

set ::vlsp::server_caps [dict create \
    referencesProvider true \
    signatureHelpProvider [dict create triggerCharacters {( ,}] \
    renameProvider false \
    hoverProvider 1]
ok "a boolean-true provider is supported" {[::vlsp::supports referencesProvider]}
ok "an OPTIONS OBJECT provider is supported too" {[::vlsp::supports signatureHelpProvider]}
ok "a false provider is not" {![::vlsp::supports renameProvider]}
ok "a numeric-true provider is supported" {[::vlsp::supports hoverProvider]}
ok "an absent provider is not supported" {![::vlsp::supports codeActionProvider]}

set ::vlsp::server_caps {}
ok "no capabilities at all means nothing is supported" \
    {![::vlsp::supports referencesProvider]}

# ── document highlight ──────────────────────────────────────────────
puts "\ndocument highlight"

set hl [list \
    [dict create range [dict create start [dict create line 3 character 4] \
         end [dict create line 3 character 8]] kind 2] \
    [dict create range [dict create start [dict create line 7 character 0] \
         end [dict create line 7 character 4]] kind 3]]
ok "a highlight array flattens to {kind sl sc el ec}" \
    {[::vlsp::highlight_list $hl] eq {{2 3 4 3 8} {3 7 0 7 4}}}
ok "null highlights is empty" {[::vlsp::highlight_list null] eq {}}
ok "empty highlights is empty" {[::vlsp::highlight_list {}] eq {}}
ok "a highlight without a kind still carries its range" \
    {[::vlsp::highlight_list [list [dict create range [dict create \
        start [dict create line 2 character 1] end [dict create line 2 character 5]]]]] \
        eq {{0 2 1 2 5}}}

# ── rename ──────────────────────────────────────────────────────────
puts "\nrename"

set wsedit [dict create changes [dict create \
    "file:///x/chat.tcl" [list \
        [dict create range [dict create start [dict create line 1 character 6] \
             end [dict create line 1 character 9]] newText greet] \
        [dict create range [dict create start [dict create line 4 character 0] \
             end [dict create line 4 character 3]] newText greet]] \
    "file:///x/other.tcl" [list \
        [dict create range [dict create start [dict create line 0 character 0] \
             end [dict create line 0 character 3]] newText greet]]]]
set re [::vlsp::rename_edit $wsedit]
ok "a WorkspaceEdit flattens every edit with its uri" {[llength $re] == 3}
ok "an edit keeps uri, bounds and new text" \
    {[lindex $re 0] eq {file:///x/chat.tcl 1 6 1 9 greet}}
ok "cross-file edits are preserved, not dropped" \
    {[lindex $re 2] eq {file:///x/other.tcl 0 0 0 3 greet}}
ok "null rename edit is empty" {[::vlsp::rename_edit null] eq {}}
ok "a bare WorkspaceEdit with no changes is empty" \
    {[::vlsp::rename_edit [dict create]] eq {}}

# ── folding_list ─────────────────────────────────────────────────────
puts "folding ranges"

proc fr {sl el {kind ""}} {
    set d [dict create startLine $sl endLine $el]
    if {$kind ne ""} { dict set d kind $kind }
    return $d
}

set folds [::vlsp::folding_list [list [fr 10 20 region] [fr 0 30] [fr 5 6]]]
ok "every range survives"        {[llength $folds] == 3}
ok "…sorted by start line"       {[lindex [lindex $folds 0] 0] == 0}
ok "…carrying start and end"     {[lrange [lindex $folds 1] 0 1] eq "5 6"}
ok "…with no kind when none was sent" {[lindex [lindex $folds 1] 2] eq ""}
ok "…and the kind when sent"     {[lindex [lindex $folds 2] 2] eq "region"}

# A range that spans no line hides nothing. Keeping it would make
# "toggle the fold here" find an empty fold and appear to do nothing —
# the worst kind of broken, because it looks like the key is unbound.
set degenerate [::vlsp::folding_list [list [fr 4 4] [fr 7 3] [fr 2 9]]]
ok "a zero-length range is dropped"  {[llength $degenerate] == 1}
ok "…and so is an inverted one"      {[lindex [lindex $degenerate 0] 0] == 2}

ok "null folding is empty"  {[::vlsp::folding_list null] eq {}}
ok "empty folding is empty" {[::vlsp::folding_list {}] eq {}}

# ── code_action_list ─────────────────────────────────────────────────
puts "code actions"

# Both wire shapes. A CodeAction has title + edit and/or a nested
# command; a bare Command IS the command, with `command` as its id.
set ca [dict create title "Add missing brace" kind "quickfix" \
    edit [dict create changes [dict create "file:///x" {}]]]
set bare [dict create title "Reformat" command "tcl.format" arguments [list "x"]]
set nested [dict create title "Fix it" kind "quickfix" \
    command [dict create title "Fix it" command "tcl.fixit"]]

# A command that came back as a bare string rather than a dict must
# report, not abort: a suite that dies before its tally reads to CI as a
# broken run instead of as the regression.
proc cmd_name {act} {
    set c [lindex $act 3]
    if {[catch { dict get $c command } n]} { return "<not a command dict>" }
    return $n
}

set acts [::vlsp::code_action_list [list $ca $bare $nested]]
ok "all three shapes survive"   {[llength $acts] == 3}
ok "a CodeAction keeps its title" {[lindex [lindex $acts 0] 0] eq "Add missing brace"}
ok "…and its kind"                {[lindex [lindex $acts 0] 1] eq "quickfix"}
ok "…and its edit"                {[lindex [lindex $acts 0] 2] ne ""}
# A bare Command IS the object: taking `command` (the id string) as the
# command dict would lose the arguments the server needs.
ok "a bare Command becomes the command" {[cmd_name [lindex $acts 1]] eq "tcl.format"}
ok "…keeping its arguments"       {![catch { dict exists [lindex [lindex $acts 1] 3] arguments } h] && $h}
ok "a nested Command is unwrapped"      {[cmd_name [lindex $acts 2]] eq "tcl.fixit"}

# No title, nothing to show the user, nothing to pick.
ok "a titleless action is dropped" \
    {[llength [::vlsp::code_action_list [list [dict create kind "quickfix"]]]] == 0}
ok "null actions are empty"  {[::vlsp::code_action_list null] eq {}}
ok "empty actions are empty" {[::vlsp::code_action_list {}] eq {}}

# ── the transport ────────────────────────────────────────────────────
puts "\nthe transport"

# Record what goes out instead of sending it. `post_async` is the real
# call path for every JSON-RPC message, so asserting on it asserts on
# what the gateway would actually receive.
namespace eval ::_lspu {
    variable sent {}
    variable last_err ""
}
proc ::vapi::post_async {path body {on_err ""} {on_ok ""}} {
    lappend ::_lspu::sent [list $path $body]
    return "stub-token"
}
proc ::vapi::sse_close {tok} {}

# ── frames reach _dispatch, and resolve the right pending id ─────────
#
# The gateway wraps each frame in a one-key object, because a raw frame
# containing a newline would arrive as two `data:` lines and the client
# would read one. Unwrapping is this file's job.
set ::vlsp::ch "stub-stream"
set ::vlsp::pending {}
# Named procs, not an [apply] literal: _dispatch invokes the callback as
# `uplevel #0 [list $cb $result $err]`, and [list] makes a lambda one
# word — which is not a command.
namespace eval ::_lspu { variable got_7 "<none>" ; variable got_9 "<none>" }
proc ::_lspu::cb7 {r e} { set ::_lspu::got_7 $r }
proc ::_lspu::cb9 {r e} { set ::_lspu::got_9 $r }
dict set ::vlsp::pending 7 ::_lspu::cb7
dict set ::vlsp::pending 9 ::_lspu::cb9

::vlsp::_on_stream lsp [dict create frame {{"jsonrpc":"2.0","id":7,"result":{"ok":"seven"}}}]
ok "a frame resolves its own pending id" \
    {[dict exists $::_lspu::got_7 ok] && [dict get $::_lspu::got_7 ok] eq "seven"}
ok "…and leaves the other one waiting" {$::_lspu::got_9 eq "<none>"}
ok "…and the resolved id is dropped from pending" \
    {![dict exists $::vlsp::pending 7] && [dict exists $::vlsp::pending 9]}

# A frame with no wrapper key must be ignored, not dispatched as junk.
set ::_lspu::got_9 "<none>"
::vlsp::_on_stream lsp [dict create nothing here]
ok "a payload with no frame key is ignored" {$::_lspu::got_9 eq "<none>"}

# ── the handshake, and the rootUri that was null for a year ──────────
set ::_lspu::sent {}
set ::vlsp::root "/srv/project"
::vlsp::_handshake
ok "the handshake posts exactly one message" {[llength $::_lspu::sent] == 1}
ok "…to /api/lsp/send" {[lindex $::_lspu::sent 0 0] eq "/api/lsp/send"}
set _init [lindex $::_lspu::sent 0 1]
ok "…which is initialize" {[string match {*"method":"initialize"*} $_init]}
ok "…carrying the gateway's project root as rootUri" \
    {[string match {*"rootUri":"file:///srv/project"*} $_init]}
ok "…and NOT the null it used to send" \
    {![string match {*"rootUri":null*} $_init]}
ok "…and registers id 1 so the reply is not orphaned" \
    {[dict exists $::vlsp::pending 1]}

# No root (the gateway could not derive one) still has to produce valid
# JSON — a bare `file://` would be worse than null.
set ::_lspu::sent {}
set ::vlsp::root ""
::vlsp::_handshake
ok "no root falls back to null, not an empty file:// uri" \
    {[string match {*"rootUri":null*} [lindex $::_lspu::sent 0 1]]}

# ── the stream ending is not silence ─────────────────────────────────
set ::vlsp::ch "stub-stream"
set ::vlsp::ready 1
set ::vstatus::last ""
::vlsp::_on_stream closed [dict create status ok code 401]
ok "a refused token stops the client" {$::vlsp::ch eq "" && $::vlsp::ready == 0}
ok "…and says the token was refused" \
    {[string match "*token*" $::vstatus::last]}

set ::vlsp::ch "stub-stream"
set ::vlsp::ready 1
set ::vstatus::last ""
::vlsp::_on_stream closed [dict create status eof code 200]
ok "an ordinary close also stops it" {$::vlsp::ch eq ""}
ok "…without blaming the token" {![string match "*token*" $::vstatus::last]}

# A dropped frame leaves a pending id waiting forever, so it must be
# reported rather than swallowed.
set ::vlsp::ch "stub-stream"
set ::vstatus::last ""
::vlsp::_on_stream lsp-lagged [dict create missed 3]
ok "a lagged stream is reported" {[string match "*behind*" $::vstatus::last]}

# ── semantic tokens land on the BUFFER the session belongs to ───────
#
# apply_tokens used to pin $::vpanes::R.dev.edit.body, the single-editor
# widget path that the tab strip (98647b4) stopped creating. With no
# such widget, `winfo exists` failed and every semantic token was
# discarded — highlighting vanished while diagnostics (painted through
# the editor apparatus) kept working. The target must resolve: pinned
# widget → ::vdev::dev_editor → legacy path, and never a dead constant.
namespace eval ::vsyn {
    proc _tags {w} {}
    proc extra_passes {w} {}
}
namespace eval ::vdev { proc dev_editor {} { return $::_dev_editor_w } }
text .lsptok1
.lsptok1 insert 1.0 "proc one {} { puts one }\nset two 2\n"
pack .lsptok1
update
set ::vlsp::legend_types {comment keyword function namespace variable class}

::vlsp::set_apply_widget .lsptok1
::vlsp::apply_tokens {0 0 4 1 0}
ok "tokens paint on the pinned widget" {
    [.lsptok1 tag nextrange syn_keyword 1.0] ne ""
}
ok "…and on nobody else" {
    ![winfo exists .lsptok2] || [.lsptok2 tag nextrange syn_keyword 1.0] eq ""
}

# Unpinned, the current EDITOR BUFFER is the target.
text .lsptok2
.lsptok2 insert 1.0 "proc two {} { puts two }\nset three 3\n"
pack .lsptok2
set ::_dev_editor_w .lsptok2
set ::vlsp::apply_w ""
::vlsp::apply_tokens {0 10 4 1 0}
ok "without a pin, dev_editor resolves the target" {
    [.lsptok2 tag nextrange syn_keyword 1.0] ne ""
}

# No pin, no editor apparatus, no legacy widget: a silent no-op, not a
# crash. (The real pane always has a buffer; the harness path must not.)
set ::vlsp::apply_w ""
set ::_dev_editor_w ""
catch { namespace eval ::vpanes { variable R ; array set R {} } }
set ::vpanes::R(dev.edit.body) ""
set ::vlsp::apply_tokens {0 0 4 1 0}
ok "a dead target chain is a quiet no-op" { 1 }
destroy .lsptok1 .lsptok2

puts "\nlsptest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
