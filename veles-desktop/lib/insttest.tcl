# veles-desktop/lib/insttest.tcl — the Instances pane + attach: a
# virt-manager for bot instances (PLAN-instances-pane.md).
#
# What these pin, mapped to the plan's own test lists:
#
#   Part 0  ::vapi's multi-instance core — with_instance sets AND
#             restores (incl. on throw), _url/_auth_headers resolve
#             through the bound instance, unknown names fall back to
#             the default, use_instance refuses unbound names, and
#             the SSE async-capture trap.
#   Part 1  the registry's new fields — host/unit/group load and
#             round-trip, groups() defaults to the URL host, validate
#             refuses non-http(s), the file stays 0600 after update.
#   Part 2-3 the pane's pure logic — button_states' reasons,
#             _uptime/_hn humanisers, ssh argv AS A LIST, the
#             host/unit validators (option injection!), polling's
#             not-current-tab short circuit and inflight coalescing,
#             the amber-vs-red distinction, clear-before-poll.
#   Part 6  the teardown checklist ORDER (LSP stop before the flip,
#             streams before cursor), the epoch discard rule, IRC
#             cursor reset, detach selecting the Instances tab,
#             last_attached persistence, attach switching EVERY
#             consumer, unattached boot leaving the process quiet.
#
# The suites source the REAL modules (api.tcl, instances.tcl,
# safeexec.tcl, attach.tcl, instpane.tcl) with http::geturl stubbed
# to record requests. A suite that restates the code it tests proves
# nothing.
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

if {![info exists ::env(XVFB_RUNNING)] && [info exists ::env(DISPLAY)]} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

# ── stubs for everything the modules call but this suite does not test
namespace eval ::vstatus { variable last "" ; proc set_msg {m} { set ::vstatus::last $m } }
namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
namespace eval ::vi18n { proc label {w key args} { if {![winfo exists $w]} { return 0 } ; catch { $w configure -text [::vi18n::mc $key {*}$args] } ; return 1 }
                    proc aname {w key args} { if {![winfo exists $w]} { return 0 } ; catch { ::va11y::name $w [::vi18n::mc $key {*}$args] } ; return 1 } }
namespace eval ::va11y  { proc name {w d} {} }
namespace eval ::vcoach { proc attach {w} {} }
namespace eval ::vctx   { proc attach {w i} {} }
namespace eval ::vplace { proc centered {w h f} {} }
namespace eval ::vt     {
    variable T
    array set T {hairline #333 bg_panel #222 fg #eee bg_hover #444 accent #0ff
                 bg_input #111 fg_dim #888 select_bg #044 green #0f0 red #f00
                 gold #ff0}
}
# ::vapi's SSE layer reports through ::vchat on the chat path.
namespace eval ::vchat {
    variable stream_session ""
    variable last_error ""
    proc on_error {m} { set ::vchat::last_error $m }
    proc on_finished {s} {}
    proc detach_clear {} { set ::vchat::detached 1 }
}

source [file join $_dir safeexec.tcl]
source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]
# ::vinst::path resolves the config base through ::vpaths now.
source [file join $_dir paths.tcl]
source [file join $_dir instances.tcl]

# ── the http stub: records every request the app would have sent ────
#
# The plan's tests demand counting requests (polling short-circuits,
# unattached boot quietness), so the stub keeps a log. `responses` maps
# a path suffix to either a JSON string (returned as the body) or a
# marker that makes the request fail with the given HTTP code.

namespace eval ::_http {
    variable requests {}          ;# list of {url method}
    variable code 200
    variable body {"status":"ok"}
    variable status "ok"          ;# the TRANSPORT status, separate from
                                  ;# the HTTP code: a request can time
                                  ;# out with no code at all
    variable geturl_cb ""
    variable throw_on ""       ;# URL substring whose geturl raises
    variable pump 0            ;# make the stub run a nested event loop
    proc reset {} {
        variable requests {}
        variable code 200
        variable body {"status":"ok"}
        variable status "ok"
    }
    proc seen {needle} {
        variable requests
        foreach r $requests {
            if {[string first $needle [lindex $r 0]] >= 0} { return 1 }
        }
        return 0
    }
    proc count {needle} {
        variable requests
        set n 0
        foreach r $requests {
            if {[string first $needle [lindex $r 0]] >= 0} { incr n }
        }
        return $n
    }
}

proc ::http::geturl {url args} {
    lappend ::_http::requests [list $url GET]
    # A host name that does not resolve: the real geturl raises
    # synchronously, before any token exists.
    if {$::_http::throw_on ne "" && [string first $::_http::throw_on $url] >= 0} {
        ::error "couldn't open socket: Name or service not known" ;# not ::http::error, the stub below
    }
    # ::vapi::get's REAL synchronous path ends in http::wait, i.e. a
    # NESTED EVENT LOOP — timers, clicks and queued virtual events all
    # get dispatched in the middle of a poll. Off by default (most
    # assertions want a quiet, deterministic stub); the selection tests
    # turn it on, because the bug they pin only exists inside that loop.
    if {$::_http::pump} { update }
    return "stub-token"
}
proc ::http::data {tok}   { return $::_http::body }
proc ::http::ncode {tok}  { return $::_http::code }
proc ::http::cleanup {tok} {}
proc ::http::status {tok} { return $::_http::status }
proc ::http::error {tok}  { return "" }
proc ::http::reset {tok}  {}

# A transfer that did NOT complete — timeout, reset, eof. geturl only
# throws when it cannot open the socket at all; a request that starts
# and then times out returns an ordinary token with status "timeout",
# empty data and ncode 0. That has to read as a FAILURE.
proc ::_http::stall_with {st} {
    variable requests {}
    variable status $st
    proc ::http::ncode {tok} { return 0 }
    proc ::http::data {tok} { return "" }
}

# Make errors visible per the plan: `get` raises on >=400, and the stub
# needs to be able to produce that.
proc ::_http::fail_with {code} {
    variable requests {}
    variable status "ok"
    proc ::http::ncode {tok} [list return $code]
    proc ::http::data {tok} [list return "{\"error\":\"stub $code\"}"]
}
proc ::_http::ok_with {body} {
    variable requests {}
    variable status "ok"
    proc ::http::ncode {tok} { return 200 }
    proc ::http::data {tok} [list return $body]
}

# ── Part 0: ::vapi multi-instance ───────────────────────────────────

puts "\n::vapi multi-instance"

::vapi::set_base "http://127.0.0.1:8642"
::vapi::set_token ""
::vapi::bind_instance alpha "http://10.0.0.1:8642" "tokA"
::vapi::bind_instance beta  "http://10.0.0.2:8642" "tokB"

# `ok` evaluates through expr, so the with_instance calls run OUTSIDE
# the assertion and land their result in a variable first.
::vapi::with_instance alpha {
    set _r [expr { [::vapi::_url /x] eq "http://10.0.0.1:8642/x" }]
}
ok "with_instance sets the current instance" { $_r }
ok "…and restores it on the way out" {
    [::vapi::_url /x] eq "http://127.0.0.1:8642/x"
}
set _threw [catch { ::vapi::with_instance alpha { error "boom" } }]
ok "…restores it EVEN WHEN THE SCRIPT THROWS" {
    $_threw == 1 && [::vapi::_url /x] eq "http://127.0.0.1:8642/x"
}
::vapi::with_instance alpha {
    set _r [expr { [::vapi::_auth_headers] eq
        {-headers {Authorization {Bearer tokA}}} }]
}
ok "_auth_headers carries the bound instance's token inside" { $_r }
::vapi::bind_instance gamma "http://10.0.0.3:8642" ""
::vapi::with_instance gamma {
    set _r [expr { [::vapi::_auth_headers] eq {} }]
}
ok "…and nothing at all when the token is empty" { $_r }
::vapi::with_instance alpha { set a [::vapi::_url /] }
::vapi::with_instance beta  { set b [::vapi::_url /] }
ok "two instances bound at once resolve independently" {
    [list $a $b] eq [list http://10.0.0.1:8642/ http://10.0.0.2:8642/]
}
set _threw [catch { ::vapi::with_instance nosuch { ::vapi::_url /api/status } } _msg]
ok "an unknown instance name is an ERROR, not a silent default fallback" {
    $_threw == 1
}
ok "…naming the instance in the error" { [string match "*nosuch*" $_msg] }
ok "…and the current instance is untouched afterwards" {
    [::vapi::_url /api/status] eq "http://127.0.0.1:8642/api/status"
}

# The async capture trap: sse_open must record which instance it was
# opened against; a handler asking later must resolve to it, not to
# whatever is current when the socket dies.
namespace eval ::_apu { variable last {} }
proc ::_apu::noop {args} { set ::_apu::last $args }
::_http::reset
set _sse_tok [::vapi::with_instance alpha { ::vapi::sse_open /api/irc/events ::_apu::noop }]
ok "sse_open inside with_instance captures the instance" {
    [::vapi::stream_url $_sse_tok] eq "http://10.0.0.1:8642"
}
set ::_apu::last {}
::vapi::_sse_finished $_sse_tok
ok "…and the handler is told which gateway the stream belonged to" {
    [dict get [lindex $::_apu::last 1] url] eq "http://10.0.0.1:8642"
}

# THE SAME TRAP ON THE EVENT-STREAM PATH. The chat stream is gone (POST
# /api/chat is fire-and-forget); the persistent session stream that
# replaced it HAS a handler, so _sse_finished reports the failure to that
# handler with the CAPTURED origin and code — `current` is back to the
# DEFAULT by then, so without the capture the handler would be told the
# wrong gateway rejected the token. The code travels too, so the chat
# supervisor can tell "retry" from "stop" (a 401 is not transient).
::_http::fail_with 401
set _ev_tok [::vapi::with_instance alpha { ::vapi::session_events s1 0 0 ::_apu::noop }]
set ::_apu::last {}
::vapi::_sse_finished $_ev_tok
ok "an event stream's failure names the gateway it TALKED TO" {
    [dict get [lindex $::_apu::last 1] url] eq "http://10.0.0.1:8642"
}
ok "…and carries the 401 so the supervisor stops instead of retrying" {
    [dict get [lindex $::_apu::last 1] code] == 401
}
::_http::ok_with {{"status":"ok"}}

# use_instance / attached
ok "use_instance accepts a bound name" {
    [::vapi::use_instance beta] eq "beta" && [::vapi::attached] eq "beta"
}
ok "use_instance accepts \"\" (detach)" {
    [::vapi::use_instance ""] eq "" && [::vapi::unattached]
}
ok "use_instance REFUSES an unknown name" {
    [catch { ::vapi::use_instance nosuch }] == 1
}
::vapi::use_instance alpha
::vapi::with_instance beta { }
ok "with_instance entered while ATTACHED restores the attachment" {
    [::vapi::attached] eq "alpha"
}
ok "attached means requests resolve to it, outside any block" {
    [::vapi::_url /s] eq "http://10.0.0.1:8642/s"
}
::vapi::use_instance ""

# ── Part 1: the registry's new fields ───────────────────────────────

puts "\nthe instance registry"

set _scratch [file join $_dir .insttest-home]
file delete -force $_scratch
set ::env(XDG_CONFIG_HOME) $_scratch
file mkdir [file dirname [::vinst::path]]

proc inst_field_str {name key} {
    set spec [::vinst::get $name]
    if {$spec eq ""} { return "<no entry>" }
    if {[catch { dict get $spec $key } v]} { return "<no $key>" }
    return $v
}

# Backward compat: a file with only url still loads.
set _fh [open [::vinst::path] w]
puts $_fh {{"legacy": {"url": "http://1.2.3.4:8642"}}}
close $_fh
ok "an old-format registry (url only) still loads" {
    [::vinst::load] == 1 && [inst_field_str legacy url] eq "http://1.2.3.4:8642"
}

# The new fields load, and round-trip through save.
::vinst::add "box" "http://10.0.20.7:8642" "sekrit"
ok "update persists host and saves" { [::vinst::update box host "me@vps.example.org"] == 1 }
ok "update persists unit"           { [::vinst::update box unit "veles-gw"] == 1 }
ok "update persists group"          { [::vinst::update box group "vps"] == 1 }
set ::vinst::instances {}
::vinst::load
ok "host survives a reload"    { [inst_field_str box host] eq "me@vps.example.org" }
ok "unit survives a reload"    { [inst_field_str box unit] eq "veles-gw" }
ok "group survives a reload"   { [inst_field_str box group] eq "vps" }
ok "the token still round-trips" { [inst_field_str box token] eq "sekrit" }
ok "the file is 0600 AFTER update, not only after add" {
    ([file attributes [::vinst::path] -permissions] & 0777) == 0600
}

# groups(): explicit group, URL-host default, sorted.
::vinst::add "lo" "http://127.0.0.1:8642"
::vinst::add "other" "http://example.com:8642"
set _groups [::vinst::groups]
ok "groups returns a dict of group -> names" { [dict size $_groups] >= 3 }
ok "explicit group wins" {
    [dict get $_groups vps] eq [list box]
}
ok "no group ⇒ the URL's host" {
    [lsearch -exact [dict get $_groups 127.0.0.1] lo] >= 0
    && [lsearch -exact [dict get $_groups example.com] other] >= 0
}
ok "groups are sorted" {
    [lsort [dict keys $_groups]] eq [dict keys $_groups]
}

# validate: http/https in, everything else out.
ok "validate accepts http"  { [::vinst::validate http://h:1] }
ok "validate accepts https" { [::vinst::validate https://h] }
ok "validate REFUSES file:"   { ![::vinst::validate file:///etc/passwd] }
ok "validate REFUSES ssh:"    { ![::vinst::validate ssh://h] }
ok "validate REFUSES javascript:" { ![::vinst::validate javascript:alert(1)] }
ok "validate REFUSES a bare path" { ![::vinst::validate /etc/passwd] }
ok "validate REFUSES the empty string" { ![::vinst::validate ""] }
ok "validate REFUSES a shell-ish string" { ![::vinst::validate {h; rm -rf /}] }

# last_attached persistence.
::vinst::set_last_attached "box"
set ::vinst::last_attached ""
::vinst::load
ok "last_attached survives a save/load round trip" {
    [::vinst::last_attached] eq "box"
}

::vinst::set_remote_join [dict create join_id j-test instance vps \
    target_session alice from_instance laptop from_session mine]
set ::vinst::remote_join {}
::vinst::load
set _restored_join [::vinst::remote_join]
ok "remote join and its local return view survive a registry reload" {
    [dict get $_restored_join join_id] eq "j-test"
    && [dict get $_restored_join instance] eq "vps"
    && [dict get $_restored_join from_instance] eq "laptop"
    && [dict get $_restored_join from_session] eq "mine"
}
ok "the join sidecar is not exposed as a gateway instance" {
    [lsearch -exact [::vinst::names] __remote_join__] < 0
}
::vinst::set_remote_join {}

# ── Part 2-5: the pane's pure logic ─────────────────────────────────

source [file join $_dir attach.tcl]
source [file join $_dir instpane.tcl]

# ── the controls strip is a BOTTOM STRIP, not a column ──────────────
#
# The plan names .instctl "the controls strip". Registered as an
# ordinary region it docked as a third panedwindow COLUMN: its group
# separators stretched the full window height and the buttons floated
# mid-cavity in dead space (2026-08-28 user report: "Why the vertical
# bars/spacers? Is something supposed to be in those panes?"). The
# registration has to mark it a strip; a strip docks packed under .mid
# and never joins [.mid panes].
puts "\nthe controls strip is a bottom strip"
source [file join $_dir dock.tcl]
::vinstpane::register_regions
ok ".instctl is registered a strip" {[::vdock::is_strip .instctl]}
ok "…the tree and detail are ordinary columns" {
    ![::vdock::is_strip .instlist] && ![::vdock::is_strip .instdetail]
}
# build_ctl's packing contract has TWO shapes, and they split by where
# the region lives. DOCKED (boot): the root window is the main window,
# and a boot-time self-pack is exactly the startup-layout bug — the
# strip landed packed AFTER .mid in a zero-height cavity, or worse,
# leaked beside the chat tab. So docked, build_ctl must NOT pack
# itself; ::vdock::dock_strip owns the canonical pack. FLOATED (undock
# rebuilds the region inside its own toplevel), the rebuilt root has to
# pack itself there, -side bottom, one row tall — not a -fill y column.
::vinstpane::build_ctl .instctl
ok "a docked build_ctl does NOT pack itself at boot" {
    [catch { pack info .instctl }] == 1
}

toplevel .floatctl
::vinstpane::build_ctl .floatctl.body
set _pi [pack info .floatctl.body]
ok "a floated build_ctl packs itself -side bottom" {
    [lindex $_pi [expr {[lsearch -exact $_pi -side] + 1}]] eq "bottom"
}
ok "…and not -fill y" {
    [lindex $_pi [expr {[lsearch -exact $_pi -fill] + 1}]] eq "x"
}
destroy .floatctl
catch { pack forget .instctl }

# ── the strip's labels are catalog keys, and it is REGISTERED ────────
#
# Two defects shipped together in this proc and every static check in the
# tree passed them. `-text $label` handed the literal straight to Tk, so
# the strip was English in all 46 locales; and because the widget was
# never registered, a locale swap would not have re-texted it either. It
# was found by probing the LIVE window, not by reading the source: a
# half-translated strip (Reload plugins WAS translated, Attach was not)
# reads as a missing translation, which is a source-level question no
# check can answer about a string that never became a key.
#
# The pin asserts what the probe measured: the button shows the active
# locale's word, and ::vi18n::wkeys carries the registration the swap
# needs. A future `-text {Attach}` regression fails both halves.
puts "\nthe strip's labels are keys and the widgets are registered"
# paths.tcl first: ::vi18n::init resolves the config through
# ::vpaths::config_file, and this suite never sourced it (it had no need
# to until now). init() registers the catalog folder and loads the active
# locale's file, so without the pair `use de` has nothing to switch to.
source [file join $_dir paths.tcl]
source [file join $_dir i18n.tcl]
::vi18n::init
set _swapped [::vi18n::current]
::vi18n::use de
::vinstpane::build_ctl .instctl
ok "the attach button shows German, not the English literal" {
    [.instctl.attach cget -text] eq [::vi18n::mc {Attach}]
}
ok "…and differs from the English source" {
    [.instctl.attach cget -text] ne "Attach"
}
ok "the button is registered, so a swap re-texts it" {
    [info exists ::vi18n::wkeys(.instctl.attach)] \
        && [lindex $::vi18n::wkeys(.instctl.attach) 0] eq "Attach"
}
# `ok` takes an EXPR, so the loop runs here and the assert reads the
# result: a multi-command body is not an expression and blew up with
# "invalid command name ..." inside uplevel's expr.
set _missing {}
foreach _id {attach detach reload pause resume start stop restart add edit remove} {
    if {![info exists ::vi18n::wkeys(.instctl.$_id)]} { lappend _missing $_id }
}
ok "every strip button is registered" {[llength $_missing] == 0}
if {[llength $_missing]} { puts "    (unregistered: $_missing)" }
# THE TOOLTIP HALF. A greyed button explains itself through its
# accessible name, "<label> — <reason>". The l10n round rewrote that
# call to pass the label instead of the id and read an unset `$lab`
# (it had set `_lab`) from a proc that never declared `labels`; the
# error died inside a catch, so every disabled button lost its reason
# and no suite noticed — they check the button text, never the name.
set ::_anames [dict create]
proc ::va11y::name {w d} { dict set ::_anames $w $d }
set ::vinstpane::selected ""
::vinstpane::_enable_buttons
ok "a disabled button's tooltip names it and says why, in German" {
    [dict exists $::_anames .instctl.attach]
    && [dict get $::_anames .instctl.attach] eq [::vi18n::mc {%1$s — %2$s} \
            [::vi18n::mc {Attach}] [::vi18n::mc {no instance selected}]]
    && [dict get $::_anames .instctl.attach] ne "Attach — no instance selected"
}
if {[dict exists $::_anames .instctl.attach]} {
    puts "    (attach tooltip: [dict get $::_anames .instctl.attach])"
}
proc ::va11y::name {w d} {}
::vi18n::use $_swapped
catch { pack forget .instctl }

puts "\nthe pane: pure logic"

ok "uptime humanises seconds"      { [::vinstpane::_uptime 45] eq "45s" }
ok "uptime humanises minutes"      { [::vinstpane::_uptime 750] eq "12m 30s" }
ok "uptime humanises hours"        { [::vinstpane::_uptime 10800] eq "3h 0m" }
ok "uptime humanises days"         { [::vinstpane::_uptime 90000] eq "1d 1h" }
ok "_hn compacts counts"           { [::vinstpane::_hn 1200000] eq "1.2M" }
ok "_host_of extracts the host"    { [::vinstpane::_host_of http://a.b:99/x] eq "a.b" }

# The host/unit validators — the plan's argument-safety section.
ok "a normal host validates"       { [::vinstpane::valid_host me@box.example] }
ok "a normal unit validates"       { [::vinstpane::valid_unit veles-gateway] }
ok "host REFUSES -oProxyCommand=x" { ![::vinstpane::valid_host {-oProxyCommand=x}] }
ok "host REFUSES a leading dash"   { ![::vinstpane::valid_host {-V}] }
ok "host REFUSES '; rm -rf /'"     { ![::vinstpane::valid_host {a; rm -rf /}] }
ok "host REFUSES 'a b'"            { ![::vinstpane::valid_host {a b}] }
ok "host REFUSES the empty string" { ![::vinstpane::valid_host ""] }
ok "unit REFUSES -oProxyCommand=x" { ![::vinstpane::valid_unit {-oProxyCommand=x}] }
ok "unit REFUSES '; rm -rf /'"     { ![::vinstpane::valid_unit {x; rm}] }
ok "unit REFUSES a path"           { ![::vinstpane::valid_unit {/bin/sh}] }

# The composed argv, AS A LIST, with BatchMode present.
set _argv [::vinstpane::ssh_argv me@box veles-gateway start]
# {ssh -o BatchMode=yes -o ConnectTimeout=5 me@box
#  systemctl --user start veles-gateway} — 10 words.
ok "ssh argv is a 10-element list"  { [llength $_argv] == 10 }
ok "ssh argv element 0 is ssh"     { [lindex $_argv 0] eq "ssh" }
ok "BatchMode=yes is present" {
    [lsearch -exact $_argv -o] >= 0
    && [lindex $_argv [expr {[lsearch -exact $_argv -o] + 1}]] eq "BatchMode=yes"
}
ok "ConnectTimeout=5 is present" {
    [string first {ConnectTimeout=5} [join $_argv { }]] >= 0
}
ok "the host is a single element"  { [lindex $_argv 5] eq "me@box" }
ok "systemctl --user verb unit are the tail" {
    [lrange $_argv 6 end] eq [list systemctl --user start veles-gateway]
}
# The plan's literal argv example, asserted as the LIST it must be.
ok "the argv matches the plan's example, as a list" {
    [list {*}$_argv] eq [list ssh -o BatchMode=yes -o ConnectTimeout=5 \
        me@box systemctl --user start veles-gateway]
}

# A hostile host gets the leading-space treatment from ::vsafe::arg,
# which defeats both exec-operators AND option injection.
set _bad [::vinstpane::ssh_argv {-oProxyCommand=evil} veles-gateway start]
ok "a hostile host would arrive space-prefixed, not as an option" {
    [string index [lindex $_bad 5] 0] eq " "
}

# A cron job id goes into a URL PATH, so it needs percent-encoding, not
# JSON escaping — the two agree on ASCII letters and on nothing that
# matters. An id with a slash addressed a different endpoint entirely.
ok "urlenc leaves an ordinary id alone"      { [::vsafe::urlenc "nightly-dream"] eq "nightly-dream" }
ok "urlenc escapes a path separator"         { [::vsafe::urlenc "a/b"] eq "a%2Fb" }
ok "urlenc escapes a query separator"        { [::vsafe::urlenc "a?b=c"] eq "a%3Fb%3Dc" }
ok "urlenc escapes a space"                  { [::vsafe::urlenc "a b"] eq "a%20b" }
ok "urlenc escapes non-ASCII as UTF-8 bytes" { [::vsafe::urlenc "é"] eq "%C3%A9" }

# ── the post-start health wait ──────────────────────────────────────
#
# systemd returns while the unit is still activating, so a successful
# `ssh … systemctl start` says nothing about the port being open.
# _wait_healthy polls /api/health for up to 30s — and GIVING UP HAS TO
# BE REPORTED. "started" printed while the port is still closed is the
# failure mode the whole pane exists to prevent.
#
# In the app, every registry row is bound at boot (::vinst::bind_all);
# with_instance on an unbound name is an ERROR (that used to fall back
# to the default gateway, which is how a row could show another
# instance's data). Bind here the way boot would.
::vinst::add "vps" "http://10.0.20.7:8642" "tk"
::vinst::bind_all
::_http::fail_with 500
set ::vinstpane::ssh_tries 29     ;# one attempt short of the window
set ::vstatus::last ""
set _wh [::vinstpane::_wait_healthy vps]
ok "_wait_healthy gives up at the end of its window" { $_wh == 0 }
ok "…and SAYS it gave up rather than going quiet" {
    [string match {*did not become healthy*} $::vstatus::last]
}
ok "…and resets the counter for the next start" { $::vinstpane::ssh_tries == 0 }
# Mid-window it keeps waiting and says nothing final.
set ::vinstpane::ssh_tries 0
set ::vstatus::last ""
set _wh [::vinstpane::_wait_healthy vps]
ok "mid-window it keeps waiting rather than reporting" {
    $_wh == 0 && ![string match {*did not become healthy*} $::vstatus::last]
    && $::vinstpane::ssh_tries == 1
}
catch { after cancel $::vinstpane::ssh_after }
set ::vinstpane::ssh_after ""
set ::vinstpane::ssh_tries 0
::_http::ok_with {{"status":"ok"}}
set ::vstatus::last ""
ok "a healthy answer inside the window reports the instance up" {
    [::vinstpane::_wait_healthy vps] == 1
    && [string match {*is up*} $::vstatus::last]
}

# button_states reasons.
::vapi::use_instance ""
set _spec [dict create url http://h:1]
set _st [::vinstpane::button_states "lo" $_spec up]
ok "attach enabled when reachable and not attached" {
    [dict get $_st attach] eq ""
}
ok "detach disabled with a reason when not attached" {
    [dict get $_st detach] ne ""
}
ok "stop disabled WITH THE NO-HOST REASON (not hidden)" {
    [dict get $_st stop] eq "no host configured for this instance"
}
# With no host there is no lifecycle AT ALL — the no-host reason wins
# over 'already up', which is the plan's intent (absent host ⇒ absent
# buttons, and the tooltip says what to add).
ok "start disabled with the no-host reason when up and hostless" {
    [dict get $_st start] eq "no host configured for this instance"
}
set _st [::vinstpane::button_states "lo" [dict create url http://h:1 host me@box] up]
ok "start disabled with 'already up' when up AND hostable" {
    [dict get $_st start] eq "already up"
}
set _st [::vinstpane::button_states "lo" [dict create url http://h:1 host me@box] down]
ok "start enabled when host set and down" { [dict get $_st start] eq "" }
ok "stop disabled 'not running' when down" { [dict get $_st stop] eq "not running" }
set _st [::vinstpane::button_states "" [dict create] never]
ok "with no selection, edit/remove say why" {
    [dict get $_st edit] ne "" && [dict get $_st remove] ne ""
}
ok "add is always enabled" {
    [dict get [::vinstpane::button_states "" [dict create] never] add] eq ""
}

# Pause/Resume enablement comes from the STRUCTURED cron counts, never
# from the rendered cron line. The line goes through ::vi18n::mc, so the
# first version's `regexp {(\d+) jobs \((\d+) paused\)}` matched in
# English and nowhere else: under a translation both buttons went
# permanently disabled, reading "no cron jobs" beside a list of them.
set ::vinstpane::cron_stats(lo) [::vinstpane::cron_counts [dict create jobs \
    {{id 1 name a enabled 1 next_run_at ""} {id 2 name b enabled 0 next_run_at ""}}]]
set _st [::vinstpane::button_states "lo" [dict create url http://h:1] up]
ok "pause enabled when the instance has jobs"        { [dict get $_st pause] eq "" }
ok "resume enabled when some of them are paused"     { [dict get $_st resume] eq "" }

# The same facts, rendered through a DIFFERENT catalogue. Nothing about
# the decision may change.
rename ::vi18n::mc ::vi18n::_mc_en
# Same varargs contract as the real ::vi18n::mc — a stub with {s} breaks
# every caller that formats (a status line carrying %1$s/%2$s).
proc ::vi18n::mc {s args} {
    if {$s eq {%1$d jobs (%2$d paused)}} {
        set r {%1$d Auftraege (%2$d pausiert)}
    } else {
        set r [::vi18n::_mc_en $s]
    }
    if {[llength $args]} { return [format $r {*}$args] }
    return $r
}
set _st_de [::vinstpane::button_states "lo" [dict create url http://h:1] up]
rename ::vi18n::mc {}
rename ::vi18n::_mc_en ::vi18n::mc
ok "pause stays enabled under a TRANSLATED catalogue" {
    [dict get $_st_de pause] eq ""
}
ok "resume stays enabled under a translated catalogue" {
    [dict get $_st_de resume] eq ""
}

# No jobs at all is a real answer, and a different one from "we could
# not ask".
set ::vinstpane::cron_stats(lo) [::vinstpane::cron_counts [dict create jobs {}]]
set _st [::vinstpane::button_states "lo" [dict create url http://h:1] up]
ok "pause disabled with 'no cron jobs' when there are none" {
    [dict get $_st pause] eq "no cron jobs"
}
unset -nocomplain ::vinstpane::cron_stats(lo)
set _st [::vinstpane::button_states "lo" [dict create url http://h:1] up]
ok "…and disabled too when the cron list was never fetched" {
    [dict get $_st pause] ne ""
}

# ── Part 3: polling behaviour (stubbed http) ────────────────────────

puts "\npolling"

# The pane works on the registry in memory; reset it to two instances.
set ::vinst::instances {}
::vinst::add "lo"   "http://127.0.0.1:8642"
::vinst::add "vps"  "http://10.0.20.7:8642" "tk"
# Boot binds every row; the pane polls through with_instance, which is
# an error for an unbound name (no silent default-gateway fallback).
::vinst::bind_all
ok "bind_all binds every registry row" {
    [::vapi::instance_bound lo] && [::vapi::instance_bound vps]
}
set ::vinst::last_attached ""

# Tab currency: the pane must not poll when the Instances tab is not
# current. ::vtabs does not exist in this suite — stub the concept.
namespace eval ::vtabs { variable current "chat" ; proc current {} { variable current ; return $current } }
set ::vtabs::current "chat"

# An instance IS selected, so a detail tick that fired would issue six
# requests — quietness has to be the guard's doing, not an empty
# selection's.
set ::vinstpane::selected lo

::_http::reset
::vinstpane::_poll_list_tick
::vinstpane::_poll_detail_tick
ok "a tick with the tab NOT current issues ZERO requests" {
    [llength $::_http::requests] == 0
}
set ::vinstpane::selected ""

set ::vtabs::current "instances"
::_http::reset
::vinstpane::_poll_list_tick
ok "a list tick health-checks every instance" {
    [::_http::count /api/health] == 2
}
ok "health goes to each instance's OWN url" {
    [::_http::seen http://10.0.20.7:8642/api/health]
    && [::_http::seen http://127.0.0.1:8642/api/health]
}

::_http::reset
set ::vinstpane::selected ""
::vinstpane::_select lo
ok "a detail poll hits all six endpoints" {
    [::_http::count /api/status] == 1
    && [::_http::count /api/usage] == 1
    && [::_http::count /api/config] == 1
    && [::_http::count /api/plugins] == 1
    && [::_http::count /api/cron] == 1
    && [::_http::count /api/logs] == 1
}

# Inflight: a second tick while the first is out issues nothing.
::_http::reset
set ::vinstpane::inflight(d:lo) 1
::vinstpane::poll_detail_once
ok "a second detail poll while one is in flight issues NOTHING" {
    [llength $::_http::requests] == 0
}
unset -nocomplain ::vinstpane::inflight(d:lo)

# The not-current-tab short circuit has to live in poll_detail_once
# ITSELF, not only in the timer — _select calls it directly, and
# ::vinstpane::boot calls _select before ::vtabs::boot has picked a tab.
# Six synchronous 5s requests fired there froze the splash for half a
# minute whenever the first registry row was a black-holed remote.
::_http::reset
set ::vtabs::current "chat"
set ::vinstpane::selected lo
::vinstpane::poll_detail_once
ok "poll_detail_once called DIRECTLY is quiet when the tab is not current" {
    [llength $::_http::requests] == 0
}
::_http::reset
::vinstpane::_select vps
ok "…and so is _select, which is what boot goes through" {
    [llength $::_http::requests] == 0
}
set ::vtabs::current "instances"
set ::vinstpane::selected lo

# ── the cron controls compose a URL, not a JSON string ──────────────
#
# "Pause the scheduler" is "pause every job", one POST per id, and the
# id goes into the PATH. The first version percent-escaped it with
# ::vapi::json_escape — a JSON *string* escaper, which passes `/` and
# `?` through untouched, so an id like `nightly/dream` addressed
# /api/cron/nightly/dream/pause: a different endpoint.
proc tk_messageBox {args} { return "yes" }
::_http::ok_with {{"jobs":[{"id":"nightly/dream a?b","name":"n","enabled":true}]}}
::_http::reset
::vinstpane::_pause_resume Pause
ok "a cron id is PERCENT-encoded into the path" {
    [::_http::seen "/api/cron/nightly%2Fdream%20a%3Fb/pause"]
}
ok "…and never reaches the path raw" {
    ![::_http::seen "/api/cron/nightly/dream"]
}
::_http::ok_with {{"status":"ok"}}

# 401 on /api/status ⇒ amber, and the wording distinguishes it.
::_http::fail_with 401
::vinstpane::_select vps
ok "a 401 on /api/status paints the state UNAUTH, not down" {
    $::vinstpane::health(vps) eq "unauth"
}
::_http::ok_with {{"status":"ok"}}

# Health 'up' must not clobber 'unauth' (the more informative state).
::_http::fail_with 401
::vinstpane::_select vps
::_http::ok_with {{"status":"ok"}}
::vinstpane::_health_one vps
ok "a reachable-health result does NOT clear unauth" {
    $::vinstpane::health(vps) eq "unauth"
}
# …and a health FAILURE must degrade unauth to down: amber means
# REACHABLE-but-unauthorized, and a host whose (unauthenticated)
# health poll fails is not reachable. Found by driving a real gateway
# and killing it — the first rule kept a dead host amber forever.
::_http::fail_with 500
::vinstpane::_health_one vps
ok "a failed health poll degrades unauth to down" {
    $::vinstpane::health(vps) eq "down"
}
# …but a successful status poll does. (ok_with, not reset: reset only
# clears the request log; fail_with REPLACED the ncode/data procs.)
::_http::ok_with {{"status":"ok"}}
::vinstpane::_select vps
ok "a successful status poll clears unauth to up" {
    $::vinstpane::health(vps) eq "up"
}
# A health FAILURE on a plain up row paints down — and never 'never'.
::_http::fail_with 500
::vinstpane::_health_one lo
ok "a failed health poll on a live row paints down" {
    $::vinstpane::health(lo) eq "down"
}
::_http::ok_with {{"status":"ok"}}

# ── A TIMED-OUT REQUEST IS A FAILURE, NOT AN EMPTY REPLY ────────────
#
# http::geturl only THROWS when it cannot open the socket at all
# (connection refused, DNS failure). A transfer that starts and then
# times out or resets comes back as a perfectly ordinary token: status
# "timeout", data "", ncode 0. ::vapi::get checked only the code, so it
# returned "" and the caller's catch never fired.
#
# Found by pointing the pane at a BLACK-HOLED host (10.255.255.1): its
# dot went GREEN and its detail said "reachable". Killing a gateway on
# loopback does NOT reproduce it — that is a refused connection, which
# geturl does throw on — which is why the earlier live check passed
# while the case the pane exists for did not.
foreach _st {timeout reset eof} {
    ::_http::stall_with $_st
    ok "a '$_st' transfer raises rather than returning an empty body" {
        [catch { ::vapi::get /api/health 2000 }] == 1
    }
}
::_http::stall_with timeout
set ::vinstpane::health(lo) up
::vinstpane::_health_one lo
ok "a black-holed host paints DOWN, not up" { $::vinstpane::health(lo) eq "down" }
ok "…and a POST that timed out must not read as success" {
    [catch { ::vapi::post /api/tcl/reload {{}} 2000 }] == 1
}
::_http::ok_with {{"status":"ok"}}

# Selecting a different instance clears the detail BEFORE the next
# poll returns — with a REAL widget, because that is where the stale
# text would otherwise sit. (Xvfb is why this suite exists.)
::vinstpane::build_detail .instdetail
pack .instdetail -fill both -expand 1
set ::vinstpane::R_detail .instdetail
::vinstpane::_detail_tags .instdetail.body
# Pretend lo's detail was painted and cached…
::vinstpane::_select lo
.instdetail.body configure -state normal
.instdetail.body delete 1.0 end
.instdetail.body insert end "STALE-FROM-LO"
.instdetail.body configure -state disabled
# …and STUB THE POLL OUT for this one assertion. With the stub in place
# the only thing that can remove the stale text is _select's own
# _clear_detail — which is the claim. Left un-stubbed, the synchronous
# http stub means _select's poll_detail_once runs to completion and
# paint_detail clears the pane on its way in, so deleting _clear_detail
# from _select left this assertion green: it was carried by the repaint,
# not by the clear.
rename ::vinstpane::poll_detail_once ::vinstpane::_poll_detail_once_real
proc ::vinstpane::poll_detail_once {} {}
::vinstpane::_select vps
rename ::vinstpane::poll_detail_once {}
rename ::vinstpane::_poll_detail_once_real ::vinstpane::poll_detail_once
ok "selecting a different instance clears the pane before the poll" {
    [string first STALE-FROM-LO [.instdetail.body get 1.0 end]] < 0
}
ok "…and paints the waiting empty-state in the meantime" {
    [string first "waiting for first poll" [.instdetail.body get 1.0 end]] >= 0
}

# ── the detail pane's RENDERER CONTRACT ─────────────────────────────
#
# Every _fmt_* returns a LIST OF LINES, and paint_detail prints the
# label once. This suite had nothing for either, and the first version
# shipped with four of the five renderers returning a bare STRING which
# paint_detail then word-split: `usage 1.2M in / 340k out / 8431 calls`
# came out as eight rows reading "usage 1.2M", "usage in", "usage /".
puts "\nthe detail pane"

ok "_fmt_usage is a ONE-element list, not a word-splittable string" {
    [llength [::vinstpane::_fmt_usage [dict create prompt_tokens 1200000 \
        completion_tokens 340000 calls 8431]]] == 1
}
ok "_fmt_route is a one-element list" {
    [llength [::vinstpane::_fmt_route [dict create default_provider anthropic \
        default_model claude-opus-5 \
        fallback_providers {{provider openai model gpt-4}}]]] == 1
}
ok "_fmt_plugins is a one-element list" {
    [llength [::vinstpane::_fmt_plugins [dict create plugins \
        {{name a enabled 1} {name b enabled 0}}]]] == 1
}
ok "_fmt_cron is a one-element list" {
    [llength [::vinstpane::_fmt_cron [dict create jobs \
        {{id 1 name nightly enabled 1 next_run_at 2099-01-01T00:00:00Z}}]]] == 1
}
ok "_fmt_triggers is a one-element list" {
    [llength [::vinstpane::_fmt_triggers [dict create triggers \
        {{name a enabled 1} {name b enabled 0}}]]] == 1
}
# /api/logs answers records, not lines: the detail printed them as raw
# dicts ("level INFO msg {…} target … time …", SMOKE-21).
ok "_fmt_logs prints a log record as its time, level, target and message" {
    [::vinstpane::_fmt_logs [dict create logs [list [dict create level INFO \
        msg {module lang-js: running (0.2.1)} target veles_core::modulehost \
        time 22:36:57.991]]]]
    eq [list {22:36:57.991 INFO veles_core::modulehost: module lang-js: running (0.2.1)}]
}
ok "_fmt_logs is the list the gateway sent" {
    [llength [::vinstpane::_fmt_logs [dict create logs {"a b c" "d e"}]]] == 2
}
# AN EMPTY ANSWER IS NOT A MISSING ONE. Both used to return {}, and {}
# is what paint_detail turns into "unavailable" — so a healthy gateway
# with no plugins and no cron jobs read as `plugins unavailable · cron
# unavailable`, i.e. broken. Seen on the live demo gateway, which has
# neither. {} is now reserved for a fetch that FAILED.
ok "an empty plugin list says none loaded, not unavailable" {
    [::vinstpane::_fmt_plugins [dict create plugins {}]] eq [list "none loaded"]
}
ok "an empty cron list says no jobs, not unavailable" {
    [::vinstpane::_fmt_cron [dict create jobs {}]] eq [list "no jobs"]
}
ok "an empty trigger list says no triggers, not unavailable" {
    [::vinstpane::_fmt_triggers [dict create triggers {}]] eq [list "no triggers"]
}
ok "trigger_counts counts enabled and disabled separately" {
    [::vinstpane::trigger_counts [dict create triggers \
        {{name a enabled 1} {name b enabled 0} {name c enabled 0}}]] \
        eq [dict create triggers 3 enabled 1 disabled 2]
}
ok "an unrouted gateway says not configured" {
    [::vinstpane::_fmt_route [dict create]] eq [list "not configured"]
}
ok "an empty log says so rather than reading as a failure" {
    [::vinstpane::_fmt_logs [dict create logs {}]] eq [list "(empty)"]
}

# And the render itself, through the real widget.
set ::vinstpane::selected vps
set ::vinstpane::health(vps) up
set ::vinstpane::meta(vps) [dict create version 0.1.0 uptime_s 90000 \
    platforms {telegram irc} session_count 12]
set ::vinstpane::detail(vps) [dict create \
    usage   [::vinstpane::_fmt_usage [dict create prompt_tokens 1200000 \
                completion_tokens 340000 calls 8431]] \
    route   [::vinstpane::_fmt_route [dict create default_provider anthropic \
                default_model claude-opus-5]] \
    plugins {} \
    logs    [::vinstpane::_fmt_logs [dict create logs {"line one here" "line two"}]]]
::vinstpane::paint_detail
set _painted [.instdetail.body get 1.0 end]
proc _lines_matching {text pat} {
    set n 0
    foreach l [split $text \n] { if {[string match $pat $l]} { incr n } }
    return $n
}
ok "usage renders as ONE line, not one line per word" {
    [_lines_matching $_painted "usage*"] == 1
}
ok "…carrying the whole figure" {
    [string first "1.2M in / 340.0k out / 8431 calls" $_painted] >= 0
}
ok "route renders as one line" { [_lines_matching $_painted "route*"] == 1 }
ok "a field the gateway could not answer says so, once" {
    [_lines_matching $_painted "plugins*"] == 1
    && [string first "unavailable" $_painted] >= 0
}
ok "the log block keeps every line" {
    [string first "line one here" $_painted] >= 0
    && [string first "line two" $_painted] >= 0
}
ok "…with the label printed once, the rest indented under it" {
    [_lines_matching $_painted "log (last 20)*"] == 1
}

::_http::ok_with {{"status":"ok"}}
array unset ::vinstpane::detail
array unset ::vinstpane::meta
set ::vinstpane::selected ""

# ── the widget and `selected` are ONE fact, not two ─────────────────
#
# refresh_list writes the treeview's selection from `selected`, and ttk
# queues <<TreeviewSelect>> from that write whether or not anything
# changed. A programmatic _select used to update only the VARIABLE, so
# the widget kept the previous row — and the next synchronous poll's
# nested event loop dispatched the queued event into _pick, which read
# the WIDGET and re-selected the row we had just navigated away from.
#
# Reproduced live against two real gateways: selecting `wrongtoken`
# painted `demo`'s name, status, usage, route and logs. That is exactly
# the failure this plan was written to prevent — the previous gateway's
# data on screen labelled as the new one's.
#
# It needs a real treeview AND a real event dispatch, which is why no
# stub-only assertion could see it.
puts "\nselection: one fact, not two"

::vinstpane::build_list   .instlist
::vinstpane::build_detail .instdetail
pack .instlist .instdetail -fill both -expand 1
set ::vinstpane::R_list   .instlist
set ::vinstpane::R_detail .instdetail
::vinstpane::_detail_tags .instdetail.body
::vinstpane::refresh_list

::vinstpane::_select lo
ok "a programmatic _select moves the WIDGET too" {
    [.instlist.list selection] eq [list lo]
}
# THE STUB NOW PUMPS THE EVENT LOOP, the way http::wait does inside a
# real ::vapi::get. That is the only condition under which the bug
# exists: the <<TreeviewSelect>> queued by the PREVIOUS refresh_list is
# dispatched in the middle of THIS poll, and _pick reads the widget.
set ::_http::pump 1
::vinstpane::_select vps
set ::_http::pump 0
ok "a poll that pumps the event loop does not drag the selection back" {
    $::vinstpane::selected eq "vps"
}
ok "…and the widget agrees with it" { [.instlist.list selection] eq [list vps] }
update
ok "draining what is left changes nothing" {
    $::vinstpane::selected eq "vps" && [.instlist.list selection] eq [list vps]
}
# And the detail on screen belongs to the row that is selected — the
# live symptom was `wrongtoken`'s row showing `demo`'s name and data.
ok "the detail pane is headed by the SELECTED instance, not the previous one" {
    [string first "vps" [.instdetail.body get 1.0 3.end]] >= 0
    && [string first "\nlo\n" [.instdetail.body get 1.0 3.end]] < 0
}

destroy .instlist
destroy .instdetail
set ::vinstpane::R_list   .instlist
set ::vinstpane::R_detail .instdetail
set ::vinstpane::selected ""

puts "\ncluster registry: automatic, inspectable and not implicitly SSH-managed"
::vinstpane::build_list .instlist
::vinstpane::build_detail .instdetail
::vinstpane::build_ctl .instctl
set ::vinstpane::fleet_nodes [list [dict create \
    id node-unmanaged name {GPU worker} host worker version 0.2.0 \
    load1 37 stale 0 endpoint http://10.0.30.9:8642 static_ip null \
    heartbeat_at {2026-11-04 00:00:00} uptime_class always-on \
    sessions_served 12 capabilities null replica null sync_peer null self 0]]
::vinstpane::refresh_list
ok "a registry node appears without a configured instance" {
    [.instlist.list exists cluster:node-unmanaged]
}
# Load in its own column: it sat under "uptime" as "load 37" (SMOKE-21).
ok "the cluster node carries its version and load, each in its column" {
    [.instlist.list set cluster:node-unmanaged version] eq "0.2.0"
    && [.instlist.list set cluster:node-unmanaged load] eq "37"
    && [.instlist.list set cluster:node-unmanaged uptime] eq ""
    && [.instlist.list set cluster:node-unmanaged platforms] eq "registered"
}
::vinstpane::_select cluster:node-unmanaged
ok "the cluster node is inspectable" {
    [string match "*GPU worker*" [.instdetail.body get 1.0 end]]
}
ok "a field the registry has no value for is left out, not printed null" {
    ![string match "*[::vi18n::mc {Static IP}]*" [.instdetail.body get 1.0 end]]
    && ![string match "*: null*" [.instdetail.body get 1.0 end]]
}
# 0.2.2: Restart on a registry row is the node restarting itself through
# its supervisor, asked over the cluster link (lib/fleetmanage.tcl); the
# ssh verbs stay a saved instance's.
ok "the cluster node offers no SSH lifecycle: Start and Stop are off, Restart is the cluster link's" {
    [.instctl.start instate disabled] && [.instctl.stop instate disabled]
    && ![.instctl.restart instate disabled]
}
ok "the cluster node cannot masquerade as an attachable instance" {
    [.instctl.attach instate disabled]
}
ok "a node without a sync peer cannot request a guessed sync" {
    [.instctl.sync instate disabled] && [.instctl.apply instate disabled]
}
ok "a fresh node cannot be forgotten, and a remote one can be probed" {
    [.instctl.forget instate disabled] && ![.instctl.probe instate disabled]
}
namespace eval ::vapp {
    variable attached_name ""
    proc attach {name} { set ::vapp::attached_name $name; return 1 }
}
rename ::vapp::attach {}
source [file join $_dir attach.tcl]
set fleet_node [lindex $::vinstpane::fleet_nodes 0]
dict set fleet_node sync_peer gpu-worker
set ::vinstpane::fleet_nodes [list $fleet_node]
::vinstpane::_enable_buttons
ok "a configured sync peer enables its explicit sync operation" {
    ![.instctl.sync instate disabled]
}
namespace eval ::vapi {
    variable fleet_fail ""
    variable fleet_posts {}
    proc post_json_async {path body on_err on_ok} {
        lappend ::vapi::fleet_posts [list $path $body]
        # A refusal arrives exactly as _post_json_async_done builds it.
        if {$::vapi::fleet_fail ne ""} { {*}$on_err $::vapi::fleet_fail ; return }
        {*}$on_ok [dict create result "nothing to push"]
    }
}
# The confirmation prompt answers from a variable, and counts its asks.
set ::confirm_answer 0
set ::confirm_asked 0
proc ::vinstpane::_confirm {message detail} {
    incr ::confirm_asked
    return $::confirm_answer
}
::vinstpane::_cluster_sync
ok "Sync now offers the kinds this node is a peer for" {
    [winfo exists .fleetsync] && [winfo exists .fleetsync.kstore]
    && ![winfo exists .fleetsync.kmemory]
}
.fleetsync.btns.ok invoke
ok "sync targets the registered node, not a pasted endpoint or token" {
    $::vapi::fleet_posts eq [list [list /api/fleet/sync [dict create kind store operation push node node-unmanaged]]]
}
ok "a store push asks no confirmation (it only lands in the peer's inbox)" {
    $::confirm_asked == 0 && ![winfo exists .fleetsync]
}
ok "the push result is shown, labelled with its kind" {
    $::vstatus::last eq "Store sync: nothing to push"
}
# SMOKE-20: a refused sync showed `HTTP 409: {"error":"…"}` in the
# status line. The engine's words, not its JSON.
set ::vapi::fleet_fail {HTTP 409: {"error":"this node has no vault to send"}}
::vinstpane::_sync_request push store node-unmanaged 0
set ::vapi::fleet_fail ""
ok "a refused sync shows the engine's reason, not its JSON" {
    $::vstatus::last eq "Store sync failed: this node has no vault to send"
}
dict set fleet_node self 1
set ::vinstpane::fleet_nodes [list $fleet_node]
::vinstpane::_enable_buttons
ok "the self node can apply received offers without a push target" {
    ![.instctl.apply instate disabled]
}
ok "this gateway's own row can neither be forgotten nor probed" {
    [.instctl.forget instate disabled] && [.instctl.probe instate disabled]
}
set ::vinstpane::fleet_sync [dict create inbox [dict create store 1 memory 0 config 2 vault 0]]
set ::vapi::fleet_posts {}
::vinstpane::_cluster_apply
ok "Apply received offers every sync kind, with what is waiting" {
    [winfo exists .fleetsync.kstore] && [winfo exists .fleetsync.kmemory]
    && [winfo exists .fleetsync.kconfig] && [winfo exists .fleetsync.kvault]
    && [string match "*2 waiting*" [.fleetsync.kconfig cget -text]]
}
ok "only the store drain is preselected; data-changing kinds are opt-in" {
    $::vinstpane::_sync_pick(store) == 1 && $::vinstpane::_sync_pick(config) == 0
    && $::vinstpane::_sync_pick(vault) == 0 && $::vinstpane::_sync_pick(memory) == 0
}
destroy .fleetsync
# Nothing waiting is nothing to preselect: the store drain was checked
# with 0 waiting beside a config row with 1 (SMOKE-21). A kind that
# changes data stays opt-in however much waits.
set ::vinstpane::fleet_sync [dict create inbox [dict create store 0 memory 0 config 1 vault 0]]
::vinstpane::_cluster_apply
ok "a kind with nothing waiting is not preselected; a data-changing one stays opt-in" {
    $::vinstpane::_sync_pick(store) == 0 && $::vinstpane::_sync_pick(config) == 0
}
destroy .fleetsync
set ::vinstpane::fleet_sync [dict create inbox [dict create store 1 memory 0 config 2 vault 0]]
::vinstpane::_cluster_apply
.fleetsync.btns.ok invoke
ok "apply received drains the local inbox, not a remote endpoint" {
    $::vapi::fleet_posts eq [list [list /api/fleet/sync [dict create kind store operation apply]]]
}
# A config apply rewrites config.toml: declining the prompt sends nothing.
set ::vapi::fleet_posts {}
::vinstpane::_cluster_apply
set ::vinstpane::_sync_pick(store) 0
set ::vinstpane::_sync_pick(config) 1
.fleetsync.btns.ok invoke
ok "declining the confirmation sends nothing" {
    $::confirm_asked == 1 && $::vapi::fleet_posts eq {}
    && $::vstatus::last eq "Sync cancelled."
}
set ::confirm_answer 1
::vinstpane::_cluster_apply
set ::vinstpane::_sync_pick(store) 0
set ::vinstpane::_sync_pick(config) 1
.fleetsync.btns.ok invoke
ok "a confirmed config apply carries confirm: true to the gateway" {
    [llength $::vapi::fleet_posts] == 1
    && [lindex $::vapi::fleet_posts 0 0] eq "/api/fleet/sync"
    && [::vapi::_json_value [lindex $::vapi::fleet_posts 0 1]] eq
       {{"kind": "config", "operation": "apply", "confirm": true}}
}
set ::confirm_answer 0
dict set fleet_node self 0
set ::vinstpane::fleet_nodes [list $fleet_node]
::vinstpane::_enable_buttons
ok "another node's Apply button stays disabled" {
    [.instctl.apply instate disabled]
}
::vinst::add worker http://127.0.0.1:8642 token
::vapi::bind_instance worker http://127.0.0.1:8642 token
::vinstpane::_enable_buttons
ok "a hostname-only saved gateway cannot masquerade as the selected node" {
    [.instctl.attach instate disabled] &&
    [::vinstpane::_cluster_gateway $fleet_node] eq ""
}
# FLEET-3: the cluster branch used to leave Fleet deploy in whatever
# state the previously selected row gave it.
.instctl.deploy configure -state normal
::vinstpane::_enable_buttons
ok "Fleet deploy is disabled on a cluster row with no bound saved instance, whatever the previous row left" {
    [.instctl.deploy instate disabled]
}
::vinst::update worker node_id other-node
ok "a different explicit node association is not enough" {
    [::vinstpane::_cluster_gateway $fleet_node] eq ""
}
::vinst::update worker node_id node-unmanaged
::vinstpane::_enable_buttons
ok "an explicitly associated saved gateway is attachable" {
    ![.instctl.attach instate disabled]
}
.instctl.deploy configure -state disabled
::vinstpane::_enable_buttons
ok "Fleet deploy is enabled on a cluster row bound to a saved instance, whatever the previous row left" {
    ![.instctl.deploy instate disabled]
}
ok "cluster attach uses the associated saved gateway, not the A2A endpoint" {
    [::vinstpane::_cluster_gateway $fleet_node] eq "worker"
}
::vinstpane::refresh_list
ok "a bound registry row says which saved instance manages it" {
    [.instlist.list set cluster:node-unmanaged platforms] eq "managed via worker"
}
# FLEET-4: the association is verified against the gateway itself.
::_http::reset
::_http::ok_with {{"nodes":[{"id":"node-elsewhere","self":true},{"id":"node-unmanaged","self":false}]}}
set ::vapp::attached_name ""
ok "attach refuses a saved gateway that is a different node" {
    [::vinstpane::attach cluster:node-unmanaged] == 0
    && [string match "*is node node-elsewhere, not node-unmanaged*" $::vstatus::last]
}
::_http::ok_with {{"nodes":[{"id":"node-unmanaged","self":true}]}}
ok "a gateway whose own self row matches verifies" {
    [::vinstpane::verify_node_binding worker node-unmanaged] eq ""
}
::_http::ok_with {{"nodes":[]}}
ok "a gateway with no node identity cannot be verified" {
    [string match "*cannot be verified*" [::vinstpane::verify_node_binding worker node-unmanaged]]
}
# FLEET-2: start / stop / restart of a saved instance bound to a node
# prove the binding first, over the instance's own gateway. ssh itself is
# never run here: `open |…` is intercepted and records the verb it got.
::vinst::update worker host worker@example.invalid
::vinst::update worker unit veles-gateway
rename ::open ::_insttest_open
set ::ssh_verbs {}
proc ::open {args} {
    set what [lindex $args 0]
    if {[string index $what 0] eq "|"} {
        lappend ::ssh_verbs [lindex [string range $what 1 end] 8]
        return -code error "insttest: ssh is not run"
    }
    return [uplevel 1 [list ::_insttest_open {*}$args]]
}
::_http::ok_with {{"nodes":[{"id":"node-elsewhere","self":true},{"id":"node-unmanaged","self":false}]}}
set refused_verbs {}
foreach verb {start stop restart} {
    lappend refused_verbs [::vinstpane::_ssh_run worker $verb]
}
ok "start, stop and restart refuse a saved instance that answers as another node" {
    $refused_verbs eq {0 0 0} && $::ssh_verbs eq {} &&
    [string match "*is node node-elsewhere, not node-unmanaged*" $::vstatus::last]
}
::_http::ok_with {{"nodes":[]}}
ok "restart refuses a saved instance that reports no node identity" {
    [::vinstpane::_ssh_run worker restart] == 0 && $::ssh_verbs eq {} &&
    [string match "*cannot be verified*" $::vstatus::last]
}
::_http::ok_with {{"nodes":[{"id":"node-unmanaged","self":true}]}}
::vinstpane::_ssh_run worker restart
ok "a saved instance that answers as its node is restarted" { $::ssh_verbs eq {restart} }
# A stopped or hung gateway cannot answer. Start is how it comes back,
# so it proceeds; a stop or a restart asks the operator first.
::_http::fail_with 500
set ::ssh_verbs {}
set ::confirm_asked 0
set ::confirm_answer 0
::vinstpane::_ssh_run worker restart
ok "a restart of a gateway that cannot be asked waits for the operator, and a no refuses" {
    $::confirm_asked == 1 && $::ssh_verbs eq {} &&
    [string match "*Cannot verify that worker is node node-unmanaged*" $::vstatus::last]
}
set ::confirm_answer 1
::vinstpane::_ssh_run worker stop
set ::confirm_answer 0
ok "…and a yes runs it" { $::confirm_asked == 2 && $::ssh_verbs eq {stop} }
::vinstpane::_ssh_run worker start
ok "a start of a gateway that cannot be asked proceeds without asking" {
    $::confirm_asked == 2 && $::ssh_verbs eq {stop start}
}
# Once up, the started gateway is checked: a wrong binding is said.
::_http::ok_with {{"nodes":[{"id":"node-elsewhere","self":true}]}}
set ::vinstpane::ssh_tries 0
::vinstpane::_wait_healthy worker
ok "a started gateway that turns out to be another node is reported" {
    [string match "*worker is up, but its node binding is wrong*node-elsewhere*" $::vstatus::last]
}
::vinst::update worker node_id ""
::_http::reset
set ::ssh_verbs {}
::vinstpane::_ssh_run worker restart
ok "a saved instance with no node binding is restarted without asking any gateway" {
    $::ssh_verbs eq {restart} && ![::_http::seen /api/fleet]
}
rename ::open {}
rename ::_insttest_open ::open
::vinst::update worker node_id node-unmanaged
::vinst::update worker host ""
::_http::ok_with {{"status":"ok"}}
set ::vinst::instances {}
::vinst::load
ok "the explicit node association survives the 0600 registry" {
    [inst_field_str worker node_id] eq "node-unmanaged" &&
    ([file attributes [::vinst::path] -permissions] & 0777) == 0600
}
# FLEET-4: drive the REAL editor save path, not ::vinst::update.
proc save_form {name url node_id} {
    return [::vinstpane::_save_entry $name [dict create name $name url $url \
        token token host "" unit veles-gateway group "" node_id $node_id]]
}
ok "the editor can clear the node association" {
    [save_form worker http://127.0.0.1:8642 ""] == 1
    && ![dict exists [::vinst::get worker] node_id]
    && [::vinstpane::_cluster_gateway $fleet_node] eq ""
}
::vinst::update worker node_id node-unmanaged
# Add… with a name that exists replaces that entry: same save path.
ok "re-adding a name does not inherit the old binding" {
    [save_form worker http://127.0.0.1:9999 ""] == 1
    && ![dict exists [::vinst::get worker] node_id]
}
ok "an id the attached registry does not know is refused" {
    [save_form worker http://127.0.0.1:8642 node-typo] == 0
    && [string match "*node-typo*" $::vstatus::last]
    && ![dict exists [::vinst::get worker] node_id]
}
ok "a known registry id is saved" {
    [save_form worker http://127.0.0.1:8642 node-unmanaged] == 1
    && [inst_field_str worker node_id] eq "node-unmanaged"
}
# FLEET-13: names that collide with the tree's reserved ids.
ok "a saved name cannot pose as a registry row or a group" {
    [save_form cluster:x http://127.0.0.1:8642 ""] == 0
    && [save_form g:x http://127.0.0.1:8642 ""] == 0
    && [::vinst::get cluster:x] eq ""
}
::vinst::add grouped http://127.0.0.1:8643 ""
::vinst::update grouped group cluster-registry
ok "a saved group named cluster-registry renders beside the registry" {
    ![catch { ::vinstpane::refresh_list }]
    && [.instlist.list exists g:cluster-registry]
    && [.instlist.list exists cluster:node-unmanaged]
}
::vinst::remove grouped
# FLEET-6: Add… from a registry row adopts the node, guessing no target.
set d [::vinstpane::add_defaults]
ok "Add… from a registry row fills its name and node id, not its endpoint" {
    [dict get $d name] eq "GPU worker" && [dict get $d node_id] eq "node-unmanaged"
    && [dict get $d url] eq "http://127.0.0.1:8642" && [dict get $d host] eq ""
}
# Forget: only a stale, non-self row, and only after the prompt.
dict set fleet_node stale 1
set ::vinstpane::fleet_nodes [list $fleet_node]
::vinstpane::_enable_buttons
ok "a stale node can be forgotten" { ![.instctl.forget instate disabled] }
set ::vapi::fleet_posts {}
set ::confirm_asked 0
::vinstpane::_ctl_forget
ok "forgetting asks first, and a no sends nothing" {
    $::confirm_asked == 1 && $::vapi::fleet_posts eq {}
}
set ::confirm_answer 1
::vinstpane::_ctl_forget
ok "a confirmed forget names the node and carries confirm: true" {
    [lindex $::vapi::fleet_posts 0 0] eq "/api/fleet/forget"
    && [::vapi::_json_value [lindex $::vapi::fleet_posts 0 1]] eq
       {{"node": "node-unmanaged", "confirm": true}}
    && [string match "Forgot node GPU worker*" $::vstatus::last]
}
set ::confirm_answer 0
set ::vinstpane::selected cluster:node-unmanaged
set ::vapi::fleet_posts {}
::vinstpane::_ctl_probe
ok "probe asks the gateway about the node id, nothing else" {
    $::vapi::fleet_posts eq [list [list /api/fleet/probe [dict create node node-unmanaged]]]
}
# The per-kind status the gateway returns is shown, not thrown away
# (FLEET-5: `fleet_sync` used to be set and read nowhere).
dict set fleet_node stale 0
dict set fleet_node sync_kinds [list memory store]
dict set fleet_node health [dict create state failing heartbeat_age_secs 12 \
    failing [list memory] probe null]
dict set fleet_node sync_status [dict create \
    memory [dict create peer gpu-worker auto true dialable true \
        backoff [dict create failures 2 retry_in_secs 90] \
        last [dict create at 2026-11-06T00:00:00Z ok false line {connection refused}] \
        cursor 4 local 9 behind 5] \
    store [dict create peer gpu-worker auto false dialable false backoff null \
        last null cursor 10 acked 8 local 12 behind 2]]
set ::vinstpane::fleet_nodes [list $fleet_node]
::vinstpane::_paint_cluster_detail cluster:node-unmanaged
::vinstpane::refresh_list
set body [.instdetail.body get 1.0 end]
ok "the detail shows each sync kind's peer, mode and cursor" {
    [string match "*Memory sync with gpu-worker (automatic)*" $body]
    && [string match "*cursor 4 of 9 (5 behind)*" $body]
    && [string match "*Store sync with gpu-worker (manual only)*" $body]
}
ok "the detail shows a failing kind's last error and backoff" {
    [string match "*last pass failed at 2026-11-06T00:00:00Z: connection refused*" $body]
    && [string match "*2 failure(s), next automatic try in 90 s*" $body]
}
ok "the detail says when a peer has no dial entry" {
    [string match "*cannot be dialed*" $body]
}
ok "health is shown and a failing node is red in the list" {
    [string match "*Health: failing*" $body]
    && [.instlist.list item cluster:node-unmanaged -tags] eq "down"
}
# FLEET-8: the SSH line follows the binding. It used to say "No SSH
# lifecycle target is configured" on every registry row, even one whose
# bound saved instance has a host and a unit and offers Fleet deploy.
ok "a node bound to a saved instance with no ssh host has no lifecycle target" {
    [string match "*No SSH lifecycle target is configured*" $body]
}
::vinst::update worker host worker@example.invalid
::vinstpane::_paint_cluster_detail cluster:node-unmanaged
set body [.instdetail.body get 1.0 end]
ok "a node bound to a saved instance with a host and unit names it, and does not deny it" {
    ![string match "*No SSH lifecycle target*" $body]
    && [string match "*SSH lifecycle: through saved instance worker (worker@example.invalid, unit veles-gateway)*" $body]
}
::vinst::update worker host ""
::vinst::remove worker
set ::vinstpane::fleet_nodes {}
destroy .instlist .instdetail .instctl
set ::vinstpane::selected ""

puts "\ncluster registry: discovered through the real poll"
# FLEET-10(a): nothing drove refresh_fleet/_fleet_loaded before; the
# rows were set by hand. This feeds the REAL tick a literal /api/fleet
# body of the shape fleet_inventory_json + fleet_ops::annotate return.
::vinstpane::build_list .instlist
::vinstpane::build_detail .instdetail
::vinstpane::build_ctl .instctl
set _tab_was $::vtabs::current
set ::vtabs::current instances
::_http::ok_with {{"nodes":[{"id":"node-a","name":"alpha","host":"alpha-host","version":"0.2.0","uptime_class":"always-on","endpoint":"http://10.0.30.9:8642","static_ip":null,"static_ip_source":null,"gpu":null,"capabilities":null,"load1":0.5,"load_at":null,"sessions_served":3,"registered_at":"2026-11-06 00:00:00","heartbeat_at":"2026-11-06 00:00:00","self":false,"stale":false,"sync_peer":"alpha","replica":null,"health":{"state":"fresh","heartbeat_age_secs":12,"failing":[],"probe":null},"sync_kinds":["memory","store"],"sync_status":{"memory":{"peer":"alpha","auto":true,"dialable":true,"backoff":null,"last":null,"cursor":4,"local":9,"behind":5},"store":{"peer":"alpha","auto":false,"dialable":true,"backoff":null,"last":null,"cursor":10,"acked":8,"local":12,"behind":2}}}],"stale_after_secs":300,"sync":{"gossip_peers":["alpha"],"memory_peers":["alpha"],"store_peers":["alpha"],"memory_auto":true,"store_auto":false,"config_auto":false,"config_waiting":0,"inbox":{"store":1,"memory":0,"config":2,"vault":0}}}}
::vinstpane::_poll_list_tick
ok "the list tick asks the attached gateway for its registry" {
    [::_http::seen /api/fleet]
}
# The stub never completes a transfer; finish it as http would.
foreach tok [array names ::vapi::_async_done] { ::vapi::_get_json_async_done $tok }
ok "the registry's nodes become rows without any configuration" {
    [.instlist.list exists cluster:node-a]
    && [lindex [.instlist.list item cluster:node-a -text] end] eq "alpha"
}
ok "the sync block is kept, not discarded" {
    [::vapi::field [::vapi::field $::vinstpane::fleet_sync inbox] config 0] == 2
}
::vinstpane::_select cluster:node-a
ok "a discovered peer node offers Sync now" {
    ![.instctl.sync instate disabled]
}
# A memory-only peer (no store peer at all) still gets Sync now, and the
# dialog offers exactly its kind.
set _memnode [lindex $::vinstpane::fleet_nodes 0]
dict set _memnode sync_peer null
dict set _memnode sync_kinds [list memory]
set ::vinstpane::fleet_nodes [list $_memnode]
::vinstpane::_enable_buttons
ok "a memory-only sync peer offers Sync now" {
    ![.instctl.sync instate disabled]
}
::vinstpane::_cluster_sync
ok "its Sync now offers memory, not a guessed store push" {
    [winfo exists .fleetsync.kmemory] && ![winfo exists .fleetsync.kstore]
}
catch { destroy .fleetsync }
after cancel $::vinstpane::list_after

# FLEET-2: a host name that stops resolving makes http::geturl raise
# synchronously. That used to unwind the tick before it re-armed.
set ::_http::throw_on /api/fleet
set ::vinstpane::list_after ""
::_http::reset
::vinst::add lohealth http://127.0.0.1:8642 ""
::vapi::bind_instance lohealth http://127.0.0.1:8642 ""
ok "a synchronous registry failure does not escape the tick" {
    ![catch { ::vinstpane::_poll_list_tick }]
}
ok "the list poll is re-armed after a synchronous failure" {
    $::vinstpane::list_after ne "" && $::vinstpane::list_after in [after info]
}
ok "health still polls when the registry fetch cannot even start" {
    [::_http::seen http://127.0.0.1:8642/api/health]
}
ok "an unreachable registry is an empty one, not stale rows" {
    $::vinstpane::fleet_nodes eq {} && ![.instlist.list exists cluster:node-a]
}
after cancel $::vinstpane::list_after
set ::_http::throw_on ""
::vinst::remove lohealth
set ::vtabs::current $_tab_was
set ::vinstpane::fleet_nodes {}
set ::vinstpane::fleet_sync {}
destroy .instlist .instdetail .instctl
set ::vinstpane::selected ""
::_http::ok_with {{"status":"ok"}}

# ── Part 6: attach ──────────────────────────────────────────────────

puts "\nattach"

# Stub the pane-adjacent consumers teardown touches, recording calls.
namespace eval ::_trace { variable calls {} ; proc note {s} { lappend ::_trace::calls $s } }
namespace eval ::virc {
    variable cursor 4242
    variable stream "irc-stream-1"
    proc disconnect {} { ::_trace::note irc-disconnect }
    proc reset_cursor {} { ::_trace::note irc-reset-cursor ; variable cursor ; set cursor 0 }
    proc reconnect {} { ::_trace::note irc-reconnect }
}
# The LSP stop records WHERE its POST would have gone, not just that it
# fired: landing it on the incoming gateway leaves two language servers
# running and the new one's diagnostics arriving for the old one's files.
namespace eval ::vlsp {
    variable stop_url ""
    proc _stop {why} {
        ::_trace::note lsp-stop
        set ::vlsp::stop_url [::vapi::_url /api/lsp/stop]
    }
}
namespace eval ::vdev { proc close_all {} { ::_trace::note dev-close } }
namespace eval ::vpanes {
    proc detach_clear {} { ::_trace::note panes-clear }
    proc refresh_files {} { ::_trace::note files-refresh }
    proc refresh_review {} { ::_trace::note review-refresh }
}
namespace eval ::vsidebar {
    proc detach_clear {} { ::_trace::note sidebar-clear }
    proc refresh {} { ::_trace::note sidebar-refresh }
}
namespace eval ::vstatus {
    proc set_attachment {spec} { ::_trace::note "attach-label" }
    proc poll_now {} { ::_trace::note status-poll }
    proc poll {} {}
    proc detach_async {} { ::_trace::note status-async-clear }
}
namespace eval ::voverlay {
    proc detach_async {} { ::_trace::note overlays-async-clear }
}
namespace eval ::vtabs { proc select {id} { ::_trace::note "select-$id" } }

set ::_trace::calls {}
::_http::reset
::vapp::attach vps
ok "attach sets ::vapi::attached" { [::vapi::attached] eq "vps" }
ok "attach persists last_attached" { [::vinst::last_attached] eq "vps" }
ok "the teardown closed the IRC stream and reset the cursor" {
    [lsearch -exact $::_trace::calls irc-disconnect] >= 0
    && $::virc::cursor == 0
}
set _flip [lsearch -exact $::_trace::calls attach-label]
set _lsp  [lsearch -exact $::_trace::calls lsp-stop]
ok "the teardown invalidated usage/status callbacks before the flip" {
    [lsearch -exact $::_trace::calls status-async-clear] >= 0
    && [lsearch -exact $::_trace::calls overlays-async-clear] >= 0
    && [lsearch -exact $::_trace::calls status-async-clear] < $_flip
    && [lsearch -exact $::_trace::calls overlays-async-clear] < $_flip
}
ok "LSP stop fired BEFORE the flip (checklist order)" {
    $_lsp >= 0 && $_flip > $_lsp
}
# …and firing early is only half of it: assert WHERE it landed. Step
# order is the only thing aiming this POST at the outgoing gateway
# (::vapi::attached returns `current`, so wrapping it in with_instance
# would be a no-op, not a second guarantee). Move the step below
# use_instance and this goes red while the order check above still
# passes on the label alone.
ok "…and the LSP stop POSTed to the OUTGOING gateway, not the new one" {
    $::vlsp::stop_url eq "http://127.0.0.1:8642/api/lsp/stop"
}
set _s [lsearch -exact $::_trace::calls irc-disconnect]
set _c [lsearch -exact $::_trace::calls irc-reset-cursor]
ok "streams closed BEFORE the cursor reset" {
    $_s >= 0 && $_c > $_s
}
ok "the rebuild refreshed the new gateway's panes" {
    [lsearch -exact $::_trace::calls sidebar-refresh] >= 0
    && [lsearch -exact $::_trace::calls files-refresh] >= 0
    && [lsearch -exact $::_trace::calls irc-reconnect] >= 0
}

# Attach switches EVERY consumer: after attaching to vps, _url resolves
# to vps's url — the same resolution chat, irc, dev and the status bar
# all use.
ok "after attach, _url resolves to vps for every consumer" {
    [::vapi::_url /api/status] eq "http://10.0.20.7:8642/api/status"
}

# ── THE EPOCH DISCARD RULE ──────────────────────────────────────────
#
# The plan's non-negotiable test, and the one this suite did not have:
# it asserted only that the COUNTER moved, which every implementation
# with a working `incr` passes. What matters is that a callback issued
# before an attach and delivered after it is DROPPED — gateway A's
# in-flight reply must not paint into gateway B's panes.
#
# The streams and the async POST both capture ::vapp::epoch at call
# time. Closing the socket is not enough on its own: the teardown
# closes it, but the socket's own -command still fires one event loop
# later with whatever was already buffered.
set ::_trace::calls {}
set _old_epoch [::vapp::epoch]
::vapp::attach lo
ok "the epoch moved on attach" { ![::vapp::fresh $_old_epoch] }

# An EVENT STREAM opened against vps, then an attach elsewhere, then the
# socket dies with a 401. The handler must never be called for a stream
# from a previous epoch — it would report into panes that now belong to a
# different gateway — and the stream must still be cleaned up, not leaked.
::_http::ok_with {{"status":"ok"}}
::vapp::attach vps
set _stale_stream [::vapi::session_events s-stale 0 0 ::_apu::noop]
::_http::fail_with 401
::vapp::attach lo                      ;# epoch N -> N+1
set ::_apu::last {}
::vapi::_sse_finished $_stale_stream
ok "a stream from the PREVIOUS attach epoch reports nothing" {
    $::_apu::last eq {}
}
ok "…and is still cleaned up, not leaked" {
    [lsearch -exact [::vapi::streaming_sessions] s-stale] < 0
}

# …while a stream from the CURRENT epoch still reports normally. Without
# this the discard could be "never report anything" and look correct.
set _live_stream [::vapi::session_events s-live 0 0 ::_apu::noop]
set ::_apu::last {}
::vapi::_sse_finished $_live_stream
ok "a stream from the CURRENT epoch still reports its failure" {
    $::_apu::last ne {}
}
::_http::ok_with {{"status":"ok"}}

# The same rule on post_async, which is how the LSP talks.
namespace eval ::_ep { variable heard "" }
proc ::_ep::note {why} { set ::_ep::heard $why }
::_http::fail_with 500
set ::_ep::heard ""
::vapi::post_async /api/lsp/didChange {{}} ::_ep::note
set _pa_epoch [::vapp::epoch]
::vapp::attach vps                     ;# the world moves while it is out
::vapi::_post_async_done ::_ep::note "" "http://10.0.20.7:8642" $_pa_epoch stub-token
ok "a post_async reply from the previous epoch is discarded" {
    $::_ep::heard eq ""
}
::vapi::_post_async_done ::_ep::note "" "http://10.0.20.7:8642" [::vapp::epoch] stub-token
ok "…and one from the current epoch is reported" { $::_ep::heard ne "" }
ok "…naming the gateway the request was actually sent to" {
    [string match {http://10.0.20.7:8642 *} $::_ep::heard]
}
::_http::ok_with {{"status":"ok"}}
::vapp::attach lo

# Detach: restores "", selects the Instances tab.
set ::_trace::calls {}
::vapp::attach ""
ok "detach restores \"\"" { [::vapi::attached] eq "" && [::vapi::unattached] }
ok "detach persists the cleared last_attached" { [::vinst::last_attached] eq "" }
::vinstpane::detach
ok "the PANE's detach selects the Instances tab" {
    [lsearch -exact $::_trace::calls select-instances] >= 0
}

# Attach to A, detach, attach to B: no residue of A.
::_http::reset
::vapp::attach lo
::vapp::attach vps
ok "switching lo→vps leaves _url pointing at vps" {
    [::vapi::_url /x] eq "http://10.0.20.7:8642/x"
}
ok "the IRC cursor is 0 after the switch" { $::virc::cursor == 0 }

# ── unattached boot quietness ───────────────────────────────────────

puts "\nunattached boot"

set ::_trace::calls {}
::_http::reset
::vapi::use_instance ""
set ::vinst::last_attached ""
::vapp::boot_decide
::vapp::boot
ok "an unattached boot attaches to nothing" { [::vapi::unattached] }
ok "it selects the Instances tab" {
    [lsearch -exact $::_trace::calls select-instances] >= 0
}
ok "the status poll, sidebar refresh and every pane stay quiet" {
    [llength $::_http::requests] == 0
}

# Boot WITH a remembered instance reattaches to it — including the
# binding step: last_attached is a NAME in the registry file, and
# nothing else binds registry names into ::vapi at boot. (Found live:
# the remembered instance was silently discarded and every boot came
# up unattached.)
::_http::reset
set ::vinst::instances {}
::vinst::add "vps" "http://10.0.20.7:8642" "tk"
::vapi::bind_instance vps "http://10.0.20.7:8642" "tk"
::vinst::set_last_attached vps
# …and the binding cleared, as it would be in a FRESH process:
array unset ::vapi::instances
::vapi::use_instance ""
::vapp::boot_decide
ok "a remembered instance is reattached at boot" {
    [::vapi::attached] eq "vps"
}
ok "the remembered instance is BOUND before attaching" {
    [::vapi::instance_bound vps]
    && [::vapi::_url /x] eq "http://10.0.20.7:8642/x"
}

file delete -force $_scratch
file delete -force [file join $_dir .apitest-home]

puts "\ninsttest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
