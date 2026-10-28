#!/usr/bin/env wish
# veles-desktop/lib/memfilestest.tcl — the Memory panel's "all files" view.
#
# memory.d is gone (2026-09-26): memory lives only in the database, so
# this view is how the desktop reaches a memory file at all. It drives the
# REAL panel (overlays.tcl), the REAL ::vmanage helpers and the REAL
# ::vapi JSON layer against a fake gateway stubbed at the WIRE —
# ::vapi::get and ::vapi::post, the two procs that would otherwise talk
# HTTP. Stubbing post_json or ::vmanage::post instead once hid a picker
# that never saved; here the body that reaches the "server" is the one
# the real encoder rendered, which is what "does a new file go out with
# base_sha null" is actually asking.
#
# Only the modal prompts (::vmanage::ask / ::vmanage::confirm — grab +
# tkwait) are replaced, with scripted answers.
#
# Run: scripts/one_tcl_test.sh veles-desktop/lib/memfilestest.tcl

set _dir [file dirname [file normalize [info script]]]

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir i18n.tcl]
::vi18n::init

namespace eval ::vstatus { proc set_msg {m} {} }
namespace eval ::vchat { variable stream_session "" ; proc on_error {m} {} ; proc on_finished {s} {} }
namespace eval ::va11y { proc name {w label} {} }
namespace eval ::vcoach { proc attach {w} {} ; proc attach_all {w} {} }
# Custom title-bar chrome: its own suite (titletest.tcl) covers it.
namespace eval ::vtitle { proc attach {args} {} }
namespace eval ::vplace { proc centered {w width height} { wm geometry $w ${width}x${height} } }

source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]
source [file join $_dir vtext.tcl]
source [file join $_dir manage.tcl]
source [file join $_dir overlays.tcl]

set passed 0
set failed 0
proc ok {name cond} {
    if {[catch { uplevel 1 [list expr $cond] } r]} {
        puts "  ✗ $name (error: $r)"
        incr ::failed
    } elseif {$r} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name"
        incr ::failed
    }
}

# ── the fake gateway, behind the wire ────────────────────────────────
#
# Paths -> {content rev}. A file's "sha" is its newest revision number,
# which is all the panel needs: an opaque token that changes when the
# file does. History keeps every revision, deletions included, exactly
# like memory_revisions.
namespace eval ::fake {
    variable files {}
    variable history {}
    variable rev 0
    variable bodies {}
    proc put {path content source} {
        variable files
        variable history
        variable rev
        incr rev
        if {$content eq ""} {
            dict unset files $path
        } else {
            dict set files $path [list $content $rev]
        }
        lappend history [list $rev $path $content $source]
    }
    proc sha {path} {
        variable files
        if {![dict exists $files $path]} { return "" }
        return "sha-[lindex [dict get $files $path] 1]"
    }
    proc q {s} { return "\"[::vapi::json_escape $s]\"" }
    proc decode {s} {
        set out ""
        set i 0
        while {$i < [string length $s]} {
            set c [string index $s $i]
            if {$c eq "%"} {
                append out [format %c 0x[string range $s $i+1 $i+2]]
                incr i 3
            } else {
                append out $c
                incr i
            }
        }
        return $out
    }
    proc param {path name} {
        if {[regexp "\[?&\]$name=(\[^&\]*)" $path -> v]} { return [decode $v] }
        return ""
    }
}

proc ::vapi::get {path {ms 0}} {
    set files $::fake::files
    if {[string match "/api/memory/files*" $path]} {
        set rows {}
        foreach p [lsort [dict keys $files]] {
            lassign [dict get $files $p] content rev
            lappend rows "{\"path\":[::fake::q $p],\"chars\":[string length $content],\"rev\":$rev,\"at\":\"2026-09-26 10:00:0$rev\",\"source\":\"agent\",\"sha256\":\"sha-$rev\"}"
        }
        return "{\"files\":\[[join $rows ,]\]}"
    }
    if {[string match "/api/memory/file?*" $path]} {
        set p [::fake::param $path path]
        if {![dict exists $files $p]} { error "HTTP 404: {\"error\":\"no such memory file: $p\"}" }
        lassign [dict get $files $p] content rev
        return "{\"path\":[::fake::q $p],\"rev\":$rev,\"sha256\":\"sha-$rev\",\"content\":[::fake::q $content]}"
    }
    if {[string match "/api/memory/log*" $path]} {
        set p [::fake::param $path path]
        set rows {}
        foreach h [lreverse $::fake::history] {
            lassign $h rev hp content source
            if {$hp ne $p} { continue }
            set del [expr {$content eq "" ? "true" : "false"}]
            lappend rows "{\"rev\":$rev,\"path\":[::fake::q $hp],\"source\":\"$source\",\"note\":\"\",\"at\":\"t$rev\",\"chars\":[string length $content],\"deleted\":$del}"
        }
        return "{\"log\":\[\],\"rows\":\[[join $rows ,]\]}"
    }
    if {[string match "/api/memory/diff*" $path]} {
        set new [::fake::param $path new]
        return "{\"diff\":[::fake::q "--- before\n+++ r$new\n+the r$new content\n"]}"
    }
    if {$path eq "/api/memory"} {
        return {{"memory":["a note"],"user":["who"],"usage":{"memory":6,"user":3},"limits":{"memory":2200,"user":1375}}}
    }
    error "HTTP 404: {\"error\":\"unrouted $path\"}"
}

proc ::vapi::post {path body {ms 0}} {
    lappend ::fake::bodies [list $path $body]
    set b [::json::json2dict $body]
    switch -- $path {
        /api/memory/file {
            set p [dict get $b path]
            set base [dict get $b base_sha]
            if {$base eq "null"} { set base "" }
            if {[::fake::sha $p] ne $base} {
                error "HTTP 409: {\"error\":\"$p changed since it was read\",\"current_sha\":null}"
            }
            ::fake::put $p [dict get $b content] desktop
            return "{\"path\":[::fake::q $p],\"recorded\":true,\"sha256\":\"[::fake::sha $p]\"}"
        }
        /api/memory/file/delete {
            ::fake::put [dict get $b path] "" delete
            return {{"deleted":"x"}}
        }
        /api/memory/file/move {
            set from [dict get $b from]
            set to [dict get $b to]
            ::fake::put $to [lindex [dict get $::fake::files $from] 0] move
            ::fake::put $from "" delete
            return {{"from":"a","to":"b"}}
        }
        /api/memory/restore {
            set p [dict get $b path]
            set r [dict get $b rev]
            foreach h $::fake::history {
                if {[lindex $h 0] == $r} { ::fake::put $p [lindex $h 2] rollback }
            }
            return {{"restored":1}}
        }
    }
    error "HTTP 404: {\"error\":\"unrouted $path\"}"
}

# Scripted answers for the two modal prompts.
set ::ask_answer ""
set ::confirm_answer 1
proc ::vmanage::ask {w title label {initial ""} {hint ""}} { return $::ask_answer }
proc ::vmanage::confirm {w title detail} { return $::confirm_answer }

proc last_body {} { return [lindex [lindex $::fake::bodies end] 1] }
proc status {} { return [.ovm.bar.status cget -text] }
proc editor {} { return [.ovm.files.edit.t get 1.0 end-1c] }
proc rows {} {
    set out {}
    foreach id [.ovm.files.list.t children {}] {
        lappend out [lindex [.ovm.files.list.t item $id -values] 0]
    }
    return $out
}
proc pick {path} {
    set i [lsearch -exact [rows] $path]
    .ovm.files.list.t selection set f$i
    update
}
proc type_over {text} {
    .ovm.files.edit.t delete 1.0 end
    .ovm.files.edit.t insert end $text
    .ovm.files.edit.t edit modified 1
}

::fake::put reference/alpha.md "alpha body\n" agent
::fake::put system/core.md "core note" agent

# ── the list ─────────────────────────────────────────────────────────
puts "\nthe files view lists every memory file"
set ::voverlay::memory_store files
::voverlay::_memory .ovm
update
ok "both files are listed" {[rows] eq {reference/alpha.md system/core.md}}
ok "the status line counts files and characters" {[string match "2 files, *characters" [status]]}
ok "the entries toolbar is not shown in this view" {[winfo manager .ovm.bar.ent] eq ""}

# ── open, edit, save ─────────────────────────────────────────────────
puts "\nopening and saving a file"
pick reference/alpha.md
ok "selecting a file loads its content into the editor" {[editor] eq "alpha body\n"}
ok "…and names it with its revision" {[string match "reference/alpha.md*r1*" [.ovm.files.edit.l cget -text]]}
type_over "alpha edited\n"
::voverlay::_mfile_save .ovm
update
ok "a save goes out with the sha the file was opened at" \
    {[string first {"base_sha": "sha-1"} [last_body]] >= 0}
ok "…and the server now holds the edit" \
    {[lindex [dict get $::fake::files reference/alpha.md] 0] eq "alpha edited\n"}
ok "the status line says it was saved" {[string match "saved reference/alpha.md" [status]]}
ok "the editor is clean after a save" {![.ovm.files.edit.t edit modified]}

# ── a write while it was open ────────────────────────────────────────
puts "\nthe agent writes the file while it is open"
::fake::put reference/alpha.md "agent rewrote it\n" agent
type_over "my stale edit\n"
::voverlay::_mfile_save .ovm
update
ok "the save is refused, and says why" {[string match "*changed since you opened it*" [status]]}
ok "…nothing was overwritten on the server" \
    {[lindex [dict get $::fake::files reference/alpha.md] 0] eq "agent rewrote it\n"}
ok "…and the typed text is still in the editor" {[editor] eq "my stale edit\n"}

# ── unsaved edits are not thrown away silently ───────────────────────
puts "\nunsaved edits"
set ::confirm_answer 0
pick system/core.md
ok "declining the discard keeps the open file" {$::voverlay::mfile_path eq "reference/alpha.md"}
ok "…and its text" {[editor] eq "my stale edit\n"}
::voverlay::_memory_mode .ovm
set ::voverlay::memory_store memory
::voverlay::_memory_mode .ovm
update
ok "leaving the view with unsaved edits asks, and stays on no" \
    {$::voverlay::memory_store eq "files" && [winfo manager .ovm.files] ne ""}
set ::confirm_answer 1
::voverlay::_mfile_reload .ovm
update
ok "Reload fetches the current content" {[editor] eq "agent rewrote it\n"}

# ── new ──────────────────────────────────────────────────────────────
puts "\ncreating a file"
set ::ask_answer reference/new.md
::voverlay::_mfile_new .ovm
update
ok "New opens an empty editor on the new path" \
    {[editor] eq "" && $::voverlay::mfile_path eq "reference/new.md"}
type_over "brand new\n"
::voverlay::_mfile_save .ovm
update
ok "a new file goes out with base_sha null" {[string first {"base_sha": null} [last_body]] >= 0}
ok "…and joins the list" {[lsearch -exact [rows] reference/new.md] >= 0}
set ::ask_answer reference/alpha.md
::voverlay::_mfile_new .ovm
type_over "clobber?\n"
::voverlay::_mfile_save .ovm
update
ok "New on a path that exists is refused as such" {[string match "*already exists*" [status]]}
ok "…and the existing file is untouched" \
    {[lindex [dict get $::fake::files reference/alpha.md] 0] eq "agent rewrote it\n"}
.ovm.files.edit.t edit modified 0

# ── rename ───────────────────────────────────────────────────────────
puts "\nrenaming"
pick reference/new.md
set ::ask_answer projects/new.md
::voverlay::_mfile_rename .ovm
update
ok "the file is listed under its new path, not the old one" \
    {[lsearch -exact [rows] projects/new.md] >= 0 && [lsearch -exact [rows] reference/new.md] < 0}
ok "…and is open there" {$::voverlay::mfile_path eq "projects/new.md" && [editor] eq "brand new\n"}

# ── delete, history, restore ─────────────────────────────────────────
puts "\ndelete, then restore from history"
::voverlay::_mfile_delete .ovm
update
ok "a deleted file leaves the list" {[lsearch -exact [rows] projects/new.md] < 0}
ok "…the path stays open so History can reach it" \
    {$::voverlay::mfile_path eq "projects/new.md" && [string match "*deleted*" [.ovm.files.edit.l cget -text]]}
::voverlay::_mfile_history .ovm
update
set _hist [.memory_file_history.split.list.t children {}]
ok "History lists the file's revisions, the deletion first" \
    {[llength $_hist] == 2 && [string match "*(deleted)*" [lindex [.memory_file_history.split.list.t item h0 -values] 3]]}
.memory_file_history.split.list.t selection set h1
update
ok "selecting a revision shows its diff" \
    {[string match "*+the r*content*" [.memory_file_history.split.diff get 1.0 end]]}
::voverlay::_mfile_restore .ovm .memory_file_history
update
ok "Restore brings the file back into the list" {[lsearch -exact [rows] projects/new.md] >= 0}
ok "…with its content" {[editor] eq "brand new\n"}
ok "…and closes the history window" {![winfo exists .memory_file_history]}

# ── the entries views still work ─────────────────────────────────────
puts "\nthe resident stores"
set ::voverlay::memory_store memory
::voverlay::_memory_mode .ovm
update
ok "switching back shows the entries editor and its toolbar" \
    {[winfo manager .ovm.split] ne "" && [winfo manager .ovm.bar.ent] ne "" && [winfo manager .ovm.files] eq ""}
ok "…filled from /api/memory" {[llength [.ovm.split.list.t children {}]] == 1}
ok "the store labels no longer name files that do not exist" \
    {[string first ".md" [.ovm.which.mem cget -text]] < 0 && [string first ".md" [.ovm.which.usr cget -text]] < 0}

catch { destroy .ovm }
puts "\nmemfilestest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
