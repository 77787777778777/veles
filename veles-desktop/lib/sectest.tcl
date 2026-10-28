# veles-desktop/lib/sectest.tcl — the desktop's adversarial suite.
#
# WHY THIS EXISTS, AND WHY IT IS SEPARATE
#
# USER: "could we maybe look at CVEs and other bugs in irssi, BitchX,
# weechat, mirc, etc so that we don't repeat any trivial mistakes? I
# imagine at least one of them must have a good test suite guarding
# against security regressions".
#
# They do. WeeChat carries tests/unit/plugins/irc/ — eighteen files
# including test-irc-protocol.cpp, test-irc-ctcp.cpp and test-irc-tag.cpp
# — and adds a case per advisory, which is why the same DCC filename bug
# could not come back a second time after WSA-2017-1 without a test going
# red. (It came back anyway, as WSA-2026-5, in a *different* function.
# That is the honest version of what a suite like this buys you: it pins
# the code it covers, not the idea.)
#
# The cases below are grouped by the class they defend against, and each
# names its precedent. A case whose name does not say what would break in
# the field is a case nobody will understand well enough to keep.
#
# Kept out of irctest.tcl deliberately: this suite spawns processes and
# writes canary files, and the other suites should stay pure. It also
# reaches past IRC — the same exec class lives in markdown.tcl and
# panes.tcl.
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

namespace eval ::vstatus { variable last "" ; proc set_msg {m} { variable last $m } }
namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
namespace eval ::vi18n { proc label {w key args} { if {![winfo exists $w]} { return 0 } ; catch { $w configure -text [::vi18n::mc $key {*}$args] } ; return 1 }
                    proc aname {w key args} { if {![winfo exists $w]} { return 0 } ; catch { ::va11y::name $w [::vi18n::mc $key {*}$args] } ; return 1 } }

source [file join $_dir paths.tcl]
source [file join $_dir safeexec.tcl]

# A scratch directory on tmpfs. These tests write canary files and spawn
# processes; the machine note for this box says heavy scratch I/O does
# not go on the SSD.
set SCRATCH [file join [expr {[info exists ::env(TMPDIR)] ? $::env(TMPDIR) : "/dev/shm"}] \
                 veles-sectest-[pid]]
file mkdir $SCRATCH
proc cleanup {} { catch { file delete -force $::SCRATCH } }

# Put known content in a canary and report whether it survived. This is
# the only honest way to test an exec fix: `is_exec_operator` returning
# the right boolean proves nothing about what `exec` actually does with
# the string, and the whole bug was a wrong belief about exactly that.
proc canary {} {
    set f [file join $::SCRATCH canary]
    set fh [open $f w] ; puts -nonewline $fh "INTACT" ; close $fh
    return $f
}
proc canary_intact {f} {
    if {![file exists $f]} { return 0 }
    set fh [open $f r] ; set c [read $fh] ; close $fh
    return [expr {$c eq "INTACT"}]
}

set ECHO [lindex [auto_execok echo] 0]
if {$ECHO eq ""} {
    puts "sectest: 0 passed, 1 failed"
    puts "  ✗ no echo binary — cannot run the exec cases"
    cleanup
    exit 1
}

# ── 1. exec argument injection ───────────────────────────────────────
#
# Precedent: XChat CVE-2010-2192 — a remote ircs:// URI reached the
# handler's --command argument. Class: a remote string that stops being
# data and becomes part of the command.
#
# The veles instance was irc.tcl's `exec $notify_path $title $body &`,
# where $title is the sender's nick and $body the message text. Both are
# attacker-chosen and a private message notifies unconditionally.

puts "\nexec argument injection (XChat CVE-2010-2192 class)"

# The classification, first — cheap, and it localises a failure.
foreach op {> >> 2> >& >>& >@ 2>>. < << <@} {
    ok "`$op` is recognised as exec grammar" {[::vsafe::is_exec_operator $op]}
}
ok "a bare pipe is recognised"        {[::vsafe::is_exec_operator "|"]}
ok "`|&` is recognised"               {[::vsafe::is_exec_operator "|&"]}
ok "ordinary text is not"             {![::vsafe::is_exec_operator "hello > there"]}
ok "a URL is not"                     {![::vsafe::is_exec_operator "https://x/y?a>b"]}
ok "an empty string is not"           {![::vsafe::is_exec_operator ""]}

# Now the part that matters: does the sanitised form actually stop exec
# from opening the file? Each of these CLOBBERS the canary unsanitised —
# verified on this machine before the fix went in, including with `&`
# and including when the command itself could not be found, because exec
# opens the redirection target before it tries to spawn.
foreach form {> >> 2> >&} {
    set f [canary]
    catch { exec $ECHO pwned [::vsafe::arg "${form}$f"] }
    ok "a message beginning `$form` cannot truncate a file" {[canary_intact $f]}
}

# The background form is what irc.tcl actually used.
set f [canary]
catch { exec $ECHO pwned [::vsafe::arg ">$f"] & }
after 300
ok "…and not with & either" {[canary_intact $f]}

# The pipeline. A nick of "|" is legal under RFC 2812 (special = "[" /
# "]" / "\\" / "`" / "_" / "^" / "{" / "|" / "}"), so the attacker
# supplies BOTH words: `exec notify-send | <text>` ran `<text>`.
set marker [file join $SCRATCH ranviapipe]
catch { file delete -force $marker }
set touch [lindex [auto_execok touch] 0]
catch { exec $ECHO [::vsafe::arg "|"] [::vsafe::arg $touch] [::vsafe::arg $marker] }
ok "a nick of `|` cannot turn the message into a command" \
    {![file exists $marker]}

# Sanity: the neutralised string still says what the sender said, modulo
# one leading space. Dropping or escaping the character would be lying
# to the reader about what arrived, which is its own kind of bug.
ok "the text is preserved, not mangled" \
    {[string trim [::vsafe::arg ">hello"]] eq ">hello"}
ok "safe text is returned byte-identical" \
    {[::vsafe::arg "hello world"] eq "hello world"}

# spawn sanitises every word it is given.
set f [canary]
::vsafe::spawn $ECHO ">$f" "also fine"
after 300
ok "spawn sanitises every argument" {[canary_intact $f]}
ok "spawn reports failure rather than throwing" {[::vsafe::spawn "" x] == 0}
ok "spawn on a missing binary returns 0" \
    {[::vsafe::spawn [file join $::SCRATCH nope] x] == 0}

# ── 2. URL scheme allow-list ─────────────────────────────────────────
#
# Precedent: XChat again (an ircs:// URI reaching a handler), plus the
# general shape slopdrop's PUBLIC_DEPLOYMENT_SECURITY.md calls SSRF
# prevention. A system opener acts on file:, data: and javascript: just
# as willingly as on http, and in panes.tcl the URL arrives from the
# AGENT with no click involved.

puts "\nURL scheme allow-list"

foreach u {http://x/y https://x/y HTTPS://X/Y mailto:a@b} {
    ok "`$u` is allowed" {[::vsafe::url $u] ne ""}
}
foreach u {file:///etc/passwd data:text/html,<script> javascript:alert(1)
           ircs://host/%23chan vbscript:x jar:http://x!/y} {
    ok "`$u` is refused" {[::vsafe::url $u] eq ""}
}
ok "a scheme-less string is refused"  {[::vsafe::url "/etc/passwd"] eq ""}
ok "an empty string is refused"       {[::vsafe::url ""] eq ""}
ok "leading whitespace does not smuggle a scheme past the check" \
    {[::vsafe::url "  file:///etc/passwd"] eq ""}
ok "case does not smuggle one either" {[::vsafe::url "FiLe:///etc/passwd"] eq ""}

# ── 3. the IRC notifier, driven for real ─────────────────────────────
#
# The unit cases above test ::vsafe. This one drives the actual proc
# that had the bug, with the actual dict shape the SSE tap delivers,
# because a fix applied to the helper and not to the call site is the
# failure mode the mutation sweep keeps finding in this repo.

puts "\nthe IRC notifier, end to end"

namespace eval ::va11y  { proc name {w d} {} ; proc describe {w} { return "" } }
namespace eval ::vctx   { proc attach {w i} {} }
namespace eval ::vfind  { proc match_all {w q} { return {} } ; proc highlight {w m} {} }
source [file join $_dir vtext.tcl]
source [file join $_dir irc.tcl]

# Point the notifier at a real binary so a successful injection would
# leave real evidence, and drive the tap-shaped dict straight in.
set ::virc::notify_path $ECHO

set f [canary]
::virc::_maybe_notify [dict create kind privmsg buffer "someone" \
    nick "attacker" text ">$f"] 2
after 300
ok "a hostile PM body cannot truncate a file" {[canary_intact $f]}

set f [canary]
::virc::_maybe_notify [dict create kind privmsg buffer "#chan" \
    nick "attacker" text ">$f"] 3
after 300
ok "…nor can a hostile highlight in a channel" {[canary_intact $f]}

# `_notify` passes exactly two words, so a piped command gets NO
# arguments — which means `touch $marker` cannot be expressed and an
# assertion built on it passes whether the bug is present or not. (It
# did, until reverting the fix failed to turn this case red.) So give
# the pipeline something that needs no arguments: a script whose only
# job is to leave evidence.
set marker [file join $SCRATCH ranvianick]
set payload [file join $SCRATCH payload.sh]
set fh [open $payload w]
puts $fh "#!/bin/sh"
puts $fh ": > \"$marker\""
close $fh
file attributes $payload -permissions 0755
catch { file delete -force $marker }
::virc::_maybe_notify [dict create kind privmsg buffer "someone" \
    nick "|" text $payload] 2
after 500
ok "a nick of `|` cannot execute the message text" {![file exists $marker]}

# ── 4. mIRC formatting codes ─────────────────────────────────────────
#
# Precedent: CVE-2012-5854 (WeeChat, buffer overflow decoding IRC colour
# codes) and WSA-2009-1 (crash on receiving colour codes). veles is Tcl,
# so the failure mode is not an overflow — it is an infinite loop, an
# index past the end, or a control byte reaching the transcript. The
# last one matters most: a raw \x01 or a \x1B in a Text widget is how a
# transcript stops being readable.

puts "\nmIRC formatting codes (CVE-2012-5854 class)"

foreach case [list \
    [list "truncated colour"        "\x03"] \
    [list "colour then comma"       "\x03,"] \
    [list "colour then double comma" "\x03,,5"] \
    [list "digits past two"         "\x0399999text"] \
    [list "comma with no bg digits" "\x0304,text"] \
    [list "unterminated bold"       "\x02bold"] \
    [list "every code at once"      "\x02\x1D\x1F\x1E\x0F\x03"] \
    [list "reset with no state"     "\x0F"] \
    [list "colour at end of line"   "text\x0304"] \
] {
    lassign $case label input
    set threw [catch { ::virc::parse_formatting $input } _]
    ok "$label parses without throwing" {!$threw}
    set threw [catch { ::virc::strip_formatting $input } out]
    ok "$label strips without throwing" {!$threw}
    if {!$threw} {
        ok "$label leaves no C0 control byte in the output" \
            {![regexp {[\x00-\x08\x0B\x0C\x0E-\x1F]} $out]}
    }
}

# Not a hang: a long line has to finish. The parser walks character by
# character, so a quadratic mistake here would be a UI freeze on a line
# a stranger chose the length of.
set long [string repeat "\x03" 20000]
set threw [catch { ::virc::strip_formatting $long } out]
ok "20k colour codes terminate" {!$threw}
ok "…and produce no output" {$out eq ""}
set long [string repeat "\x0304a\x0F" 20000]
set threw [catch { ::virc::strip_formatting $long } out]
ok "20k colour/reset pairs terminate" {!$threw}
ok "…keeping only the text" {$out eq [string repeat "a" 20000]}

# ── 5. Tcl injection through remote names ────────────────────────────
#
# Precedent: the format-string family — WSA-2013-1 (crash on monitored
# nicks containing format characters), WSA-2013-2, WSA-2013-3,
# WSA-2006-1. In C the bug is %n reaching printf. Tcl has no printf
# problem, but it has a worse one: a nick containing [brackets] or
# $variables is live code if it ever reaches eval, subst, or an unbraced
# expr. A nick is remote text and it flows into colouring, the hotlist
# and completion.

puts "\nTcl injection through remote names (WSA-2013-1 class)"

set hostile_nicks [list \
    {[exec touch /dev/shm/veles-tclinj]} \
    {$env(HOME)} \
    "nick\{brace" \
    "nick\"quote" \
    {nick\backslash} \
    {[[[} \
    {$$$} \
    "" \
]
catch { file delete -force /dev/shm/veles-tclinj }
foreach n $hostile_nicks {
    set threw [catch { ::virc::nick_colour $n } _]
    ok "nick_colour survives [string range $n 0 18]" {!$threw}
    set threw [catch { ::virc::strip_formatting $n } _]
    ok "strip_formatting survives [string range $n 0 18]" {!$threw}
    set threw [catch { ::virc::nick_of $n } _]
    ok "nick_of survives [string range $n 0 18]" {!$threw}
}
ok "no nick was ever evaluated as Tcl" \
    {![file exists /dev/shm/veles-tclinj]}

# ── 6. bidi and zero-width characters ────────────────────────────────
#
# Not a CVE so much as a permanent trick: U+202E RIGHT-TO-LEFT OVERRIDE
# reorders everything after it, which is how a filename or a nick is
# made to read as something it is not. Every client that renders
# untrusted text has to decide about this one; deciding by accident is
# the bug. veles strips C0 today and passes bidi controls through, so
# the assertion below records the CURRENT answer and will go red the day
# somebody changes it without meaning to.

puts "\nbidi and zero-width characters"

set rtl "abc‮def"
set threw [catch { ::virc::strip_formatting $rtl } out]
ok "an RTL override does not throw" {!$threw}
ok "an RTL override is currently PASSED THROUGH (documented, not endorsed)" \
    {[string first "‮" $out] >= 0}
set zw "a​b﻿c"
ok "zero-width characters survive stripping" \
    {[catch { ::virc::strip_formatting $zw }] == 0}

cleanup
puts "\nsectest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
