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

set ::vlsp::ready 0
set ::vlsp::_pending_text {proc demo {} {}}
::vlsp::_on_init [dict create capabilities [dict create renameProvider true]] {server is already running}
ok "a duplicate initialize response preserves the capability snapshot" {
    [::vlsp::supports renameProvider]
}

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

# A callback is a command PREFIX, not one command name. Real operations
# use prefixes such as {::vdev::_rename_done $widget}; expanding only
# the outer list discarded those captured arguments at reply time.
proc ::_lspu::prefixed {which result error} {
    set ::_lspu::prefixed_called [list $which $result $error]
}
set ::vlsp::pending [dict create 42 [list ::_lspu::prefixed extension-buffer]]
::vlsp::_on_stream lsp [dict create frame {{"jsonrpc":"2.0","id":42,"result":{"path":"plugin.tcl"}}}]
ok "an LSP callback prefix retains its captured arguments" {
    $::_lspu::prefixed_called eq [list extension-buffer [dict create path plugin.tcl] ""]
}
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
set ::vlsp::session_ready 0
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

# ── every extension language (0.2.0, owner ask 287976.2) ─────────────
puts "\npositions: Tk characters and the server's units"
# tcl-lsp negotiates utf-8 and means BYTES. Probed live (2026-11-07) on
#   set s "éé€"; proc foo {} {}
# it answers semantic tokens 0,0,3,7,0  0,4,1,6,1  0,13,4,7,0 — `proc`
# at 4+13 = byte 17, which is character 13. The client painted byte
# offsets as characters, so every non-ASCII Tcl line was mis-highlighted.
set ::vlsp::legend_types {comment function namespace class method variable parameter keyword}
::vlsp::_remember_text "set s \"\u00e9\u00e9\u20ac\"; proc foo {} {}\nfoo\n"
set ::vlsp::encoding utf-8
ok "utf-8: byte 17 is character 13"   {[::vlsp::_from_server 0 17] == 13}
ok "utf-8: character 13 is byte 17"   {[::vlsp::_to_server 0 13] == 17}
ok "an ASCII line is the identity"    {[::vlsp::_from_server 1 2] == 2 && [::vlsp::_to_server 1 2] == 2}
ok "past the end keeps the overshoot" {[::vlsp::_from_server 1 9] == 9}
text .enc
.enc insert 1.0 "set s \"\u00e9\u00e9\u20ac\"; proc foo {} {}\nfoo\n"
::vlsp::set_apply_widget .enc
::vlsp::apply_tokens {0 0 3 7 0  0 4 1 6 1  0 13 4 7 0}
ok "tcl-lsp's real tokens paint `proc` on proc" \
    {[.enc tag ranges syn_keyword] eq {1.0 1.3 1.13 1.17}}
# tsserver negotiates nothing: UTF-16, where an astral character is two
# units. Under Tcl 8.6 Tk counts it as two characters as well, so the
# conversion must come out as the identity there and as +1 under Tcl 9.
set ::vlsp::encoding utf-16
set _astral [encoding convertfrom utf-8 [binary format H* 22f09f98802220]]
::vlsp::_remember_text "const s = ${_astral}x"
set _tk_x [expr {[string length "const s = ${_astral}"]}]
set _u16_x [expr {[string length "const s = \"\""] + 2 + 1}]
ok "utf-16: the character after an emoji" \
    {[::vlsp::_to_server 0 $_tk_x] == $_u16_x && [::vlsp::_from_server 0 $_u16_x] == $_tk_x}
# clangd and rust-analyzer take the utf-32 offered: one unit per
# character, whatever Tk counts.
set ::vlsp::encoding utf-32
set _cp_x [expr {[llength [split "const s = ${_astral}" ""]]}]
ok "utf-32: one unit per character" \
    {[::vlsp::_to_server 0 $_tk_x] == $_cp_x && [::vlsp::_from_server 0 $_cp_x] == $_tk_x}
destroy .enc

puts "\nURIs are the gateway's, for the gateway's platform"
# dev-ext-13, exercised on Linux with the platform as a parameter.
ok "a Windows path is a three-slash drive URI" {
    [::vlsp::path_to_uri {C:\Users\Ann\plugins\nested dir\x.tcl} windows] eq
    "file:///C:/Users/Ann/plugins/nested%20dir/x.tcl"
}
ok "…a verbatim path loses its prefix" {
    [::vlsp::path_to_uri {\\?\C:\Users\Ann\x.ts} windows] eq "file:///C:/Users/Ann/x.ts"
}
ok "…a UNC path keeps its host" {
    [::vlsp::path_to_uri {\\srv\share\a.c} windows] eq "file://srv/share/a.c"
}
ok "what a server sends back names the Windows path" {
    [::vlsp::uri_to_path file:///c%3A/Users/Ann/x.rs windows] eq {C:\Users\Ann\x.rs}
}
ok "…and the same file in two spellings is the same document" {
    [::vlsp::same_path {C:\Users\Ann\x.rs} c:/users/ann/X.RS windows]
}
ok "a unix path is encoded and decoded back exactly" {
    [::vlsp::uri_to_path [::vlsp::path_to_uri "/home/a b/\u00e9#1.tcl" unix] unix] eq "/home/a b/\u00e9#1.tcl"
}
ok "…with the reserved characters encoded" {
    [::vlsp::path_to_uri "/home/a b/#1.tcl" unix] eq "file:///home/a%20b/%231.tcl"
}
ok "a non-file URI names no path" {[::vlsp::uri_to_path untitled:1 unix] eq ""}
# The fs request query: json_escape doubled every backslash of a
# Windows path, so the Dev pane asked a Windows gateway for C:\\Users.
ok "a Windows path in a query is percent-encoded, not JSON-escaped" {
    [::vapi::urlencode {C:\Users\Ann\nested dir\x.tcl}] eq
    "C%3A%5CUsers%5CAnn%5Cnested%20dir%5Cx.tcl"
}

puts "\nthe handshake and the document, per language"
set ::vlsp::ch "stub-stream"
set ::_lspu::sent {}
set ::vlsp::session_ready 0
set ::vlsp::root "/srv/ts-ext"
set ::vlsp::root_uri "file:///srv/ts-ext"
set ::vlsp::language typescript
set ::vlsp::session typescript
::vlsp::_handshake
set _init [lindex $::_lspu::sent 0 1]
ok "a TypeScript session's messages carry its session key" \
    {[lindex $::_lspu::sent 0 0] eq "/api/lsp/send?session=typescript"}
ok "…and offer utf-32 then utf-16 (Tk-friendly first)" \
    {[string match {*"positionEncodings":["utf-32","utf-16"]*} $_init]}
ok "…work-done progress, so indexing shows" \
    {[string match {*"workDoneProgress":true*} $_init]}
# typescript-language-server publishes NO diagnostics to a client that
# does not advertise textDocument.publishDiagnostics (features.
# diagnosticsSupport = Boolean(publishDiagnostics)); the live veles-core
# test found the TypeScript buffers silent.
ok "…and the publishDiagnostics capability, without which tsserver says nothing" {
    [dict exists [::json::json2dict $_init] params capabilities textDocument publishDiagnostics]
}
ok "…and the gateway's root URI and workspace folder" \
    {[string match {*"rootUri":"file:///srv/ts-ext"*} $_init] && [string match {*"workspaceFolders":*} $_init]}
ok "the handshake is valid JSON" {![catch {::json::json2dict $_init}]}
set ::_lspu::sent {}
set ::vlsp::session_ready 0
set ::vlsp::language tcl
::vlsp::_handshake
set _tinit [lindex $::_lspu::sent 0 1]
ok "Tcl keeps the handshake tcl-lsp was probed with (utf-8 only)" \
    {[string match {*"positionEncodings":["utf-8"]*} $_tinit] && ![string match {*workspaceFolders*} $_tinit]}
set ::_lspu::sent {}
set ::vlsp::language rust
set ::vlsp::uri "file:///srv/rplug/src/lib.rs"
::vlsp::_open_doc "fn main() {}\n"
ok "didOpen names the document's language" \
    {[dict get [::json::json2dict [lindex $::_lspu::sent 0 1]] params textDocument languageId] eq "rust"}
# The negotiated encoding is read, with the protocol default when absent.
proc ::vlsp::_open_doc {text {on_ok ""}} {}
::vlsp::_on_init [dict create capabilities [dict create positionEncoding utf-32]] ""
ok "the negotiated encoding is kept" {$::vlsp::encoding eq "utf-32"}
::vlsp::_on_init [dict create capabilities [dict create hoverProvider true]] ""
ok "…and an unnegotiating server means utf-16" {$::vlsp::encoding eq "utf-16"}
::vlsp::_on_init [dict create capabilities [dict create] offsetEncoding utf-8] ""
ok "…clangd's older offsetEncoding counts too" {$::vlsp::encoding eq "utf-8"}

puts "\nwhat a server asks, and what it publishes"
set ::vlsp::ch "stub-stream"
set ::_lspu::sent {}
::vlsp::_on_stream lsp [dict create frame {{"jsonrpc":"2.0","id":5,"method":"workspace/configuration","params":{"items":[{"section":"a"},{"section":"b"}]}}}]
set _ans [::json::json2dict [lindex $::_lspu::sent 0 1]]
ok "workspace/configuration is answered with a default per item" \
    {[dict get $_ans id] == 5 && [llength [dict get $_ans result]] == 2}
set ::_lspu::sent {}
::vlsp::_on_stream lsp [dict create frame {{"jsonrpc":"2.0","id":"t1","method":"window/workDoneProgress/create","params":{"token":"x"}}}]
ok "a progress token is acknowledged (string id kept)" \
    {[string match {*"id":"t1","result":null*} [lindex $::_lspu::sent 0 1]]}
set ::_lspu::sent {}
::vlsp::_on_stream lsp [dict create frame {{"jsonrpc":"2.0","id":6,"method":"something/else"}}]
ok "an unknown request gets method-not-found, not silence" \
    {[string match {*"code":-32601*} [lindex $::_lspu::sent 0 1]]}
set ::vstatus::last ""
::vlsp::_on_stream lsp [dict create frame {{"jsonrpc":"2.0","method":"$/progress","params":{"token":"x","value":{"kind":"report","title":"Indexing","message":"3/9","percentage":33}}}}]
ok "indexing progress reaches the status line" {[string match "*Indexing 3/9 33%*" $::vstatus::last]}
set ::_lspu::diag "<none>"
proc ::_lspu::on_diag {rows} { set ::_lspu::diag $rows }
set ::vlsp::on_diag ::_lspu::on_diag
set ::vlsp::gateway_os unix
set ::vlsp::uri "file:///srv/rplug/src/lib.rs"
::vlsp::_remember_text "fn main() {}\n"
::vlsp::_on_stream lsp [dict create frame {{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///srv/rplug/src/other.rs","diagnostics":[{"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":2}},"severity":1,"message":"elsewhere"}]}}}]
ok "another file's diagnostics are not painted on this buffer" {$::_lspu::diag eq "<none>"}
::vlsp::_on_stream lsp [dict create frame {{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///srv/rplug/src/lib.rs","diagnostics":[{"range":{"start":{"line":0,"character":3},"end":{"line":0,"character":7}},"severity":2,"message":"unused"}]}}}]
ok "…this file's are" {$::_lspu::diag eq [list [list 1 3 WARNING unused]]}
set ::vstatus::last ""
::vlsp::_on_stream lsp-exit [dict create reason "rust-analyzer stopped: it said: panicked"]
ok "a server that dies says so, in its own words" \
    {$::vlsp::ch eq "" && [string match "*panicked*" $::vstatus::last]}

puts "\na request the server could not answer YET is asked again"
# rust-analyzer answers ContentModified (-32801) while it reloads its
# workspace; the live Dev-pane drive (scripts/dev-lsp-live.tcl) once got
# an empty go-to-definition that way. The protocol's answer is to send
# the request again, not to show the user nothing.
set ::vlsp::ch "stub-stream"
set ::vlsp::ready 1
set ::vlsp::uri "file:///srv/rplug/src/lib.rs"
::vlsp::_remember_text "fn main() {}\n"
set ::_lspu::sent {}
set ::_lspu::defs {}
proc ::_lspu::on_def {result err} { lappend ::_lspu::defs [list $result $err] }
::vlsp::definition_at 0 3 ::_lspu::on_def
set _first [::json::json2dict [lindex $::_lspu::sent 0 1]]
set _id1 [dict get $_first id]
::vlsp::_on_stream lsp [dict create frame "{\"jsonrpc\":\"2.0\",\"id\":$_id1,\"error\":{\"code\":-32801,\"message\":\"content modified\"}}"]
ok "ContentModified does not reach the caller" {![llength $::_lspu::defs]}
set ::_lspu::retry_wait 0
after 600 { set ::_lspu::retry_wait 1 }
vwait ::_lspu::retry_wait
ok "…the same request is sent again, under a new id" {
    [llength $::_lspu::sent] == 2 &&
    [dict get [::json::json2dict [lindex $::_lspu::sent 1 1]] method] eq "textDocument/definition" &&
    [dict get [::json::json2dict [lindex $::_lspu::sent 1 1]] params] eq [dict get $_first params] &&
    [dict get [::json::json2dict [lindex $::_lspu::sent 1 1]] id] != $_id1
}
set _id2 [dict get [::json::json2dict [lindex $::_lspu::sent 1 1]] id]
::vlsp::_on_stream lsp [dict create frame "{\"jsonrpc\":\"2.0\",\"id\":$_id2,\"result\":\[{\"uri\":\"file:///srv/rplug/src/lib.rs\",\"range\":{\"start\":{\"line\":0,\"character\":3},\"end\":{\"line\":0,\"character\":7}}}\]}"]
ok "…and its answer reaches the caller once" \
    {[llength $::_lspu::defs] == 1 && [lindex $::_lspu::defs 0 1] eq ""}
# Any other error is the caller's, at once.
set ::_lspu::sent {}
set ::_lspu::defs {}
::vlsp::definition_at 0 3 ::_lspu::on_def
set _id3 [dict get [::json::json2dict [lindex $::_lspu::sent 0 1]] id]
::vlsp::_on_stream lsp [dict create frame "{\"jsonrpc\":\"2.0\",\"id\":$_id3,\"error\":{\"code\":-32603,\"message\":\"internal\"}}"]
ok "an ordinary error is not retried" \
    {[llength $::_lspu::defs] == 1 && [llength $::_lspu::sent] == 1}

puts "\nresults in other servers' shapes"
ok "a LocationLink is a definition target" {
    [::vlsp::definition_target [list [dict create targetUri file:///x/a.rs \
        targetSelectionRange [dict create start [dict create line 4 character 7]]]]] eq "file:///x/a.rs 4 7"
}
ok "a MarkedString array is rendered as its text" {
    [::vlsp::render_hover [dict create contents [list [dict create language typescript value {function helper(): number}] {Adds one.}]]] eq
    "function helper(): number\n\nAdds one."
}
ok "a language + value MarkedString too" {
    [::vlsp::render_hover [dict create contents [dict create language rust value {fn helper()}]]] eq {fn helper()}
}
set ::vlsp::legend_types {comment string number macro struct enumMember selfKeyword operator}
ok "the standard legend maps (string, number, macro, struct, member, self)" {
    [::vlsp::_tag_for 1 0] eq "syn_string" && [::vlsp::_tag_for 2 0] eq "syn_number" &&
    [::vlsp::_tag_for 3 0] eq "syn_func" && [::vlsp::_tag_for 4 0] eq "syn_ns" &&
    [::vlsp::_tag_for 5 0] eq "syn_var" && [::vlsp::_tag_for 6 0] eq "syn_keyword" &&
    [::vlsp::_tag_for 7 0] eq ""
}
# A server without semantic tokens: the regex highlighter keeps an edited
# buffer coloured (the Dev pane syncs to the server instead of running it).
set ::_lspu::hl 0
proc ::vsyn::highlight {w} { incr ::_lspu::hl }
text .nosem
set ::vlsp::apply_w .nosem
set ::vlsp::ready 1
set ::vlsp::server_caps [dict create hoverProvider true]
set ::_lspu::sent {}
::vlsp::_request_semantic
ok "no semanticTokensProvider: no request, the fallback highlighter" \
    {$::_lspu::hl == 1 && ![llength $::_lspu::sent]}
destroy .nosem

puts "\nlsptest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
