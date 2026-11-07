# scripts/dev-lsp-live.tcl — drive the Dev pane's REAL LSP client
# (veles-desktop/lib/lspclient.tcl, over a real gateway's /api/lsp) on a
# file of each extension language, and report what came back.
#
# The unit suites stub one side each: lsptest/lspstarttest stub the
# gateway, the veles-core live tests speak to real servers without the
# Tcl client. This is the two together, the way the editor runs: the
# gateway picks the server by file type, the client handshakes with the
# capabilities it advertises for that language, converts positions, and
# asks for hover, definition and completion while diagnostics arrive.
#
# Usage (under a throwaway gateway, never a live one):
#   wish scripts/dev-lsp-live.tcl <gateway-url> <token> <workdir> ?language ...?
# languages: c rust typescript javascript tcl racket lisp (default: all).
# Exit 0 when every check of every language whose server the gateway has
# passed; a language whose server is missing or cannot run is reported
# SKIP with the gateway's own message, which is itself checked to name
# the gateway and the program (racket-langserver and a Lisp server are
# not in nixpkgs, so under the devShell those two exercise exactly that:
# `racket` without the package exits at once, cl-lsp is not found).
# Needs a display (the client paints tokens into a text widget): run it
# on an Xvfb, never on a desktop session.

set lib [file normalize [file join [file dirname [info script]] .. veles-desktop lib]]
lassign $argv url token work
set only [lrange $argv 3 end]
if {$work eq ""} {
    puts stderr "usage: wish dev-lsp-live.tcl <gateway-url> <token> <workdir> ?language ...?"
    exit 2
}
package require Tk
wm withdraw .
namespace eval ::vstatus { variable last ""; proc set_msg {m} { set ::vstatus::last $m; puts "  status: $m" } }
namespace eval ::vsyn {
    proc _tags {w} {}
    proc extra_passes {w} {}
    proc highlight {w} {}
}
source [file join $lib api.tcl]
source [file join $lib paths.tcl]
source [file join $lib i18n.tcl]
source [file join $lib lspclient.tcl]
set ::vapi::base $url
set ::vapi::token $token

set failed 0
set skipped {}
proc check {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  PASS $name"
    } else {
        puts "  FAIL $name"
        incr ::failed
    }
}

# Wait for a condition, pumping the event loop, up to `secs`.
proc await {cond secs} {
    set deadline [expr {[clock milliseconds] + $secs * 1000}]
    while {[clock milliseconds] < $deadline} {
        if {[uplevel #0 [list expr $cond]]} { return 1 }
        after 100 [list set ::_tick 1]
        vwait ::_tick
    }
    return [uplevel #0 [list expr $cond]]
}

proc write_file {path text} {
    file mkdir [file dirname $path]
    set f [open $path w]
    fconfigure $f -encoding utf-8
    puts -nonewline $f $text
    close $f
}

set ::diags {}
proc ::on_diag {rows} { set ::diags $rows }
proc ::got {slot result err} { set ::reply($slot) [list $result $err] }

# Each case: language, file, text, the 0-based (line, char) of a use of
# `helper` whose definition is on line `defline`, and the column three
# characters into that use (where completion is asked, after "hel").
set cases {}
lappend cases c [file join $work cplug plugin.c] \
    "/* é€ helper docs */\nstatic int helper(int x) \{ return x + 1; \}\nint veles_entry(void) \{ return helper(41); \}\nint broken(void) \{ return missing; \}\n" \
    2 33 1 34
lappend cases rust [file join $work rplug src lib.rs] \
    "/// Adds one.\npub fn helper(x: i32) -> i32 \{\n    x + 1\n\}\n\npub fn entry() -> i32 \{\n    helper(41) + missing\n\}\npub fn broken() -> i32 \{ 1 + \}\n" \
    6 6 1 7
lappend cases typescript [file join $work tsplug plugin.ts] \
    "function helper(x: number): number \{ return x + 1; \}\nconst n: number = helper(1);\nconst bad: number = \"text\";\n" \
    1 20 0 21
lappend cases javascript [file join $work jsplug plugin.js] \
    "function helper(x) \{ return x + 1; \}\nconst n = helper(1);\nconst bad = ;\n" \
    1 12 0 13
lappend cases tcl [file join $work tclplug plugin.tcl] \
    "proc helper \{x\} \{ return \[expr \{\$x + 1\}\] \}\nset n \[helper 41\]\nproc broken \{a \{\n" \
    1 9 0 10
lappend cases racket [file join $work rktplug plugin.rkt] \
    "#lang racket/base\n(define (helper x) (+ x 1))\n(helper 41)\n" \
    2 2 1 4
lappend cases lisp [file join $work lispplug plugin.lisp] \
    "(defun helper (x) (+ x 1))\n(helper 41)\n" \
    1 2 0 4
# The program each language's server is, as the gateway names it.
array set program {c clangd rust rust-analyzer typescript typescript-language-server
    javascript typescript-language-server tcl tcl-lsp racket racket lisp cl-lsp}
array set language_id {lisp commonlisp}
write_file [file join $work rplug Cargo.toml] "\[package\]\nname = \"rplug\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\[workspace\]\n"
write_file [file join $work tsplug plugin.toml] "name = \"tsplug\"\nkind = \"js\"\nentry = \"plugin.ts\"\n"
write_file [file join $work tsplug tsconfig.json] "\{\"compilerOptions\": \{\"target\": \"es2022\", \"module\": \"esnext\", \"moduleDetection\": \"force\", \"lib\": \[\"es2022\"\], \"noEmit\": true\}\}\n"
write_file [file join $work jsplug plugin.toml] "name = \"jsplug\"\nkind = \"js\"\nentry = \"plugin.js\"\n"
write_file [file join $work cplug plugin.toml] "name = \"cplug\"\n"
write_file [file join $work tclplug plugin.toml] "name = \"tclplug\"\n"
write_file [file join $work rktplug plugin.toml] "name = \"rktplug\"\nkind = \"racket\"\nentry = \"plugin.rkt\"\n"
write_file [file join $work lispplug plugin.toml] "name = \"lispplug\"\nkind = \"lisp\"\nentry = \"plugin.lisp\"\n"

text .buf
foreach {lang path text line char defline compcol} $cases {
    if {[llength $only] && $lang ni $only} { continue }
    puts "\n== $lang: $path"
    write_file $path $text
    .buf delete 1.0 end
    .buf insert 1.0 $text
    set ::diags {}
    array unset ::reply
    ::vlsp::start $path $text ::on_diag
    # AFTER start, as dev.tcl does: start clears the paint target (a new
    # document belongs to whichever buffer opened it), so a target set
    # before it is forgotten and no token is ever painted.
    ::vlsp::set_apply_widget .buf
    if {![::vlsp::running]} {
        set why [::vlsp::last_error]
        puts "  SKIP: $why"
        lappend skipped $lang
        check "$lang: the reason names the gateway and $program($lang)" \
            {[string match "*on the gateway*" $why] && [string match "*$program($lang)*" $why]}
        continue
    }
    set want_id $lang
    if {[info exists language_id($lang)]} { set want_id $language_id($lang) }
    check "$lang: a session for its language" {$::vlsp::session ne "" && [::vlsp::language] eq $want_id}
    check "$lang: initialized" {[await {$::vlsp::ready} 120]}
    puts "  server [::vlsp::server_label], encoding $::vlsp::encoding"
    # rust-analyzer answers null until its workspace is loaded: ask again.
    set deadline [expr {[clock seconds] + 180}]
    while {1} {
        unset -nocomplain ::reply(hover)
        ::vlsp::hover_at $line $char [list ::got hover]
        await {[info exists ::reply(hover)]} 60
        set h ""
        catch { set h [::vlsp::render_hover [lindex $::reply(hover) 0]] }
        if {$h ne "" || [clock seconds] > $deadline} { break }
        after 1000
    }
    puts "  hover: [string range [string map {\n { | }} $h] 0 100]"
    check "$lang: hover names helper" {[string match "*helper*" $h]}
    ::vlsp::definition_at $line $char [list ::got def]
    await {[info exists ::reply(def)]} 60
    set target {}
    catch { set target [::vlsp::definition_target [lindex $::reply(def) 0]] }
    puts "  definition: $target[expr {[lindex $::reply(def) 1] ne {} ? " (error: [lindex $::reply(def) 1])" : ""}]"
    check "$lang: definition is helper's line" {[lindex $target 1] == $defline && [::vlsp::same_path [::vlsp::uri_to_path [lindex $target 0]] $path]}
    ::vlsp::complete_at $line $compcol [list ::got comp]
    await {[info exists ::reply(comp)]} 60
    set labels {}
    catch { foreach item [::vlsp::completion_items [lindex $::reply(comp) 0]] { lappend labels [string trim [lindex $item 0]] } }
    puts "  completion: [lrange $labels 0 7] ..."
    check "$lang: completion offers helper" {[lsearch -glob $labels helper*] >= 0}
    check "$lang: diagnostics for the broken line arrive" {[await {[llength $::diags] > 0} 120]}
    puts "  diagnostics: [lrange $::diags 0 2]"
    check "$lang: semantic tokens painted" {[await {[llength [.buf tag ranges syn_func]] + [llength [.buf tag ranges syn_proc]] + [llength [.buf tag ranges syn_keyword]] > 0} 30]}
}
::vlsp::_stop "" 1
puts "\ndev-lsp-live: [expr {$failed ? "FAILED ($failed)" : "ok"}][expr {[llength $skipped] ? ", skipped: $skipped" : ""}]"
exit [expr {$failed ? 1 : 0}]
