# veles-desktop/lib/paths.tcl — the three directories, computed once.
#
# Since the 2026-09-04 XDG split veles has THREE roots, not one, and the
# authority for where each thing lives is Rust: `config::config_dir`,
# `config::data_dir` and `config::cache_dir` in crates/veles-core/src/config.rs.
# This file mirrors those three, including the two rules that are easy to
# get wrong from the outside:
#
#   - `VELES_HOME` forces the single-directory layout. It is not a
#     synonym for "the config dir": config and data both ARE that directory,
#     and cache is `<VELES_HOME>/cache`. The OCI image and every Rust test
#     fixture set it, and both want exactly one directory.
#   - an XDG variable that is empty, whitespace, or RELATIVE is ignored, and
#     the `~/.config`-style fallback applies. Rust requires `is_absolute()`;
#     a relative `$XDG_CACHE_HOME` here would put the desktop's cache
#     somewhere the gateway never looks.
#
# Why this exists: the desktop missed the split. Seven call sites still
# reached for `~/.veles` directly, so on a migrated box `ui.font_scale`,
# `ui.coach` and `tui.locale` were read from a file that is no longer there
# (silently defaulting), user themes were searched in an empty directory,
# and the converted-image cache RE-CREATED `~/.veles/cache/media` on every
# launch — undoing the migration the gateway had just performed and
# re-arming its collision check. A directory the engine declared moved was
# being written to by its own frontend.
#
# Sourcing order: FIRST, before i18n.tcl, because `::vi18n::init` reads
# config.toml and everything after it may translate at source time.
#
# Nothing here touches the filesystem. These are pure path computations;
# callers create what they need.

namespace eval ::vpaths {}

# The user's home directory.
#
# `env(HOME)` first because that is what the rest of the desktop reads and
# what a test harness sets. `glob ~` next: env(HOME) can be absent under
# wish/nix, and tilde expansion is the only other way to ask — but it is
# NOT unconditional, because Tcl's `~` expansion itself consults HOME and
# raises "couldn't find HOME environment variable to expand path" when it is
# missing. The old `::veles_home` called it bare and would have died at
# boot in exactly the situation its comment claimed it was handling.
#
# "." last, matching Rust's `user_home()`. A relative root is a bad guess,
# but an EMPTY one is worse: `file join "" .veles` is `.veles`, which
# silently means "inside whatever directory we were started from" and looks
# like a real path in every log line.
proc ::vpaths::_home {} {
    if {[info exists ::env(HOME)] && [string trim $::env(HOME)] ne ""} {
        return [string trim $::env(HOME)]
    }
    if {![catch {set h [glob -nocomplain ~]}] && $h ne ""} { return $h }
    return "."
}

# `$VELES_HOME` when it is set to something, else "".
proc ::vpaths::forced {} {
    if {[info exists ::env(VELES_HOME)]} {
        set h [string trim $::env(VELES_HOME)]
        if {$h ne ""} { return $h }
    }
    return ""
}

# `$<var>/veles`, else `<home>/<fallback>/veles`. Mirrors Rust's `xdg()`,
# including the absolute-path requirement.
proc ::vpaths::_xdg {var fallback} {
    set base ""
    if {[info exists ::env($var)]} { set base [string trim [set ::env($var)]] }
    if {$base ne "" && [file pathtype $base] eq "absolute"} {
        return [file join $base veles]
    }
    # Windows without an XDG variable: config in %APPDATA%\veles, state in
    # %LOCALAPPDATA%\veles, cache under it — what config.rs's
    # `windows_root` does, so the window and the engine agree.
    if {$::tcl_platform(platform) eq "windows"} {
        set w [_windows_root $var]
        if {$w ne ""} { return $w }
    }
    return [file join [_home] $fallback veles]
}

proc ::vpaths::_windows_root {var} {
    set env_abs {k {
        if {![info exists ::env($k)]} { return "" }
        set v [string trim $::env($k)]
        if {$v eq "" || [file pathtype $v] ne "absolute"} { return "" }
        return $v
    }}
    switch -- $var {
        XDG_CONFIG_HOME {
            set a [apply $env_abs APPDATA]
            if {$a ne ""} { return [file join $a veles] }
        }
        XDG_DATA_HOME {
            set l [apply $env_abs LOCALAPPDATA]
            if {$l ne ""} { return [file join $l veles] }
        }
        XDG_CACHE_HOME {
            set l [apply $env_abs LOCALAPPDATA]
            if {$l ne ""} { return [file join $l veles cache] }
        }
    }
    return ""
}

# CONFIGURATION: config.toml, .env, secrets.env, themes/, commands/,
# plugins/, skills/, failover-scripts/.
proc ::vpaths::profile {} {
    if {![info exists ::env(VELES_PROFILE)]} { return "" }
    set p [string trim $::env(VELES_PROFILE)]
    if {$p eq "" || $p eq "default"} { return "" }
    # Match profiles.rs: never turn a path into an unrelated directory.
    if {[string index $p 0] eq "." || ![regexp {^[A-Za-z0-9_.-]+$} $p]} {
        return -code error "invalid VELES_PROFILE: use letters, digits, '-', '_' or '.'; not a path"
    }
    return $p
}

proc ::vpaths::config_dir {} {
    if {[set p [profile]] ne ""} { return [data_dir] }
    set f [forced]
    if {$f ne ""} { return $f }
    return [_xdg XDG_CONFIG_HOME .config]
}

# STATE: state.db (sessions and memory), checkpoints/, media/, certs/, backups/,
# logs/, sessions, desktop/, desktop_state.json.
proc ::vpaths::data_dir {} {
    set f [forced]
    if {$f ne ""} { set base $f } else { set base [_xdg XDG_DATA_HOME .local/share] }
    if {[set p [profile]] ne ""} { return [file join $base profiles $p] }
    return $base
}

# REGENERABLE working files. The one directory that is a subdirectory of a
# forced home rather than the home itself.
proc ::vpaths::cache_dir {} {
    if {[profile] ne ""} { return [file join [data_dir] cache] }
    set f [forced]
    if {$f ne ""} { return [file join $f cache] }
    return [_xdg XDG_CACHE_HOME .cache]
}

# The config file the gateway reads. Three desktop features grep it
# directly rather than asking the API (locale, font_scale, coach) because
# they run before there is an attachment — so all three have to agree on
# WHICH file, and "the one the gateway uses" is a claim, not a default.
proc ::vpaths::config_file {} {
    return [file join [config_dir] config.toml]
}

# User themes are config (the engine's theme.rs reads `config_dir()`),
# so searching anywhere else finds nothing.
proc ::vpaths::theme_dir {} {
    return [file join [config_dir] themes]
}

# Skills and plugins, for the same reason and from the same list.
proc ::vpaths::skills_dir {} {
    return [file join [config_dir] skills]
}

proc ::vpaths::plugins_dir {} {
    return [file join [config_dir] plugins]
}

# The hot-patch file: sourced at startup and on "Reload UI patches", and
# the file the dev editor opens. `desktop/` is state, under the data dir
# beside desktop_state.json — pointing at the config dir would load nothing
# and write a second, ignored patches file.
proc ::vpaths::patch_file {} {
    return [file join [data_dir] desktop patches.tcl]
}
