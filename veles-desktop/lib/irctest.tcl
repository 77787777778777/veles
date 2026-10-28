# veles-desktop/lib/irctest.tcl — the IRC client pane.
#
# What has real logic in it, and therefore real ways to be wrong:
#
#   - mIRC control codes. IRC lines are NOT markdown and must never go
#     through ::vmd::: a message containing *stars* is a message with
#     stars in it, and interpreting them corrupts what someone typed.
#     What lines do carry is \x02/\x1D/\x1F/\x1E/\x0F and \x03N,M, which
#     have to become tags — and an unrecognised control byte has to be
#     DROPPED, because a raw \x01 in a transcript is worse than losing
#     the styling it asked for.
#   - the resume cursor. Only REPLAYED lines carry a durable id; live
#     ones have not been written yet. Advancing the cursor from a live
#     event would leave it pointing at nothing, and the next reconnect
#     would either re-render or skip.
#   - roster order. Ops, then voiced, then the rest, alphabetical
#     within each — thirty years of every IRC client, and the reason is
#     that the people who can act on your request are the ones you are
#     scanning for.
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

namespace eval ::vstatus { variable last "" ; proc set_msg {m} { variable last $m } }
namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
namespace eval ::vi18n { proc label {w key args} { if {![winfo exists $w]} { return 0 } ; catch { $w configure -text [::vi18n::mc $key {*}$args] } ; return 1 }
                    proc aname {w key args} { if {![winfo exists $w]} { return 0 } ; catch { ::va11y::name $w [::vi18n::mc $key {*}$args] } ; return 1 } }
# `keepout` records into ::va11y::_zk so a test can assert the IRC
# composer actually registered itself; without it the stub throws
# "invalid command name" the moment build_view calls it.
namespace eval ::va11y {
    variable _zk {}
    proc name {w d} {}
    proc describe {w} { return "" }
    proc keepout {w} { variable _zk; if {!($w in $_zk)} { lappend _zk $w } }
}
namespace eval ::vctx   { proc attach {w i} {} }
namespace eval ::vcoach { proc attach {w} {} }
namespace eval ::vdock  { proc register {w b {r ""}} {} }
namespace eval ::vstate { proc get {k {d ""}} { return $d } ; proc put {k v} { return 1 } ; proc merge {d} { return 1 } }
namespace eval ::vt {
    variable T
    array set T {hairline #333 bg_panel #222 fg #eee bg_hover #444 accent #0ff
                 bg_input #111 fg_dim #888 select_bg #044 green #0f0 red #f00
                 accent_fg #000 pink #f0f gold #fc0}
}
# A stand-in gateway. Every request is recorded so the tests can assert
# what the pane ASKED for, not just what it did with the answer.
namespace eval ::vapi {
    variable requests {}
    variable state_reply {}
    variable backlog_reply {}
    variable opened {}
    variable fail_open 0
    variable get_error ""
    # refresh_state consults the real unattached guard (api.tcl is not
    # sourced here; define the guard this suite needs: always attached.
    proc unattached {} { return 0 }
    proc get_json {path} {
        variable requests ; lappend requests $path
        variable state_reply
        variable backlog_reply
        variable get_error
        # ::vapi::get_json throws "HTTP <code>: <body>" on a 4xx. The
        # IRC pane has to tell those codes apart, so the stub must be
        # able to produce them.
        if {$get_error ne ""} { error $get_error }
        if {[string match "/api/irc/state*" $path]}   { return $state_reply }
        if {[string match "/api/irc/backlog*" $path]} { return $backlog_reply }
        return {}
    }
    proc fatal_code {code} { return [expr {$code in {401 403 404}}] }
    proc http_reason {code} { return "gateway returned HTTP $code" }
    proc sse_open {path handler} {
        variable fail_open
        if {$fail_open} { error "connection refused" }
        variable opened ; lappend opened [list $path $handler]
        return "tok1"
    }
    proc sse_close {tok} {}
}
# paths.tcl before irc.tcl: the pane's not-configured text names the config
# file the gateway actually reads, so it has to be able to ask where that is.
source [file join $_dir paths.tcl]
source [file join $_dir safeexec.tcl]
source [file join $_dir vtext.tcl]
source [file join $_dir findbar.tcl]
source [file join $_dir irc.tcl]

# ── mIRC formatting ─────────────────────────────────────────────────
puts "mIRC control codes"

proc runs_text {s} {
    set out {}
    foreach r [::virc::parse_formatting $s] { lappend out [lindex $r 0] }
    return $out
}
proc tags_of {s idx} {
    return [lindex [lindex [::virc::parse_formatting $s] $idx] 1]
}

ok "plain text is one unstyled run" \
    {[::virc::parse_formatting "hello"] eq [list [list "hello" {}]]}

ok "bold opens a styled run" \
    {[lsearch -exact [tags_of "\x02bold\x02" 0] irc_bold] >= 0}
ok "…and closing it ends the styling" \
    {[lsearch -exact [tags_of "\x02bold\x02plain" 1] irc_bold] < 0}

ok "italic is tagged"    {[lsearch -exact [tags_of "\x1Dit\x1D" 0] irc_italic] >= 0}
ok "underline is tagged" {[lsearch -exact [tags_of "\x1Fun\x1F" 0] irc_under] >= 0}
ok "strike is tagged"    {[lsearch -exact [tags_of "\x1Est\x1E" 0] irc_strike] >= 0}

ok "reset clears everything at once" \
    {[tags_of "\x02\x1D\x1Fmess\x0Fclean" 1] eq {}}

ok "a colour code is tagged" \
    {[lsearch -exact [tags_of "\x034red" 0] irc_fg_red] >= 0}
ok "a bare colour code turns colour off" \
    {[tags_of "\x034red\x03plain" 1] eq {}}

# The codes themselves must never reach the transcript.
ok "control bytes never survive into the text" \
    {[::virc::strip_formatting "\x02b\x0F \x034c\x03 \x1Fu\x1F"] eq "b c u"}
ok "a background colour is parsed away, not printed" \
    {[::virc::strip_formatting "\x034,8warn"] eq "warn"}
ok "an unknown control byte is dropped" \
    {[::virc::strip_formatting "a\x01b\x07c"] eq "abc"}

# The thing that makes this NOT markdown.
ok "asterisks are literal text, not emphasis" \
    {[::virc::strip_formatting "a *literal* star"] eq "a *literal* star"}
ok "underscores are literal too" \
    {[::virc::strip_formatting "snake_case_name"] eq "snake_case_name"}

# A truncated colour code at end-of-line must not throw.
ok "a truncated colour code is survivable" \
    {![catch { ::virc::parse_formatting "text\x03" }]}
ok "…and so is a lone reset" {![catch { ::virc::parse_formatting "\x0F" }]}
ok "…and an empty string"    {[::virc::parse_formatting ""] eq {}}

# ── nick colours ────────────────────────────────────────────────────
puts "\nnick colours"

# Hashed, not assigned in arrival order: arrival order differs between
# a live view and a replayed backlog, so an order-assigned colour would
# change when you scroll up.
ok "a nick always gets the same colour" \
    {[::virc::nick_colour "wrath"] eq [::virc::nick_colour "wrath"]}
ok "case does not change it" \
    {[::virc::nick_colour "Wrath"] eq [::virc::nick_colour "wrath"]}
ok "different nicks can differ" \
    {[llength [lsort -unique [list [::virc::nick_colour a] [::virc::nick_colour b] \
        [::virc::nick_colour c] [::virc::nick_colour d] [::virc::nick_colour e]]]] > 1}

# ── roster order ────────────────────────────────────────────────────
puts "\nroster order"

set sorted [::virc::sort_roster {zed @bob alice +carol @adam %hop}]
ok "ops come first"            {[lrange $::sorted 0 1] eq {@adam @bob}}
ok "then half-ops"             {[lindex $::sorted 2] eq "%hop"}
ok "then voiced"               {[lindex $::sorted 3] eq "+carol"}
ok "then everyone else"        {[lrange $::sorted 4 5] eq {alice zed}}
ok "alphabetical within a rank" \
    {[::virc::sort_roster {@zoe @amy}] eq {@amy @zoe}}
ok "an empty roster is fine"   {[::virc::sort_roster {}] eq {}}

# ── hostmasks in NAMES ──────────────────────────────────────────────
#
# client.rs requests `userhost-in-names`, so a NAMES reply carries
# `nick!user@host`, not `nick`. Found live against the gateway on this
# machine, whose roster read:
#
#   jackma_veles!~u@example.com   wrathirc!~u@example.com
#
# That is not just ugly. `selected_nick` stripped the @%+ prefix and
# nothing else, so whois / op / deop / voice / devoice / kick were every
# one of them addressed to `nick!user@host` — a target no server matches.
puts "\nnicks out of hostmasks"

ok "a bare nick is itself"        {[::virc::nick_of {bob}] eq "bob"}
ok "a prefix is dropped"          {[::virc::nick_of {@bob}] eq "bob"}
ok "a hostmask is dropped"        {[::virc::nick_of {bob!~u@example.com}] eq "bob"}
ok "both at once"                 {[::virc::nick_of {@bob!~u@example.com}] eq "bob"}
ok "multi-prefix, both at once"   {[::virc::nick_of {@+bob!~u@e.com}] eq "bob"}
ok "an ident containing @ is not a nick boundary" \
    {[::virc::nick_of {bob!~a@b@c.example}] eq "bob"}
ok "a nick with a legal ! -less body survives" \
    {[::virc::nick_of {a-b_c\[0\]}] eq {a-b_c\[0\]}}

# The prefix is what says who is opped, so it has to survive.
ok "the prefix is kept for display" {[::virc::prefix_of {@bob!~u@e.com}] eq "@"}
ok "multi-prefix is kept whole"     {[::virc::prefix_of {@+bob}] eq "@+"}
ok "no prefix is empty"             {[::virc::prefix_of {bob!~u@e.com}] eq ""}
ok "the label is prefix + nick, no mask" \
    {[::virc::roster_label {@bob!~u@example.com}] eq "@bob"}

# The sort key is the NICK, not the entry.
#
# For a plain `@nick` the two are the same and the old `string range $m 1
# end` was right — but it strips ONE character, and multi-prefix (also
# negotiated) gives `@+nick`. That left "+" in the sort key, and "+"
# sorts below every letter, so a voiced op jumped to the top of its rank
# regardless of name.
ok "multi-prefix does not hijack the ordering" \
    {[::virc::sort_roster {@+zed @amy}] eq {@amy @+zed}}
ok "rank still wins over name" \
    {[::virc::sort_roster {alice!~a@h @bob!~z@h}] eq {@bob!~z@h alice!~a@h}}
ok "the full mask is what is KEPT — a ban needs it" \
    {[lindex [::virc::sort_roster {@bob!~u@example.com}] 0] eq {@bob!~u@example.com}}

# ── the whole way through ────────────────────────────────────────────
#
# Drive the REAL refresh from the REAL roster store, which is the path a
# NAMES reply takes. Populating the listbox by hand here would assert
# only that the test can call roster_label.
::virc::build_roster .ircroster
set _k [::virc::key irc.example bot #masked]
set ::virc::rosters($_k) {@jackma!~u@example.com wrathirc!~u@example.com}
::virc::_refresh_roster $_k

ok "the list shows a nick, not a mask" {[.ircroster.list get 0] eq "@jackma"}
ok "…and so does the second row"       {[.ircroster.list get 1] eq "wrathirc"}
ok "no row contains a hostmask" \
    {![string match "*!*" [join [.ircroster.list get 0 end] " "]]}

.ircroster.list selection set 0
ok "selecting an op yields the bare nick" {[::virc::selected_nick] eq "jackma"}
.ircroster.list selection clear 0
.ircroster.list selection set 1
ok "…and so does a plain member"          {[::virc::selected_nick] eq "wrathirc"}

# Belt and braces: even handed a raw mask — which is what the list held
# before this — the action target must still be a nick. whois, op, deop,
# voice, devoice and kick all go through selected_nick.
.ircroster.list delete 0 end
.ircroster.list insert end {@jackma!~u@example.com}
.ircroster.list selection set 0
ok "a raw mask in the list still yields a nick" {[::virc::selected_nick] eq "jackma"}

# ── the resume cursor ───────────────────────────────────────────────
puts "\nthe resume cursor"

proc reset_irc {} {
    set ::virc::cursor 0
    set ::virc::live 0
    set ::virc::current ""
    set ::virc::buffers {}
    set ::virc::sendable {}
    set ::virc::highlight_words {}
    array unset ::virc::complete
    array unset ::virc::unread ; array set ::virc::unread {}
    array unset ::virc::meta   ; array set ::virc::meta {}
    array unset ::virc::activity
    array unset ::virc::seen
    array unset ::virc::read_marker
    array unset ::virc::aways
    array unset ::virc::lasttouch
    array unset ::virc::recency
    array unset ::virc::own_nick
    array unset ::virc::linebuf
    array unset ::virc::earliest
    array unset ::virc::netstate
    set ::vapi::opened {}
    set ::vapi::requests {}
}

reset_irc
::virc::_ingest [dict create id 7 network n presence bot buffer #a kind privmsg nick w text hi]
ok "a replayed line advances the cursor" {$::virc::cursor == 7}

# A live event has NOT been written yet and carries no id. Advancing
# from it would leave the cursor pointing at nothing, and the next
# reconnect would either re-render or skip lines.
::virc::_ingest [dict create network n presence bot buffer #a kind privmsg nick w text live]
ok "a live line does not move it" {$::virc::cursor == 7}

::virc::_ingest [dict create id 3 network n presence bot buffer #a kind privmsg nick w text old]
ok "and it never goes backwards" {$::virc::cursor == 7}

# ── buffers and unread ──────────────────────────────────────────────
puts "\nbuffers and unread"

reset_irc
::virc::_ingest [dict create id 1 network n presence bot buffer #a kind privmsg nick w text one]
::virc::_ingest [dict create id 2 network n presence bot buffer #b kind privmsg nick w text two]
ok "each buffer is listed once" {[llength $::virc::buffers] == 2}
::virc::_ingest [dict create id 3 network n presence bot buffer #a kind privmsg nick w text three]
ok "…even when it speaks again"  {[llength $::virc::buffers] == 2}

ok "a line in a buffer you are not looking at is unread" \
    {$::virc::unread([::virc::key n bot #a]) == 2}
::virc::mark_read [::virc::key n bot #a]
ok "marking read clears it" {$::virc::unread([::virc::key n bot #a]) == 0}

# The same buffer name on two presences is two buffers: the admin's
# #veles is not the bot's #veles.
reset_irc
::virc::_ingest [dict create id 1 network n presence bot   buffer #v kind privmsg nick w text a]
::virc::_ingest [dict create id 2 network n presence admin buffer #v kind privmsg nick w text b]
ok "presence separates buffers of the same name" {[llength $::virc::buffers] == 2}

# ── connecting ──────────────────────────────────────────────────────
puts "\nconnecting and resuming"

reset_irc
::virc::connect
ok "a first connection asks for live only" \
    {[lindex [lindex $::vapi::opened 0] 0] eq "/api/irc/events"}

reset_irc
set ::virc::cursor 42
::virc::connect
ok "a reconnect resumes from the cursor" \
    {[lindex [lindex $::vapi::opened 0] 0] eq "/api/irc/events?since=42"}

# A stream that cannot be opened must SAY so — retrying quietly forever
# makes "IRC shows nothing" indistinguishable from "nobody is talking".
reset_irc
set ::vstatus::last ""
set ::vapi::fail_open 1
::virc::connect
ok "a failed open is reported" {[string match "*event stream*" $::vstatus::last]}
set ::vapi::fail_open 0

# The live marker flips the stream out of replay.
reset_irc
::virc::on_event irc-live {}
ok "the live marker ends the replay" {$::virc::live == 1}

# ── state ───────────────────────────────────────────────────────────
puts "\nstate"

reset_irc
set ::vapi::state_reply [dict create latest_id 99 networks [list \
    [dict create network irc.example presence bot nick jackma connected 1 \
        channels {#veles} buffers {#veles #dev wrath} \
        rosters [dict create #veles {@wrath +nyx bob}] \
        topics  [dict create #veles "the topic"]]]]
::virc::refresh_state

ok "buffers with history are listed, not just live channels" \
    {[llength $::virc::buffers] == 3}
ok "…including a channel we have left" \
    {[::virc::key irc.example bot #dev] in $::virc::buffers}
ok "rosters arrive"  {$::virc::rosters([::virc::key irc.example bot #veles]) eq {@wrath +nyx bob}}
ok "topics arrive"   {$::virc::topics([::virc::key irc.example bot #veles]) eq "the topic"}

# A first run starts at "now" rather than replaying the entire store.
ok "a fresh client starts live" {$::virc::cursor == 99}

reset_irc
set ::virc::cursor 5
::virc::refresh_state
ok "…but an existing cursor is not clobbered" {$::virc::cursor == 5}

# An unreachable gateway is a state, not a crash.
reset_irc
set ::vapi::state_reply {}
ok "an empty state reply is survivable" {![catch { ::virc::refresh_state }]}

# ── quoted insert ───────────────────────────────────────────────────
#
# IRC formatting is control BYTES, and there is no keyboard that types
# one without a text widget swallowing it as a shortcut first. C-v arms;
# the next keystroke goes in literally.
puts "\nquoted insert"

::virc::build_view .ircview
pack .ircview
set inp .ircview.input
focus -force $inp
update
# The composer sits under a tall transcript, so it must be a tooltip
# keep-out zone or hovering near the seam covers what you are typing
# (USER 2026-09-06, reported against the chat pane — same geometry here).
ok "the IRC composer registers as a keep-out" \
    {".ircview.input" in $::va11y::_zk}

proc composer {} { return [string trimright [.ircview.input get 1.0 end] "\n"] }
proc clear_composer {} {
    .ircview.input delete 1.0 end
    set ::virc::quoting 0
}

clear_composer
set ::vstatus::last ""
event generate $inp <Key-a> -when now
update
ok "ordinary typing is untouched" {[composer] eq "a"}
ok "…and does not arm anything"  {$::virc::quoting == 0}
# Unarmed, the handler must not act at all. Checking only the inserted
# text cannot show this — the handler returns `break`, so if it DID run
# it would insert the same character the class binding would have, and
# the composer looks identical either way.
ok "…and the handler stayed out of it" \
    {![string match "inserted*" $::vstatus::last]}

# The consequence that is impossible to miss: %A for BackSpace is
# \x08, so a handler that acts while unarmed inserts a control byte
# instead of deleting a character.
clear_composer
.ircview.input insert 1.0 "abc"
event generate $inp <Key-BackSpace> -when now
update
ok "BackSpace still deletes"        {[composer] eq "ab"}
ok "…rather than inserting \x08"   {[string first "\x08" [composer]] < 0}

clear_composer
event generate $inp <Control-Key-v> -when now
update
ok "C-v arms" {$::virc::quoting == 1}
ok "…and inserts nothing itself" {[composer] eq ""}
ok "…and says so" {[string match "*literally*" $::vstatus::last]}

# The whole point: a real \x03, the colour code.
event generate $inp <Control-Key-c> -when now
update
ok "C-v C-c inserts a literal ^C" {[scan [composer] %c] == 3}
ok "…and disarms afterwards"      {$::virc::quoting == 0}
ok "…naming what it did"          {[string match "*colour*" $::vstatus::last]}

# The other codes an IRC user actually types.
foreach {seq code name} {
    <Control-Key-b> 2  bold
    <Control-Key-o> 15 reset
} {
    clear_composer
    event generate $inp <Control-Key-v> -when now
    event generate $inp $seq -when now
    update
    ok "C-v [string range $seq 9 end-1] gives $name" {[scan [composer] %c] == $code}
}

# A printable character quoted is just that character.
clear_composer
event generate $inp <Control-Key-v> -when now
event generate $inp <Key-x> -when now
update
ok "quoting a printable key inserts it plainly" {[composer] eq "x"}

# Arming must survive the bare modifier press that precedes a chord —
# Ctrl arrives as its own keypress with no character, and disarming
# there would make C-v Ctrl-C impossible.
clear_composer
event generate $inp <Control-Key-v> -when now
event generate $inp <Key-Control_L> -when now
update
ok "a bare modifier does not disarm" {$::virc::quoting == 1}
event generate $inp <Control-Key-c> -when now
update
ok "…so the chord still lands" {[scan [composer] %c] == 3}

# Invisible bytes are made visible: a composer holding formatting must
# not look empty.
clear_composer
.ircview.input insert 1.0 "a\x02b\x03c"
::virc::mark_controls $inp
update
set marked [.ircview.input tag ranges irc_ctrl]
ok "control bytes are tagged"     {[llength $::marked] == 4}
ok "…and printable ones are not"  {[.ircview.input tag nextrange irc_ctrl 1.0 "1.0 + 1 chars"] eq ""}
clear_composer
::virc::mark_controls $inp
ok "clearing removes the tags"    {[.ircview.input tag ranges irc_ctrl] eq {}}

ok "an empty composer is fine"    {![catch { ::virc::mark_controls $inp }]}
ok "a missing widget is fine"     {![catch { ::virc::mark_controls .nope }]}

# Paste is not lost, only moved: Tk generates <<Paste>> from C-y and
# Shift-Insert as well as C-v.
ok "paste survives on C-y" \
    {[lsearch -exact [event info <<Paste>>] <Control-Key-y>] >= 0}
ok "…and on Shift-Insert" \
    {[lsearch -exact [event info <<Paste>>] <Shift-Key-Insert>] >= 0}

# The registry documents both, because this composer behaves
# differently from the one above it.
source [file join $_dir bindings.tcl]
set irc_rows {}
foreach row [::vbindings::table normie] {
    lassign $row keys target action help category
    if {$target eq "irc"} { lappend irc_rows $keys }
}
ok "the registry documents C-v here" {"C-v" in $::irc_rows}
ok "…and that paste moved to C-y"    {"C-y" in $::irc_rows}

# ── sending ─────────────────────────────────────────────────────────
puts "\nsending"

namespace eval ::vapi {
    variable posted {}
    variable post_error 0
    variable post_reply {}
    proc post_json {path body} {
        variable posted ; lappend posted [list $path $body]
        variable post_error
        if {$post_error} { error "gateway unreachable" }
        variable post_reply ; return $post_reply
    }
    # The real marker contract (api.tcl), faithfully enough for the
    # send tests: irc.tcl now types free text through json_string, and
    # the assertions unwrap the marker.
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
}

# What the gateway was actually posted, by key — SENTINEL, not a throw.
#
# `dict get` on an absent key aborts the interpreter, and a suite that
# aborts prints no tally at all, which precommit.sh scores as a BROKEN
# run rather than a red one. That matters most exactly here: breaking
# the slash dispatch makes a command post a body of a different shape,
# so the key vanishes and the suite dies instead of reporting a failure.
# Verified by mutation — removing the `//` escape used to abort.
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
proc sent {n key} {
    set body [lindex [lindex $::vapi::posted $n] 1]
    if {$body eq "" || ![dict exists $body $key]} { return "<no $key>" }
    return [json_unwrap [dict get $body $key]]
}
proc sent_path {n} { return [lindex [lindex $::vapi::posted $n] 0] }
proc sent_count {} { return [llength $::vapi::posted] }

# The composer's text rides the body as a TYPED JSON string (the
# ::vapi::json_string marker), because multi-word free text through the
# shape inference serialized as an array or an object and the gateway
# 422'd (RM3 #10). These helpers unwrap the marker for assertions —
# and say so loudly when the value is NOT typed, because that is the
# regression this whole block exists to catch.
proc posted_body {} {
    # The last /api/irc/send post — the composer also posts typing
    # indicators, which carry no text.
    for {set i [expr {[llength $::vapi::posted] - 1}]} {$i >= 0} {incr i -1} {
        if {[lindex [lindex $::vapi::posted $i] 0] eq "/api/irc/send"} {
            return [lindex [lindex $::vapi::posted $i] 1]
        }
    }
    return {}
}
proc sent_text_is_typed {} {
    set v [dict get [posted_body] text]
    expr {[llength $v] == 2 && [lindex $v 0] eq "::vapi::__json__"}
}
proc sent_text {} {
    return [json_unwrap [dict get [posted_body] text]]
}

proc reset_send {} {
    set ::vapi::posted {}
    set ::vapi::post_error 0
    set ::vapi::post_reply {}
    clear_composer
    set ::virc::current [::virc::key n bot #a]
    set ::virc::meta([::virc::key n bot #a]) [dict create network n presence bot buffer #a]
    set ::virc::sendable {bot admin}
}

reset_send
.ircview.input insert 1.0 "hello there"
ok "Enter sends"          {[::virc::send_current]}
# Counting SEND posts, not every post: the composer also posts typing
# indicators (Tier 3), which are not sends.
proc count_posts {path} {
    set n 0
    foreach p $::vapi::posted {
        if {[lindex $p 0] eq $path} { incr n }
    }
    return $n
}
ok "…exactly once"        {[count_posts /api/irc/send] == 1}
ok "…to the send route"   {[lindex [lindex $::vapi::posted 0] 0] eq "/api/irc/send"}
ok "…and clears the composer" {[composer] eq ""}

set body [lindex [lindex $::vapi::posted 0] 1]
ok "the target is the buffer" {[dict get $::body target] eq "#a"}
ok "the presence is carried" {[dict get $::body presence] eq "bot"}

# The whole point of the C-v work: what you typed is what goes out.
reset_send
.ircview.input insert 1.0 "a \x02bold\x02 word"
::virc::send_current
ok "control bytes are sent verbatim" \
    {[sent_text_is_typed] && [sent_text] eq "a \x02bold\x02 word"}

# And markdown is NOT converted on the way out. Rendering mIRC codes as
# markup coming in is right; doing the reverse to a human's typing would
# mangle any message that merely contains an asterisk.
reset_send
.ircview.input insert 1.0 "see *this* and _that_"
::virc::send_current
set body [lindex [lindex $::vapi::posted 0] 1]
ok "markdown is left alone on the way out" \
    {[sent_text] eq "see *this* and _that_"}

# /me is not a command so much as a way of speaking.
reset_send
.ircview.input insert 1.0 "/me waves"
::virc::send_current
set body [lindex [lindex $::vapi::posted 0] 1]
ok "/me becomes an action" {[dict get $::body kind] eq "action"}
ok "…with the slash stripped" {[sent_text] eq "waves"}

reset_send
.ircview.input insert 1.0 "ordinary"
::virc::send_current
ok "ordinary text stays a privmsg" \
    {[dict get [lindex [lindex $::vapi::posted 0] 1] kind] eq "privmsg"}

# THE field report (RM3 #10): "hello there everyone" — odd word count,
# no identifier shapes — serialized as a JSON ARRAY and the gateway
# answered `HTTP 422: text: invalid type: sequence, expected a string`.
# Free text is TYPED at the call site now; the typed marker is the
# contract this pins.
reset_send
.ircview.input insert 1.0 "hello there everyone"
::virc::send_current
ok "a multi-word message serializes as ONE JSON string" \
    {[sent_text_is_typed] && [sent_text] eq "hello there everyone"}

# An empty composer must not post.
reset_send
ok "an empty composer sends nothing" {[::virc::send_current] == 0}
ok "…and posts nothing"              {[llength $::vapi::posted] == 0}

# No buffer selected: say so rather than posting into the void.
reset_send
set ::virc::current ""
set ::vstatus::last ""
.ircview.input insert 1.0 "into the void"
ok "no buffer means no send" {[::virc::send_current] == 0}
ok "…and it says so"        {[string match "*no IRC buffer*" $::vstatus::last]}

# A failure must NEVER be silent — a message that goes nowhere without
# saying so looks sent.
reset_send
set ::vapi::post_error 1
set ::vstatus::last ""
.ircview.input insert 1.0 "will fail"
ok "a failed send reports false" {[::virc::send_current] == 0}
ok "…and says why"               {[string match "*not sent*" $::vstatus::last]}
ok "…and keeps the text so it is not lost" {[composer] eq "will fail"}

# The gateway can also refuse in a 200 body.
reset_send
set ::vapi::post_reply [dict create error "that presence is not connected"]
set ::vstatus::last ""
.ircview.input insert 1.0 "refused"
ok "a refusal in the body is a failure too" {[::virc::send_current] == 0}
ok "…and is reported"  {[string match "*not connected*" $::vstatus::last]}

# Which presences can be sent as.
reset_send
ok "a sendable presence can send" {[::virc::can_send]}
set ::virc::sendable {admin}
ok "…and one that is not, cannot" {![::virc::can_send]}
set ::virc::sendable {}
ok "nothing sendable means no" {![::virc::can_send]}

# ── driving a connection ────────────────────────────────────────────
puts "\nops"

# tk_messageBox is modal, so it is replaced with a recording stub. The
# question TEXT is what gets asserted: a confirm that does not say who
# it will act as does not prevent acting as the wrong one.
set ::confirm_answer yes
set ::confirm_asked {}
rename tk_messageBox _real_tk_messageBox
proc tk_messageBox {args} {
    set msg ""
    foreach {k v} $args { if {$k eq "-message"} { set msg $v } }
    lappend ::confirm_asked $msg
    return $::confirm_answer
}

proc reset_ops {} {
    set ::vapi::posted {}
    set ::vapi::post_error 0
    set ::vapi::post_reply {}
    set ::confirm_asked {}
    set ::confirm_answer yes
    set ::vstatus::last ""
    set ::virc::current [::virc::key irc.example bot #veles]
    set ::virc::meta([::virc::key irc.example bot #veles]) \
        [dict create network irc.example presence bot buffer #veles]
    set ::virc::rosters([::virc::key irc.example bot #veles]) {@wrath +nyx bob}
    ::virc::_refresh_roster [::virc::key irc.example bot #veles]
    update
}

proc select_member {n} {
    .ircroster.list selection clear 0 end
    .ircroster.list selection set $n
}

::virc::build_roster .ircroster
pack .ircroster
reset_ops

# A non-destructive op goes straight through.
::virc::whois_selected
ok "whois needs no selection to be safe" {[llength $::confirm_asked] == 0}
select_member 0
reset_ops
select_member 0
::virc::whois_selected
ok "whois posts an op"        {[llength $::vapi::posted] == 1}
ok "…to the op route"         {[lindex [lindex $::vapi::posted 0] 0] eq "/api/irc/op"}
set body [lindex [lindex $::vapi::posted 0] 1]
ok "…naming the action"       {[dict get $::body action] eq "whois"}
ok "…and the nick, unprefixed" {[dict get $::body nick] eq "wrath"}
ok "…without confirming"      {[llength $::confirm_asked] == 0}

# Destructive ops confirm FIRST.
reset_ops
select_member 0
::virc::kick_selected
ok "a kick confirms"          {[llength $::confirm_asked] == 1}
ok "…naming who is kicked"    {[string match "*wrath*" [lindex $::confirm_asked 0]]}
ok "…and from where"          {[string match "*#veles*" [lindex $::confirm_asked 0]]}
# The failure mode of this whole pane.
ok "…and WHICH PRESENCE it acts as" \
    {[string match "*Acting as: bot*" [lindex $::confirm_asked 0]]}
ok "…then posts it"           {[llength $::vapi::posted] == 1}
ok "…as a kick"               {[dict get [lindex [lindex $::vapi::posted 0] 1] action] eq "kick"}

# Saying no must do nothing at all.
reset_ops
select_member 0
set ::confirm_answer no
::virc::kick_selected
ok "declining confirms"       {[llength $::confirm_asked] == 1}
ok "…and posts NOTHING"       {[llength $::vapi::posted] == 0}

reset_ops
select_member 0
set ::confirm_answer no
::virc::mode_selected +o
ok "a mode change also confirms" {[llength $::confirm_asked] == 1}
ok "…and declining posts nothing" {[llength $::vapi::posted] == 0}

reset_ops
select_member 1
::virc::mode_selected +o
set body [lindex [lindex $::vapi::posted 0] 1]
ok "op sets +o on the channel" \
    {[dict get $::body action] eq "mode" && [dict get $::body target] eq "#veles"}
ok "…for the selected nick"   {[dict get $::body modes] eq "+o nyx"}

# Leaving is destructive too, and only makes sense for a channel.
reset_ops
::virc::part_current
ok "leaving a channel confirms" {[llength $::confirm_asked] == 1}
reset_ops
set ::virc::current [::virc::key irc.example bot wrath]
set ::virc::meta([::virc::key irc.example bot wrath]) \
    [dict create network irc.example presence bot buffer wrath]
::virc::part_current
ok "you cannot leave a query"   {[llength $::vapi::posted] == 0}
ok "…and it says why"           {[string match "*not a channel*" $::vstatus::last]}

# The op acts as whoever owns the buffer you are looking at.
reset_ops
set ::virc::current [::virc::key irc.example admin #veles]
set ::virc::meta([::virc::key irc.example admin #veles]) \
    [dict create network irc.example presence admin buffer #veles]
::virc::op_here action join channel "#other"
ok "an op carries the buffer's presence" \
    {[dict get [lindex [lindex $::vapi::posted 0] 1] presence] eq "admin"}

# Failures are never silent.
reset_ops
set ::vapi::post_error 1
select_member 0
ok "a failed op reports false" {[::virc::whois_selected] == 0}
ok "…and says so"              {[string match "*failed*" $::vstatus::last]}

reset_ops
set ::vapi::post_reply [dict create error "you're not channel operator"]
select_member 0
ok "a server refusal is a failure" {[::virc::whois_selected] == 0}
ok "…and is shown verbatim"        {[string match "*not channel operator*" $::vstatus::last]}

# Success reports split by kind: ACTIONS announce in the status bar
# ("did my kick work"), and INTROSPECTION answers stay OUT of it — the
# engine publishes their Info event into the buffer the op asked from,
# which is durable scrollback instead of a transient whisper.
reset_ops
set ::vapi::post_reply [dict create result "wrath is wrath@example"]
select_member 0
set ::vstatus::last ""
ok "a successful op reports true" {[::virc::whois_selected] == 1}
ok "…scoped to the buffer the ask came from" \
    {[dict get [lindex [lindex $::vapi::posted 0] 1] scope] eq "#veles"}
ok "…but whois stays out of the status bar" \
    {[string match "*wrath@example*" $::vstatus::last] == 0}

reset_ops
set ::vapi::post_reply [dict create result "joining #new"]
::virc::op_here action join channel "#new"
ok "a wire action still announces success in the status bar" \
    {[string match "*joining #new*" $::vstatus::last] == 1}

rename tk_messageBox {}
rename _real_tk_messageBox tk_messageBox

# ── which presence am I? ────────────────────────────────────────────
puts "\nwhich presence am I"

reset_irc
::virc::_ingest [dict create id 1 network n presence bot buffer #v kind privmsg nick w text a]
# Mark read first: an unread count adds its own column, and this
# assertion is about the NAME, not the padding.
::virc::mark_read [::virc::key n bot #v]
ok "one presence needs no label" \
    {[::virc::_label [::virc::key n bot #v]] eq "#v"}

::virc::_ingest [dict create id 2 network n presence admin buffer #v kind privmsg nick w text b]
::virc::mark_read [::virc::key n admin #v]
ok "two presences, same channel name" {[llength $::virc::buffers] == 2}
ok "…so the label names the bot's"   {[string match "*#v (bot)*"   [::virc::_label [::virc::key n bot #v]]]}
ok "…and the admin's"                {[string match "*#v (admin)*" [::virc::_label [::virc::key n admin #v]]]}
ok "presences are listed"            {[::virc::presences] eq {admin bot}}

# The transcript header says who you are before you type, not somewhere
# you have to go and check.
::virc::_refresh_topic [::virc::key n admin #v]
ok "the header names the presence" \
    {[string match "*as admin*" [.ircview.topic cget -text]]}
::virc::_refresh_topic [::virc::key n bot #v]
ok "…and changes with the buffer" \
    {[string match "*as bot*" [.ircview.topic cget -text]]}

# ── an empty pane says why it is empty ──────────────────────────────
#
# USER (spot test, 2026-08-25): "the IRC tab is 3 empty panes".
#
# On that machine two separate things were true at once and the pane
# distinguished neither: the running gateway was 19 hours old so
# /api/irc/state 404'd, AND the config had no [platforms.irc] (its IRC
# ran through an ircx plugin that is no longer installed). "gateway
# unreachable" was the only thing this code could say, and it was false.
puts "\nthe empty pane explains itself"

proc why_for {err {reply {}}} {
    reset_irc
    set ::vapi::get_error $err
    set ::vapi::state_reply $reply
    ::virc::refresh_state
    set ::vapi::get_error ""
    return $::virc::why
}

ok "a 404 means the gateway predates the pane" \
    {[why_for {HTTP 404: Not Found}] eq "gateway-too-old"}
ok "a 401 means the token, not the routes" \
    {[why_for {HTTP 401: {"error":"unauthorized"}}] eq "unauthorized"}
ok "a 403 too" {[why_for {HTTP 403: nope}] eq "unauthorized"}
ok "anything else is an unreachable gateway" \
    {[why_for {connection refused}] eq "unreachable"}
ok "a reachable gateway with no networks is not configured" \
    {[why_for "" [dict create networks {} latest_id 0]] eq "not-configured"}
ok "…and so is a reply with no networks key at all" \
    {[why_for "" [dict create latest_id 0]] eq "not-configured"}

# A working connection must NOT be reported as a problem.
ok "a live network leaves no complaint" \
    {[why_for "" [dict create latest_id 9 networks [list \
        [dict create network n presence bot nick x connected 1 \
            channels {#c} buffers {#c} rosters {} topics {}]]]] eq ""}

# Every reason has words for the user, and they differ. A reason that
# falls through to the empty string renders a blank pane again.
reset_irc
::virc::build_view .ircview
foreach r {gateway-too-old unauthorized unreachable not-configured} {
    set ::virc::why $r
    ok "$r has a short form" {[::virc::_why_short] ne ""}
    ok "$r has a long form"  {[string length [::virc::_why_long]] > 40}
}

# The words have to say what to DO, not just what is wrong.
set ::virc::why "not-configured"
ok "not-configured names the config section" \
    {[string match "*platforms.irc*" [::virc::_why_long]]}
ok "…and warns that ircx is a different thing" \
    {[string match "*ircx*" [::virc::_why_long]]}
# The snippet IS a config surface: a key `apply_mapping` does not read is a
# knob that does nothing, and this pane printed `tls = true` where the
# adapter reads `use_tls` — the same mistake the setup wizard made
# (60786dd), in a second place nobody compared against the reader.
set _long [::virc::_why_long]
ok "…and its TLS key is the one the adapter reads" \
    {[string first "use_tls" $_long] >= 0 && ![regexp {(?m)^[ \t]*tls[ \t]*=} $_long]}
# It also tells the operator WHICH file to edit, so the path has to come
# from the same place the engine gets it: a hardcoded ~/.veles/config.toml
# sends them to a file nothing reads.
ok "…and it names the config file the gateway actually reads" \
    {[string first [::vpaths::config_file] $_long] >= 0}
set ::virc::why "gateway-too-old"
ok "gateway-too-old says to rebuild" \
    {[string match "*cargo build*" [::virc::_why_long]]}
set ::virc::why "unauthorized"
ok "unauthorized names the variable to fix" \
    {[string match "*VELES_TOKEN*" [::virc::_why_long]]}

# And it lands in the transcript, which is the big empty region someone
# is actually looking at.
set ::virc::why "not-configured"
::virc::build_view .ircview
ok "the explanation is written into the transcript" {[::virc::_explain_empty]}
set _body [.ircview.body get 1.0 end]
ok "…naming the reason"        {[string match "*no IRC connection*" $_body]}
ok "…and the fix"              {[string match "*platforms.irc*" $_body]}
ok "…and the topic bar says so too" \
    {[string match "*nothing to show*" [.ircview.topic cget -text]]}

# With a real reason absent there is nothing to explain, and the pane
# must not invent one.
set ::virc::why ""
::virc::build_view .ircview
ok "no reason writes nothing" {![::virc::_explain_empty]}

# ── slash commands ─────────────────────────────────────────────────
puts "\nslash commands"

reset_send
.ircview.input insert 1.0 "/join #newchan secret"
::virc::send_current
ok "/join posts an op, not speech" {[llength $::vapi::posted] == 1}
ok "…to the op route"  {[lindex [lindex $::vapi::posted 0] 0] eq "/api/irc/op"}
ok "…as a join"        {[sent 0 action] eq "join"}
ok "…with the channel" {[sent 0 channel] eq "#newchan"}
ok "…and its key"      {[sent 0 key] eq "secret"}

reset_send
.ircview.input insert 1.0 "/msg wrath hello there"
::virc::send_current
ok "/msg is a privmsg to the nick" {[sent 0 target] eq "wrath"}
ok "…carrying the text"            {[sent 0 text] eq "hello there"}
ok "…as a privmsg"                 {[sent 0 kind] eq "privmsg"}

reset_send
.ircview.input insert 1.0 "/notice wrath fyi"
::virc::send_current
ok "/notice is a notice" {[dict get [lindex [lindex $::vapi::posted 0] 1] kind] eq "notice"}

reset_send
.ircview.input insert 1.0 "/away lunch break"
::virc::send_current
ok "/away is an op"     {[sent 0 action] eq "away"}
ok "…with the message"  {[sent 0 message] eq "lunch break"}

reset_send
.ircview.input insert 1.0 "/nick newnick"
::virc::send_current
ok "/nick is an op"   {[sent 0 action] eq "nick"}
ok "…to the new nick" {[sent 0 nick] eq "newnick"}

reset_send
.ircview.input insert 1.0 "//leads with a slash"
::virc::send_current
ok "// sends a literal leading slash" {[sent 0 text] eq "/leads with a slash"}
ok "…as a privmsg"                   {[sent 0 kind] eq "privmsg"}

reset_send
set ::vstatus::last ""
.ircview.input insert 1.0 "/definitelynotacommand"
::virc::send_current
ok "an unknown command refuses" {[llength $::vapi::posted] == 0}
ok "…naming the command"        {[string match "*definitelynotacommand*" $::vstatus::last]}
ok "…and pointing at /help"     {[string match "*help*" $::vstatus::last]}

reset_send
set ::vstatus::last ""
.ircview.input insert 1.0 "/join"
::virc::send_current
ok "a missing /join argument refuses" {[llength $::vapi::posted] == 0}
ok "…and says what it needs"          {[string match "*channel*" $::vstatus::last]}

# ── highlights ──────────────────────────────────────────────────────
puts "\nhighlights"

reset_irc
set ::virc::own_nick(n/bot) jackma
set ::virc::highlight_words {urgent}
ok "own nick highlights" {[::virc::_is_highlight [dict create network n presence bot text "jackma: look"]]}
ok "…but not a longer nick" {![::virc::_is_highlight [dict create network n presence bot text "jackman is someone else"]]}
ok "an extra word highlights" {[::virc::_is_highlight [dict create network n presence bot text "this is urgent"]]}
ok "…case-insensitively"      {[::virc::_is_highlight [dict create network n presence bot text "URGENT now"]]}
ok "punctuation still highlights" {[::virc::_is_highlight [dict create network n presence bot text "(urgent) now"]]}
ok "no match, no highlight"       {![::virc::_is_highlight [dict create network n presence bot text "nothing here"]]}

# The highlight actually tags the rendered line.
reset_irc
set ::virc::own_nick(n/bot) jackma
::virc::build_view .ircview
.ircview.body delete 1.0 end
set ::virc::_hdline [dict create network n presence bot buffer #a kind privmsg nick wrath text "jackma here"]
::virc::render_line .ircview.body $::virc::_hdline
ok "a highlight tags the line" {[llength [.ircview.body tag ranges irc_highlight]] > 0}
# A fresh widget, so the previous line's tags cannot leak into the plain
# one (render_line leaves the widget disabled, which drops a `delete`).
::virc::build_view .ircview
::virc::render_line .ircview.body [dict create network n presence bot buffer #a kind privmsg nick wrath text "nothing"]
ok "a plain line is not tagged" {[.ircview.body tag ranges irc_highlight] eq {}}

# ── activity levels ─────────────────────────────────────────────────
puts "\nactivity levels"

reset_irc
ok "a message is level 2" {[::virc::_activity_level x [dict create kind privmsg text hi]] == 2}
ok "a join is level 1"    {[::virc::_activity_level x [dict create kind join]] == 1}
ok "our own echo is level 0" {[::virc::_activity_level x [dict create kind privmsg text hi own 1]] == 0}
set ::virc::own_nick(n/bot) jackma
ok "a highlight is level 3" \
    {[::virc::_activity_level x [dict create kind privmsg network n presence bot text "jackma!"]] == 3}

# ── hotlist ordering ───────────────────────────────────────────────
puts "\nhotlist ordering"

reset_irc
set ::virc::activity(hl) 3
set ::virc::activity(msg) 2
set ::virc::activity(noise) 1
set ::virc::activity(quiet) 0
ok "a highlight beats a message beats noise beats silence" \
    {[lsort -command ::virc::_buffer_sort_compare {quiet noise msg hl}] eq {hl msg noise quiet}}

set ::virc::activity(a) 2
set ::virc::activity(b) 2
set ::virc::lasttouch(a) 100
set ::virc::lasttouch(b) 200
ok "tied levels fall through to recency" \
    {[lsort -command ::virc::_buffer_sort_compare {a b}] eq {b a}}

# ── Tab completion ─────────────────────────────────────────────────
puts "\ntab completion"

reset_irc
set ::virc::current [::virc::key n bot #a]
set ::virc::meta([::virc::key n bot #a]) [dict create network n presence bot buffer #a]
set ::virc::recency([::virc::key n bot #a]) {wrath nyx bob}
set ::virc::rosters([::virc::key n bot #a]) {@wrath +nyx bob}

ok "nicks complete most-recent first" \
    {[::virc::_completion_candidates w nick] eq {wrath}}
ok "…all of them for an empty prefix" \
    {[::virc::_completion_candidates {} nick] eq {wrath nyx bob}}
ok "commands complete after a slash" \
    {[::virc::_completion_candidates n command] eq {nick notice}}

set ::virc::buffers [list \
    [::virc::key n bot #a] [::virc::key n bot #dev] [::virc::key n admin #a]]
set ::virc::meta([::virc::key n bot #dev]) [dict create network n presence bot buffer #dev]
set ::virc::meta([::virc::key n admin #a]) [dict create network n presence admin buffer #a]
ok "channels complete from the current network only" \
    {[lindex [::virc::_completion_candidates "#d" channel] 0] eq "#dev"}

# ── the proc Tab is actually bound to ──────────────────────────────
#
# Everything above tests _completion_candidates — the candidate LIST.
# Nothing tested ::virc::complete, which is the proc bound to <Tab> and
# which owns every behaviour that makes completion feel right: where the
# suffix goes, cycling, and the replacement in the composer. Verified by
# mutation: `proc ::virc::complete {} { return 0 }` left the whole suite
# green.
puts "\ncompletion in the composer"

proc composer {} { return [string trim [.ircview.input get 1.0 end-1c]] }
proc type_and_tab {text} {
    clear_composer
    .ircview.input insert 1.0 $text
    .ircview.input mark set insert end-1c
    catch { ::virc::complete }
    return [composer]
}

reset_send
array unset ::virc::complete
set ::virc::recency([::virc::key n bot #a]) {wrath nyx bob}
set ::virc::rosters([::virc::key n bot #a]) {@wrath +nyx bob}

# At the START of a line a nick gets "name: " — weechat's behaviour, and
# the thing that makes addressing someone one keystroke.
ok "a nick at column 0 completes with a colon" {[type_and_tab "wr"] eq "wrath:"}
ok "…mid-line it completes with a space instead" \
    {[type_and_tab "hello wr"] eq "hello wrath"}

# Repeated Tab CYCLES. Without this, one of two matching nicks is
# unreachable from the keyboard.
clear_composer
.ircview.input insert 1.0 "b"
.ircview.input mark set insert end-1c
set ::virc::recency([::virc::key n bot #a]) {bob bill}
set ::virc::rosters([::virc::key n bot #a]) {bob bill}
array unset ::virc::complete
catch { ::virc::complete }
set _first [composer]
catch { ::virc::complete }
set _second [composer]
catch { ::virc::complete }
set _third [composer]
ok "the first Tab takes the most recent" {$_first eq "bob:"}
ok "a second Tab cycles to the next"     {$_second eq "bill:"}
ok "…and a third wraps back"             {$_third eq "bob:"}

# A different prefix restarts at the first match rather than continuing
# somebody else's cycle.
array unset ::virc::complete
ok "a new prefix starts over" {[type_and_tab "bi"] eq "bill:"}

# `/join ` completes CHANNELS, not nicks — the context, not just the
# prefix, decides what is offered.
set ::virc::buffers [list [::virc::key n bot #a] [::virc::key n bot #dev]]
set ::virc::meta([::virc::key n bot #dev]) [dict create network n presence bot buffer #dev]
array unset ::virc::complete
ok "/join completes a channel" {[type_and_tab "/join #d"] eq "/join #dev"}

# A leading slash completes COMMANDS, and keeps the slash.
array unset ::virc::complete
ok "a slash completes a command" {[type_and_tab "/wh"] eq "/whois"}
ok "…keeping the leading slash"  {[string match "/*" [type_and_tab "/to"]]}

# No candidate must leave the composer exactly as it was — not eat the
# word, not insert an empty completion.
array unset ::virc::complete
ok "an unmatched prefix changes nothing" {[type_and_tab "zzz"] eq "zzz"}
ok "…and an unmatched command too"       {[type_and_tab "/zzz"] eq "/zzz"}

# ── read marker ────────────────────────────────────────────────────
puts "\nread marker"

reset_irc
set ::virc::current [::virc::key n bot #a]
set ::virc::meta([::virc::key n bot #a]) [dict create network n presence bot buffer #a]
set ::virc::read_marker([::virc::key n bot #a]) 2
set ::virc::linebuf([::virc::key n bot #a]) [list \
    [dict create id 1 network n presence bot buffer #a kind privmsg nick w text one ts "2026-01-01T12:00:00"] \
    [dict create id 2 network n presence bot buffer #a kind privmsg nick x text two ts "2026-01-01T12:01:00"] \
    [dict create id 3 network n presence bot buffer #a kind privmsg nick y text three ts "2026-01-01T12:02:00"]]
::virc::build_view .ircview
::virc::_render_transcript
ok "the read marker draws a rule" \
    {[string match "*read up to here*" [.ircview.body get 1.0 end]]}

# ── scrollback paging ──────────────────────────────────────────────
puts "\nscrollback paging"

reset_irc
set ::virc::current [::virc::key n bot #a]
set ::virc::meta([::virc::key n bot #a]) [dict create network n presence bot buffer #a]
set ::virc::linebuf([::virc::key n bot #a]) [list \
    [dict create id 3 network n presence bot buffer #a kind privmsg nick w text three ts "2026-01-01T12:02:00"]]
set ::virc::earliest([::virc::key n bot #a]) 3
set ::vapi::backlog_reply [dict create lines [list \
    [dict create id 1 network n presence bot buffer #a kind privmsg nick x text one ts "2026-01-01T12:00:00"] \
    [dict create id 2 network n presence bot buffer #a kind privmsg nick y text two ts "2026-01-01T12:01:00"]]]
::virc::build_view .ircview
::virc::_load_older [::virc::key n bot #a]
ok "paging fetches before the cursor" \
    {[string match "*before=3*" [lindex $::vapi::requests end]]}
ok "…and prepends the older lines" \
    {[llength $::virc::linebuf([::virc::key n bot #a])] == 3}
ok "…advancing the earliest id" \
    {$::virc::earliest([::virc::key n bot #a]) == 1}

# ── smart filter ───────────────────────────────────────────────────
puts "\nsmart filter"

reset_irc
set ::virc::smart_filter 1
set ::virc::recency([::virc::key n bot #a]) {wrath}
ok "a join from a stranger is filtered" \
    {[::virc::_is_filtered [dict create kind join network n presence bot buffer #a nick newguy]]}
ok "…but not from someone who spoke" \
    {![::virc::_is_filtered [dict create kind join network n presence bot buffer #a nick wrath]]}
ok "…and a message is never filtered" \
    {![::virc::_is_filtered [dict create kind privmsg network n presence bot buffer #a nick newguy]]}
set ::virc::smart_filter 0
ok "turning the filter off shows everything" \
    {![::virc::_is_filtered [dict create kind join network n presence bot buffer #a nick newguy]]}
set ::virc::smart_filter 1

# ── paste guard ────────────────────────────────────────────────────
puts "\npaste guard"

reset_send
rename tk_messageBox _real_tk_messageBox
proc tk_messageBox {args} { return $::confirm_answer }
set ::confirm_answer no
.ircview.input insert 1.0 "line one\nline two\nline three"
ok "a multi-line send is not posted on its own" {[::virc::send_current] == 0}
ok "…and posts nothing" {[llength $::vapi::posted] == 0}
ok "…and keeps the text" {[composer] ne ""}
set ::confirm_answer yes
.ircview.input delete 1.0 end
.ircview.input insert 1.0 "line one\nline two\nline three"
ok "confirming sends it" {[::virc::send_current] == 1}
ok "…exactly once" {[count_posts /api/irc/send] == 1}
rename tk_messageBox {}
rename _real_tk_messageBox tk_messageBox

# ── URL grabbing ───────────────────────────────────────────────────
puts "\nurl grabbing"

reset_irc
::virc::build_view .ircview
.ircview.body configure -state normal
.ircview.body delete 1.0 end
.ircview.body insert end "see https://a.example/x and http://b.example/y and https://a.example/x again\n"
.ircview.body configure -state disabled
set ::virc::_urls [::virc::_urls_in_buffer]
ok "URLs are collected" {[llength $::virc::_urls] == 2}
ok "…and de-duplicated" {[lsearch -exact $::virc::_urls "https://a.example/x"] >= 0}
ok "…the second one too" {[lsearch -exact $::virc::_urls "http://b.example/y"] >= 0}

# ── notifications (guarded, so it never crashes without a daemon) ───
puts "\nnotifications"

reset_irc
ok "a missing notification daemon is not an error" \
    {![catch { ::virc::_maybe_notify [dict create kind privmsg buffer #a text hi nick w] 3 }]}

# ── /lastlog reuses the find-bar search core ────────────────────────
puts "\n/lastlog"

reset_irc
::virc::build_view .ircview
.ircview.body configure -state normal
.ircview.body delete 1.0 end
.ircview.body insert end "hello world\nand then more hello later\n"
.ircview.body configure -state disabled
ok "the shared search core finds every occurrence" \
    {[llength [::vfind::match_all .ircview.body "hello"]] == 2}

# ── away-marked nicks ───────────────────────────────────────────────
puts "\naway-marked nicks"

reset_irc
set ::virc::rosters([::virc::key irc.example bot #veles]) {@wrath +nyx bob}
set ::virc::aways([::virc::key irc.example bot #veles]) {bob}
::virc::build_roster .ircroster
::virc::_refresh_roster [::virc::key irc.example bot #veles]
ok "an away nick is marked in the roster" \
    {[string match "*bob (away)*" [.ircroster.list get end]]}
ok "…and a here nick is not" \
    {![string match "*(away)*" [.ircroster.list get 0]]}

# ── the invariants and the wiring, not just the helpers ────────────
#
# Everything in this section was found by MUTATION: each of these
# behaviours could be deleted outright and the suite stayed green,
# because what was covered was the pure helper and not the place it is
# used from.
puts "\ninvariants and wiring"

# NOTHING EVER DEMOTES. The hotlist is a level, not a tally: a highlight
# followed by ordinary channel noise must still read as a highlight, or
# the ranking that makes 40 buffers navigable is worthless.
reset_irc
set ::virc::current [::virc::key n bot #here]
set ::virc::meta([::virc::key n bot #here]) [dict create network n presence bot buffer #here]
set ::virc::own_nick(n/bot) "me"
set _other [::virc::key n bot #other]
::virc::_ingest [dict create network n presence bot buffer #other kind privmsg nick x text "me: look"]
ok "a highlight sets level 3" {[::virc::_activity_of $_other] == 3}
::virc::_ingest [dict create network n presence bot buffer #other kind join nick y text ""]
ok "…and channel noise after it does NOT demote" {[::virc::_activity_of $_other] == 3}
::virc::_ingest [dict create network n presence bot buffer #other kind privmsg nick z text "hi"]
ok "…nor does an ordinary message" {[::virc::_activity_of $_other] == 3}
# Only reading it clears the level.
::virc::mark_read $_other
ok "reading the buffer clears it" {[::virc::_activity_of $_other] == 0}

# THE SMART FILTER IS WIRED IN. `_is_filtered` was tested; whether
# render_line consults it was not, so the predicate could be perfect and
# the transcript still a wall of join/part noise.
reset_irc
::virc::build_view .ircview
set ::virc::smart_filter 1
set _quiet [dict create network n presence bot buffer #a kind join nick ghost text ""]
::virc::render_line [::virc::transcript] $_quiet
ok "a join from a nick who never spoke is not drawn" \
    {![string match "*ghost*" [.ircview.body get 1.0 end]]}
::virc::_bump_recency [::virc::key n bot #a] talker
::virc::render_line [::virc::transcript] \
    [dict create network n presence bot buffer #a kind join nick talker text ""]
ok "…but a join from someone who has spoken IS" \
    {[string match "*talker*" [.ircview.body get 1.0 end]]}
set ::virc::smart_filter 0
::virc::render_line [::virc::transcript] $_quiet
ok "…and with the filter off, everything is drawn" \
    {[string match "*ghost*" [.ircview.body get 1.0 end]]}
set ::virc::smart_filter 1

# THE READ MARKER SURVIVES THE APP CLOSING. Both halves were no-ops
# under mutation with the suite green: the marker is only useful if it
# is still there next time.
reset_irc
namespace eval ::vstate {
    variable blob {}
    proc put {k v} { variable blob ; dict set blob $k $v ; return 1 }
    proc get {k {d ""}} { variable blob
        if {[dict exists $blob $k]} { return [dict get $blob $k] } ; return $d }
}
set ::vstate::blob {}
set _k [::virc::key n bot #a]
set ::virc::current $_k
set ::virc::meta($_k) [dict create network n presence bot buffer #a]
set ::virc::seen($_k) 42
::virc::mark_read $_k
ok "marking read writes the marker out" \
    {[dict exists $::vstate::blob irc_read_marker]}
# Sentinel reads, not `dict get` / a bare array element: both THROW when
# the value is absent, and a suite that throws prints no tally, which
# precommit.sh scores as a broken run rather than a red one. Gutting
# _save_read_marker used to abort here instead of failing.
proc marker_in_blob {k} {
    set m [::vstate::get irc_read_marker {}]
    if {![dict exists $m $k]} { return "<unset>" }
    return [dict get $m $k]
}
proc marker_in_memory {k} {
    if {![info exists ::virc::read_marker($k)]} { return "<unset>" }
    return $::virc::read_marker($k)
}
ok "…with the line id it stopped at" {[marker_in_blob $_k] == 42}

# Restart: the array is gone, the blob is not.
array unset ::virc::read_marker
ok "a fresh start has no marker in memory" {[marker_in_memory $_k] eq "<unset>"}
::virc::_load_read_marker
ok "loading brings it back" {[marker_in_memory $_k] == 42}

# Junk in the blob must not become a marker — a non-integer id would be
# compared against line ids on every render.
set ::vstate::blob [dict create irc_read_marker [dict create $_k "not-a-number"]]
array unset ::virc::read_marker
::virc::_load_read_marker
ok "a non-integer id is dropped, not stored" {[marker_in_memory $_k] eq "<unset>"}

# NOTIFICATIONS FIRE FOR THE TWO CASES THAT MATTER, and no others.
namespace eval ::_notified { variable calls {} }
proc ::virc::_notify {title body} { lappend ::_notified::calls [list $title $body] }

set ::_notified::calls {}
::virc::_maybe_notify [dict create kind privmsg buffer "#chan" nick a text "hi"] 3
ok "a highlight in a channel notifies" {[llength $::_notified::calls] == 1}
ok "…naming who said it" {[lindex $::_notified::calls 0 0] eq "a"}

set ::_notified::calls {}
::virc::_maybe_notify [dict create kind privmsg buffer "someone" nick b text "hi"] 2
ok "a private message notifies even without a highlight" \
    {[llength $::_notified::calls] == 1}

set ::_notified::calls {}
::virc::_maybe_notify [dict create kind privmsg buffer "#chan" nick c text "hi"] 2
ok "an ordinary channel message does NOT" {[llength $::_notified::calls] == 0}

set ::_notified::calls {}
::virc::_maybe_notify [dict create kind join buffer "#chan" nick d text ""] 3
ok "…and neither does a join, whatever its level" \
    {[llength $::_notified::calls] == 0}


# ── the roster follows the event stream, not just the snapshot ───────
#
# PENDING's "rosters lag" entry. The pane re-read the snapshot only in
# `refresh_state`, and — checked, not assumed — there is no state event
# on the SSE stream to prompt it, so a roster was stale until the buffer
# was reopened. The pane now applies join/part/quit/nick locally, the
# way every IRC client has for thirty years.
puts "\nroster deltas from the event stream"

proc _rset {k members} { array set ::virc::rosters [list $k $members] }
proc _rget {k} {
    if {[info exists ::virc::rosters($k)]} { return $::virc::rosters($k) }
    return "<missing>"
}
proc _ev {kind network presence buffer nick args} {
    set d [dict create kind $kind network $network presence $presence \
        buffer $buffer nick $nick]
    foreach {k v} $args { dict set d $k $v }
    return $d
}

array unset ::virc::rosters
_rset "n/bot/#a" {@alice bob}
set touched [::virc::_roster_apply [_ev join n bot #a carol]]
ok "a join adds the nick" {[lsearch -exact [_rget "n/bot/#a"] carol] >= 0}
ok "…and reports the buffer that changed" {$touched eq "n/bot/#a"}

set touched [::virc::_roster_apply [_ev join n bot #a carol]]
ok "a duplicate join does not add twice" \
    {[llength [_rget "n/bot/#a"]] == 3}
ok "…and reports nothing changed" {$touched eq ""}

# With userhost-in-names the snapshot holds nick!user@host, so a rejoin
# must match on the NICK or the same person appears twice.
array unset ::virc::rosters
_rset "n/bot/#a" {alice!u@h @bob!u@h}
set touched [::virc::_roster_apply [_ev join n bot #a alice]]
ok "a rejoin matches on the nick, not the whole mask" \
    {[llength [_rget "n/bot/#a"]] == 2}

array unset ::virc::rosters
_rset "n/bot/#a" {@alice bob carol}
::virc::_roster_apply [_ev part n bot #a bob]
ok "a part removes the nick" {[_rget "n/bot/#a"] eq "@alice carol"}

array unset ::virc::rosters
_rset "n/bot/#a" {@alice bob carol}
::virc::_roster_apply [_ev kick n bot #a alice target bob]
ok "a kick removes the VICTIM, not the kicker" \
    {[_rget "n/bot/#a"] eq "@alice carol"}

array unset ::virc::rosters
_rset "n/bot/#a" {@alice bob}
::virc::_roster_apply [_ev kick n bot #a alice]
ok "a kick with no target changes nothing" \
    {[_rget "n/bot/#a"] eq "@alice bob"}

# A quit is not scoped to a channel: it has to reach every buffer on
# that connection, and no further.
array unset ::virc::rosters
_rset "n/bot/#a" {@alice bob}
_rset "n/bot/#b" {bob carol}
_rset "other/bot/#a" {bob dave}
set touched [::virc::_roster_apply [_ev quit n bot * bob]]
ok "a quit fans out across the connection's buffers" \
    {[_rget "n/bot/#a"] eq "@alice" && [_rget "n/bot/#b"] eq "carol"}
ok "…and stops at the connection boundary" \
    {[_rget "other/bot/#a"] eq "bob dave"}
ok "…reporting both buffers it touched" {[llength $touched] == 2}

# A nick change keeps the mode prefix — losing it would silently deop
# somebody in the display.
array unset ::virc::rosters
_rset "n/bot/#a" {@alice bob}
_rset "n/bot/#b" {alice}
::virc::_roster_apply [_ev nick n bot * alice text alyssa]
ok "a nick change renames in every buffer" \
    {[_rget "n/bot/#a"] eq "@alyssa bob" && [_rget "n/bot/#b"] eq "alyssa"}

array unset ::virc::rosters
_rset "n/bot/#a" {@alice}
::virc::_roster_apply [_ev nick n bot * alice]
ok "a nick change with no new nick is ignored" \
    {[_rget "n/bot/#a"] eq "@alice"}

# A join into a channel we hold no roster for must not invent one — the
# snapshot is the only thing that knows the full membership.
array unset ::virc::rosters
::virc::_roster_apply [_ev join n bot #never carol]
ok "no roster is invented for an unknown buffer" \
    {[_rget "n/bot/#never"] eq "<missing>"}

# An event with no nick cannot move anything.
array unset ::virc::rosters
_rset "n/bot/#a" {@alice}
::virc::_roster_apply [dict create kind join network n presence bot buffer #a]
ok "an event with no nick is ignored" {[_rget "n/bot/#a"] eq "@alice"}

# Mode is deliberately NOT applied locally: deriving who is opped from a
# mode string means reimplementing PREFIX/ISUPPORT in Tcl. It schedules
# a snapshot re-read instead, coalesced so a +ooo burst is one request.
array unset ::virc::rosters
set ::virc::resync_pending 0
::virc::_roster_apply [_ev mode n bot #a alice]
ok "a mode schedules a resync" {$::virc::resync_pending == 1}
::virc::_roster_apply [_ev mode n bot #a alice]
::virc::_roster_apply [_ev mode n bot #a alice]
ok "…and a burst of them coalesces into one" {$::virc::resync_pending == 1}
set ::virc::resync_pending 0


# AND IT IS WIRED INTO `_ingest`. The block above calls `_roster_apply`
# directly, which proves the delta logic and nothing about whether the
# event stream ever reaches it — the exact shape of the six vacuous
# features the last review round found. Drive the real ingest path.
reset_irc
::virc::build_view .ircview
::virc::build_roster .ircroster
set _k [::virc::key n bot #wired]
set ::virc::current $_k
set ::virc::meta($_k) [dict create network n presence bot buffer #wired]
array set ::virc::rosters [list $_k {@alice bob}]
::virc::_ingest [dict create network n presence bot buffer #wired kind join nick carol text ""]
ok "a join arriving on the stream updates the roster" \
    {[lsearch -exact $::virc::rosters($_k) carol] >= 0}
::virc::_ingest [dict create network n presence bot buffer #wired kind part nick bob text ""]
ok "…and a part arriving on the stream removes them" \
    {[lsearch -exact $::virc::rosters($_k) bob] < 0}
::virc::_ingest [dict create network n presence bot buffer * kind quit nick carol text ""]
ok "…and a quit reaches the channel buffers too" \
    {[lsearch -exact $::virc::rosters($_k) carol] < 0}
# The visible half: the list widget on screen was repainted, not just
# the array behind it.
ok "the roster WIDGET shows the result" \
    {[.ircroster.list get 0 end] eq "@alice"}


# ── Tier 3: ignore, aliases, per-buffer settings, bare, mIRC 16–98 ───
puts "\nTier 3"

# The extended palette. It used to render plain "rather than guessed
# at", which was the right instinct and the wrong conclusion — the table
# is published, so use it.
ok "colour 4 is still the classic red" {[::virc::_mirc_colour 4] eq "red"}
ok "colour 15 is still lightgrey" {[::virc::_mirc_colour 15] eq "lightgrey"}
ok "colour 16 is the first extended entry" {[::virc::_mirc_colour 16] eq "#470000"}
ok "colour 52 is extended red" {[::virc::_mirc_colour 52] eq "#ff0000"}
ok "colour 98 is the last entry" {[::virc::_mirc_colour 98] eq "#ffffff"}
ok "colour 99 is default, not a colour" {[::virc::_mirc_colour 99] eq ""}
ok "colour 100 does not exist" {[::virc::_mirc_colour 100] eq ""}
proc _colours_mapped {} {
    set n 0
    for {set i 0} {$i <= 98} {incr i} {
        if {[::virc::_mirc_colour $i] ne ""} { incr n }
    }
    return $n
}
ok "every code 0-98 maps to something" {[_colours_mapped] == 99}
ok "the extended table has exactly 83 entries" \
    {[llength [::virc::_mirc_extended]] == 166}
# A \x03NN run must actually reach a tag, or the table is decoration.
set _runs52 [::virc::parse_formatting "\x0352red\x03"]
ok "a \\x03 52 run is tagged with the extended colour" \
    {[lsearch -exact [lindex $_runs52 0 1] "irc_fg_#ff0000"] >= 0}

# ── /ignore ─────────────────────────────────────────────────────────
set ::virc::ignores {}
set _l [dict create kind privmsg network n presence bot buffer #a nick spammer text hi]
ok "nothing is ignored by default" {![::virc::_is_ignored $_l]}
set ::virc::ignores {spammer}
ok "an exact nick is ignored" {[::virc::_is_ignored $_l]}
ok "…and someone else is not" {
    ![::virc::_is_ignored [dict create kind privmsg network n presence bot \
        buffer #a nick friend text hi]]
}
set ::virc::ignores {spam*}
ok "a glob matches the botnet" {[::virc::_is_ignored $_l]}
set ::virc::ignores {SPAMMER}
ok "matching is case-insensitive" {[::virc::_is_ignored $_l]}
set ::virc::ignores {*!*@bad.host}
ok "a hostmask pattern matches when the host is known" {
    [::virc::_is_ignored [dict create kind privmsg network n presence bot \
        buffer #a nick whoever host "u@bad.host" text hi]]
}
ok "…and does not match when it is not that host" {
    ![::virc::_is_ignored [dict create kind privmsg network n presence bot \
        buffer #a nick whoever host "u@good.host" text hi]]
}
set ::virc::ignores {}
ok "an event with no nick is never ignored" {
    ![::virc::_is_ignored [dict create kind join network n presence bot buffer #a]]
}

# An ignore outranks the smart filter's "but they spoke" exemption —
# that exemption keeps people you care about visible, which is the
# opposite of what an ignore is for.
reset_irc
set ::virc::smart_filter 1
set _k [::virc::key n bot #a]
::virc::_bump_recency $_k spammer
set ::virc::ignores {spammer}
ok "an ignored nick is filtered even after speaking" {
    [::virc::_is_filtered [dict create kind privmsg network n presence bot \
        buffer #a nick spammer text hi]]
}
set ::virc::ignores {}

# ── aliases ─────────────────────────────────────────────────────────
array unset ::virc::aliases
set ::virc::aliases(j) "/join"
ok "an alias expands to its command" {[::virc::_expand_alias "j #chan"] eq "join #chan"}
ok "…and the arguments are appended" \
    {[::virc::_expand_alias "j #chan key"] eq "join #chan key"}
ok "a non-alias passes through untouched" \
    {[::virc::_expand_alias "join #chan"] eq "join #chan"}
set ::virc::aliases(op) {/mode $1 +o $2}
ok "\$1 and \$2 place the arguments" \
    {[::virc::_expand_alias "op #chan bob"] eq "mode #chan +o bob"}
ok "…and are NOT appended a second time" \
    {![string match "*bob*bob*" [::virc::_expand_alias "op #chan bob"]]}
set ::virc::aliases(say) {/msg #chan $*}
ok "\$* takes everything" \
    {[::virc::_expand_alias "say hello there"] eq "msg #chan hello there"}
# One level only: a two-alias cycle is two keystrokes away, and a
# fixpoint loop would hang the UI with no way out.
array unset ::virc::aliases
set ::virc::aliases(a) "/b"
set ::virc::aliases(b) "/a"
ok "expansion is single-pass, so a cycle cannot hang" \
    {[::virc::_expand_alias "a"] eq "b"}
array unset ::virc::aliases

# A built-in may not be shadowed: /alias join /part would make the most
# destructive commands mean whatever a stray keystroke last set them to.
::virc::_slash_alias "join /part"
ok "a built-in cannot be aliased" {![info exists ::virc::aliases(join)]}
ok "…and it says why" {[string match "*built in*" $::vstatus::last]}
::virc::_slash_alias "jj /join"
ok "a free name can be" {[info exists ::virc::aliases(jj)]}
::virc::_slash_alias "jj"
ok "…and naming it with no body removes it" {![info exists ::virc::aliases(jj)]}

# Every built-in must be completable. Four hand-kept copies of this
# list existed; a command you can type but cannot complete is exactly
# the drift that produces.
proc _uncompletable {} {
    set missing {}
    foreach c [::virc::_builtin_slash_names] {
        if {![llength [::virc::_completion_candidates $c command]]} { lappend missing $c }
    }
    return $missing
}
ok "every built-in command is completable" {[llength [_uncompletable]] == 0}

# ── per-buffer settings ─────────────────────────────────────────────
reset_irc
array unset ::virc::bufopt
set _k [::virc::key n bot #a]
ok "an unset option falls back to the default" {[::virc::_bufopt_get $_k filter 1] == 1}
set ::virc::bufopt($_k/filter) 0
ok "a set option overrides it" {[::virc::_bufopt_get $_k filter 1] == 0}
ok "…for that buffer only" \
    {[::virc::_bufopt_get [::virc::key n bot #b] filter 1] == 1}

# And it is WIRED: the filter consults the per-buffer value, not just
# the global. The predicate being right proves nothing about that.
set ::virc::smart_filter 1
set _noise [dict create kind join network n presence bot buffer #a nick ghost text ""]
ok "the filter honours the per-buffer override" {![::virc::_is_filtered $_noise]}
unset ::virc::bufopt($_k/filter)
ok "…and filters again once it is cleared" {[::virc::_is_filtered $_noise]}

# ── bare display ────────────────────────────────────────────────────
reset_irc
::virc::build_view .ircview
set _k [::virc::key n bot #a]
set ::virc::current $_k
set ::virc::meta($_k) [dict create network n presence bot buffer #a]
array unset ::virc::bufopt
::virc::render_line [::virc::transcript] \
    [dict create kind privmsg network n presence bot buffer #a nick bob \
        text hello ts "2026-08-25T12:34:56Z"]
ok "a normal line carries its timestamp" \
    {[string match "*12:34*" [.ircview.body get 1.0 end]]}
::virc::_slash_bare ""
ok "/bare turns it on for this buffer" {[::virc::_bufopt_get $_k bare 0] == 1}
.ircview.body configure -state normal
.ircview.body delete 1.0 end
.ircview.body configure -state disabled
::virc::render_line [::virc::transcript] \
    [dict create kind privmsg network n presence bot buffer #a nick bob \
        text hello ts "2026-08-25T12:34:56Z"]
ok "…and then the timestamp is gone" \
    {![string match "*12:34*" [.ircview.body get 1.0 end]]}
ok "…but the message is not" {[string match "*hello*" [.ircview.body get 1.0 end]]}
::virc::_slash_bare ""
ok "/bare toggles back off" {[::virc::_bufopt_get $_k bare 0] == 0}

# ── the prefs survive a restart ─────────────────────────────────────
reset_irc
namespace eval ::vstate {
    variable blob {}
    proc put {k v} { variable blob ; dict set blob $k $v ; return 1 }
    proc get {k {d ""}} { variable blob
        if {[dict exists $blob $k]} { return [dict get $blob $k] } ; return $d }
}
set ::vstate::blob {}
set ::virc::ignores {spam*}
array unset ::virc::aliases ; set ::virc::aliases(j) "/join"
array unset ::virc::bufopt ; set ::virc::bufopt(n/bot/#a/bare) 1
::virc::_save_prefs
set ::virc::ignores {} ; array unset ::virc::aliases ; array unset ::virc::bufopt
::virc::_load_prefs
ok "ignores survive a restart" {$::virc::ignores eq "spam*"}
ok "aliases survive a restart" {[::virc::_expand_alias "j #x"] eq "join #x"}
ok "per-buffer settings survive a restart" \
    {[::virc::_bufopt_get [::virc::key n bot #a] bare 0] == 1}

# A blob written by an older build, or hand-edited, must not take the
# pane down on start: `array set` on an odd-length list throws.
set ::vstate::blob {}
::vstate::put irc_aliases {j /join dangling}
::vstate::put irc_bufopt {a}
array unset ::virc::aliases
ok "a malformed alias blob is refused, not fatal" \
    {[catch { ::virc::_load_prefs }] == 0}
ok "…and nothing half-applied survives it" {![info exists ::virc::aliases(j)]}


# ── and all three are WIRED, not just present ───────────────────────
#
# The blocks above call `_expand_alias`, `_load_prefs` and `_is_ignored`
# directly. Mutation said that was not enough: removing the alias
# expansion from `_slash`, and removing the `_load_prefs` call from
# `refresh_state`, both left the suite green. Helpers covered, call
# sites not — the exact shape of the six vacuous features the previous
# review round found.

# An alias must work through the REAL dispatch path, not just through
# the expander.
reset_irc
::virc::build_view .ircview
array unset ::virc::aliases
set ::virc::aliases(hi) "/me waves"
set ::virc::current [::virc::key n bot #a]
set ::virc::meta($::virc::current) [dict create network n presence bot buffer #a]
set ::vstatus::last ""
::virc::_slash "hi"
ok "an alias reaches the dispatcher, not just the expander" \
    {![string match "*unknown command*" $::vstatus::last]}
set ::vstatus::last ""
::virc::_slash "definitelynotacommand"
ok "…and a genuine unknown still refuses" \
    {[string match "*unknown command*" $::vstatus::last]}
array unset ::virc::aliases

# `refresh_state` must load the prefs. Removing that call left every
# assertion above green because they all call `_load_prefs` themselves.
reset_irc
namespace eval ::vstate {
    variable blob {}
    proc put {k v} { variable blob ; dict set blob $k $v ; return 1 }
    proc get {k {d ""}} { variable blob
        if {[dict exists $blob $k]} { return [dict get $blob $k] } ; return $d }
}
set ::vstate::blob {}
::vstate::put irc_ignores {loadedfromstate}
set ::virc::ignores {}
set ::vapi::state_reply [dict create latest_id 1 networks [list \
    [dict create network n presence bot nick me buffers {#a}]]]
::virc::refresh_state
ok "refresh_state loads the saved prefs" \
    {$::virc::ignores eq "loadedfromstate"}
set ::vapi::state_reply {}


# ── /ignore repaints what is ALREADY on screen ──────────────────────
#
# The point of an ignore is the wall of noise you can see, not the next
# line. The first version of `_repaint_current` called a proc that does
# not exist (`select_buffer`; it is `show`) inside a bare `catch`, so
# nothing repainted and the suite stayed green because these tests used
# to redraw by hand. This one does not.
reset_irc
::virc::build_view .ircview
set _k [::virc::key n bot #a]
set ::virc::current $_k
set ::virc::meta($_k) [dict create network n presence bot buffer #a]
set ::virc::linebuf($_k) [list \
    [dict create id 1 kind privmsg network n presence bot buffer #a nick spammer text "buy things"] \
    [dict create id 2 kind privmsg network n presence bot buffer #a nick friend text "hello"]]
set ::virc::ignores {}
::virc::_render_transcript
ok "both lines are drawn to begin with" {
    [string match "*buy things*" [.ircview.body get 1.0 end]]
    && [string match "*hello*" [.ircview.body get 1.0 end]]}
::virc::_slash_ignore "spammer"
ok "/ignore repaints and the noise is gone" \
    {![string match "*buy things*" [.ircview.body get 1.0 end]]}
ok "…and the rest is still there" \
    {[string match "*hello*" [.ircview.body get 1.0 end]]}
::virc::_slash_unignore "spammer"
ok "/unignore brings it back without a refetch" \
    {[string match "*buy things*" [.ircview.body get 1.0 end]]}

# And it must not throw on a pane where nothing has loaded yet.
reset_irc
::virc::build_view .ircview
set ::virc::current [::virc::key n bot #never]
ok "a repaint before any buffer has loaded is not an error" \
    {[catch { ::virc::_repaint_current }] == 0}


# ── split windows ───────────────────────────────────────────────────
puts "\nsplit windows"

proc _mkbuf {net pres buf {lines {}}} {
    set k [::virc::key $net $pres $buf]
    ::virc::_note_buffer $k $net $pres $buf
    set ::virc::linebuf($k) $lines
    return $k
}

reset_irc
::virc::build_view .ircview
::virc::build_bufs .ircbufs
set _a [_mkbuf n bot #a [list [dict create id 1 kind privmsg network n \
    presence bot buffer #a nick alice text "in A"]]]
set _b [_mkbuf n bot #b [list [dict create id 2 kind privmsg network n \
    presence bot buffer #b nick bob text "in B"]]]
set ::virc::current $_a

ok "nothing is split to begin with" {[llength $::virc::splits] == 0}
::virc::_slash_split "#b"
ok "/split adds a pane" {[llength $::virc::splits] == 1}
ok "…pinned to the named buffer" {[lindex $::virc::splits 0] eq $_b}
ok "…with a real widget" {[winfo exists $::virc::split_w($_b)]}
ok "…painted from that buffer's lines" \
    {[string match "*in B*" [$::virc::split_w($_b) get 1.0 end]]}
ok "…and the primary still shows the current buffer" \
    {$::virc::current eq $_a}

# A split does NOT follow the current buffer. That is the whole point.
::virc::show $_b
ok "switching buffers leaves the split pinned" \
    {[lindex $::virc::splits 0] eq $_b}
::virc::show $_a
ok "…and it is still pinned after switching back" \
    {[info exists ::virc::split_w($_b)]}

# A line for the split's buffer reaches it even though it is not current.
::virc::_ingest [dict create network n presence bot buffer #b kind privmsg \
    nick bob text "live into the split"]
ok "a line for a split's buffer is drawn there while another is current" \
    {[string match "*live into the split*" [$::virc::split_w($_b) get 1.0 end]]}
ok "…and NOT into the primary" \
    {![string match "*live into the split*" [.ircview.body get 1.0 end]]}

# A buffer that is both current AND split appears in both.
::virc::show $_b
::virc::_ingest [dict create network n presence bot buffer #b kind privmsg \
    nick bob text "in both"]
ok "a buffer that is current and split is drawn in both panes" {
    [string match "*in both*" [.ircview.body get 1.0 end]]
    && [string match "*in both*" [$::virc::split_w($_b) get 1.0 end]]}
::virc::show $_a

::virc::_slash_split "#b"
ok "splitting the same buffer twice is refused" {[llength $::virc::splits] == 1}
::virc::_slash_split "#nosuchbuffer"
ok "splitting an unknown buffer is refused" {[llength $::virc::splits] == 1}
ok "…and says so" {[string match "*no buffer*" $::vstatus::last]}

::virc::_slash_unsplit ""
ok "/unsplit removes it" {[llength $::virc::splits] == 0}
ok "…and destroys the widget" {![info exists ::virc::split_w($_b)]}
::virc::_slash_unsplit ""
ok "/unsplit with nothing split is refused, not an error" \
    {[string match "*nothing is split*" $::vstatus::last]}

# The cap. Four is where each pane stops being readable; refusing beats
# silently producing five two-line windows.
reset_irc
::virc::build_view .ircview
::virc::build_bufs .ircbufs
foreach c {#c1 #c2 #c3 #c4 #c5} { _mkbuf n bot $c }
set ::virc::current [::virc::key n bot #c1]
foreach c {#c1 #c2 #c3 #c4 #c5} { ::virc::_slash_split $c }
ok "no more than four splits" {[llength $::virc::splits] == 4}
ok "…and the fifth says why" {[string match "*maximum*" $::vstatus::last]}

# ── the <<TreeviewSelect>> re-entrancy guard ────────────────────────
#
# `show` ends in _refresh_buffers, which does `selection set $current`;
# ttk fires <<TreeviewSelect>> from that unconditionally, and the event
# is QUEUED. _fetch_backlog's nested event loop then dispatches it, so
# without the guard in _pick the desktop recurses show -> _pick -> show
# until the stack goes. It only reproduces with the buffer LIST built
# AND the event loop driven, which is why no suite had it: the old
# tests built one or the other.
#
# The counting wrapper is BOUNDED on purpose — an unguarded _pick would
# otherwise hang this suite rather than fail it.
puts "\n<<TreeviewSelect>> re-entrancy"

reset_irc
::virc::build_view .ircview
::virc::build_bufs .ircbufs
set _ra [_mkbuf n bot #ra [list [dict create id 1 kind privmsg network n \
    presence bot buffer #ra nick alice text "in A"]]]
set _rb [_mkbuf n bot #rb [list [dict create id 2 kind privmsg network n \
    presence bot buffer #rb nick bob text "in B"]]]

set ::_shows 0
rename ::virc::show ::virc::_show_real
proc ::virc::show {k} {
    incr ::_shows
    if {$::_shows > 5} { return 0 }   ;# bound the loop this test exists to catch
    return [::virc::_show_real $k]
}

::virc::show $_ra
update                    ;# dispatch the <<TreeviewSelect>> show queued
ok "show does not re-enter itself through <<TreeviewSelect>>" {
    $::_shows == 1
}
ok "…and the buffer it was asked for is the one showing" {
    $::virc::current eq $_ra
}

# A selection change to a DIFFERENT buffer must still work — the guard
# must not turn the buffer list into a dead tree.
set ::_shows 0
.ircbufs.list selection set $_rb
update
ok "selecting a different buffer still shows it" {
    $::_shows >= 1 && $::virc::current eq $_rb
}

# The context menu's Open re-renders the CURRENT buffer deliberately;
# the guard is on the event path only.
set ::_shows 0
::virc::_pick_selected
ok "the context menu's Open re-shows the current buffer" { $::_shows == 1 }

rename ::virc::show {}
rename ::virc::_show_real ::virc::show

# ── the reconnect backoff survives an attach switch ─────────────────
#
# `disconnect` (the attach teardown's step 1) cancels the pending retry
# so a backoff armed against gateway A cannot fire into gateway B. For
# a while it could not: _schedule_retry did a bare `after $retry
# ::virc::connect` whose id went nowhere, so the `after cancel
# $retry_after` in disconnect tested a variable nothing ever set. The
# `disconnected` flag caught the fallout, but the timer still outlived
# the switch and fired one spurious reconnect the moment `reconnect`
# cleared the flag.
puts "\nthe reconnect backoff"

set ::virc::retry 60000        ;# far enough out that it cannot fire mid-test
set ::virc::retry_after ""
::virc::_schedule_retry
ok "_schedule_retry records its timer id" { $::virc::retry_after ne "" }
set _armed $::virc::retry_after
ok "…and the id is a live `after` event" {
    [lsearch -exact [after info] $_armed] >= 0
}
::virc::_schedule_retry
ok "re-arming cancels the previous timer rather than stacking one" {
    [lsearch -exact [after info] $_armed] < 0 && $::virc::retry_after ne $_armed
}
set _armed2 $::virc::retry_after
::virc::disconnect
ok "disconnect cancels the pending backoff for real" {
    [lsearch -exact [after info] $_armed2] < 0
}
ok "…and clears the id, so a later cancel is not a stale one" {
    $::virc::retry_after eq ""
}
ok "…and marks the pane disconnected so connect no-ops" {
    $::virc::disconnected == 1
}
set ::virc::disconnected 0
set ::virc::retry 1000

# ── merged server buffer ────────────────────────────────────────────
puts "\nmerged server buffer"

reset_irc
::virc::build_view .ircview
::virc::build_bufs .ircbufs
set ::virc::merged 0
set _srv [dict create network alpha presence bot buffer "*" kind notice \
    nick server text "*** alpha MOTD"]
::virc::_ingest $_srv
ok "with merging off there is no merged buffer" \
    {[lsearch -exact $::virc::buffers [::virc::merged_key]] < 0}

::virc::_slash_merge "on"
ok "/merge on creates it" \
    {[lsearch -exact $::virc::buffers [::virc::merged_key]] >= 0}
::virc::_ingest $_srv
::virc::_ingest [dict create network beta presence bot buffer "*" kind notice \
    nick server text "*** beta MOTD"]
ok "server lines from two networks land in one buffer" {
    [string match "*alpha MOTD*" $::virc::linebuf([::virc::merged_key])]
    && [string match "*beta MOTD*" $::virc::linebuf([::virc::merged_key])]}

# A channel message must NOT appear there, or the merged buffer becomes
# a second copy of everything.
::virc::_ingest [dict create network alpha presence bot buffer "#chan" \
    kind privmsg nick alice text "channel chatter"]
ok "a channel message does not reach the merged buffer" \
    {![string match "*channel chatter*" $::virc::linebuf([::virc::merged_key])]}

ok "the merged buffer is labelled as such, not as \"*\"" \
    {[string match "*all servers*" [::virc::_label [::virc::merged_key]]]}

# It spans networks, so there is no answer to "which one does this go
# to" — the composer must refuse rather than pick one.
#
# `*` IS in sendable on purpose. With sendable {bot} this assertion
# passed against a tree whose merged-key guard had been DELETED: the
# merged buffer's presence is "*", "*" is not in {bot}, and can_send's
# ordinary last line returned 0 for that reason instead. Putting "*" in
# sendable removes the second explanation, so only the guard can produce
# the 0.
set ::virc::sendable {bot *}
ok "the merged buffer refuses to send" \
    {[::virc::can_send [::virc::merged_key]] == 0}
ok "…and not merely because its presence is unsendable" \
    {"*" in $::virc::sendable}
set ::virc::sendable {bot}
_mkbuf alpha bot #real
ok "…while a real buffer still sends" \
    {[::virc::can_send [::virc::key alpha bot #real]]}

::virc::_slash_merge "off"
ok "/merge off removes the buffer" \
    {[lsearch -exact $::virc::buffers [::virc::merged_key]] < 0}
::virc::_ingest $_srv
ok "…and stops collecting" \
    {![info exists ::virc::linebuf([::virc::merged_key])]}
::virc::_slash_merge "sideways"
ok "/merge with nonsense is refused" {[string match "*on or off*" $::vstatus::last]}

# The setting survives a restart.
namespace eval ::vstate {
    variable blob {}
    proc put {k v} { variable blob ; dict set blob $k $v ; return 1 }
    proc get {k {d ""}} { variable blob
        if {[dict exists $blob $k]} { return [dict get $blob $k] } ; return $d }
}
set ::vstate::blob {}
set ::virc::merged 1
::virc::_save_prefs
set ::virc::merged 0
::virc::_load_prefs
ok "merging survives a restart" {$::virc::merged == 1}
set ::virc::merged 0

# ── the hotlist sort returns a PLAIN int ────────────────────────────
#
# lsort -command takes a plain int; `clock milliseconds` is 64-bit wide,
# so a compare that RETURNS the difference of two timestamps throws
# "-compare command returned non-integer result" mid-sort. The values
# below are today's epoch, not toy numbers — that is the point.
puts "\nbuffer sort returns plain ints"
array set ::virc::activity {}
array set ::virc::lasttouch {a 1787697755894 b 1787697755994}
set rc [catch {lsort -command ::virc::_buffer_sort_compare {a b}} r]
ok "wide-int tiebreak does not blow up the sort" {$rc == 0}
ok "…and orders fresher first" {[lindex $r 0] eq "b"}
array set ::virc::lasttouch {a 1787697755894 b 1787697755994}
array unset ::virc::activity ; array set ::virc::activity {a 2 b 1}
ok "activity still outranks freshness" \
    {[lindex [lsort -command ::virc::_buffer_sort_compare {a b}] 0] eq "a"}
array unset ::virc::lasttouch
array unset ::virc::activity

# ── typing indicators (Tier 3) ──────────────────────────────────────
puts "\ntyping indicators"

reset_irc
::virc::build_view .ircview
# build_view no longer self-packs (that was the startup-layout bug), so
# map it explicitly — the event-generate below only reaches a mapped
# widget. The quoted-insert section does the same.
pack .ircview
set _k [::virc::key n bot #a]
_mkbuf n bot #a
set ::virc::current $_k
::virc::_refresh_topic $_k

# INBOUND: a typing event lights the topic line, a done event clears it.
::virc::_ingest [dict create network n presence bot buffer #a kind typing \
    nick alice text active]
ok "an active indicator names the nick" \
    {[string match "*alice*" [.ircview.topic cget -text]]}
::virc::_ingest [dict create network n presence bot buffer #a kind typing \
    nick bob text active]
ok "two typists are both listed" {
    [string match "*alice*" [.ircview.topic cget -text]]
    && [string match "*bob*" [.ircview.topic cget -text]]}
::virc::_ingest [dict create network n presence bot buffer #a kind typing \
    nick alice text done]
ok "a done event removes one of them" {
    [string match "*bob*" [.ircview.topic cget -text]]
    && ![string match "*alice*" [.ircview.topic cget -text]]}

# …and the indicator never becomes scrollback, an unread count or a
# hotlist entry — it is ephemeral state, not a message.
ok "typing events do not reach the transcript" \
    {![string match "*alice*" [.ircview.body get 1.0 end]]}
ok "…do not raise the unread count" {![info exists ::virc::unread($_k)]}
ok "…and do not touch the cursor" {$::virc::cursor == 0}

# A done event for the LAST typist empties the label entirely.
::virc::_ingest [dict create network n presence bot buffer #a kind typing \
    nick bob text done]
ok "the last done event clears the label" \
    {![string match "*✍*" [.ircview.topic cget -text]]}

# A crash mid-sentence must not pin an eternal "typing": the expiry
# timer drops the nick on our side. Driven DIRECTLY (the timer's own
# callback), so the suite never waits on wall-clock time.
::virc::_ingest [dict create network n presence bot buffer #a kind typing \
    nick carol text active]
ok "carol is listed before expiry" \
    {[string match "*carol*" [.ircview.topic cget -text]]}
ok "…and an expiry timer is armed" {[info exists ::virc::typing_after($_k)]}
::virc::_typing_expire $_k carol
ok "an expired indicator is dropped" \
    {![string match "*carol*" [.ircview.topic cget -text]]}

# OUTBOUND: keys in the composer post a /api/irc/typing body; sending
# posts done and cancels the idle re-announce.
reset_send
clear_composer
focus -force $inp
update
event generate $inp <Key-a> -when now
event generate $inp <KeyRelease> -keysym a -when now
update
set _tposts [count_posts /api/irc/typing]
ok "typing in the composer posts an indicator" {$_tposts >= 1}
ok "…as active, to this buffer" \
    {[sent end target] ne "<no target>"}
clear_composer
.ircview.input insert 1.0 "hello"
set ::confirm_answer yes
ok "sending works" {[::virc::send_current] == 1}
set _done 0
foreach _p $::vapi::posted {
    if {[lindex $_p 0] eq "/api/irc/typing"
        && [dict get [lindex $_p 1] state] eq "done"} { incr _done }
}
ok "send posts done" {$_done >= 1}

# ── ban lists (Tier 3) ──────────────────────────────────────────────
puts "\nban lists"

reset_ops
::virc::_slash_bans ""
ok "/bans with no channel asks for the current one" \
    {[llength $::vapi::posted] >= 1 && [sent 0 action] eq "bans"}
reset_ops
::virc::_slash_bans "#other"
ok "/bans takes an explicit channel" {[sent 0 channel] eq "#other"}

# ── server status (weechat parity) ─────────────────────────────────
#
# USER: "In irc I don't see server status or motd?" The MOTD half is
# engine-side (client.rs numerics → Info events on `*`, pinned in
# Rust); this pins the pane half: the lifecycle events render, and the
# connection state stays visible under the topic instead of living as
# `●`/`○` lines that scroll away.
puts "\nserver status"

::virc::build_view .ircview
ok "the view has a status row" {[winfo exists .ircview.status]}

::virc::render_line .ircview.body \
    [dict create network n presence bot buffer * kind connecting \
         text "reconnecting in 4s" ts ""]
ok "a connecting line renders" \
    {[string match "*◌ reconnecting in 4s*" [.ircview.body get 1.0 end]]}
ok "…and ranks as lifecycle noise, not activity" {
    [::virc::_activity_level x \
        [dict create kind connecting text "reconnecting in 4s"]] == 0
}

# The state snapshot seeds the status before any live event.
reset_irc
::virc::build_view .ircview
set ::vapi::state_reply [dict create latest_id 1 networks [list \
    [dict create network irc.example presence bot nick jackma connected 1 \
        channels {} buffers {*} \
        rosters [dict create] topics [dict create]]]]
::virc::refresh_state
set _sk [::virc::key irc.example bot *]
::virc::show $_sk
ok "the snapshot says connected" \
    {[string match "*● connected*" [.ircview.status cget -text]]}
ok "…and names who you are there" \
    {[string match "*jackma*" [.ircview.status cget -text]]}

# Live lifecycle moves the row whichever buffer is on screen.
::virc::_ingest [dict create network irc.example presence bot buffer * \
    kind disconnected text "connection closed"]
ok "a drop is visible at once" \
    {[string match "*○ disconnected*" [.ircview.status cget -text]]}
::virc::_ingest [dict create network irc.example presence bot buffer * \
    kind connecting text "reconnecting in 8s"]
ok "…and so is the reconnect" \
    {[string match "*◌ reconnecting in 8s*" [.ircview.status cget -text]]}
::virc::_ingest [dict create network irc.example presence bot buffer * \
    kind connected text "registered as jackma"]
ok "…until registration lands" \
    {[string match "*● connected*" [.ircview.status cget -text]]}

# A server line (MOTD arrives as Info on `*`) fans out to the merged
# buffer exactly like the lifecycle lines do — the merged view is why
# a multi-network user sees every MOTD in one place.
reset_irc
::virc::build_view .ircview
::virc::show [::virc::key irc.example bot *] ;# no-op until meta exists
set ::virc::merged 1
::virc::_ingest [dict create id 1 network irc.example presence bot buffer * \
    kind info text "MOTD — be excellent to each other"]
ok "a MOTD line reaches the merged buffer" \
    {[lsearch -exact $::virc::buffers [::virc::merged_key]] >= 0}
set ::virc::merged 0

# ── ACL management from the tab ──────────────────────────────────────
#
# USER (2026-08-30): "there was also supposed to be direct management
# over the IRC tab". /acl opens the ACL overlay scoped to this network,
# and the roster's right-click actions grant/ban the selected member's
# mask. The engine resolves masks (nick!user@host, glob/re: per
# segment) with per-network/per-channel scope — pinned in Rust; here we
# pin that the tab SENDS the right shape.
puts "\nacl from the irc tab"

namespace eval ::vacl {
    variable opens {}
    proc open_for {identity network} {
        variable opens
        lappend opens [list $identity $network]
    }
}

reset_send
::virc::_slash "acl"
ok "/acl opens the ACL overlay" {[llength $::vacl::opens] == 1}
ok "…scoped to the buffer's network" \
    {[lindex $::vacl::opens 0] eq [list "" n]}

::virc::build_roster .ircroster
pack .ircroster

# The identity a roster action targets: the full mask when NAMES carried
# one (userhost-in-names), nick!*@* otherwise — prefix chars stripped.
.ircroster.list delete 0 end
.ircroster.list insert end "@wrath!~w@gateway/veles"
.ircroster.list selection set 0
ok "a masked member targets the full mask" \
    {[::virc::_roster_acl_identity] eq "wrath!~w@gateway/veles"}
# A bare nick ASKS. `nick!*@*` is the most spoofable identity on IRC —
# a grant keyed on it belongs to whoever takes the nick next — so the
# roster does a WHOIS first and only falls back when that fails.
.ircroster.list delete 0 end
.ircroster.list insert end "+plainnick"
.ircroster.list selection set 0
proc fake_whois {nick} {
    return "$nick: verified account pn; hostmask $nick!~p@real.host; server irc.example;"
}
ok "a bare nick is resolved by WHOIS, not guessed" \
    {[::virc::_roster_acl_identity fake_whois] eq "plainnick!~p@real.host"}
proc dead_whois {nick} { return "" }
ok "…and only falls back to nick!*@* when the ask fails" \
    {[::virc::_roster_acl_identity dead_whois] eq "plainnick!*@*"}
proc broken_whois {nick} { error "no such nick" }
ok "…or errors" \
    {[::virc::_roster_acl_identity broken_whois] eq "plainnick!*@*"}
# A member whose roster entry already carries the mask never asks at
# all — userhost-in-names has already answered.
.ircroster.list delete 0 end
.ircroster.list insert end "@wrath!~w@gateway/veles"
.ircroster.list selection set 0
proc must_not_ask {nick} { error "WHOIS must not be issued when NAMES carried the mask" }
ok "a masked member does not re-ask" \
    {[::virc::_roster_acl_identity must_not_ask] eq "wrath!~w@gateway/veles"}

reset_send
.ircroster.list delete 0 end
.ircroster.list insert end "@nyx!~n@box.example"
.ircroster.list selection set 0
::virc::_roster_acl_grant trusted
ok "a roster grant POSTs identity+role" \
    {[sent_path end] eq "/api/acl/grant"
     && [sent end identity] eq "nyx!~n@box.example"
     && [sent end role] eq "trusted"}
ok "…scoped to this network" {[sent end network] eq "n"}

::virc::_roster_acl_grant banned
ok "a roster ban POSTs role banned" {[sent end role] eq "banned"}

# `admin` is grantable from the roster: the backend has always accepted
# it (role_catalog carries it) and the menu did not offer it, so the
# one role an operator most often hands out from a channel needed the
# overlay (PENDING.md).
::virc::_roster_acl_grant admin
ok "a roster grant can name admin" {[sent end role] eq "admin"}

::virc::_roster_acl_revoke
ok "a roster revoke POSTs the same identity" \
    {[sent_path end] eq "/api/acl/revoke"
     && [sent end identity] eq "nyx!~n@box.example"}

# No selection → nothing is posted, and it says so.
set ::vapi::posted {}
.ircroster.list selection clear 0 end
::virc::_roster_acl_grant trusted
ok "no selection posts nothing" {[llength $::vapi::posted] == 0}

puts "\nirctest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
