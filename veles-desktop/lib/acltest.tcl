# veles-desktop/lib/acltest.tcl — the ACL overlay, driven like the user
# would use it: build the real window, feed it a real-shaped payload,
# click grant/revoke, assert what got POSTed and what got painted.
#
# The engine's own half is pinned in Rust (pairing::grant/revoke tests,
# gateway /pair and /acl tests); this pins the UI half: rows, catalogs,
# legacy-mode note, grant dialog body, revoke gating on config pins.
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

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

source [file join $_dir paths.tcl]
source [file join $_dir i18n.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T

# ── stubs ────────────────────────────────────────────────────────────
namespace eval ::voverlay {
    proc _header {w title} {}
    # Mirror the real just enough: build the acl overlay if it is
    # not up yet (it usually is in these tests — ::vacl::build ran
    # first — so this is normally a no-op).
    proc open {which} {
        if {$which eq "acl" && ![winfo exists .ov_acl]} { ::vacl::build .ov_acl }
    }
}
namespace eval ::vplace { proc centered {w width height args} {} }
# Stubs, with the REAL signatures.
#
# `tab_order` took {w l} here and takes {widgets} in a11y.tcl — so the
# suite validated a call shape that did not exist, acl.tcl was written
# against the fiction, and ::vacl::build threw "wrong # args" the moment
# anyone opened the ACL overlay. Nothing caught it because this was the
# only thing that ever called build. A stub that disagrees with the
# thing it stands in for is worse than no stub.
namespace eval ::va11y {
    proc name {w t} {}
    proc tab_order {widgets} {}
}
namespace eval ::vapi {
    variable posts {}
    variable body_ok [dict create ok true]
    variable legacy 0
    variable payload_curated [dict create \
        configured 1 \
        owners [list irc:wrath] \
        default_role guest \
        role_catalog [list \
            [dict create name owner desc "the principal"] \
            [dict create name trusted desc "user + memory and skills"]] \
        cap_catalog [list \
            [dict create name chat desc "start agent turns"] \
            [dict create name shell desc "run terminal and execute_code"]] \
        identities [list \
            [dict create identity irc:wrath source config role owner effective_role owner caps [list all]] \
            [dict create identity irc:friend source grant role user effective_role user caps [list chat web]] \
            [dict create identity {nyx!~n@*.example.com} source grant role trusted effective_role trusted caps {} network libera channel #veles] \
            [dict create identity {*!*@bad.host} source entry role banned effective_role banned caps {} network "" channel ""]] \
        users [list \
            [dict create name wrath role owner identities [list irc:wrath telegram:77] source config] \
            [dict create name nyx role trusted identities [list {nyx!~n@*.example.com}] source grant]] \
        lockdown null]
    # …and the same view with the brake engaged. `text` is whatever
    # `lockdown::status` produced — the overlay renders the engine's
    # sentence rather than composing its own, so the two cannot drift.
    variable payload_locked [dict create \
        by tui:owner at 1757000000 reason {an incident} from_config 0 \
        text {lockdown: ON since 2026-09-09 by tui:owner — an incident. Connectors are refused; the local frontends (desktop/TUI/REPL) and cron are unaffected. Lift it with `/lockdown off` from a LOCAL frontend.}]
    variable payload_legacy [dict create \
        configured 0 \
        owners {} \
        default_role "" \
        role_catalog [list \
            [dict create name guest desc "unverified"]] \
        cap_catalog [list \
            [dict create name chat desc "start agent turns"]] \
        identities {}]
    variable locked 0
    proc get_json {path args} {
        variable legacy
        variable locked
        variable payload_curated
        variable payload_legacy
        variable payload_locked
        if {[regexp {/api/acl$} $path]} {
            if {$legacy} { return $payload_legacy }
            if {$locked ne "0"} {
                return [dict replace $payload_curated lockdown $payload_locked]
            }
            return $payload_curated
        }
        error "unexpected GET $path"
    }
    proc post_json {path body} {
        variable posts
        lappend posts [list $path $body]
        variable body_ok
        return $body_ok
    }
    # The real constructor, verbatim from api.tcl. It exists because
    # `_json_value` infers an ARRAY only from a multi-element list, so
    # a user with exactly ONE identity serialized as a JSON string and
    # /api/acl/user answered 422. Stubbed here rather than sourced so
    # the assertion below sees the same shape the wire does.
    proc json_raw {text} { return [list ::vapi::__json__ $text] }
    # Verbatim from api.tcl, and load-bearing for the promote dialog:
    # `_json_value` infers a JSON NUMBER from 1/0, and the endpoint's
    # serde `bool` fields answer 422 to a number. A stub that returned
    # the bare value would have let that through.
    proc json_bool {v} { return [json_raw [expr {$v ? "true" : "false"}]] }
    proc json_escape {s} {
        set out ""
        foreach ch [split $s ""] {
            switch -exact -- $ch {
                "\"" { append out {\"} }
                "\\" { append out {\\} }
                "\n" { append out {\n} }
                default { append out $ch }
            }
        }
        return $out
    }
    proc json_strings {items} {
        set parts {}
        foreach i $items { lappend parts "\"[json_escape $i]\"" }
        return [json_raw "\[[join $parts ", "]\]"]
    }
    proc reset {{flag 0}} {
        variable posts
        set posts {}
        variable body_ok
        set body_ok [dict create ok true]
        variable legacy
        set legacy $flag
    }
}

source [file join $_dir acl.tcl]

# ── build the real overlay ───────────────────────────────────────────
::vacl::build .ov_acl
update
set t .ov_acl.split.list.t
ok "the overlay maps and loads" {[winfo exists $t]}
ok "every identity row is painted" {
    [llength [$t children {}]] == 4
}
set first [$t item [lindex [$t children {}] 0] -values]
ok "rows carry identity role scope source" {
    [lindex $first 0] eq "irc:wrath" && [lindex $first 2] eq "everywhere"
    && [lindex $first 3] eq "config"
}
set scoped [$t item [lindex [$t children {}] 2] -values]
ok "a scoped row shows its scope" {
    [lindex $scoped 0] eq "nyx!~n@*.example.com"
    && [lindex $scoped 2] eq "libera / #veles"
}
set entry [$t item [lindex [$t children {}] 3] -values]
ok "a config entry row is painted too" {
    [lindex $entry 1] eq "banned" && [lindex $entry 3] eq "entry"
}
ok "users are listed" {
    [llength [.ov_acl.split.list.u children {}]] == 2
}
set urow [.ov_acl.split.list.u item [lindex [.ov_acl.split.list.u children {}] 1] -values]
ok "…with their identities joined" {
    [lindex $urow 0] eq "nyx" && [string first "nyx!~n@" [lindex $urow 2]] >= 0
}
set h .ov_acl.split.help
set htext [$h get 1.0 end-1c]
ok "the catalog explains roles" { [string first "Roles" $htext] >= 0 }
ok "…and capabilities"          { [string first "chat" $htext] >= 0 }
ok "…and the mask syntax"       { [string first "nick!user@host" $htext] >= 0 }
ok "owners are shown"           { [string first "irc:wrath" $htext] >= 0 }

# ── grant dialog ─────────────────────────────────────────────────────
::vacl::grant_ask .ov_acl
update
ok "grant dialog opens" {[winfo exists .acl_grant.f.identity]}
ok "…with scope fields" {
    [winfo exists .acl_grant.f.network] && [winfo exists .acl_grant.f.channel]
}
set ::vapi::posts {}
::vacl::grant_do .ov_acl .acl_grant
ok "an empty identity does not POST" {[llength $::vapi::posts] == 0}
.acl_grant.f.identity insert 0 "nyx!~n@*.example.com"
.acl_grant.f.network insert 0 "libera"
.acl_grant.f.channel insert 0 "#veles"
::vacl::grant_do .ov_acl .acl_grant
ok "a grant POSTs identity and role" {
    [llength $::vapi::posts] == 1
    && [dict get [lindex [lindex $::vapi::posts 0] 1] identity] eq "nyx!~n@*.example.com"
    && [dict get [lindex [lindex $::vapi::posts 0] 1] role] eq "trusted"
}
ok "…and its scope" {
    [dict get [lindex [lindex $::vapi::posts 0] 1] network] eq "libera"
    && [dict get [lindex [lindex $::vapi::posts 0] 1] channel] eq "#veles"
}
ok "…to the grant endpoint" {
    [lindex [lindex $::vapi::posts 0] 0] eq "/api/acl/grant"
}
ok "…and the dialog closes on success" {![winfo exists .acl_grant]}

# The IRC tab's entry point prefills identity and network.
::vacl::open_for "bob!~b@box" "oftc"
update
ok "open_for prefills the dialog" {
    [.acl_grant.f.identity get] eq "bob!~b@box"
    && [.acl_grant.f.network get] eq "oftc"
}
destroy .acl_grant

# ── revoke gating ────────────────────────────────────────────────────
# A config pin is not revokable through the API; the overlay must say
# so (and send nothing) instead of blaming the server's {ok:false}.
set ::vapi::posts {}
$t selection set [lindex [$t children {}] 0]   ;# irc:wrath — config
::vacl::revoke_selected .ov_acl
ok "revoking a config pin POSTs nothing" {[llength $::vapi::posts] == 0}
ok "…and says to edit config.toml" {
    [string first "config.toml" [.ov_acl.bar.status cget -text]] >= 0
}
# A config ENTRY is config too.
set ::vapi::posts {}
$t selection set [lindex [$t children {}] 3]   ;# *!*@bad.host — entry
::vacl::revoke_selected .ov_acl
ok "revoking a config entry POSTs nothing" {[llength $::vapi::posts] == 0}
# A grant row revokes through the API.
set ::vapi::posts {}
$t selection set [lindex [$t children {}] 1]   ;# irc:friend — grant
::vacl::revoke_selected .ov_acl
ok "revoking a grant POSTs to the revoke endpoint" {
    [lindex [lindex $::vapi::posts 0] 0] eq "/api/acl/revoke"
    && [dict get [lindex [lindex $::vapi::posts 0] 1] identity] eq "irc:friend"
}

# ── users ────────────────────────────────────────────────────────────
# A runtime user POSTs name/role/identities; a CONFIG user is read-only
# here, exactly like a config pin.
set ::vapi::posts {}
::vacl::user_ask .ov_acl
update
ok "the user dialog opens" {[winfo exists .acl_user.f.name]}
.acl_user.f.name insert 0 "pal"
.acl_user.f.ids insert end "irc:pal\npal!~p@*.pal.net\n"
::vacl::user_do .ov_acl .acl_user
ok "saving a user POSTs to /api/acl/user" {
    [lindex [lindex $::vapi::posts 0] 0] eq "/api/acl/user"
    && [dict get [lindex [lindex $::vapi::posts 0] 1] name] eq "pal"
}
# The identities go out as an explicit JSON ARRAY, not as a bare Tcl
# list: `_json_value` infers an array only from a MULTI-element list,
# so a user with exactly one identity serialized as a JSON string and
# the endpoint answered 422 — "adding an identity 422s" (PENDING.md).
ok "…with the identities typed as a JSON array" {
    [lindex [dict get [lindex [lindex $::vapi::posts 0] 1] identities] 0]
        eq "::vapi::__json__"
    && [lindex [dict get [lindex [lindex $::vapi::posts 0] 1] identities] 1]
        eq {["irc:pal", "pal!~p@*.pal.net"]}
}
ok "…and the dialog closes" {![winfo exists .acl_user]}

# THE 422: exactly one identity. The bare-list path made this a JSON
# string and the endpoint refused it.
set ::vapi::posts {}
::vacl::user_ask .ov_acl
update
.acl_user.f.name insert 0 "solo"
.acl_user.f.ids insert end "solo!~s@only.host\n"
::vacl::user_do .ov_acl .acl_user
ok "a user with ONE identity is still an array" {
    [lindex [dict get [lindex [lindex $::vapi::posts 0] 1] identities] 1]
        eq {["solo!~s@only.host"]}
}

set ::vapi::posts {}
set u .ov_acl.split.list.u
$u selection set [lindex [$u children {}] 1]   ;# nyx — runtime user
::vacl::user_delete_selected .ov_acl
ok "deleting a runtime user POSTs" {
    [lindex [lindex $::vapi::posts 0] 0] eq "/api/acl/user/delete"
    && [dict get [lindex [lindex $::vapi::posts 0] 1] name] eq "nyx"
}
set ::vapi::posts {}
$u selection set [lindex [$u children {}] 0]   ;# wrath — config user
::vacl::user_delete_selected .ov_acl
ok "a config user is not deletable here" {[llength $::vapi::posts] == 0}
ok "…and says where to edit" {
    [string first "acl.users" [.ov_acl.bar.status cget -text]] >= 0
}

# ── the emergency brake ──────────────────────────────────────────────
#
# The banner is the only place in the UI that says the connectors are
# being refused; a locked-down bot with no visible reason looks broken.
# And the LIFT button belongs here because the desktop is one of the
# three surfaces the engine will accept a lift from.
destroy .ov_acl
::vapi::reset 0
set ::vapi::locked 0
::vacl::build .ov_acl
update
ok "no banner while the brake is off" {![winfo ismapped .ov_acl.lock]}

set ::vapi::locked 1
::vacl::load .ov_acl
update
ok "an engaged lockdown is stated in the overlay" {
    [string first "lockdown: ON" [.ov_acl.lock.msg cget -text]] >= 0
}
ok "…with the engine's own sentence, not a second copy of it" {
    [string first "an incident" [.ov_acl.lock.msg cget -text]] >= 0
    && [string first "LOCAL frontend" [.ov_acl.lock.msg cget -text]] >= 0
}
ok "…and a lift button, because this surface may" {
    [winfo ismapped .ov_acl.lock.act]
}

set ::vapi::posts {}
::vacl::lockdown_set .ov_acl off
ok "lifting POSTs the action, and the engine decides" {
    [llength $::vapi::posts] == 1
    && [lindex [lindex $::vapi::posts 0] 0] eq "/api/lockdown"
    && [dict get [lindex [lindex $::vapi::posts 0] 1] action] eq "off"
}
set ::vapi::posts {}
::vacl::lockdown_set .ov_acl on
ok "engaging POSTs on, with a reason naming the surface" {
    [dict get [lindex [lindex $::vapi::posts 0] 1] action] eq "on"
    && [string first "desktop" \
            [dict get [lindex [lindex $::vapi::posts 0] 1] reason]] >= 0
}

# A config-declared lockdown cannot be lifted by ANY frontend, so
# offering a button that cannot work is worse than offering none.
set ::vapi::payload_locked \
    [dict replace $::vapi::payload_locked from_config 1 \
         text {lockdown: ON, declared in config.toml}]
::vacl::load .ov_acl
update
ok "a config-declared lockdown offers no lift button" {
    ![winfo ismapped .ov_acl.lock.act]
}
ok "…and says where it came from" {
    [string first "config.toml" [.ov_acl.lock.msg cget -text]] >= 0
}
set ::vapi::locked 0
destroy .ov_acl

# ── legacy mode note ─────────────────────────────────────────────────
destroy .ov_acl
::vapi::reset 1
::vacl::build .ov_acl
update
ok "legacy mode says so loudly" {
    [string first "legacy" [.ov_acl.split.help get 1.0 end-1c]] >= 0
}
destroy .ov_acl

# ── the surface exists where the user will look for it ───────────────
set _pal [open [file join $_dir palette.tcl] r]
set _palette [read $_pal]; close $_pal
ok "the palette offers ACL" {
    [string first {Access control (ACL)} $_palette] >= 0
    && [string first {::voverlay::open acl} $_palette] >= 0
}
set _ch [open [file join $_dir chat.tcl] r]
set _chat [read $_ch]; close $_ch
ok "/acl opens the overlay from the composer" {
    [string first {open acl} $_chat] >= 0
    && [string first {::voverlay::open acl} $_chat] >= 0
}
set _irc [open [file join $_dir irc.tcl] r]
set _ircsrc [read $_irc]; close $_irc
ok "/acl works from the IRC tab too" {
    [string first {acl    { _slash_acl }} $_ircsrc] >= 0
    && [string first {::vacl::open_for} $_ircsrc] >= 0
}
ok "the roster menu manages the ACL" {
    [string first {::virc::_roster_acl_grant trusted} $_ircsrc] >= 0
    && [string first {::virc::_roster_acl_grant banned} $_ircsrc] >= 0
}
# `admin` was accepted by the backend and missing from the menu, so the
# one role an operator most often hands out from a channel needed the
# overlay (PENDING.md).
ok "…including admin and guest" {
    [string first {::virc::_roster_acl_grant admin} $_ircsrc] >= 0
    && [string first {::virc::_roster_acl_grant guest} $_ircsrc] >= 0
}
set _ov [open [file join $_dir overlays.tcl] r]
set _overlays [read $_ov]; close $_ov
ok "the overlay switch delegates to the module" {
    [string first {acl     { ::vacl::build $w }} $_overlays] >= 0
}

# ── promote ─────────────────────────────────────────────────────────
#
# What the engine decides about a promote is pinned in Rust
# (acl_listing_tests: the standing verdicts, the refusals, the file that
# comes out the other side). What is pinned here is the pane's half: that
# it asks for the DRY RUN before it offers to write, that the options go
# out as JSON booleans, that the engine's report is what gets painted, and
# that "Write it" cannot mean a set of options nobody previewed.
set ::vapi::posts {}
# The legacy-mode section above tore the overlay down; build a curated one
# to hang the dialog off.
::vapi::reset
::vacl::build .ov_acl
update
set ::vapi::body_ok [dict create ok true wrote false \
    text {dry run — nothing written. would append 1 runtime grant.}]
ok "the overlay has a promote control" {[winfo exists .ov_acl.bar.promote]}
::vacl::promote_ask .ov_acl
update
ok "opening it previews instead of writing" {
    [llength $::vapi::posts] == 1
    && [lindex [lindex $::vapi::posts 0] 0] eq "/api/acl/promote"
    && [lindex [dict get [lindex [lindex $::vapi::posts 0] 1] dry_run] 1] eq "true"
}
ok "…with the options as JSON booleans, not 1/0" {
    [lindex [dict get [lindex [lindex $::vapi::posts 0] 1] users] 0] eq "::vapi::__json__"
    && [lindex [dict get [lindex [lindex $::vapi::posts 0] 1] users] 1] eq "true"
    && [lindex [dict get [lindex [lindex $::vapi::posts 0] 1] include_expiring] 1] eq "false"
}
ok "runtime users are folded in by default" {
    [lindex [dict get [lindex [lindex $::vapi::posts 0] 1] users] 1] eq "true"
}
ok "…and the engine's report is what gets painted" {
    [string first "dry run" [.acl_promote.f.report get 1.0 end-1c]] >= 0
}

# The write, with the preview matching the options on screen.
set ::vapi::posts {}
set ::vapi::body_ok [dict create ok true wrote true \
    text {promoted 1 runtime grant into /home/x/.config/veles/config.toml — in effect now}]
::vacl::promote_do .ov_acl .acl_promote
ok "Write it POSTs the same options without dry_run" {
    [llength $::vapi::posts] == 1
    && [lindex [dict get [lindex [lindex $::vapi::posts 0] 1] dry_run] 1] eq "false"
}
ok "…and reports what was written" {
    [string first "in effect now" [.acl_promote.f.report get 1.0 end-1c]] >= 0
}

# A refusal is a report, not an error to swallow: the pane paints it.
set ::vapi::posts {}
set ::vapi::body_ok [dict create ok false wrote false \
    text {refusing to promote: `[acl]` is not configured} \
    error {refusing to promote: `[acl]` is not configured}]
::vacl::promote_do .ov_acl .acl_promote
ok "a refusal is painted rather than swallowed" {
    [string first "refusing to promote" [.acl_promote.f.report get 1.0 end-1c]] >= 0
}

# Changing an option afterwards means the report on screen is not the
# write that would happen. The gate is the point: "Write it" has to mean
# what the operator just read.
set ::vapi::posts {}
set ::vapi::body_ok [dict create ok true wrote false text {dry run — nothing written.}]
set ::vacl::promote_prune 1
::vacl::promote_do .ov_acl .acl_promote
ok "Write it after an option changed previews again" {
    [llength $::vapi::posts] == 1
    && [lindex [dict get [lindex [lindex $::vapi::posts 0] 1] dry_run] 1] eq "true"
}
ok "…and says why it did not write" {
    [string first "again" [.acl_promote.f.report get 1.0 end-1c]] >= 0
}
set ::vapi::posts {}
set ::vapi::body_ok [dict create ok true wrote true text {promoted 1 runtime grant}]
::vacl::promote_do .ov_acl .acl_promote
ok "the second press writes, with the new option" {
    [llength $::vapi::posts] == 1
    && [lindex [dict get [lindex [lindex $::vapi::posts 0] 1] dry_run] 1] eq "false"
    && [lindex [dict get [lindex [lindex $::vapi::posts 0] 1] prune] 1] eq "true"
}
set ::vacl::promote_prune 0
set ::vapi::body_ok [dict create ok true]
destroy .acl_promote

# a11y: every control in the overlay has an accessible name (the
# source-walking gate from the a11y breadth pass). The window-level
# controls are built on `$w`, the dialogs' on `$g`.
set _acl [open [file join $_dir acl.tcl] r]
set _aclsrc [read $_acl]; close $_acl
set names 0
foreach want {bar.grant bar.revoke bar.promote bar.user_new bar.user_edit \
              bar.user_del bar.refresh bar.lock lock.act split.list.t \
              split.list.u split.help} {
    if {[string first "::va11y::name \$w.$want " $_aclsrc] >= 0} { incr names }
}
foreach want {f.identity f.network f.channel f.role} {
    if {[string first "::va11y::name \$g.$want " $_aclsrc] >= 0} { incr names }
}
ok "every overlay control is a11y-named ($names/16)" {$names == 16}
set unames 0
foreach want {f.name f.role f.ids} {
    if {[string first "::va11y::name \$g.$want " $_aclsrc] >= 0} { incr unames }
}
ok "the user dialog is a11y-named ($unames/3)" {$unames == 3}
set pnames 0
foreach want {f.report f.opts.users f.opts.expiring f.opts.prune f.b.preview \
              f.b.write} {
    if {[string first "::va11y::name \$g.$want " $_aclsrc] >= 0} { incr pnames }
}
ok "the promote dialog is a11y-named ($pnames/6)" {$pnames == 6}

# ── the stubs must match what they stand in for ──────────────────────
#
# `tab_order` was stubbed here as {w l} and is {widgets} in a11y.tcl, so
# this suite happily validated a call shape that did not exist,
# acl.tcl was written against the fiction, and ::vacl::build threw
# "wrong # args" for every real user who opened the ACL overlay. This
# suite is the only thing that calls build, so nothing else could have
# caught it. A stub that disagrees with what it stands in for is worse
# than no stub.
puts "\nthe stubs match the real procs"
set _fh [open [file join $_dir a11y.tcl] r]
set _a11y [read $_fh]
close $_fh

# The argument list a11y.tcl declares for one proc, or "" if absent.
# The pattern is BRACED: in a quoted string Tcl would read the [^...]
# character class as a command substitution.
proc real_args {src name} {
    foreach line [split $src \n] {
        if {[regexp {^proc ::va11y::([a-z_]+) \{([^\}]*)\}} $line -> p args]} {
            if {$p eq $name} { return [string trim $args] }
        }
    }
    return "<absent>"
}

foreach p {name tab_order} {
    set real [real_args $_a11y $p]
    set stub [info args ::va11y::$p]
    if {$real eq "<absent>"} {
        ok "a11y.tcl declares $p" {0}
        continue
    }
    if {[llength $stub] != [llength $real]} {
        puts "      ::va11y::$p — stub takes ($stub), a11y.tcl takes ($real)"
    }
    ok "::va11y::$p stub has the real arity" {[llength $stub] == [llength $real]}
}

puts "\nacltest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
