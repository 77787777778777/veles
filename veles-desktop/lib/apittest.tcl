# veles-desktop/lib/apittest.tcl — the desktop API client carries a
# token, says so when one is refused, and remembers gateways locally.
#
# `::vapi` used to be a single global base URL with no auth. Remote
# connectivity needs three things this pins:
#
#   1. an Authorization: Bearer header on EVERY request when a token is
#      set (and none when it is not — loopback stays unauth);
#   2. a REJECTED token that is visible. `http::geturl -handler` does not
#      throw on a 401: it is a successful transfer whose body happens to
#      be an error, so the stream just ends. Without the code on the
#      terminal event, ::virc reconnects on a backoff forever and the
#      pane shows nothing, silently — which is what shipped;
#   3. a registry of saved gateways stored on THIS machine at 0600,
#      rather than posted into whichever gateway is selected.
#
# Pure-logic assertions plus stubs of http::geturl / http::ncode that
# record what was asked, so the header and the code are asserted on the
# real call path rather than by reading a helper back.
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

# `dict get` on an absent key THROWS, and a suite that throws prints no
# tally — which precommit.sh scores as a broken run rather than a red
# one. Every assertion below therefore reads through a helper that
# returns a sentinel instead of dying, so breaking the code under test
# produces a ✗ that can be counted.
proc field {d key} {
    if {![dict exists $d $key]} { return "<missing:$key>" }
    return [dict get $d $key]
}
proc inst_field {name key} { return [field [::vinst::get $name] $key] }

namespace eval ::vstatus { variable last "" ; proc set_msg {m} { set ::vstatus::last $m } }
# _sse_finished tells ::vchat which transcript a stream belonged to, and
# reports an HTTP failure through it. Both are real code paths here, so
# stub the namespace rather than routing around it — an unstubbed
# `set ::vchat::stream_session` ABORTS the suite, which prints no tally
# and is scored as a broken run rather than a red one.
namespace eval ::vchat {
    variable stream_session ""
    variable last_error ""
    proc on_error {m} { set ::vchat::last_error $m }
    proc on_finished {s} {}
}
source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]
# ::vinst::path resolves the config base through ::vpaths now.
source [file join $_dir paths.tcl]
source [file join $_dir instances.tcl]

# ── the header, on the pure helpers ────────────────────────────────
puts "the bearer header"

ok "no token → no auth header" {[::vapi::_auth_headers] eq {}}

::vapi::set_token "tok"
ok "a token → a Bearer header" \
    {[::vapi::_auth_headers] eq {-headers {Authorization {Bearer tok}}}}

# The window's language rides along once ::vi18n is loaded — in the SAME
# -headers list, because http::geturl keeps only the last -headers option
# and a second one would silently drop the bearer.
namespace eval ::vi18n { proc current {} { return de } }
set _h [::vapi::_auth_headers]
ok "the window's language travels with the bearer, in one list" {
    [lindex $_h 0] eq "-headers" && [llength $_h] == 2
    && [dict get [lindex $_h 1] X-Veles-Locale] eq "de"
    && [dict get [lindex $_h 1] Authorization] eq "Bearer tok"
}
::vapi::set_token ""
ok "…and alone when there is no token" {
    [::vapi::_auth_headers] eq {-headers {X-Veles-Locale de}}
}
rename ::vi18n::current {}

# ── the header travels on the real call path ──────────────────────
#
# Stub http::geturl to record its arguments, then drive ::vapi::get.
# This asserts the header is spliced into the actual request, not that
# _auth_headers exists in isolation.

namespace eval ::_apu {
    variable last_url ""
    variable last_opts {}
    variable code 200
}

proc ::http::geturl {args} {
    set ::_apu::last_opts $args
    set ::_apu::last_url [lindex $args 0]
    return "apu-token"
}
proc ::http::data {tok} { return "{}" }
proc ::http::ncode {tok} { return $::_apu::code }
proc ::http::cleanup {tok} {}
proc ::http::status {tok} { return "ok" }
proc ::http::reset {tok} {}

::vapi::set_token "header-tok"
::vapi::get /api/status

set _hi [lsearch -exact $::_apu::last_opts "-headers"]
ok "the request carries a -headers option" {$_hi >= 0}
ok "…whose value is the Bearer list" \
    {$_hi >= 0
     && [lindex $::_apu::last_opts [expr {$_hi + 1}]] eq {Authorization {Bearer header-tok}}}

::vapi::set_token ""
::vapi::get /api/status
ok "no header when the token is cleared" \
    {[lsearch -exact $::_apu::last_opts "-headers"] < 0}

# The SSE stream must carry it too — an authenticated REST call and an
# unauthenticated event stream would mean the IRC pane alone stays blank.
::vapi::set_token "stream-tok"
::vapi::sse_open /api/irc/events ::_apu::noop
set _hi [lsearch -exact $::_apu::last_opts "-headers"]
ok "sse_open sends the bearer as well" \
    {$_hi >= 0
     && [lindex $::_apu::last_opts [expr {$_hi + 1}]] eq {Authorization {Bearer stream-tok}}}
::vapi::set_token ""

# ── a refused token is not silence ────────────────────────────────
puts "\na refused token"

ok "401 is fatal, not a retry" {[::vapi::fatal_code 401]}
ok "403 is fatal, not a retry" {[::vapi::fatal_code 403]}
ok "404 is fatal — the engine is too old for this route" {[::vapi::fatal_code 404]}
ok "503 is NOT fatal; a restarting gateway comes back" {![::vapi::fatal_code 503]}
ok "0 (no response at all) is NOT fatal" {![::vapi::fatal_code 0]}

::vapi::set_base "http://10.0.20.7:8642"
ok "the reason names the gateway and the variable to fix" \
    {[string match "*10.0.20.7:8642*VELES_TOKEN*" [::vapi::http_reason 401]]}
ok "…and does not claim a token problem for a 500" \
    {![string match "*VELES_TOKEN*" [::vapi::http_reason 500]]}
::vapi::set_base "http://127.0.0.1:8642"

# The terminal event must carry the code, or a consumer cannot tell
# "the gateway went away" from "the gateway refused me".
namespace eval ::_apu { variable closed {} }
proc ::_apu::handler {ev d} {
    if {$ev eq "closed"} { set ::_apu::closed $d }
}
set ::_apu::code 401
set _tok [::vapi::sse_open /api/irc/events ::_apu::handler]
::vapi::_sse_finished $_tok
ok "a closed stream reports its HTTP code" \
    {[field $::_apu::closed code] == 401}

set ::_apu::closed {}
set ::_apu::code 200
set _tok [::vapi::sse_open /api/irc/events ::_apu::handler]
::vapi::_sse_finished $_tok
ok "…and a clean close reports 200, not a rejection" \
    {[field $::_apu::closed code] == 200}
ok "…and names the token, so a stale close cannot be mistaken for the live one" \
    {[field $::_apu::closed token] eq $_tok}

# A RESET end is one the CLIENT started — sse_close/http::reset runs the
# token's -command from inside the caller's own teardown, so reporting it
# as "closed" made every consumer treat its own cleanup as a drop and arm
# a phantom reconnect; each reconnect closed the LIVE stream, whose reset
# armed the next, and the client storm-looped once a second (the
# transcript/sidebar flicker through every live turn). It must report
# NOTHING: the stream was ended on purpose, and the closer owns what
# happens next.
set ::_apu::closed {}
proc ::http::status {tok} { return "reset" }
set _tok [::vapi::sse_open /api/irc/events ::_apu::handler]
::vapi::_sse_finished $_tok
ok "a stream WE reset reports nothing to its handler" {$::_apu::closed eq {}}
proc ::http::status {tok} { return "ok" }

# ── JSON escaping, which everything above rides on ────────────────
puts "\njson_escape"

# The map's first rule was `\\` → `\\`, i.e. nothing, so a LONE
# backslash went out raw and JSON read the next character as an escape.
# `a\b` came back as `a` — no error, just a shorter string — through
# every body the desktop writes, /api/fs/write included.
#
# Assert the ROUND TRIP, not the escaped text: what matters is that the
# gateway's parser reconstructs what was sent.
proc roundtrip {v} {
    if {[catch { ::json::json2dict "\{\"k\": \"[::vapi::json_escape $v]\"\}" } d]} {
        return "<parse failed: $d>"
    }
    return [dict get $d k]
}
foreach {_label _val} [list \
        "a lone backslash" "a\\b" \
        "a doubled backslash" "back\\\\slash" \
        "a quote" "q\"q" \
        "a tab" "t\tt" \
        "a newline" "n\nn" \
        "text that LOOKS like an escape" "\\u0041" \
        "plain text" "nothing to escape"] {
    ok "$_label survives the round trip" {[roundtrip $_val] eq $_val}
}

# Control characters get a PROPERTY assertion, not a round trip: JSON
# forbids raw U+0000-U+001F inside a string and serde_json (the gateway's
# parser, and the only one that matters) rejects them — but tcllib's
# json2dict accepts them, so a round trip through IT stays green with the
# escaping removed. Assert what goes out on the wire instead.
proc has_raw_control {s} { return [regexp {[\u0000-\u001f]} $s] }

ok "a bare control char is escaped, not sent raw" \
    {![has_raw_control [::vapi::json_escape "c\u0001c"]]}
ok "...in \\u form" \
    {[::vapi::json_escape "c\u0001c"] eq {c\u0001c}}
ok "...and so is every other control character" \
    {![has_raw_control [::vapi::json_escape \
        [join [lmap i {0 1 7 11 14 27 31} {format %c $i}] ""]]]}
ok "a tab keeps its short form rather than \\u0009" \
    {[::vapi::json_escape "a\tb"] eq {a\tb}}

# ── what actually goes out on the wire ────────────────────────────
puts "\noutbound encoding"

# `-binary 1` (added 2026-08-10 to fix INBOUND mojibake) also makes the
# socket binary outbound, and Tcl writes a string to a binary channel one
# LOW BYTE per character. So every POST body had its non-ASCII truncated:
#
#   "—" U+2014 -> 0x14   silently wrong
#   "·" U+00B7 -> 0xB7   a lone continuation byte: INVALID UTF-8
#
# The gateway rejects the second kind with a 400, so /api/fs/write could
# not save a file containing a middle dot. Found by driving the real LSP
# path against a live gateway: didOpen of api.tcl 400'd, because api.tcl
# contains a "·".
#
# Assert the BYTES, which is the thing that was wrong.
ok "an ascii body is unchanged" {[::vapi::_utf8_out "plain"] eq "plain"}
ok "a middle dot becomes its two UTF-8 bytes" \
    {[::vapi::_utf8_out "a\u00b7b"] eq "a\xc2\xb7b"}
ok "…not the single truncated byte that was invalid UTF-8" \
    {[::vapi::_utf8_out "a\u00b7b"] ne "a\xb7b"}
ok "an em dash becomes three bytes" \
    {[string length [::vapi::_utf8_out "a\u2014b"]] == 5}
ok "a box-drawing char (this file is full of them) survives" \
    {[::vapi::_utf8_out "\u2500"] eq "\xe2\x94\x80"}

# And on the real call path, not just the helper.
::vapi::set_token ""
::vapi::post /api/x "\{\"t\":\"a\u00b7b\"\}"
set _qi [lsearch -exact $::_apu::last_opts "-query"]
ok "post sends the encoded bytes" \
    {$_qi >= 0 && [string match "*a\xc2\xb7b*" [lindex $::_apu::last_opts [expr {$_qi + 1}]]]}
ok "…and every byte of the body is < 0x100, i.e. writable to a binary channel" \
    {![regexp {[\u0100-\uffff]} [lindex $::_apu::last_opts [expr {$_qi + 1}]]]}

# ── the saved-gateway registry, on this machine ───────────────────
puts "\nthe instance registry"

set _scratch [file join [file dirname [file normalize [info script]]] .apittest-home]
file delete -force $_scratch
set ::env(XDG_CONFIG_HOME) $_scratch

ok "the registry is under XDG_CONFIG_HOME, not VELES_HOME" \
    {[::vinst::path] eq [file join $_scratch veles-desktop instances.json]}

ok "a missing file is a fresh install, not an error" \
    {[::vinst::load] == 0 && [::vinst::error_text] eq ""}

::vinst::add "local" "http://127.0.0.1:8642"
::vinst::add "vps" "http://10.0.20.7:8642" "sekrit"
ok "save writes the file" {[::vinst::save] == 1}
ok "…at 0600, so the token is not world-readable" \
    {([file attributes [::vinst::path] -permissions] & 0777) == 0600}

# WIPE, then load — the point of a load test. The version this replaces
# asserted on entries that were still in memory from the save, so
# gutting `load` to `return 0` left it green.
set ::vinst::instances {}
ok "the registry really is empty before the load" {[llength [::vinst::names]] == 0}
ok "load restores both entries" {[::vinst::load] == 2}
ok "…with the URL intact" \
    {[inst_field vps url] eq "http://10.0.20.7:8642"}
ok "…and the token intact" \
    {[inst_field vps token] eq "sekrit"}
ok "…and a tokenless entry stays tokenless" \
    {[inst_field local token] eq ""}

# Values that would have broken the "url<TAB>token" packing this
# replaces, and that a naive JSON writer mangles.
::vinst::add "odd" "http://h:8642/x?a=1&b=\"2\"" "tok\twith\ttabs\nand\\slash"
ok "an awkward url/token round-trips" {[::vinst::save] == 1}
set ::vinst::instances {}
::vinst::load
ok "…the token survives tabs, quotes and backslashes" \
    {[inst_field odd token] eq "tok\twith\ttabs\nand\\slash"}
ok "…and so does the url" \
    {[inst_field odd url] eq "http://h:8642/x?a=1&b=\"2\""}

# A hand-edited file is the supported way to fix a typo'd URL, so a
# hand-edit that loses one must be reported rather than silently kept —
# an entry with no url cannot be selected, and ::vinst::select would
# quietly point the client at "".
set _fh [open [::vinst::path] w]
puts $_fh {{"good": {"url": "http://127.0.0.1:8642"}, "broken": {"token": "t"}}}
close $_fh
set ::vinst::instances {}
ok "a url-less entry is dropped" {[::vinst::load] == 1 && [::vinst::get broken] eq {}}
ok "…and says so rather than vanishing" \
    {[string match "*broken*" [::vinst::error_text]]}

# Put the registry back for the assertions below — on DISK as well as in
# memory, because `select` re-reads the file when it does not recognise a
# name (that is how a hand-edit is picked up), and the url-less document
# above is what is currently there.
set ::vinst::instances {}
::vinst::add "local" "http://127.0.0.1:8642"
::vinst::add "vps" "http://10.0.20.7:8642" "sekrit"
::vinst::save

# select points ::vapi at a saved gateway, and an explicit env wins.
::vapi::set_base "http://127.0.0.1:8642"
::vapi::set_token ""
::vinst::select "vps"
ok "select points the client at the saved URL" \
    {[::vapi::_url /api/status] eq "http://10.0.20.7:8642/api/status"}
ok "…and carries its token" {[::vapi::token] eq "sekrit"}

::vapi::set_base "http://127.0.0.1:9999"
::vinst::select "vps" 1 0
ok "an explicit VELES_API beats the saved URL" \
    {[::vapi::_url /x] eq "http://127.0.0.1:9999/x"}

ok "an unknown instance is an error, not a silent no-op" \
    {[catch { ::vinst::select "nope" }] == 1}

ok "remove drops an entry" \
    {[::vinst::remove vps] == 1 && [::vinst::get vps] eq {}}

file delete -force $_scratch

# ── post_async's on_ok: the ordering seam for dependent messages ──
#
# Every LSP message is its own POST, and separate connections can land
# them at the gateway in any order. A request that only makes sense
# once an earlier one is ACCEPTED (semanticTokens/full after a didOpen)
# is queued from the earlier POST's on_ok instead of fired alongside
# it. Without on_ok, didOpen raced its own semantic request, lost, and
# highlighting only appeared on the second or third reopen.
puts "\npost_async on_ok"

namespace eval ::_apu {
    variable ok_runs 0
    variable err_runs 0
}
proc ::_apu::on_ok {} { incr ::_apu::ok_runs }
proc ::_apu::on_err {why} { incr ::_apu::err_runs }
# Fire the -command http::geturl was given — exactly how the real http
# package reports a finished transfer.
proc ::_apu::fire {} {
    set ci [lsearch -exact $::_apu::last_opts "-command"]
    if {$ci < 0} { return }
    set cmd [lindex $::_apu::last_opts [expr {$ci + 1}]]
    uplevel #0 [list {*}$cmd apu-token]
}

::vapi::set_base "http://127.0.0.1:8642"
::vapi::set_token ""
set ::_apu::code 200
set ::_apu::ok_runs 0
set ::_apu::err_runs 0
::vapi::post_async /api/lsp/send "{}" ::_apu::on_err ::_apu::on_ok
ok "nothing runs before the gateway answers" \
    {$::_apu::ok_runs == 0 && $::_apu::err_runs == 0}
::_apu::fire
ok "an accepted POST runs on_ok" {$::_apu::ok_runs == 1}
ok "…and not the error callback" {$::_apu::err_runs == 0}

set ::_apu::code 500
set ::_apu::ok_runs 0
set ::_apu::err_runs 0
::vapi::post_async /api/lsp/send "{}" ::_apu::on_err ::_apu::on_ok
::_apu::fire
ok "a refused POST runs on_err instead" {$::_apu::err_runs == 1}
ok "…and never on_ok" {$::_apu::ok_runs == 0}

# The pre-existing two-argument shape still completes.
set ::_apu::code 200
set ::_apu::ok_runs 0
set ::_apu::err_runs 0
::vapi::post_async /api/lsp/send "{}" ::_apu::on_err
::_apu::fire
ok "the two-argument form still completes, with no on_ok" \
    {$::_apu::ok_runs == 0 && $::_apu::err_runs == 0}

# ── saying what you mean: json_bool / json_strings / json_raw ───────
#
# `_json_value` INFERS a type from an untyped Tcl value. Two of those
# inferences were silently wrong for real payloads, and both were caught
# by driving the actual UI rather than by reading the code:
#
#   * There is no boolean, so `true` became the STRING "true" and the
#     engine's `as_bool()` fell through to its default. The hooks manager
#     sent "enable" whichever way you clicked it.
#   * A ONE-ELEMENT list is indistinguishable from a scalar, so saving a
#     single memory entry sent a string and was refused with
#     "send `entries` (a list)".
#
# The constructors are the escape hatch. These pin them, and pin that
# the inference they bypass is still wrong in exactly those two ways —
# so the next caller reaches for the constructor instead of trusting it.
puts "\nexplicit JSON types"

proc field {json key} {
    set d [::json::json2dict $json]
    if {![dict exists $d $key]} { return "<missing>" }
    return [dict get $d $key]
}

ok "json_bool emits a literal true" \
    {[::vapi::_json_value [dict create v [::vapi::json_bool 1]] ] eq {{"v": true}}}
ok "json_bool emits a literal false" \
    {[::vapi::_json_value [dict create v [::vapi::json_bool 0]] ] eq {{"v": false}}}
ok "…and it parses back as a boolean, not the word" \
    {[field [::vapi::_json_value [dict create v [::vapi::json_bool 0]]] v] eq "false"}

# The inference this exists to bypass.
ok "a bare `true` would have been a STRING" \
    {[::vapi::_json_value [dict create v true]] eq {{"v": "true"}}}

ok "json_strings keeps a one-element list an array" \
    {[::vapi::_json_value [dict create e [::vapi::json_strings [list "one entry"]]]] \
        eq {{"e": ["one entry"]}}}
ok "…and an empty one" \
    {[::vapi::_json_value [dict create e [::vapi::json_strings {}]]] eq {{"e": []}}}
ok "…and does not read prose as an object" \
    {[::vapi::_json_value [dict create e [::vapi::json_strings [list "one entry" "two words"]]]] \
        eq {{"e": ["one entry", "two words"]}}}

# The inference this exists to bypass: a two-word entry whose first word
# is a clean identifier reads as {"one": "entry"}.
ok "a bare one-element list would have been a string" \
    {[::vapi::_json_value [dict create e [list "solo"]]] eq {{"e": "solo"}}}

ok "json_string forces a number to a string" \
    {[::vapi::_json_value [dict create v [::vapi::json_string 42]]] eq {{"v": "42"}}}
ok "json_object preserves dotted option keys and typed string values" \
    {[::vapi::_json_value [dict create options [::vapi::json_object [dict create platforms.irc.server [::vapi::json_string "two words"]]]]] eq {{"options": {"platforms.irc.server": "two words"}}}}
ok "an empty explicit JSON object is not an empty array" \
    {[::vapi::_json_value [dict create options [::vapi::json_object {}]]] eq {{"options": {}}}}
ok "json_raw is emitted verbatim" \
    {[::vapi::_json_value [dict create v [::vapi::json_raw {[1,2]}]]] eq {{"v": [1,2]}}}
# The field is an ARRAY, so index into it: comparing the whole list
# against a bare string passes only by accident of Tcl quoting.
ok "a tagged value survives quoting hostile content" \
    {[lindex [field [::vapi::_json_value \
        [dict create v [::vapi::json_strings [list "he said \"hi\""]]]] v] 0] eq {he said "hi"}}

# ── RM3 #10: free-text fields must be typed at the call site ────────
#
# USER: "trying to talk as the bot immediately errors — HTTP 422: text:
# invalid type: sequence, expected a string". `::virc::send` put the
# message into the body dict RAW, and `_json_value`'s shape inference
# read multi-word free text as a Tcl LIST (array) or an even/identifier
# list (object). The same class was already noted for steer and acl
# identities; this lint makes the WHOLE lib tree answer for it: every
# post_json body that binds a known free-text key must type the value
# with a ::vapi::json_* constructor at the dict site. Values for these
# keys are user text by definition — "works today because the words
# happen to be odd in number" is not a schema.
puts "\nfree-text fields are typed at the call site"

proc slurp {path} {
    set fh [open $path r]
    set data [read $fh]
    close $fh
    return $data
}

proc free_text_offenders {directory} {
set ft_keys {text prompt preamble toml source message title summary args name schedule deliver model question}
set offenders {}
foreach f [glob -directory $directory *.tcl] {
    set base [file tail $f]
    if {[string match "*test.tcl" $base] || $base eq "api.tcl"} { continue }
    set lines [split [slurp $f] \n]
    set n [llength $lines]
    for {set i 0} {$i < $n} {incr i} {
        if {![string match "*::vapi::post_json*" [lindex $lines $i]]} { continue }
        # The request body starts on this call, or on a continuation
        # below it. Never scan backwards into an unrelated local state
        # dict (pending_action carries source/name, not an HTTP body).
        # A variable body is typed in its own construction tests.
        set lo $i
        set hi [expr {$i + 4}] ; if {$hi >= $n} { set hi [expr {$n - 1}] }
        for {set j $lo} {$j <= $hi} {incr j} {
            set ln [lindex $lines $j]
            set s [string trim $ln]
            if {$s eq "" || [string index $s 0] eq "#"} { continue }
            if {[string first "::vapi::json_" $s] >= 0} { continue }
            # Does this line look like a dict body entry (or a one-line
            # `set x [dict create ...]`)? Commands that merely MENTION a
            # key word are denied, so `wm title` and `proc ... text ...`
            # do not count.
            set entryish 0
            if {[regexp {^([A-Za-z_][A-Za-z0-9_]*)[[:space:]]+\S} $s -> first]} {
                if {$first ni {proc wm if catch return puts lappend foreach
                               while expr error incr set list lassign variable
                               global namespace source package}} {
                    set entryish 1
                }
            }
            if {[regexp {^set \S+ \[dict create} $s]} { set entryish 1 }
            if {[regexp {\[dict create} $s]} { set entryish 1 }
            if {[regexp {\{dict create} $s]} { set entryish 1 }
            if {!$entryish} { continue }
            foreach k $ft_keys {
                # The key bound to a VALUE-SHAPED token ($var, [command],
                # or "literal") — not merely mentioned in prose.
                set pat "(^|\[ \t\])$k\[ \t\]+(\\\[|\\\$|\")"
                if {[regexp -- $pat $s]} {
                    lappend offenders "$base:[expr {$j + 1}] $k — $s"
                }
            }
        }
    }
}
return $offenders
}
set offenders [free_text_offenders $_dir]
if {[llength $offenders]} {
    puts "      untyped free-text fields:"
    foreach o $offenders { puts "        $o" }
}
ok "every post_json free-text field carries a json_* type" \
    {[llength $offenders] == 0}
# Mutation-check the actual lint above, not a weaker replacement: a
# request body containing untyped user text must still be refused, and
# unrelated pending state before that body must not hide or invent it.
set _lint_dir [file join /tmp "veles-json-lint-[pid]"]
file mkdir $_lint_dir
set _fixture [file join $_lint_dir fixture.tcl]
set _fh [open $_fixture w]
puts $_fh {set pending [dict create kind review name $name]}
puts $_fh "::vapi::post_json_async /example \[dict create text \$text]"
close $_fh
set offenders [free_text_offenders $_lint_dir]
file delete -force $_lint_dir
ok "the free-text lint still rejects an untyped request body" {
    [llength $offenders] == 1
    && [string first "fixture.tcl" [lindex $offenders 0]] >= 0
    && [string first " text " [lindex $offenders 0]] >= 0
}

# ── ::vapi::field — the engine's null is absence ─────────────────────
#
# tcllib maps JSON null to the STRING "null", so `dict get` hands back
# four characters that every `ne ""` test reads as a value. That put
# the ↳ child glyph on all 563 rows of the session list and an
# [attach] link on every shell in the tasks overlay (USER 2026-09-18).
#
# Asserted against a REAL parse, not a hand-built dict: a hand-built
# dict is exactly the fixture that hid this for months.
puts "\na JSON null is absence, not the word null"
set _parsed [::json::json2dict {{"here":"x","nulled":null,"n":0,"empty":""}}]
ok "a present field reads as itself" \
    {[::vapi::field $_parsed here] eq "x"}
ok "a null field reads as absent" \
    {[::vapi::field $_parsed nulled] eq ""}
ok "a MISSING key reads as absent (and does not throw)" \
    {[::vapi::field $_parsed nosuchkey] eq ""}
ok "an explicit default covers both absent shapes" \
    {[::vapi::field $_parsed nulled "fallback"] eq "fallback"
     && [::vapi::field $_parsed nosuchkey "fallback"] eq "fallback"}
# The two values that are NOT absence and must survive: a zero and an
# empty string are things the engine said, not things it omitted.
ok "a zero is a value, not a default" \
    {[::vapi::field $_parsed n "fallback"] eq "0"}
ok "an empty string is returned as itself" \
    {[::vapi::field $_parsed empty "fallback"] eq ""}

# ── an async GET calls back: the wire, not the function ─────────────
#
# Its one caller redraws a session's open ask when the session is opened
# (chat.tcl). Its tests fed ::vchat::_redraw_asks a reply directly, so
# nothing ever ran this path, and it never called back, twice over: the
# pending table was set as a LOCAL array, so the reply found nothing to
# hand on; and the callback, a command prefix, was called as one word.
# A session opened with a question waiting showed none (emulator,
# 2026-09-30; the laptop the same since it was written). A real socket,
# and the real http package, in an interpreter this suite's stubs do not
# reach.
puts "\nan async GET calls back"
set _child [interp create]
$_child eval [list set _dir $_dir]
$_child eval {
    namespace eval ::vstatus { proc set_msg {m} {} }
    source [file join $_dir jsonfield.tcl]
    source [file join $_dir api.tcl]
    # One thread serves and asks: the request is read as it arrives,
    # never waited for (a blocking read here would hold the event loop
    # the request is written from).
    proc serve {ch addr port} {
        fconfigure $ch -blocking 0 -translation crlf
        fileevent $ch readable [list request $ch]
    }
    proc request {ch} {
        if {[gets $ch line] < 0} {
            if {[eof $ch]} { close $ch }
            return
        }
        if {$line ne ""} { return }
        fileevent $ch readable {}
        set body {{"asks":[{"id":"a1","session_id":"s1"}]}}
        puts $ch "HTTP/1.0 200 OK"
        puts $ch "Content-Type: application/json"
        puts $ch "Content-Length: [string length $body]"
        puts $ch ""
        fconfigure $ch -translation binary
        puts -nonewline $ch $body
        close $ch
    }
    set srv [socket -server serve -myaddr 127.0.0.1 0]
    ::vapi::set_base "http://127.0.0.1:[lindex [fconfigure $srv -sockname] 2]"
    set ::got {}
    # A command prefix, as its caller passes one (`_redraw_asks $sid $shown`).
    proc got {sid ok val} { set ::got [list $sid $ok $val] }
    ::vapi::get_json_async /api/asks [list got s1]
    after 5000 { set ::got timeout }
    vwait ::got
    close $srv
}
set _got [$_child eval { set ::got }]
interp delete $_child
# (`field` means something else by this point in the file.)
ok "the callback gets the parsed reply" {
    [lrange $_got 0 1] eq {s1 1}
    && ![catch { dict get [lindex [dict get [lindex $_got 2] asks] 0] id } _id]
    && $_id eq "a1"
}

puts "\napittest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
