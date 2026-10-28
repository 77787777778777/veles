# veles-desktop/lib/sandboxtest.tcl — the sandboxes overlay, driven
# like the user would use it: build the real window, feed it a
# real-shaped /api/sandboxes payload, use the context-menu actions,
# assert what got painted and what got POSTed.
#
# The engine half is pinned in Rust
# (`the_sandbox_endpoints_declare_assign_and_report`); this pins the UI
# half. It exists because `[sandbox]` was config-only and the desktop
# had no `user_envs` reference at all — the one surface with room for a
# table of containers and who lands in which could not see them.
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

# ── stubs, with the REAL signatures ──────────────────────────────────
namespace eval ::vctx { proc attach {w items} {} }
namespace eval ::voverlay { proc _header {w title} {} }
namespace eval ::vplace { proc centered {w width height args} {} }
namespace eval ::va11y {
    proc name {w t} {}
    proc tab_order {widgets} {}
}
namespace eval ::vapi {
    variable posts {}
    variable body_ok [dict create ok true]
    variable payload [dict create \
        admin_default_zone sandbox \
        local_default_zone host \
        max_concurrent_exec 4 \
        shared shared \
        defs [list \
            [dict create name quarantine image {localhost/veles-sandbox:1} \
                 memory_mb 512 network false note {guests only} \
                 created_by desktop created_at 1757000000] \
            [dict create name shared image null memory_mb null network null \
                 note null created_by null created_at null]] \
        binds [list \
            [dict create identity null network libera channel null platform null \
                 role guest sandbox quarantine key {*|libera|*|*|guest}] \
            [dict create identity {jimmy!*@*} network null channel {#veles} \
                 platform irc role null sandbox quarantine \
                 key {jimmy!*@*|*|#veles|irc|*}]] \
        live [list \
            [dict create key abc123 name quarantine container veles-env-abc123 \
                 running true workspace /home/u/.local/share/veles/envs/abc123] \
            [dict create key def456 name null container veles-env-def456 \
                 running false workspace /home/u/.local/share/veles/envs/def456]]]
    proc get_json {path args} {
        variable payload
        if {[string match */api/sandboxes $path]} { return $payload }
        error "unexpected GET $path"
    }
    proc post_json {path body} {
        variable posts
        lappend posts [list $path $body]
        variable body_ok
        return $body_ok
    }
    proc json_raw {text} { return [list ::vapi::__json__ $text] }
    proc json_bool {v} { return [json_raw [expr {$v ? "true" : "false"}]] }
    proc json_string {s} { return [json_raw "\"$s\""] }
}
# Confirmations: `--work` deletes people's files, so it asks. The stub
# answers whatever the test set.
set ::answer yes
proc tk_messageBox {args} { return $::answer }

source [file join $_dir sandbox.tcl]

proc sent_path {i} { return [lindex [lindex $::vapi::posts $i] 0] }
proc sent {i k} {
    set b [lindex [lindex $::vapi::posts $i] 1]
    if {![dict exists $b $k]} { return "" }
    set v [dict get $b $k]
    # Unwrap the typed-JSON pair the real constructors return.
    if {[llength $v] == 2 && [lindex $v 0] eq "::vapi::__json__"} {
        return [lindex $v 1]
    }
    return $v
}

# ── build and paint ──────────────────────────────────────────────────
::vsandbox::build .ov_sandbox
update
ok "the overlay maps and loads" {[winfo exists .ov_sandbox.split.d]}

set d .ov_sandbox.split.d
set rows [$d children {}]
ok "every declared box is painted" {[llength $rows] == 2}
set q [lindex $rows 0]
ok "…with its image and memory" {
    [lindex [$d item $q -values] 1] eq {localhost/veles-sandbox:1}
    && [lindex [$d item $q -values] 2] eq "512"
}
ok "…and network shown as on/off, not as a JSON word" \
    {[lindex [$d item $q -values] 3] eq "off"}
ok "…and its note" {[lindex [$d item $q -values] 4] eq "guests only"}
# A box with no declaration detail must not render "null" at the user.
set s [lindex $rows 1]
ok "an undeclared field is blank, never the word null" {
    [lindex [$d item $s -values] 1] eq ""
    && [lindex [$d item $s -values] 3] eq ""
}

set b .ov_sandbox.split.b
ok "every assignment is painted" {[llength [$b children {}]] == 2}
ok "…with its selectors spelled out" {
    [lindex [$b item {*|libera|*|*|guest} -values] 0] eq "network libera + role guest"
}
ok "…and only the selectors that are set" {
    [lindex [$b item {jimmy!*@*|*|#veles|irc|*} -values] 0]
        eq "identity jimmy!*@* + platform irc + channel #veles"
}
ok "…and the box it points at" {
    [lindex [$b item {*|libera|*|*|guest} -values] 1] eq "quarantine"
}

set l .ov_sandbox.split.l
ok "the live containers are listed" {[llength [$l children {}]] == 2}
ok "…with their run state" {
    [lindex [$l item abc123 -values] 1] eq "running"
    && [lindex [$l item def456 -values] 1] eq "stopped"
}
# An implicit box has no declared NAME, so the key is what identifies
# it — showing an empty cell would make the row unusable.
ok "…and a nameless one falls back to its key" \
    {[lindex [$l item def456 -values] 0] eq "def456"}
ok "the admin default zone is stated" \
    {[string first "sandbox" [.ov_sandbox.zone cget -text]] >= 0}
ok "…along with the fact that it is not optional below admin" \
    {[string first "not optional" [.ov_sandbox.zone cget -text]] >= 0}
# The desktop carries no principal, so `local_default_zone` is THIS
# window's own answer — an admin comparing the two needs both on screen.
ok "…and the local zone, which is this window's own" \
    {[string first "local (this window): host" [.ov_sandbox.zone cget -text]] >= 0}
# The bound on a SHARED box: the number that refuses a call when the
# room is busy is not something an operator should have to be refused
# to learn.
ok "…and how many execs one box runs at a time" \
    {[string first "exec-slots per box: 4" [.ov_sandbox.zone cget -text]] >= 0}

# ── purge: the container, and the files, are separate decisions ──────
set ::vapi::posts {}
$d selection set $q
::vsandbox::purge_selected 0 .ov_sandbox
ok "purge POSTs the sandbox NAME, not a container key" {
    [sent_path 0] eq "/api/sandboxes/purge"
    && [sent 0 name] eq "quarantine"
}
ok "…and keeps the workspace by default" {[sent 0 delete_work] eq "false"}

# `--work` deletes people's files. It asks first, and a NO stops it.
set ::vapi::posts {}
set ::answer no
::vsandbox::purge_selected 1 .ov_sandbox
ok "deleting a workspace asks first, and no means no" {[llength $::vapi::posts] == 0}
set ::answer yes
set ::vapi::posts {}
# A successful post RELOADS the overlay, which repaints the tree and so
# drops the selection — re-select before each action rather than
# assuming it survived.
$d selection set [lindex [$d children {}] 0]
::vsandbox::purge_selected 1 .ov_sandbox
ok "…and yes sends delete_work as a real boolean" {[sent 0 delete_work] eq "true"}

# ── drop is not purge ────────────────────────────────────────────────
set ::vapi::posts {}
$d selection set [lindex [$d children {}] 0]
::vsandbox::drop_selected .ov_sandbox
ok "forgetting a declaration hits the drop endpoint" \
    {[sent_path 0] eq "/api/sandboxes/drop" && [sent 0 name] eq "quarantine"}

# ── unassign sends back the same selectors ───────────────────────────
set ::vapi::posts {}
set b .ov_sandbox.split.b
$b selection set {jimmy!*@*|*|#veles|irc|*}
::vsandbox::unassign_selected .ov_sandbox
ok "unassign POSTs the assignment's own selectors" {
    [sent_path 0] eq "/api/sandboxes/unassign"
    && [sent 0 sandbox] eq "quarantine"
    && [sent 0 identity] eq {"jimmy!*@*"}
    && [sent 0 channel] eq {"#veles"}
}
# An absent selector must not become an empty-string one — the engine
# refuses a bind with no selectors, and a blank field is "no selector".
ok "…and nothing it did not have" {[sent 0 network] eq ""}

# ── nothing selected ─────────────────────────────────────────────────
set ::vapi::posts {}
$d selection remove [$d children {}]
::vsandbox::purge_selected 0 .ov_sandbox
ok "no selection posts nothing" {[llength $::vapi::posts] == 0}
ok "…and says so" \
    {[string first "select" [.ov_sandbox.bar.status cget -text]] >= 0}

# ── a dead gateway ───────────────────────────────────────────────────
proc ::vapi::get_json {path args} { error "connection refused" }
ok "an unreachable gateway is reported, not a traceback" {
    [::vsandbox::load .ov_sandbox] == 0
    && [string first "unavailable" [.ov_sandbox.bar.status cget -text]] >= 0
}

# ── the palette and the overlay switch know about it ─────────────────
set _p [open [file join $_dir palette.tcl] r]
set _psrc [read $_p]; close $_p
ok "the palette offers the overlay" \
    {[string first {::voverlay::open sandbox} $_psrc] >= 0}
set _o [open [file join $_dir overlays.tcl] r]
set _osrc [read $_o]; close $_o
ok "the overlay switch delegates to the module" \
    {[string first {::vsandbox::build $w} $_osrc] >= 0}

puts "\nsandboxtest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
