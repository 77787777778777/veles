#!/usr/bin/env wish
# Real Tk contribution workflow, mocked HTTP. No git, push, upload or PR.
set _dir [file dirname [file normalize [info script]]]
if {![info exists ::env(XVFB_RUNNING)] && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}
source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir vtext.tcl]
source [file join $_dir api.tcl]
source [file join $_dir manage.tcl]
source [file join $_dir overlays.tcl]

namespace eval ::vi18n { proc mc {s args} { return $s } }
namespace eval ::va11y { proc name {w d} {} }
namespace eval ::vcoach { proc attach {w} {}; proc attach_all {w} {} }
namespace eval ::vtitle { proc attach {w title args} {} }
namespace eval ::vstatus { proc set_msg {m} {} }
namespace eval ::voverlay { proc _header {w title} {} }
set ::prepare_conflict 0
namespace eval ::vapi {
    proc get_json {path} {
        lappend ::wire_gets $path
        if {$path eq "/api/plugins/demo-tool/contribution/files"} {
            return [dict create files [list \
                [dict create path icon.bin bytes 4 text false suggested false] \
                [dict create path nested/helper.tcl bytes 51 text true suggested true] \
                [dict create path plugin.tcl bytes 42 text true suggested true] \
                [dict create path state.json bytes 3 text true suggested false]]]
        }
        if {$path eq "/api/plugins/other-tool/contribution/files"} {
            return [dict create files [list \
                [dict create path plugin.tcl bytes 30 text true suggested true]]]
        }
        error "unexpected GET $path"
    }
    proc post {path body} {
        lappend ::wire_posts [list $path [::json::json2dict $body]]
        if {$path eq "/api/plugins/contribution/review"} {
            return {{"operation":"review","files":[{"extension":"demo-tool","path":"plugin.tcl","destination":"plugins/examples/demo-tool/plugin.tcl"}],"diff":"--- /dev/null\n+++ b/plugins/examples/demo-tool/plugin.tcl\n+proc demo {} {return ok}","diff_base":"uncommitted additions","checks":[],"findings":["manual review required"],"prepare_allowed":true,"next":"review then prepare"}}
        }
        if {$path eq "/api/plugins/contribution/checks"} {
            return {{"operation":"checks","checks":[{"path":"plugin.tcl","destination":"plugins/examples/demo-tool/plugin.tcl","status":"passed"}],"passed":true}}
        }
        if {$path eq "/api/plugins/contribution/prepare"} {
            if {$::prepare_conflict && ![dict exists [::json::json2dict $body] replace_existing]} {
                return {{"operation":"prepare","prepared":false,"conflict":true,"existing":["plugins/examples/demo-tool/plugin.tcl"],"next":"confirm replacing"}}
            }
            return {{"operation":"prepare","prepared":true,"origin":"https://example.invalid/owner/repo.git","branch":"feature/contribution","commit":"0123456789abcdef","worktree":"/tmp/local worktree/contribution","diff":"diff --git a/plugins/examples/demo-tool/plugin.tcl b/plugins/examples/demo-tool/plugin.tcl","next":"inspect then publish separately"}}
        }
        if {$path eq "/api/plugins/contribution/publish"} {
            return {{"operation":"publish","origin":"https://example.invalid/owner/repo.git","branch":"feature/contribution","commit":"0123456789abcdef","next":"open PR separately"}}
        }
        if {$path eq "/api/plugins/contribution/pr"} {
            return {{"operation":"pr","url":"https://example.invalid/pr/1"}}
        }
        error "unexpected request $path"
    }
}

set passed 0
set failed 0
proc ok {name condition} {
    if {[uplevel 1 [list expr $condition]]} {
        puts "  ✓ $name"; incr ::passed
    } else { puts "  ✗ $name"; incr ::failed }
}
proc endpoint_count {expected} {
    set count 0
    foreach row $::wire_posts {
        if {[lindex $row 0] eq $expected} { incr count }
    }
    return $count
}
proc mutation_count {} {
    return [expr {[endpoint_count /api/plugins/contribution/prepare] +
                  [endpoint_count /api/plugins/contribution/publish] +
                  [endpoint_count /api/plugins/contribution/pr]}]
}
proc last_wire {path} {
    for {set i [expr {[llength $::wire_posts]-1}]} {$i >= 0} {incr i -1} {
        set row [lindex $::wire_posts $i]
        if {[lindex $row 0] eq $path} { return [lindex $row 1] }
    }
    return {}
}
# {name {files}} pairs of a wire body's extensions array.
proc wire_extensions {body} {
    set out {}
    foreach ext [dict get $body extensions] {
        lappend out [list [dict get $ext name] [dict get $ext files]]
    }
    return $out
}
proc confirm_titles {} {
    set titles {}
    foreach row $::vmanage::confirms { lappend titles [lindex $row 0] }
    return $titles
}

set ::vmanage::confirms {}
set ::vmanage::confirm_answer 0
proc ::vmanage::confirm {w title detail} {
    lappend ::vmanage::confirms [list $title $detail]
    if {[info exists ::confirm_answers($title)]} { return $::confirm_answers($title) }
    return $::vmanage::confirm_answer
}
set ::vmanage::asked {}
proc ::vmanage::ask {w title label {initial ""} {hint ""}} {
    lappend ::vmanage::asked [list $title $initial]
    switch -- $title {
        {Contribution destination} { return https://example.invalid/owner/repo.git }
        {Contribution branch} { return feature/contribution }
        {Pull request repository} { return $initial }
        {Pull request base} { return $initial }
        {Pull request title} { return $initial }
        {Pull request description} {
            return {A multiword description with a quoted "detail"}
        }
        default { error "unexpected prompt: $title" }
    }
}
proc asked_initial {title} {
    foreach row $::vmanage::asked {
        if {[lindex $row 0] eq $title} { set found [lindex $row 1] }
    }
    return $found
}

set ::wire_posts {}
set ::wire_gets {}
::voverlay::_contribution_dialog {demo-tool other-tool} demo-tool
set c .plugin_contribute

ok "a new dialog cannot publish or open a PR" {
    [$c.bar.publish instate disabled] && [$c.bar.pr instate disabled]
}
ok "every user extension's files are listed" {
    $::wire_gets eq {/api/plugins/demo-tool/contribution/files /api/plugins/other-tool/contribution/files}
}
ok "only the selected extension's suggested sources start ticked" {
    [::voverlay::_contribution_selected] eq {{demo-tool nested/helper.tcl} {demo-tool plugin.tcl}}
}
ok "a binary file is offered disabled and unticked" {
    [$c.files.c.inner.f0 instate disabled] && !$::contribution_file(0)
}
ok "opening the dialog reviews once and mutates nothing" {
    [endpoint_count /api/plugins/contribution/review] == 1 && [mutation_count] == 0
}
set review [last_wire /api/plugins/contribution/review]
ok "review sends extensions and the repository directory" {
    [wire_extensions $review] eq {{demo-tool {nested/helper.tcl plugin.tcl}}} &&
    [dict get $review directory] eq {plugins/examples}
}
ok "review shows each file's repository path" {
    [string match "*demo-tool/plugin.tcl → plugins/examples/demo-tool/plugin.tcl*" [$c.preview get 1.0 end]]
}

$c.pick.all invoke
ok "select all takes every extension's suggested sources, not state files" {
    [::voverlay::_contribution_selected] eq {{demo-tool nested/helper.tcl} {demo-tool plugin.tcl} {other-tool plugin.tcl}}
}
ok "changing the selection invalidates the review" {
    $::contribution_review eq {} && [$c.bar.publish instate disabled]
}
::voverlay::_contribution_prepare $c
ok "an invalidated review cannot be prepared" {
    [mutation_count] == 0
}
::voverlay::_contribution_review $c
ok "one review carries both extensions" {
    [wire_extensions [last_wire /api/plugins/contribution/review]] eq
        {{demo-tool {nested/helper.tcl plugin.tcl}} {other-tool plugin.tcl}}
}

set ::vmanage::confirm_answer 0
::voverlay::_contribution_prepare $c
ok "declining preparation performs no git operation" {
    [mutation_count] == 0 && [confirm_titles] eq {{Prepare this branch and commit?}}
}
ok "the destination defaults to the veles upstream" {
    [asked_initial {Contribution destination}] eq {https://github.com/77787777778777/veles.git}
}
ok "the confirmation names every extension and the directory" {
    [string match "*demo-tool, other-tool*plugins/examples/<extension>/*" [lindex $::vmanage::confirms 0 1]]
}

# An upstream that already has one of the paths: nothing is replaced
# without its own confirmation.
set ::prepare_conflict 1
set ::vmanage::confirms {}
set ::vmanage::confirm_answer 1
set ::confirm_answers(Replace\ existing\ repository\ files?) 0
::voverlay::_contribution_prepare $c
ok "a conflict asks before replacing and declining stops there" {
    [confirm_titles] eq {{Prepare this branch and commit?} {Replace existing repository files?}} &&
    [endpoint_count /api/plugins/contribution/prepare] == 1 &&
    $::contribution_prepared eq {} && [$c.bar.publish instate disabled]
}
ok "the conflict confirmation lists the existing path" {
    [string match "*plugins/examples/demo-tool/plugin.tcl*" [lindex $::vmanage::confirms 1 1]]
}
set ::vmanage::confirms {}
set ::confirm_answers(Replace\ existing\ repository\ files?) 1
::voverlay::_contribution_prepare $c
set prepare [last_wire /api/plugins/contribution/prepare]
ok "confirming the replacement re-sends with replace_existing" {
    [endpoint_count /api/plugins/contribution/prepare] == 3 &&
    [dict get $prepare replace_existing] eq "true"
}
ok "prepare serializes destination, branch, directory and both extensions" {
    [dict get $prepare destination] eq {https://example.invalid/owner/repo.git} &&
    [dict get $prepare branch] eq {feature/contribution} &&
    [dict get $prepare directory] eq {plugins/examples} &&
    [wire_extensions $prepare] eq {{demo-tool {nested/helper.tcl plugin.tcl}} {other-tool plugin.tcl}}
}
ok "a prepared receipt enables push but not PR" {
    ![$c.bar.publish instate disabled] && [$c.bar.pr instate disabled]
}
ok "preparation did not push or open a PR" {
    [endpoint_count /api/plugins/contribution/publish] == 0 &&
    [endpoint_count /api/plugins/contribution/pr] == 0
}

set ::vmanage::confirms {}
set ::vmanage::confirm_answer 0
::voverlay::_contribution_publish $c
ok "declining push sends no request" {
    [confirm_titles] eq [list {Push this branch?}] &&
    [endpoint_count /api/plugins/contribution/publish] == 0
}
set ::vmanage::confirm_answer 1
::voverlay::_contribution_publish $c
ok "push is a separate confirmed action after prepare" {
    [endpoint_count /api/plugins/contribution/publish] == 1 &&
    [endpoint_count /api/plugins/contribution/pr] == 0
}
ok "publish sends a scalar worktree path" {
    [dict get [last_wire /api/plugins/contribution/publish] worktree] eq {/tmp/local worktree/contribution}
}
ok "a publish receipt enables the separate PR action" {
    ![$c.bar.pr instate disabled]
}
::voverlay::_contribution_pr $c
set pr [last_wire /api/plugins/contribution/pr]
ok "PR has a separate confirmation and no further push" {
    [endpoint_count /api/plugins/contribution/pr] == 1 &&
    [endpoint_count /api/plugins/contribution/publish] == 1
}
ok "the PR targets veles by default and names every extension" {
    [dict get $pr repo] eq {77787777778777/veles} &&
    [dict get $pr title] eq {Contribute extensions: demo-tool, other-tool} &&
    [dict get $pr base] eq {master}
}
ok "the PR body survives JSON serialization" {
    [dict get $pr body] eq {A multiword description with a quoted "detail"}
}
$c.files.c.inner.f1 invoke
ok "changing selection invalidates review, preparation and publication" {
    $::contribution_review eq {} && $::contribution_prepared eq {} &&
    $::contribution_published eq {} && [$c.bar.publish instate disabled] &&
    [$c.bar.pr instate disabled]
}

# dev-ext-3: Contribute in Manage extensions offers every USER extension
# (bundled ones are not the user's to contribute), pre-selecting the one
# highlighted in the list, or all of them when nothing user-owned is.
set ::dialog_calls {}
proc ::voverlay::_contribution_dialog {extensions {only ""}} {
    lappend ::dialog_calls [list $extensions $only]
}
set ::voverlay::plugins_data [dict create \
    zeta [dict create name zeta bundled 0] \
    anthropic [dict create name anthropic bundled 1] \
    alpha [dict create name alpha bundled 0]]
set ov .plugins_overlay_fixture
toplevel $ov
ttk::frame $ov.split
ttk::frame $ov.split.list
ttk::treeview $ov.split.list.t
foreach name {zeta anthropic alpha} { $ov.split.list.t insert {} end -id $name }
$ov.split.list.t selection set zeta
::voverlay::_plugin_contribute $ov
$ov.split.list.t selection set anthropic
::voverlay::_plugin_contribute $ov
ok "Contribute offers every user extension, never a bundled one" {
    $::dialog_calls eq {{{alpha zeta} zeta} {{alpha zeta} {}}}
}
destroy $ov

puts "\ncontributetest: $passed passed, $failed failed"
exit [expr {$failed ? 1 : 0}]
