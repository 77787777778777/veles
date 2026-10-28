# veles-desktop/lib/devtest.tcl — one editor, many buffers.
#
# USER: "hopefully the new script editor shares all the conveniences of
# the repl/dev pane".
#
# It could not, because it was not shareable: every helper in dev.tcl
# named `$::vpanes::R.dev.edit.body` directly, so LSP go-to-definition,
# references, signature help, hover docs, Tab completion, diagnostics,
# check and C-c C-c all existed exactly once — inside the Dev pane. Any
# other code buffer in the app got a bare Text widget.
#
# These tests pin the seam that fixed it: ::vdev::attach installs the
# apparatus on ANY widget, and every command acts on whichever editor is
# in focus. A regression that re-hardcodes the Dev pane's path goes red
# here rather than silently hollowing out every other editor.
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
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
namespace eval ::vstatus {
    variable last ""
    proc set_msg {m} { set ::vstatus::last $m }
}
namespace eval ::vsyn {
    variable calls 0
    proc highlight {w} { incr ::vsyn::calls }
    proc diagnostics {w rows} {}
    proc hide_tip {} {}
}
namespace eval ::vlsp {
    variable up 0
    variable doc_path ""
    variable started {}
    proc running {} { return $::vlsp::up }
    # The Dev pane compares this against its own buffer path before
    # syncing — a buffer that is not the open document must never
    # didChange over it. The tests set it like the real `start` would.
    proc doc_path {} { variable doc_path ; return $doc_path }
    # Record every (re)open and move doc_path, like the real start does
    # once the gateway acks — so a test can assert buf_show re-points us.
    proc start {path text cb} {
        lappend ::vlsp::started $path
        set ::vlsp::doc_path $path
        return 0
    }
    proc sync {text} { set ::vlsp::synced $text }
    # Capabilities + a drivable definition request, for the M-. tests.
    variable caps {definitionProvider 1}
    proc supports {name} {
        variable caps
        return [expr {[dict exists $caps $name] && [dict get $caps $name]}]
    }
    variable def_calls 0
    variable def_result {}
    proc definition_at {line char cb} {
        incr ::vlsp::def_calls
        uplevel #0 [list $cb $::vlsp::def_result ""]
    }
    proc definition_target {r} { return $r }
}
namespace eval ::vemacs { proc install {w} { set ::emacs_installed 1 } }
namespace eval ::vapi {
    variable files
    variable last_url ""
    variable posts {}
    variable mirror_root ""
    # Path-aware, because the tab tests open several files and each tab
    # has to end up holding a DIFFERENT buffer — a stub that returns the
    # same empty string for everything would pass a broken switcher.
    proc get_json {url} {
        variable last_url $url
        if {$url eq "/api/plugins/slopdrop/status"} {
            return [dict create root $::vapi::mirror_root]
        }
        if {[regexp {path=([^&]*)} $url -> path]} {
            if {[info exists ::vapi::files($path)]} {
                return [dict create text $::vapi::files($path)]
            }
            return [dict create text "# contents of $path\n"]
        }
        return [dict create text ""]
    }
    proc post {path body} {
        variable posts
        lappend posts [list $path $body]
        # Raw JSON strings, like the real /api transport returns — the
        # callers hand them to ::json::json2dict.
        if {$path eq "/api/fs/write"} { return "{\"bytes\":12}" }
        if {$path eq "/api/plugins/slopdrop/save"} {
            return "{\"ok\":true,\"message\":\"deployed (stub)\"}"
        }
        if {$path eq "/api/plugins/slopdrop/sync"} {
            return "{\"ok\":true,\"procs\":3,\"vars\":1}"
        }
        return "{}"
    }
    proc post_json {path body} { return [dict create] }
    proc json_escape {s} { return $s }
}
namespace eval ::va11y { proc name {w t} {} }
namespace eval ::json {
    # Two shapes matter for the save paths: the fs write reply
    # ({"bytes":N}) and the slopdrop deploy reply ({"ok":true,
    # "message":…}). Everything else stays the empty dict the callers
    # already wrote their refusal checks against.
    proc json2dict {s} {
        # Only what the mirror save paths need: the fs write reply
        # ({"bytes":N}) and the slopdrop deploy reply ({"ok":true,
        # "message":…}). The REAL parser would be sourced here if
        # json.tcl were local; free-text JSON that these suites feed
        # through other paths still lands in the empty-dict refusal
        # arms they were written against.
        if {[string match {*"ok":true*} $s] && [string match {*"message"*} $s]} {
            return [dict create ok 1 message "deployed (stub)"]
        }
        if {[string match {*"bytes"*} $s]} {
            return [dict create bytes 12]
        }
        return [dict create]
    }
}
namespace eval ::vpanes { variable R .devroot }

# i18n first: dev.tcl labels its live-patching control through
# ::vi18n::mc, and an unresolved `mc` is a hard error at
# widget-build time, not a fallback. init resolves the identity
# catalog, which is what a suite wants.
source [file join $_dir i18n.tcl]
::vi18n::init
source [file join $_dir dev.tcl]

wm geometry . 800x600+0+0
catch { wm attributes . -topmost 0 }

# A stand-in for the Dev pane's editor, so `editor_widget`'s fallback
# has something real to point at.
frame .devroot
frame .devroot.dev
frame .devroot.dev.repl
frame .devroot.dev.edit
text .devroot.dev.repl.out
text .devroot.dev.edit.body
label .devroot.dev.edit.status -text ""
pack .devroot.dev.edit.body
update idletasks

puts "attach installs the apparatus on any widget"
toplevel .other
text .other.body
label .other.msg -text ""
pack .other.body
update idletasks

set ::emacs_installed 0
::vdev::attach .other.body -path /tmp/hook.tcl -status .other.msg -save {set ::saved 1}
update

foreach {seq what} {
    <Alt-period>            "go to definition"
    <Alt-question>          "find references"
    <Tab>                   "completion at point"
    <Motion>                "hover docs"
    <Control-x><Control-s>  "save"
    <Control-c><Control-c>  "source into the live app"
    <KeyRelease>            "live re-highlight"
} {
    ok "$what is bound on an attached editor" {[bind .other.body $seq] ne ""}
}
ok "…and emacs editing came with it" {$::emacs_installed}

puts "\ncommands follow the editor in focus"
ok "attach makes it the current editor" {[::vdev::editor_widget] eq ".other.body"}
ok "…and knows what file it is"        {[::vdev::editor_path] eq "/tmp/hook.tcl"}

::vdev::_editor_focus .devroot.dev.edit.body
ok "focus moves the target"            {[::vdev::editor_widget] eq ".devroot.dev.edit.body"}
ok "…and the path with it"             {[::vdev::editor_path] eq ""}
::vdev::_editor_focus .other.body

puts "\nthe status line goes to the editor's own window"
::vdev::editor_status "hello"
ok "an attached editor writes into its own label" \
    {[.other.msg cget -text] eq "hello"}
ok "…and not into the Dev pane's"      {[.devroot.dev.edit.status cget -text] eq ""}

::vdev::_editor_focus .devroot.dev.edit.body
::vdev::attach .devroot.dev.edit.body -status .devroot.dev.edit.status
::vdev::editor_status "dev says this"
ok "the Dev pane still writes into its own" \
    {[.devroot.dev.edit.status cget -text] eq "dev says this"}
ok "…without disturbing the other editor" {[.other.msg cget -text] eq "hello"}

puts "\ngo-to-definition: capability gate, and same-file is per editor"
set ::vlsp::up 1
# The docked pane and the focused editor point at DIFFERENT files.
set ::vdev::current_file /tmp/docked.tcl
::vdev::attach .other.body -path /tmp/hook.tcl -status .other.msg
::vdev::_editor_focus .other.body
.other.body delete 1.0 end
.other.body insert end "line one\nline two\nline three\n"

# No definitionProvider -> a clear message, and no request goes out.
set ::vlsp::caps {definitionProvider 0}
set ::vlsp::def_calls 0
::vdev::_goto_def
ok "M-. without definitionProvider says so" \
    {[string match "*does not provide*" [.other.msg cget -text]]}
ok "…and sends nothing to the server"        {$::vlsp::def_calls == 0}

# A target in the FOCUSED editor's own file is a same-file jump — it must
# move the cursor, not open /tmp/hook.tcl into the docked Dev pane.
set ::vlsp::caps {definitionProvider 1}
set before [llength [::vdev::buf_list]]
set ::vlsp::def_result [list file:///tmp/hook.tcl 2 3]
::vdev::_goto_def
update
ok "a def in the focused editor's file jumps in place" \
    {[.other.body index insert] eq "3.3"}
ok "…and does not spawn a Dev-pane tab for it" \
    {[llength [::vdev::buf_list]] == $::before}
::vdev::_editor_focus .devroot.dev.edit.body

# An editor with no status label of its own must not shout into
# somebody else's window: it falls back to the app status bar.
toplevel .bare
text .bare.body
::vdev::attach .bare.body -path /tmp/bare.tcl
::vdev::editor_status "no label here"
ok "a status-less editor uses the app status bar" \
    {$::vstatus::last eq "no label here"}
ok "…and leaves the Dev pane alone" \
    {[.devroot.dev.edit.status cget -text] eq "dev says this"}

puts "\na mirror proc saves through the interpreter, not as a file"
# The mirror root is discovered once from GET /api/plugins/slopdrop/status; the stub
# serves whatever mirror_root holds, and the cache is per-process.
set ::vapi::mirror_root "/tmp/xdg/veles/slopdrop/local"
set ::vdev::_mirror_root ""
set ::vdev::_mirror_root_fetched 0
set mirror_file "/tmp/xdg/veles/slopdrop/local/procs/nitterize.tcl"
ok "the mirror-root probe answers and caches" \
    {[::vdev::_slopdrop_mirror_root] eq "/tmp/xdg/veles/slopdrop/local"}

puts "\nthe mirror router"
ok "a path under the root is a mirror file" {[::vdev::_slopdrop_is_mirror $mirror_file]}
ok "a path outside it is not" {![::vdev::_slopdrop_is_mirror "/tmp/elsewhere/x.tcl"]}
ok "…and neither is an empty path" {![::vdev::_slopdrop_is_mirror ""]}

puts "\neditor_save routes the mirror path to the deploy endpoint"
# A dedicated editor for the mirror tests: borrowing .other.body would
# clobber the -save override the attach test armed, which a LATER test
# still asserts on.
toplevel .mirror
text .mirror.body
label .mirror.msg -text ""
pack .mirror.body
update idletasks
::vdev::attach .mirror.body -path /tmp/mirror-scratch.tcl -status .mirror.msg
::vdev::_editor_focus .mirror.body
set ::vdev::editor_file(.mirror.body) $mirror_file
set ::vapi::posts {}
# The editor holds text; saving deploys it.
.mirror.body delete 1.0 end
.mirror.body insert 1.0 "proc nitterize {u} {puts NEW}"
::vdev::set_editor_dirty 1
::vdev::editor_save
ok "the save went to /api/plugins/slopdrop/save" \
    {[lindex [lindex [lindex $::vapi::posts 0] 0] 0] eq "/api/plugins/slopdrop/save"}
ok "the body names the mirror path" \
    {[string match "*procs/nitterize.tcl*" [string map {\" {}} [lindex [lindex $::vapi::posts 0] 1]]]}
ok "the buffer un-dirties on a deployment" {![::vdev::editor_dirty]}

puts "\nwithout a mirror the old fs path is untouched"
set ::vapi::mirror_root ""
set ::vdev::_mirror_root ""
set ::vdev::_mirror_root_fetched 0
set ::vapi::posts {}
set ::vdev::editor_file(.mirror.body) "/tmp/plain.tcl"
.mirror.body delete 1.0 end
.mirror.body insert 1.0 "proc plain {} {}"
::vdev::set_editor_dirty 1
::vdev::editor_save
ok "…and a plain file still saves via /api/fs/write" \
    {[string match "/api/fs/write*" [lindex [lindex $::vapi::posts 0] 0]]}

# A stale file is NOT saved, and the source action must not hide that
# refusal by sourcing the dirty buffer anyway. Preserve the real fixture
# procs so the rest of this suite continues to use their ordinary paths.
rename ::vapi::post ::vapi::post_before_stale_fixture
proc ::vapi::post {path body} {
    lappend ::vapi::posts [list $path $body]
    if {$path eq "/api/fs/write"} {
        return {{"error":"file changed since it was loaded; your buffer is still unsaved","conflict":true}}
    }
    return "{}"
}
rename ::json::json2dict ::json::json2dict_before_stale_fixture
proc ::json::json2dict {s} {
    if {[string match {*"conflict":true*} $s]} {
        return [dict create error "file changed since it was loaded; your buffer is still unsaved" conflict 1]
    }
    return [dict create bytes 12]
}
rename ::vdev::eval_in ::vdev::eval_in_before_stale_fixture
set ::vdev::stale_eval_calls 0
proc ::vdev::eval_in {target text} {
    incr ::vdev::stale_eval_calls
    return [list 1 sourced]
}
set ::vdev::editor_sha(.mirror.body) old-base
::vdev::set_editor_dirty 1
::vdev::editor_source_app
ok "a stale save does not get sourced past its conflict" {
    $::vdev::stale_eval_calls == 0
    && [::vdev::editor_dirty]
    && [string match "*changed since*" [.mirror.msg cget -text]]
}
rename ::vapi::post {}
rename ::vapi::post_before_stale_fixture ::vapi::post
rename ::json::json2dict {}
rename ::json::json2dict_before_stale_fixture ::json::json2dict
rename ::vdev::eval_in {}
rename ::vdev::eval_in_before_stale_fixture ::vdev::eval_in
destroy .mirror
::vdev::_editor_focus .other.body

puts "\ndirty state is per buffer"
::vdev::_editor_focus .other.body
::vdev::set_editor_dirty 1
ok "the focused editor is dirty"       {[::vdev::editor_dirty]}
::vdev::_editor_focus .bare.body
ok "…and its neighbour is not"         {![::vdev::editor_dirty]}
::vdev::_editor_focus .other.body
ok "…and the first one still is"       {[::vdev::editor_dirty]}
ok "a dirty buffer says so in its status" \
    {[string match "*•*" [::vdev::editor_status_base]]}

puts "\na buffer that is not a file saves its own way"
# The failover hook goes through an endpoint that SOURCES it before
# installing it, so C-x C-s cannot be a plain write to disk.
set ::saved 0
::vdev::_editor_focus .other.body
::vdev::editor_save
ok "-save runs instead of writing the file" {$::saved}

# Without -save the normal path runs — and with no path it says so
# rather than writing somewhere arbitrary.
::vdev::_editor_focus .devroot.dev.edit.body
set ::vdev::current_file ""
::vdev::editor_save
ok "a pathless buffer refuses to save" \
    {[string match "*no path*" [.devroot.dev.edit.status cget -text]]}

puts "\na destroyed editor stops being the target"
::vdev::_editor_focus .other.body
destroy .other
update
ok "the target falls back to the Dev pane" \
    {[::vdev::editor_widget] eq ".devroot.dev.edit.body"}
ok "…and its state went with it" \
    {![info exists ::vdev::editor_file(.other.body)]}

puts "\ntyping re-highlights the editor in focus"
set ::vsyn::calls 0
set ::vlsp::up 0
set ::vlsp::doc_path ""
::vdev::_editor_focus .bare.body
::vdev::_edit_sync_now
ok "with no LSP, the regex highlighter runs" {$::vsyn::calls == 1}
set ::vlsp::up 1
set ::vlsp::doc_path /tmp/bare.tcl
set ::vlsp::synced ""
.bare.body insert end "proc x {} {}"
::vdev::_edit_sync_now
ok "with an LSP, the buffer is synced to it" \
    {[string match "*proc x*" $::vlsp::synced]}
ok "…and not re-scanned by the fallback" {$::vsyn::calls == 1}

# A buffer that is NOT the open document must never sync over it: a
# fresh scratch (or another file's editor) used to didChange its own
# text — even empty text — against the previous document's URI,
# blanking the server's copy mid-session.
set ::vlsp::doc_path /tmp/somewhere-else.tcl
set ::vlsp::synced ""
::vdev::_edit_sync_now
ok "a different file falls back to the regex highlighter" {$::vsyn::calls == 2}
ok "…instead of syncing over the open document" {$::vlsp::synced eq ""}
set ::vlsp::doc_path /tmp/bare.tcl

catch { destroy .bare }

puts "\npop out / re-attach the editor"
::vdev::_editor_focus .devroot.dev.edit.body
.devroot.dev.edit.body delete 1.0 end
.devroot.dev.edit.body insert end "the buffer content"
::vdev::pop_out
ok "popping out moves the editor to a float window" {[winfo exists .vdevfloat.body]}
ok "…with its content carried over" \
    {[.vdevfloat.body get 1.0 end-1c] eq "the buffer content"}
ok "…and the docked editor body is gone" {![winfo exists .devroot.dev.edit.body]}
::vdev::reattach
ok "re-attaching rebuilds the docked editor" {[winfo exists .devroot.dev.edit.body]}
ok "…with its content restored" \
    {[.devroot.dev.edit.body get 1.0 end-1c] eq "the buffer content"}
ok "…and the float window is gone" {![winfo exists .vdevfloat]}
catch { destroy .vdevfloat }

# ── the tab strip ───────────────────────────────────────────────────
#
# USER: "make the dev editor detachable and re-attachable" — then "Tab
# strip would also be nice."
#
# Everything above drives the SINGLE-editor fallback, which is what a
# bare ::vdev::attach still gets. From here the real Dev pane is built,
# so `buf_ready` is true and the buffer model is live. The regression
# these guard against is the one the pane had all along: opening a
# second file threw the first buffer away, unsaved edits with it.

puts "\na tab strip, with a buffer per file"
destroy .devroot.dev
::vpanes::_build_dev .devroot.dev
# Mapped for real, with a real width: the strip's overflow behaviour is
# geometry, and an unmapped widget reports a width of 1.
pack .devroot -fill both -expand 1
pack .devroot.dev -fill both -expand 1
wm geometry . 900x600+0+0
update

proc tabnames {} {
    set out {}
    foreach id [::vdev::buf_list] { lappend out [::vdev::buf_widget $id] }
    return $out
}
proc tabstyle {i} {
    set c [lindex [winfo children [::vdev::buf_strip]] $i]
    if {$c eq "" || ![winfo exists $c.b]} { return "<none>" }
    return [$c.b cget -style]
}
# Is the tab you are editing actually on screen? That is the claim —
# not any particular scroll fraction, which the trailing "+" button
# makes impossible to state as "scrolled fully right".
proc tabvisible {} {
    set c [::vdev::buf_strip_canvas]
    set f [::vdev::buf_strip]
    set i [lsearch -exact [::vdev::buf_list] [::vdev::buf_current]]
    if {$i < 0} { return 0 }
    set tab $f.t[expr {$i + 1}]
    if {![winfo exists $tab]} { return 0 }
    set wid [winfo reqwidth $f]
    set view [winfo width $c]
    set left [expr {[lindex [$c xview] 0] * $wid}]
    set x [winfo x $tab]
    set right [expr {$x + [winfo reqwidth $tab]}]
    return [expr {$x >= $left - 2 && $right <= $left + $view + 2}]
}
proc bufins {w text} {
    if {$w eq "" || ![winfo exists $w]} { return 0 }
    $w insert end $text
    return 1
}
proc bufget {w} {
    if {$w eq "" || ![winfo exists $w]} { return "<gone>" }
    return [$w get 1.0 end]
}
proc striptext {} {
    set out {}
    foreach c [winfo children [::vdev::buf_strip]] {
        if {[winfo exists $c.b]} { lappend out [$c.b cget -text] }
    }
    return $out
}

ok "the pane opens with one buffer"      {[llength [::vdev::buf_list]] == 1}
ok "…which is the editor commands act on" \
    {[::vdev::editor_widget] eq [::vdev::buf_widget [::vdev::buf_current]]}
ok "…and the strip shows it"             {[llength [striptext]] == 1}
ok "…named for an empty buffer"          {[string match "*scratch*" [lindex [striptext] 0]]}

set ::vapi::files(/tmp/one.tcl) "proc one {} { return 1 }"
set ::vapi::files(/tmp/two.tcl) "proc two {} { return 2 }"

set ::vdev::current_file /tmp/one.tcl
::vdev::editor_open
update
ok "opening a file fills the empty buffer" {[llength [::vdev::buf_list]] == 1}
ok "…and it reads the whole-file endpoint, not the preview head" \
    {[string match "/api/fs/file*" $::vapi::last_url]}
ok "…and the tab takes its name"          {[lindex [striptext] 0] eq "one.tcl"}
ok "…and the buffer holds the file"       {[string match "*proc one*" [bufget [::vdev::editor_widget]]]}

set ::vdev::current_file /tmp/two.tcl
::vdev::editor_open
update
ok "a SECOND file gets its own tab"       {[llength [::vdev::buf_list]] == 2}
ok "…and the first buffer survives it" \
    {[string match "*proc one*" [bufget [::vdev::buf_widget [lindex [::vdev::buf_list] 0]]]]}
ok "…while the second is what you see"    {[string match "*proc two*" [bufget [::vdev::editor_widget]]]}
ok "…and the strip lists both"            {[striptext] eq {one.tcl two.tcl}}

# Re-opening must not stack a second copy: two buffers on one file race,
# and whichever saved last wins silently.
set first [lindex [::vdev::buf_list] 0]
set ::vdev::current_file /tmp/one.tcl
::vdev::editor_open
update
ok "re-opening a file focuses its tab"    {[llength [::vdev::buf_list]] == 2}
ok "…rather than opening it twice"        {[::vdev::buf_current] eq $::first}

puts "\nswitching tabs moves everything with them"
set second [lindex [::vdev::buf_list] 1]
::vdev::buf_show $second
update
ok "the shown buffer is the target"       {[::vdev::buf_current] eq $::second}
ok "…commands follow it"                  {[::vdev::editor_widget] eq [::vdev::buf_widget $::second]}
ok "…and the path entry follows too"      {$::vdev::current_file eq "/tmp/two.tcl"}
ok "…only one buffer is packed at a time" \
    {[llength [pack slaves [::vdev::buf_stack]]] == 1}

::vdev::buf_next 1
ok "C-x <Right> wraps to the first"       {[::vdev::buf_current] eq $::first}
::vdev::buf_next -1
ok "…and C-x <Left> comes back"           {[::vdev::buf_current] eq $::second}
ok "the strip marks which tab is active" {[tabstyle 1] eq "DevTabOn.TButton"}
ok "…and only that one"                   {[tabstyle 0] eq "DevTab.TButton"}

puts "\nswitching tabs re-points the language server"
# Without this, ::vlsp::doc_path keeps naming the last-opened file and
# hover / M-. / rename / references answer for the wrong document at the
# new buffer's cursor — silently.
set ::vlsp::started {}
::vdev::buf_show $first
update
ok "showing a tab re-opens its file on the LSP" \
    {[lindex $::vlsp::started end] eq "/tmp/one.tcl" && [::vlsp::doc_path] eq "/tmp/one.tcl"}
set ::vlsp::started {}
::vdev::buf_show $first
ok "…but re-showing the tab already open on the LSP does nothing" {$::vlsp::started eq {}}
set ::vlsp::started {}
::vdev::buf_show $second
ok "…and switching back re-points again"  {[lindex $::vlsp::started end] eq "/tmp/two.tcl"}

puts "\ndirty state is per tab, and visible in the strip"
bufins [::vdev::buf_widget $second] "\nmore"
::vdev::_editor_focus [::vdev::buf_widget $second]
::vdev::set_editor_dirty 1
update
ok "a dirty tab is marked"                {[string match "*•*" [lindex [striptext] 1]]}
ok "…and its neighbour is not"            {![string match "*•*" [lindex [striptext] 0]]}

puts "\nclosing a tab"
# A dirty buffer refuses once and says so — the emacs bargain, not a
# modal that steals focus and not a silent discard.
set n [llength [::vdev::buf_list]]
::vdev::buf_close $second
ok "a dirty tab refuses to close"         {[llength [::vdev::buf_list]] == $::n}
ok "…and says why"                        {[string match "*unsaved*" [.devroot.dev.edit.status cget -text]]}
::vdev::buf_close $second
ok "…but closes when asked again"         {[llength [::vdev::buf_list]] == $::n - 1}
ok "…and the survivor is showing"         {[::vdev::buf_current] eq $::first}
ok "…packed, not just current"            {[llength [pack slaves [::vdev::buf_stack]]] == 1}

# The pane must never be left with no editor: every command would have
# nothing to act on and it would just look broken.
::vdev::buf_close $first
ok "closing the last tab opens a fresh one" \
    {[llength [::vdev::buf_list]] == 1 && [::vdev::buf_current] ne $::first}
ok "…which is empty"                        {[string trim [bufget [::vdev::editor_widget]]] eq ""}
ok "…and has no file"                       {[::vdev::buf_path_of [::vdev::buf_current]] eq ""}

puts "\nany tab pops out, and the rest stay put"
set ::vdev::current_file /tmp/one.tcl
::vdev::editor_open
set ::vdev::current_file /tmp/two.tcl
::vdev::editor_open
update
set a [lindex [::vdev::buf_list] 0]
set b [lindex [::vdev::buf_list] 1]
::vdev::buf_pop $b
update
ok "the popped tab has its own window"    {[winfo exists [::vdev::_buf_float_name $::b]]}
ok "…carrying its content"                {[string match "*proc two*" [bufget [::vdev::buf_widget $::b]]]}
ok "…and its path"                        {[::vdev::buf_path_of $::b] eq "/tmp/two.tcl"}
ok "…with the editing apparatus attached" {[bind [::vdev::buf_widget $::b] <Alt-period>] ne ""}
ok "…and it is still a tab in the strip"  {[llength [::vdev::buf_list]] == 2}
ok "…marked as detached"                  {[string match "*⧉*" [lindex [striptext] 1]]}
ok "the other tab stayed docked"          {![::vdev::buf_detached $::a]}
ok "…and is what the pane now shows"      {[::vdev::buf_current] eq $::a}
ok "…still holding its own file"          {[string match "*proc one*" [bufget [::vdev::buf_widget $::a]]]}

::vdev::buf_dock $b
update
ok "re-attaching closes the window"       {![winfo exists [::vdev::_buf_float_name $::b]]}
ok "…puts the buffer back in the stack"   {[string match "[::vdev::buf_stack]*" [::vdev::buf_widget $::b]]}
ok "…with its content intact"             {[string match "*proc two*" [bufget [::vdev::buf_widget $::b]]]}
ok "…showing again"                       {[::vdev::buf_current] eq $::b}
ok "…and the strip drops the marker"      {![string match "*⧉*" [lindex [striptext] 1]]}

# Two windows at once: the whole point of "every tab detachable".
::vdev::buf_pop $a
::vdev::buf_pop $b
update
ok "two tabs can be out at the same time" \
    {[::vdev::buf_detached $::a] && [::vdev::buf_detached $::b]}
ok "…in separate windows" \
    {[::vdev::_buf_float_name $::a] ne [::vdev::_buf_float_name $::b]}
ok "…and the pane still has an editor"    {[winfo exists [::vdev::editor_widget]]}

# Closing the window by any route — the WM's close button, a stray
# `destroy`, a session teardown — must re-dock the buffer rather than
# strand the tab pointing at a widget that no longer exists.
destroy [::vdev::_buf_float_name $a]
update
ok "a float destroyed behind our back re-docks" {![::vdev::buf_detached $::a]}
ok "…with a live widget again"                 {[winfo exists [::vdev::buf_widget $::a]]}
ok "…still in the strip"                       {[lsearch -exact [::vdev::buf_list] $::a] >= 0}
::vdev::buf_dock $b
update

puts "\na strip that outgrows the pane still shows the active tab"
# Tabs are packed in a row; past a certain count that row is wider than
# the pane. Clipping it would hide buffers — the same "it is doing
# something you cannot see" failure the tabs exist to end.
for {set i 0} {$i < 16} {incr i} {
    set ::vapi::files(/tmp/overflow$i.tcl) "# buffer $i"
    set ::vdev::current_file /tmp/overflow$i.tcl
    ::vdev::editor_open
}
update
set canvas [::vdev::buf_strip_canvas]
ok "the strip is wider than the pane" \
    {[winfo reqwidth [::vdev::buf_strip]] > [winfo width $::canvas]}
ok "…so the strip scrolls"             {[lindex [$::canvas xview] 1] < 1.0}
ok "…and the active tab is in view"    {[tabvisible]}
::vdev::buf_show [lindex [::vdev::buf_list] 0]
update
ok "switching to the first scrolls back to it" \
    {[lindex [$::canvas xview] 0] == 0.0 && [tabvisible]}
::vdev::buf_show [lindex [::vdev::buf_list] end]
update
ok "…and to the last scrolls forward again"    {[tabvisible]}
ok "…which really did require scrolling"       {[lindex [$::canvas xview] 0] > 0.0}

# A long name must not push every other tab off the strip.
set ::vapi::files(/tmp/a-really-quite-long-file-name-indeed.tcl) "# long"
set ::vdev::current_file /tmp/a-really-quite-long-file-name-indeed.tcl
::vdev::editor_open
update
ok "a long tab title is truncated" {[string length [lindex [striptext] end]] <= 24}
ok "…but still recognisable"       {[string match "a-really*" [lindex [striptext] end]]}

# Back to two buffers for the button tests below.
foreach id [lrange [::vdev::buf_list] 2 end] { ::vdev::buf_close $id }
update

puts "\nthe strip's own controls are wired"
# The procs above are what the keybindings call; these buttons are what
# most people will actually click, and a -command that drifted would
# leave the strip looking right and doing nothing.
update
set before [llength [::vdev::buf_list]]
[::vdev::buf_strip].new invoke
update
ok "+ opens a new buffer"                 {[llength [::vdev::buf_list]] == $::before + 1}

set tabs [winfo children [::vdev::buf_strip]]
[lindex $tabs 0].b invoke
update
ok "clicking a tab shows that buffer"     {[::vdev::buf_current] eq [lindex [::vdev::buf_list] 0]}
[lindex $tabs 0].p invoke
update
ok "…its ⧉ pops it out"                   {[::vdev::buf_detached [lindex [::vdev::buf_list] 0]]}
[lindex [winfo children [::vdev::buf_strip]] 0].p invoke
update
ok "…and the ⤢ it turns into puts it back" \
    {![::vdev::buf_detached [lindex [::vdev::buf_list] 0]]}

set before [llength [::vdev::buf_list]]
set doomed [lindex [::vdev::buf_list] end]
[lindex [winfo children [::vdev::buf_strip]] end-1].x invoke
update
ok "× closes a clean tab"                 {[llength [::vdev::buf_list]] == $::before - 1}
ok "…the one it belonged to"              {[lsearch -exact [::vdev::buf_list] $::doomed] < 0}

puts "\ndevtest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
