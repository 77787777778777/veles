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
    variable docs_route 1
    variable servers_asked 0
    variable core_root /repo/
    proc get_json {url} {
        # The Dev pane asks what each opened path is (DEV-12); that probe
        # must not hide which endpoint the OPEN read.
        if {[regexp {^/api/extensions/classify\?path=(.*)$} $url -> path]} {
            set path [string map {%20 " " %5C \\} $path]
            if {[string match "$::vapi::core_root*" $path]} { return [dict create kind core] }
            if {[string match "*/plugins/*" $path]} { return [dict create kind extension] }
            return [dict create kind other]
        }
        variable last_url $url
        if {$url eq "/api/plugins/api"} { return [dict create version 0.11] }
        if {$url in {/api/extensions/docs/plugins /api/extensions/docs/extending}} {
            if {!$::vapi::docs_route} { error "HTTP 404: not found" }
            set name [file tail $url]
            return [dict create name $name text [expr {$name eq "plugins"
                ? "# Plugins\n\n## Process plugins\n\nA child process."
                : "# Extension API\n\nThe stable surface."}]]
        }
        if {$url eq "/api/lsp/servers"} {
            incr ::vapi::servers_asked
            return [dict create servers [list \
                [dict create language tcl display Tcl present 1 shipped 1 \
                    program tcl-lsp hint {This build of veles does not include a Tcl language server.}] \
                [dict create language typescript display TypeScript present 1 shipped 0 \
                    program typescript-language-server hint {npm install -g typescript-language-server}] \
                [dict create language racket display Racket present 0 program "" \
                    hint {install Racket and the racket-langserver package on the gateway}]]]
        }
        if {[regexp {path=([^&]*)} $url -> path] && [info exists ::vapi::shas($path)]} {
            return [dict create text $::vapi::files($path) sha256 $::vapi::shas($path)]
        }
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
    # The typed-value constructors in api.tcl's representation. The stub
    # below reads the typed body back; a raw dict (the DEV-1 bug, whose
    # values the real encoder infers) is refused, as the gateway would.
    proc json_raw {text} { return [list ::vapi::__json__ $text] }
    proc json_string {v} { return [json_raw "\"$v\""] }
    proc json_object {pairs} { return [list ::vapi::__json_dict__ $pairs] }
    proc _typed_fields {body} {
        if {[lindex $body 0] ne "::vapi::__json_dict__"} {
            error "the body is not a typed JSON object: $body"
        }
        set out {}
        foreach {k v} [lindex $body 1] {
            dict set out $k [string range [lindex $v 1] 1 end-1]
        }
        return $out
    }
    proc post_json {path body} {
        if {$path eq "/api/plugins/preview"} {
            set body [_typed_fields $body]
            set name [dict get $body name]
            set kind [dict get $body kind]
            set entry [expr {$kind eq "js" ? "plugin.ts" : "plugin.tcl"}]
            return [dict create name $name kind $kind description [dict get $body description] files [list \
                [dict create name plugin.toml text "api = \"0.11\"\ndescription = \"[dict get $body description]\""] \
                [dict create name $entry text "::veles::hook post_llm_call ::example::handle"]]]
        }
        return [dict create]
    }
    proc json_escape {s} { return $s }
    # The real one percent-encodes everything; this one only the two
    # characters a Windows path brings (so the path-keyed stub above still
    # reads the unix paths it was given), which is enough to tell it from
    # json_escape. lsptest.tcl pins the real encoding.
    proc urlencode {s} { return [string map [list \\ %5C " " %20] $s] }
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
source [file join $_dir vtext.tcl]
source [file join $_dir extensionguide.tcl]

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
::vdev::attach .other.body -path /tmp/hook.tcl -status .other.msg -save {set ::saved 1}
set focus_attach_script [bind .other.body <FocusIn>]
set destroy_attach_script [bind .other.body <Destroy>]
ok "re-attaching an editor leaves one focus handler" \
    {[regexp -all -- {::vdev::_editor_focus} $focus_attach_script] == 1}
ok "re-attaching an editor leaves one destroy handler" \
    {[regexp -all -- {::vdev::_editor_gone} $destroy_attach_script] == 1}
destroy .other.body
text .other.body
pack .other.body
::vdev::attach .other.body -path /tmp/hook.tcl -status .other.msg -save {set ::saved 1}
set rebuilt_focus_script [bind .other.body <FocusIn>]
ok "an editor rebuilt at the same path gets its focus handler" \
    {[regexp -all -- {::vdev::_editor_focus} $rebuilt_focus_script] == 1}
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

puts "\nextensions are the first-line Dev workflow"
namespace eval ::vmanage {
    proc window {w title geometry minsize} {
        toplevel $w
        ttk::frame $w.bar
        pack $w.bar -fill x
    }
    proc button {bar name label cmd {help ""}} {
        ttk::button $bar.$name -text $label -command $cmd
        pack $bar.$name -side left
        return $bar.$name
    }
    proc error_text {error} { return $error }
    proc say {w msg {kind info}} {}
}
ok "New extension is directly available in the Dev toolbar" {
    [winfo exists .devroot.dev.edit.bar.newext]
}
.devroot.dev.edit.bar.newext invoke
ok "the button opens the extension form, not a name-only prompt" {
    [winfo exists .vdev_new_extension.form.name] &&
    [winfo exists .vdev_new_extension.form.kind] &&
    [.vdev_new_extension.form.api cget -text] eq "0.11"
}
# DEV-12: the out-of-tree Rust tier is a New extension choice too.
ok "the form offers every tier it can scaffold" {
    [.vdev_new_extension.form.kind cget -values] eq {tcl js racket lisp rust}
}
set ::vdev_new_name demo-tool
set ::vdev_new_purpose {A tool example}
set ::vdev_new_kind js
::vdev::_extension_preview
ok "the form previews exact files before creating anything" {
    [string match {*api = "0.11"*} [.vdev_new_extension.preview get 1.0 end-1c]] &&
    [string match {*plugin.ts*} [.vdev_new_extension.preview get 1.0 end-1c]] &&
    [string match {*A tool example*} [.vdev_new_extension.preview get 1.0 end-1c]]
}
namespace eval ::vmanage {
    proc confirm {w title detail} { set ::confirm_seen $title; return 0 }
    proc post {w path body} { lappend ::create_posts [list $path $body]; return [dict create] }
}
set ::create_posts {}
::vdev::_extension_create
ok "declining creation writes nothing and starts no publish action" {
    $::confirm_seen eq {Create this extension?} && [llength $::create_posts] == 0
}
set ::vdev_new_purpose {Changed purpose}
::vdev::_extension_create
ok "editing after the preview requires a new preview" {
    [llength $::create_posts] == 0
}
# DEV-12: a Rust extension's entry is the library its build makes, so
# nothing of it is loaded until it is built. The dialog said "Created and
# reloaded" for a crate nobody had built.
namespace eval ::vmanage {
    proc confirm {w title detail} { return 1 }
    proc post {w path body} { return [dict create path /plugins/demo-tool/created] }
    proc say {w msg {kind info}} { set ::created_said [list $msg $kind] }
}
if {![llength [info commands ::voverlay::_open_in_dev]]} {
    namespace eval ::voverlay { proc _open_in_dev {path} {} }
}
foreach kind {rust tcl} {
    set ::vdev_new_kind $kind
    ::vdev::_extension_preview
    set ::created_said {}
    ::vdev::_extension_create
    set ::created_said_$kind $::created_said
}
ok "a created Rust extension says it must be built, not that it was reloaded" {
    [string match "*cargo build --release*" [lindex $::created_said_rust 0]] &&
    ![string match "*reloaded*" [lindex $::created_said_rust 0]] &&
    [lindex $::created_said_rust 1] eq "warn"
}
ok "…and a Tcl one says it was created and reloaded" {
    [string match "Created and reloaded*" [lindex $::created_said_tcl 0]] &&
    [lindex $::created_said_tcl 1] eq "ok"
}
# SMOKE-2: TypeScript, Racket and Lisp extensions stayed `pending` while
# the dialog said "Created and reloaded". The gateway now loads each
# through its own tier and says whether that worked; the dialog repeats
# exactly that.
proc ::smoke2_create {kind reply} {
    set ::smoke2_reply $reply
    namespace eval ::vmanage {
        proc post {w path body} { return $::smoke2_reply }
    }
    set ::vdev_new_kind $kind
    ::vdev::_extension_preview
    set ::created_said {}
    ::vdev::_extension_create
    return $::created_said
}
set said [::smoke2_create js [dict create path /plugins/demo-tool/plugin.ts kind js reloaded true loaded true load {js:demo-tool: loaded in 12 ms — plugin.ts}]]
ok "a TypeScript extension the gateway loaded says it was created and loaded" {
    [string match "Created and loaded*" [lindex $said 0]] && [lindex $said 1] eq "ok"
}
set why {this gateway has no racket host: the lang-racket module is not installed or not enabled, so nothing runs racket extensions here}
set said [::smoke2_create racket [dict create path /plugins/demo-tool/plugin.rkt kind racket reloaded true loaded false load $why]]
ok "a Racket extension the gateway could not load says so, and why, not 'reloaded'" {
    [string match "Created, not loaded*" [lindex $said 0]] &&
    [string match "*no racket host*lang-racket*" [lindex $said 0]] &&
    ![string match "*reloaded*" [lindex $said 0]] && [lindex $said 1] eq "warn"
}
set said [::smoke2_create lisp [dict create path /plugins/demo-tool/plugin.lisp kind lisp reloaded false reload_error {other: broken} loaded true load {lisp:demo-tool: loaded in 30 ms}]]
ok "another plugin's Tcl reload error does not make a loaded Lisp extension a failure" {
    [string match "Created and loaded*" [lindex $said 0]] && [lindex $said 1] eq "ok"
}
set said [::smoke2_create lisp [dict create path /plugins/demo-tool/plugin.lisp reloaded true]]
ok "a gateway that does not say whether a Lisp extension loaded is not taken to have loaded it" {
    ![string match "*reloaded*" [lindex $said 0]] &&
    [string match "*Manage extensions*" [lindex $said 0]] && [lindex $said 1] eq "warn"
}
namespace eval ::vmanage {
    proc confirm {w title detail} { set ::confirm_seen $title; return 0 }
    proc post {w path body} { lappend ::create_posts [list $path $body]; return [dict create] }
    proc say {w msg {kind info}} {}
}
destroy .vdev_new_extension
.devroot.dev.edit.bar.extguide invoke
set guide [.vdev_extension_guide.body get 1.0 end]
ok "the guide includes the real service override contract" {
    [string match "*::veles::service_next*" $guide] &&
    [string match "*::veles::service_request*" $guide] &&
    [string match "*::veles::register_service*" $guide]
}
ok "the guide explains extension-first and honest LSP availability" {
    [string match "*survives upgrades*" $guide] &&
    [string match "*remote instances*" $guide] &&
    [string match "*every language the gateway has a server for*" $guide] &&
    [string match "*which program to install there*" $guide]
}
ok "the guide names every extension language's server and checker" {
    [string match "*rust-analyzer*" $guide] && [string match "*clangd*" $guide] &&
    [string match "*typescript-language-server*" $guide] && [string match "*cargo check*" $guide]
}
ok "the guide describes the four AI assistance levels" {
    [string match "*four levels*" $guide] && [string match "*whole extension*" $guide]
}
ok "the guide has working documentation buttons" {
    [winfo exists .vdev_extension_guide.bar.author] &&
    [winfo exists .vdev_extension_guide.bar.api]
}
# DEV-11: nothing lets the author choose an identity; Prepare commits
# with git's ambient one, as its own dialog says.
ok "the guide says the ambient git identity is used" {
    ![string match "*chosen git identity*" $guide] &&
    [string match "*ambient git identity*" $guide]
}
# DEV-6: whether each language has a server is the gateway's answer,
# with the gateway's own instructions when it has none. The Linux
# packages ship tcl-lsp: the row says so, and the prose no longer says
# that none ships.
ok "the guide lists each language's server as the gateway reports it" {
    [string match "*TypeScript: typescript-language-server on the gateway\n*" $guide] &&
    [string match "*Tcl: tcl-lsp on the gateway (ships with veles)*" $guide] &&
    [string match "*Racket: no language server on the gateway. install Racket*" $guide] &&
    ![string match "*none ships with veles*" $guide] &&
    [string match "*the Linux packages of veles ship it*" $guide] &&
    $::vapi::servers_asked == 1
}
# DEV-2: no shipped layout (nix package, AppImage, Windows zip, APK) has
# docs/ beside veles-desktop, and against a remote gateway a local path
# is the wrong machine's file. The guides come from the gateway.
set ::lib_dir_saved $::vdev::lib_dir
set ::vdev::lib_dir /nonexistent/prefix/veles-desktop/lib
set ::vstatus::last ""
.vdev_extension_guide.bar.author invoke
update
ok "Author guide opens the plugin author guide from an installed layout" {
    [winfo exists .vdev_extension_doc] &&
    [string match "*## Process plugins*" [.vdev_extension_doc.body get 1.0 end]] &&
    $::vapi::last_url eq "/api/extensions/docs/plugins" &&
    ![string match "*source checkout*" $::vstatus::last]
}
.vdev_extension_guide.bar.api invoke
update
ok "API reference opens the extension API guide the same way" {
    [winfo exists .vdev_extension_doc] &&
    [string match "*Extension API*" [.vdev_extension_doc.body get 1.0 end]] &&
    $::vapi::last_url eq "/api/extensions/docs/extending"
}
set ::vapi::docs_route 0
destroy .vdev_extension_doc
.vdev_extension_guide.bar.author invoke
ok "a gateway without the route says so instead of opening nothing" {
    ![winfo exists .vdev_extension_doc] && [string match "*older than 0.2.2*" $::vstatus::last]
}
set ::vapi::docs_route 1
set ::vdev::lib_dir $::lib_dir_saved
# dev-ext-7: every example must load as written. Each one is sourced into
# a fresh interpreter with the ::veles::* registration commands stubbed;
# a `proc ::my_plugin::x` before its namespace exists fails with
# "unknown namespace", exactly as it would in the gateway.
set ::snippet_errors {}
set ::snippet_ranges [.vdev_extension_guide.body tag ranges snippet]
foreach {from to} $::snippet_ranges {
    set code [.vdev_extension_guide.body get $from $to]
    set child [interp create]
    $child eval {
        namespace eval ::veles {
            foreach cmd {hook register_tool register_service register_provider json_get service_next} {
                proc $cmd args {}
            }
        }
    }
    if {[catch { $child eval $code } err]} {
        lappend ::snippet_errors $err
    } elseif {![llength [$child eval {info procs ::my_plugin::*}]]} {
        lappend ::snippet_errors "no ::my_plugin proc defined"
    }
    interp delete $child
}
ok "all four guide examples load as written (namespace before proc)" {
    [llength $::snippet_ranges] == 8 && [llength $::snippet_errors] == 0
}
destroy .vdev_extension_guide
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

puts "\nevery extension language goes to the gateway's server for it"
# dev-ext-8 / owner ask 287976.2: a TypeScript, Racket, Lisp, Rust or C
# buffer was first diagnosed as Tcl, then gated off with "no language
# server". It is now opened like a Tcl buffer; the GATEWAY picks the
# server by file type and says when it has none.
set ::lsp_up_before $::vlsp::up
set ::vlsp::up 1
set ::vlsp::stops {}
proc ::vlsp::_stop {why {tell_gateway 1}} {
    set ::vlsp::up 0
    lappend ::vlsp::stops $tell_gateway
}
set ::ts_w [::vdev::editor_widget]
set ::ts_path_before ""
catch { set ::ts_path_before $::vdev::editor_file($::ts_w) }
foreach {file lang} {/tmp/plugin.ts typescript /tmp/plugin.js javascript
                     /tmp/src/lib.rs rust /tmp/plugin.c c /tmp/plugin.rkt racket
                     /tmp/plugin.lisp commonlisp /tmp/plugin.tcl tcl} {
    ok "a $lang buffer has a language ([file extension $file])" \
        {[::vdev::_lsp_language $file] eq $lang}
}
ok "…and a README has none" {[::vdev::_lsp_language /tmp/README.md] eq ""}
set ::vdev::editor_file($::ts_w) /tmp/plugin.ts
set ::vlsp::started {}
::vdev::_lsp_open
ok "a TypeScript buffer is opened on the gateway's server" \
    {$::vlsp::started eq {/tmp/plugin.ts}}
ok "…and the status says it loaded on a server" \
    {[string match "*loaded (*)*" [$::vpanes::R.dev.edit.status cget -text]]}
# The gateway has no server for it: the regex highlighter, and the
# gateway's own reason, which names the program and the machine.
set ::vlsp::up 0
proc ::vlsp::last_error {} {
    return "no TypeScript language server is installed on the gateway (tried typescript-language-server); install it"
}
set ::calls_before $::vsyn::calls
::vdev::_lsp_open
ok "…without a server on the gateway the fallback highlighter runs" \
    {$::vsyn::calls > $::calls_before}
ok "…and the status carries the gateway's reason" \
    {[string match "*no language server*on the gateway*typescript-language-server*" [$::vpanes::R.dev.edit.status cget -text]]}
rename ::vlsp::last_error ""
# A file no server is known for is never sent to one.
set ::vlsp::up 1
set ::vdev::editor_file($::ts_w) /tmp/notes.md
set ::vlsp::started {}
::vdev::_lsp_open
ok "a file no server is known for is not opened on any server" {$::vlsp::started eq {}}
ok "…the client steps off the previous document, leaving the gateway's server up" \
    {!$::vlsp::up && [lindex $::vlsp::stops end] eq {0}}
ok "…and says there is no language server for it" \
    {[string match "*no language server for .md files*" [$::vpanes::R.dev.edit.status cget -text]]}

puts "\nCheck runs the file type's checker on the gateway"
# It used to exec tclsh and the tcl-lsp driver on THIS machine against a
# gateway path, with a scratch copy in /tmp, and had nothing for any
# other language (W6 / dev-ext-13).
namespace eval ::vapi {
    variable async_posts {}
    variable check_reply {}
    proc json_object {pairs} { return $pairs }
    proc json_string {v} { return $v }
    proc post_json_async {path body on_err on_ok {ms 0}} {
        lappend ::vapi::async_posts [list $path $body $ms]
        uplevel #0 [list {*}$on_ok $::vapi::check_reply]
    }
}
set ::vdev::editor_file($::ts_w) /tmp/plugin.ts
set ::vapi::check_reply [dict create status failed method {tsc --noEmit} diagnostics [list \
    [dict create path /tmp/plugin.ts line 2 col 6 severity ERROR message {TS2304: Cannot find name 'velez'.}] \
    [dict create path /tmp/other.ts line 1 col 0 severity WARNING message {elsewhere}]]]
::vdev::editor_check
set ::last_check [lindex $::vapi::async_posts end]
ok "Check asks the gateway, for the saved file, with a compiler's patience" \
    {[lindex $::last_check 0] eq "/api/lsp/check" && [dict get [lindex $::last_check 1] path] eq "/tmp/plugin.ts" && [lindex $::last_check 2] >= 300000}
ok "…paints only this file's findings" \
    {$::vdev::editor_diagnostics($::ts_w) eq [list [list 2 6 ERROR {TS2304: Cannot find name 'velez'.}]]}
ok "…and the status names the checker and counts every finding" \
    {[string match "*tsc --noEmit*2 problem(s), 1 in this file*" [$::vpanes::R.dev.edit.status cget -text]]}
set ::vapi::check_reply [dict create status unavailable diagnostics {} \
    note {no TypeScript checker is installed on the gateway (looked for tsc); install typescript}]
::vdev::editor_check
ok "an unavailable checker is never reported clean" \
    {[string match "*check unavailable*looked for tsc*" [$::vpanes::R.dev.edit.status cget -text]]}
set ::vapi::check_reply [dict create status passed method {cargo check} diagnostics {}]
set ::vdev::editor_file($::ts_w) /tmp/src/lib.rs
::vdev::editor_check
ok "a clean Rust crate says which checker passed it" \
    {[string match "*check clean (cargo check)*" [$::vpanes::R.dev.edit.status cget -text]]}
set ::posts_before [llength $::vapi::async_posts]
set ::vdev::editor_file($::ts_w) ""
set ::cf_before $::vdev::current_file
set ::vdev::current_file ""
::vdev::editor_check
set ::vdev::current_file $::cf_before
ok "a buffer with no file is not copied to /tmp and checked" \
    {[llength $::vapi::async_posts] == $::posts_before &&
     [string match "*save the buffer first*" [$::vpanes::R.dev.edit.status cget -text]]}
set ::vdev::editor_file($::ts_w) $::ts_path_before
# The file request names a gateway path in a URL query: a Windows path's
# backslashes were JSON-escaped into `C:\\Users\\…`, a file that does not
# exist on the gateway (dev-ext-13).
set ::cf_before $::vdev::current_file
set ::vdev::current_file {C:\Users\Ann\plugins\demo\plugin.tcl}
catch { ::vdev::editor_open }
ok "a Windows path is URL-encoded into the file request" \
    {[string match {*/api/fs/file?path=C:%5CUsers%5CAnn%5Cplugins%5Cdemo%5Cplugin.tcl} $::vapi::last_url]}
set ::win_tab [::vdev::buf_for_path {C:\Users\Ann\plugins\demo\plugin.tcl}]
if {$::win_tab ne ""} { ::vdev::buf_close $::win_tab }
set ::vdev::current_file $::cf_before
::vdev::buf_show $first
ok "…the next Tcl buffer starts tcl-lsp again" \
    {[lindex $::vlsp::started end] eq "/tmp/one.tcl"}
set ::vlsp::up $::lsp_up_before


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

puts "\nthe editor saves against the hash it loaded"
# editor_save read `editor_sha` without declaring it: a local that never
# existed, so every save went out with base_sha null and a file changed
# on the gateway since it was opened was overwritten silently.
set ::vapi::files(/tmp/hashed.tcl) "proc hashed {} {}\n"
set ::vapi::shas(/tmp/hashed.tcl) sha-loaded
set ::vdev::current_file /tmp/hashed.tcl
::vdev::editor_open
update
[::vdev::editor_widget] insert end "# edited\n"
update
::vdev::editor_save
ok "a save names the hash the buffer was loaded at" \
    {[string match {*"base_sha":"sha-loaded"*} [lindex [lindex $::vapi::posts end] 1]]}

puts "\nextension first when core source is patched (DEV-12)"
# Until 0.2.2 the only offer was a refusal inside AI assist: opening,
# editing and saving veles' own source in the Dev pane never suggested
# an extension.
set ::confirm_titles {}
set ::confirm_details {}
set ::confirm_answer 1
proc ::vmanage::confirm {w title detail} {
    lappend ::confirm_titles $title
    lappend ::confirm_details $detail
    return $::confirm_answer
}
set ::new_ext_opened 0
rename ::vdev::new_extension ::vdev::_real_new_extension
proc ::vdev::new_extension {} { incr ::new_ext_opened; set ::vdev_new_purpose "" }
set core /repo/crates/veles-core/src/agent.rs
set ::vapi::files($core) "fn agent() {}\n"
set ::vstatus::last ""
set ::vdev::current_file $core
::vdev::editor_open
update
ok "opening core source says an extension is usually the better change" \
    {[string match "*core source*extension*" $::vstatus::last]}
[::vdev::editor_widget] insert end "// patched\n"
update
set ::posts_before [llength $::vapi::posts]
::vdev::editor_save
ok "saving core source offers to draft the change as an extension" {
    $::confirm_titles eq {{Draft this change as an extension?}} && $::new_ext_opened == 1 &&
    [string match "*Instead of patching agent.rs*" $::vdev_new_purpose]
}
ok "…naming every tier the guide describes" {
    [string match "*Tcl, TypeScript, Racket or Common Lisp*" [lindex $::confirm_details 0]] &&
    [string match "*out-of-tree Rust*" [lindex $::confirm_details 0]] &&
    [string match "*module or process plugin*" [lindex $::confirm_details 0]]
}
ok "…accepting writes nothing and keeps the edit" {
    [llength $::vapi::posts] == $::posts_before &&
    [string match "*// patched*" [bufget [::vdev::editor_widget]]]
}
set ::confirm_titles {}
::vdev::editor_save
ok "…and the next save patches core without asking again" {
    $::confirm_titles eq {} && [lindex [lindex $::vapi::posts end] 0] eq "/api/fs/write"
}
set ext /home/u/.config/veles/plugins/demo/plugin.tcl
set ::vapi::files($ext) "proc demo {} {}\n"
set ::vdev::current_file $ext
::vdev::editor_open
update
[::vdev::editor_widget] insert end "# more\n"
update
set ::confirm_titles {}
::vdev::editor_save
ok "saving a user extension asks nothing" {
    $::confirm_titles eq {} && [lindex [lindex $::vapi::posts end] 0] eq "/api/fs/write"
}
rename ::vdev::new_extension ""
rename ::vdev::_real_new_extension ::vdev::new_extension

# SMOKE-8: the editor's toolbar was one packed row of 16 controls, 1882 px
# wide. At 1440 px Check, the path entry and pop-out were never mapped
# (Check had no key either, so it could not be reached at all and no file
# could be opened by path); at 1080x720 more went, and the REPL and the
# inspector left the code five lines. The real Dev pane, mapped at the
# desktop's default size, at 1440x1000 and at a phone's width (1080 px
# at density 2.625 is 412 dp), measured.
puts "\nevery Dev control is reachable and the editor has room, at every size"
proc ::smoke8_controls {f} {
    set out {}
    foreach k [winfo children $f] {
        if {[winfo class $k] in {TFrame Frame}} {
            lappend out {*}[::smoke8_controls $k]
        } else {
            lappend out $k
        }
    }
    return $out
}
proc ::smoke8_rect {k} {
    list [winfo rootx $k] [winfo rooty $k] [winfo width $k] [winfo height $k]
}
proc ::smoke8_overlap {a b} {
    lassign [::smoke8_rect $a] ax ay aw ah
    lassign [::smoke8_rect $b] bx by bw bh
    expr {$ax < $bx + $bw && $bx < $ax + $aw && $ay < $by + $bh && $by < $ay + $ah}
}
set ::vapi::files(/tmp/smoke8.tcl) [string repeat "set x 1\n" 80]
foreach size {1080x720 1440x1000 412x915} {
    destroy .devroot.dev
    ::vpanes::_build_dev .devroot.dev
    pack .devroot.dev -fill both -expand 1
    wm geometry . $size+0+0
    update
    set ::vdev::current_file /tmp/smoke8.tcl
    ::vdev::editor_open
    update
    set pane .devroot.dev
    lassign [::smoke8_rect $pane] px py pw ph
    set bad {}
    foreach bar [list $pane.repl.bar $pane.edit.bar $pane.dbg.bar] {
        set kids [::smoke8_controls $bar]
        # The inspector's caption and argument entry flow with its verbs.
        if {$bar eq "$pane.dbg.bar"} { lappend kids $pane.dbg.h $pane.dbg.arg }
        foreach k $kids {
            lassign [::smoke8_rect $k] x y kw kh
            if {![winfo ismapped $k]} { lappend bad "[winfo name $k] unmapped"; continue }
            if {$x < $px || $x + $kw > $px + $pw || $y < $py || $y + $kh > $py + $ph} {
                lappend bad "[winfo name $k] outside the pane"
            }
            if {[winfo class $k] ni {TEntry Entry} && $kw < [winfo reqwidth $k]} {
                lappend bad "[winfo name $k] squeezed to $kw of [winfo reqwidth $k] px"
            }
            if {[winfo class $k] in {TEntry Entry} && $kw < 120} {
                lappend bad "[winfo name $k] only $kw px wide"
            }
        }
        foreach a $kids {
            foreach b $kids {
                if {$a < $b && [winfo ismapped $a] && [winfo ismapped $b] && [::smoke8_overlap $a $b]} {
                    lappend bad "[winfo name $a] under [winfo name $b]"
                }
            }
        }
    }
    ok "$size: every REPL, editor and inspector control is mapped, whole and inside the pane[expr {[llength $bad] ? " ([join [lrange $bad 0 5] {; }])" : ""}]" {
        [llength $bad] == 0
    }
    set ed [::vdev::editor_widget]
    # What holds on every machine: the editor has more room than the REPL
    # or the inspector, and a floor of ten lines, twice the five of the
    # defect this guards. Neither a line count nor a share of the pane
    # does: both are the font's doing. With the flake sandbox's font the
    # inspector's verbs wrap to more rows at 1080 px, and the editor has
    # 281 px of 720 (14 lines) where this machine's font gives it half;
    # `>= 15 lines` and `half the pane` were both red there on 2026-11-11
    # with the layout right.
    set lines [expr {[winfo height $ed] / [font metrics [$ed cget -font] -linespace]}]
    set eh [winfo height $ed]
    set rh [winfo height $pane.repl]
    set ih [winfo height $pane.dbg]
    ok "$size: the editor shows $lines lines (at least 10) and has the most room (editor $eh px, REPL $rh px, inspector $ih px of $ph px)" {
        [winfo ismapped $ed] && $lines >= 10 && $eh > $rh && $eh > $ih
    }
}
# The live desktop builds the pane while another tab shows, and Tk's
# panedwindow then gave the REPL 159 px of its 111 px ask at 1080x720:
# seven lines of code were left. Start from such a share and resize.
set pw .devroot.dev.pw
wm geometry . 1080x720+0+0
update
$pw sash place 0 0 [expr {[winfo reqheight .devroot.dev.repl] + 60}]
$pw sash place 1 0 [expr {[winfo height $pw] - [winfo reqheight .devroot.dev.dbg] - 80}]
update
wm geometry . 1080x700+0+0
update
ok "a resize gives the REPL and the inspector what they ask, the editor the rest" {
    [winfo height .devroot.dev.repl] == [winfo reqheight .devroot.dev.repl] &&
    [winfo height .devroot.dev.dbg] ==
        max([winfo reqheight .devroot.dev.dbg], [$pw panecget .devroot.dev.dbg -minsize])
}
::vdev::_sash_moved $pw
$pw sash place 0 0 [expr {[winfo reqheight .devroot.dev.repl] + 60}]
update
wm geometry . 1080x720+0+0
update
ok "…until the user drags a sash: then their sizes stand" {
    [winfo height .devroot.dev.repl] == [winfo reqheight .devroot.dev.repl] + 60
}
ok "Check has a key in the editor (C-c C-k)" {
    [string match "*editor_check*" [bind [::vdev::editor_widget] <Control-c><Control-k>]]
}
set m .devroot.dev.edit.bar.guts.m
ok "the guts menu on the editor bar opens every guts file and patches.tcl" {
    [winfo exists $m] && [$m index end] == 9 &&
    [string match "*open_lib chat.tcl*" [$m entrycget 0 -command]] &&
    [string match "*open_lib overlays.tcl*" [$m entrycget 7 -command]] &&
    [string match "*open_patches*" [$m entrycget 9 -command]]
}

puts "\ndevtest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
