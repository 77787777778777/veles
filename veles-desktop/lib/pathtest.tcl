# veles-desktop/lib/pathtest.tcl — the desktop asks where things are.
#
# Since the 2026-09-04 XDG split the engine has THREE roots and the
# authority for them is Rust (config::config_dir / data_dir / cache_dir).
# The desktop missed the split: seven call sites hardcoded ~/.veles, so on
# a migrated box
#
#   - tui.locale, ui.font_scale and [ui] coach were read from a config.toml
#     that is no longer there, which is indistinguishable from "the user
#     did not set one" — three knobs that silently stopped working;
#   - user themes were searched in a directory the migration had emptied;
#   - "Edit UI patches" created ~/.veles/desktop/patches.tcl and opened it,
#     while the loader read the same wrong path — so the pair agreed with
#     each other and neither was where the user's patches had been moved;
#   - the converted-image cache RE-CREATED ~/.veles/cache/media on every
#     launch, undoing the migration the gateway had just performed and
#     re-arming its collision check.
#
# What these pin:
#
#   - the resolution rules, including the two that are easy to get wrong
#     from outside Rust: VELES_HOME forces ONE directory (config and data
#     ARE it, cache is `<it>/cache`), and a relative or blank XDG variable
#     is ignored rather than joined onto the cwd;
#   - every consumer asking the helper instead of remembering a layout;
#   - the regression that started this: exercising all of it leaves no
#     ~/.veles behind;
#   - and that no lib file computes a path through `.veles` again.
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

source [file join $_dir paths.tcl]

# ── the environment IS the fixture ───────────────────────────────────
#
# Every assertion here is about a path computation, so the test owns HOME,
# the three XDG variables and VELES_HOME, and restores them on the way out.
# Nothing touches the real home directory — which is the whole point, since
# the bug was the desktop writing into it. (Running this suite used to be
# enough to recreate ~/.veles/cache/media on the developer's own account:
# the gate dirtied the home it was checking.)
set tmpbase "/tmp"
if {[info exists ::env(TMPDIR)] && $::env(TMPDIR) ne ""} { set tmpbase $::env(TMPDIR) }
set scratch [file normalize [file join $tmpbase \
    "veles-pathtest-[pid]-[clock microseconds]"]]
file mkdir $scratch

set saved_env {}
foreach k {HOME XDG_CONFIG_HOME XDG_DATA_HOME XDG_CACHE_HOME VELES_HOME VELES_PROFILE
           VELES_LOCALE LANG LC_ALL} {
    dict set saved_env $k [expr {[info exists ::env($k)] ? $::env($k) : "\x00UNSET"}]
}

# Apply a dict of env values; "" means UNSET (not empty — Tcl cannot store
# an unset array element, and "unset" is a distinct case the code has to
# handle: env(HOME) can be absent under wish/nix).
proc envset {d} {
    foreach {k v} $d {
        if {$v eq ""} {
            if {[info exists ::env($k)]} { unset ::env($k) }
        } else {
            set ::env($k) $v
        }
    }
}

# The consumers under test. Sourced AFTER paths.tcl, which is the ordering
# main.tcl uses; a lib that needs the helper must not be loadable without
# it, or the dependency is invisible until runtime.
namespace eval ::vstatus { proc set_msg {m} { variable last $m } }
namespace eval ::vstate {
    proc get {key {default ""}} { return $default }
    proc put {key value} { return 1 }
}
namespace eval ::vapi {
    # No gateway here: theme_names must fall back to the local discovery
    # path, which is the one that had the wrong directory.
    proc get_json {path} { return -code error "no gateway in pathtest" }
}
source [file join $_dir i18n.tcl]
source [file join $_dir a11y.tcl]
source [file join $_dir theme.tcl]
source [file join $_dir coach.tcl]
source [file join $_dir media.tcl]
source [file join $_dir hotpatch.tcl]
source [file join $_dir dev.tcl]
# AFTER dev.tcl, which defines the real editor_open: a stub installed before
# the source line is silently replaced by it, and the test then drives a text
# widget stack that does not exist here.
proc ::vdev::editor_open {} { variable current_file ; return $current_file }

# ── the resolution rules ─────────────────────────────────────────────

set home [file join $scratch home]
envset [list HOME $home XDG_CONFIG_HOME "" XDG_DATA_HOME "" XDG_CACHE_HOME "" \
    VELES_HOME "" VELES_PROFILE ""]

ok "with nothing set, the three roots are the XDG defaults under HOME" \
    {[::vpaths::config_dir] eq "$home/.config/veles"
     && [::vpaths::data_dir] eq "$home/.local/share/veles"
     && [::vpaths::cache_dir] eq "$home/.cache/veles"}

set xcfg [file join $scratch xdg cfg]
set xdat [file join $scratch xdg dat]
set xcache [file join $scratch xdg cache]
envset [list XDG_CONFIG_HOME $xcfg XDG_DATA_HOME $xdat XDG_CACHE_HOME $xcache]
ok "each XDG variable is honoured, with /veles appended" \
    {[::vpaths::config_dir] eq "$xcfg/veles"
     && [::vpaths::data_dir] eq "$xdat/veles"
     && [::vpaths::cache_dir] eq "$xcache/veles"}

# Mutation: dropping the absolute-path test in _xdg makes a relative
# XDG_CACHE_HOME resolve against the cwd, which is where the suite happens
# to be running — a cache directory nobody else can find.
envset [list XDG_CACHE_HOME "relative/cache" XDG_CONFIG_HOME "  " XDG_DATA_HOME ""]
ok "a relative or blank XDG variable is ignored, not joined onto the cwd" \
    {[::vpaths::cache_dir] eq "$home/.cache/veles"
     && [::vpaths::config_dir] eq "$home/.config/veles"
     && [::vpaths::data_dir] eq "$home/.local/share/veles"}

envset [list VELES_PROFILE laptop XDG_DATA_HOME $xdat XDG_CACHE_HOME $xcache]
set proot [file join $xdat veles profiles laptop]
ok "a named profile is one config/state root and does not inherit the default" \
    {[::vpaths::config_dir] eq $proot && [::vpaths::data_dir] eq $proot
     && [::vpaths::cache_dir] eq "$proot/cache"
     && [::vpaths::skills_dir] eq "$proot/skills"}
set before [catch {::vpaths::data_dir} bad]
envset [list VELES_PROFILE ../default]
ok "a path-like profile is rejected instead of aliasing another root" \
    {[catch {::vpaths::data_dir}] == 1 && [catch {::vpaths::config_dir}] == 1}
envset [list VELES_PROFILE ""]
set forced [file join $scratch forced]
envset [list VELES_HOME $forced XDG_CACHE_HOME $xcache]
ok "VELES_HOME forces ONE directory: config and data both ARE it" \
    {[::vpaths::config_dir] eq $forced && [::vpaths::data_dir] eq $forced}
ok "…and cache is a subdirectory of it, not a third root" \
    {[::vpaths::cache_dir] eq "$forced/cache"}

envset [list VELES_HOME "   " XDG_CONFIG_HOME "" XDG_DATA_HOME "" XDG_CACHE_HOME ""]
ok "a whitespace VELES_HOME is the same as no VELES_HOME" \
    {[::vpaths::config_dir] eq "$home/.config/veles"
     && [::vpaths::cache_dir] eq "$home/.cache/veles"}

# env(HOME) can be absent under wish/nix. Tcl's own `~` expansion consults
# HOME too and RAISES when it is missing, so this is not a theoretical
# fallback: the old helper called `glob ~` bare and would have died at boot
# in exactly the case its comment claimed to handle. What matters here is
# that the app still gets a root — a relative one is a bad guess, but an
# error at source time is no app at all.
envset [list HOME "" VELES_HOME ""]
set guessed ""
set errored [catch {set guessed [::vpaths::_home]}]
ok "with no HOME it neither raises nor returns an empty root" \
    {!$errored && $guessed ne ""}
set c "" ; set k ""
ok "…and the three roots still compute, so boot does not abort" \
    {![catch {set c [::vpaths::config_dir]}]
     && ![catch {set k [::vpaths::cache_dir]}]
     && $c ne "" && $k ne ""}
envset [list HOME $home]

# ── the consumers ask, instead of remembering ────────────────────────

ok "the converted-image cache lives under the cache root" \
    {[::vmedia::cache_dir] eq [file join [::vpaths::cache_dir] media]}
ok "…and not under a home the engine declared moved" \
    {![string match "*/.veles/*" [::vmedia::cache_dir]]}

::vhotpatch::init
ok "the hot-patch loader reads the data dir, where the migration put desktop/" \
    {$::vhotpatch::patch_file eq [file join [::vpaths::data_dir] desktop patches.tcl]}

# Same file from the other side: the editor that opens it. These two agreed
# with each other while both were wrong, which is why nothing looked broken.
set opened [::vdev::open_patches]
ok "the dev editor opens the SAME file the loader reads" \
    {$opened eq $::vhotpatch::patch_file}
ok "…and creates its stub there, not in a legacy home" \
    {[file exists $opened] && ![file exists [file join $home .veles]]}

# A user theme in the config dir is discovered. theme_names tries the API
# first (stubbed to fail above), so this exercises the local fallback —
# the branch that was searching an empty ~/.veles/themes.
set tdir [::vpaths::theme_dir]
file mkdir $tdir
set tf [open [file join $tdir pathtesttheme.tcl] w]
puts $tf {array set ::vt::T {bg #000}}
close $tf
set names [::vt::theme_names]
ok "a user theme in the config dir's themes/ is discovered" \
    {"pathtesttheme" in $names}
ok "the built-ins are still there beside it" {"synthwave" in $names}

# Three knobs read the config file directly, because they run before there
# is an attachment. One fixture, all three: they have to agree on WHICH
# file, and "the one the gateway uses" is a claim about a path.
set cfg [::vpaths::config_file]
file mkdir [file dirname $cfg]
set cf [open $cfg w]
puts $cf "\[tui\]\nlocale = \"de\"\n\n\[ui\]\nfont_scale = 1.5\ncoach = false\n"
close $cf

envset [list VELES_LOCALE "" LANG "en_GB.UTF-8" LC_ALL ""]
ok "i18n takes tui.locale from the config dir's config.toml" \
    {[::vi18n::init] eq "de"}
ok "a11y takes ui.font_scale from the same file" \
    {[::va11y::init] eq "1.5"}
ok "the coach takes \[ui\] coach = false from the same file" \
    {[::vcoach::init] eq "0"}

# The regression, stated as the operator sees it: a desktop that has done
# everything it does at boot leaves no ~/.veles behind.
ok "nothing above recreated the legacy home" \
    {![file exists [file join $home .veles]]}

# ── and it cannot come back quietly ──────────────────────────────────
#
# The seven call sites were each locally reasonable; what was missing was
# anything that would notice an eighth. This walks the lib directory and
# refuses any CODE line that builds a path through `.veles`. Comments are
# excluded on purpose — several files explain the old layout on purpose,
# and a guard that forbids mentioning a bug is a guard that gets deleted.
set offenders {}
foreach f [glob -nocomplain -directory $_dir *.tcl] {
    set base [file tail $f]
    if {[string match "*test.tcl" $base] || $base eq "paths.tcl"} { continue }
    set fh [open $f r]
    set n 0
    while {[gets $fh line] >= 0} {
        incr n
        # A path THROUGH `.veles` has a separator or home anchor beside the
        # name (/.veles, ~/.veles, .veles/). Requiring one keeps a class
        # name that merely CONTAINS the substring (chat.janitor.veles.*,
        # matched before) from tripping a filesystem guard.
        if {[regexp {^[ \t]*(set|return|lappend|append|proc|if|expr|file)[^\n#]*(/\.veles|~/\.veles|\.veles/)} $line]} {
            lappend offenders "$base:$n"
        }
    }
    close $fh
}
ok "no lib file computes a path through .veles" {[llength $offenders] eq 0}
if {[llength $offenders]} { puts "      [join $offenders {, }]" }

# ── paths.tcl sources first ──────────────────────────────────────────
#
# main.tcl sources paths.tcl at the top and every later module calls
# ::vpaths::* at source time. A reorder surfaces only at runtime as an
# unknown command — nothing asserted the order. The rule: no main.tcl
# line may call ::vpaths:: BEFORE the line that sources paths.tcl.
set mainfile [file join $_dir .. main.tcl]
set paths_line 0
set early_call {}
set fh [open $mainfile r]
set n 0
while {[gets $fh line] >= 0} {
    incr n
    if {[regexp {source .*lib[ /]paths\.tcl} $line]} { set paths_line $n }
    if {[string match "*::vpaths::*" $line] && ![regexp {source .*paths\.tcl} $line]} {
        if {$paths_line == 0 || $n < $paths_line} {
            lappend early_call $n
        }
    }
}
close $fh
ok "main.tcl sources paths.tcl before any ::vpaths:: call" \
    {$paths_line > 0 && [llength $early_call] eq 0}
if {$paths_line == 0} { puts "      paths.tcl never sourced in main.tcl" }
if {[llength $early_call]} { puts "      ::vpaths:: called before paths.tcl at lines: [join $early_call {, }]" }

# ── restore + tally ──────────────────────────────────────────────────
foreach k [dict keys $saved_env] {
    set v [dict get $saved_env $k]
    if {$v eq "\x00UNSET"} {
        if {[info exists ::env($k)]} { unset ::env($k) }
    } else {
        set ::env($k) $v
    }
}
catch { file delete -force $scratch }

puts "\npathtest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
