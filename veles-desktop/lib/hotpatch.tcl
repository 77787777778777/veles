# veles-desktop/lib/hotpatch.tcl — THE differentiator.
#
# veles-desktop is Tcl: the running window can re-source UI code without
# a restart. <data dir>/desktop/patches.tcl is sourced at startup and on
# demand ("Reload UI patches" in the palette). Anything in it runs in the
# app's interpreter: redefine procs, restyle, add widgets, bind keys.
# hermes-desktop (Electron) cannot do this; the plugin system there needs
# a reload of the app and a build step for anything structural.
#
# Contract: patches.tcl must be idempotent (it re-runs on every reload).
#
# The path comes from ::vpaths, which is where `desktop/` actually is since
# the 2026-09-04 XDG split — under the data dir beside
# desktop_state.json, because it is state the desktop writes, not config. The old `::veles_home` helper that pointed at ~/.veles is
# gone rather than aliased: a name that returns "the veles root" has no
# honest answer now that there are three.

namespace eval ::vhotpatch {
    variable patch_file ""
}

proc ::vhotpatch::init {} {
    variable patch_file
    set patch_file [::vpaths::patch_file]
}

proc ::vhotpatch::load {} {
    variable patch_file
    if {![file exists $patch_file]} {
        ::vstatus::set_msg [format [::vi18n::mc {no patches file (%s)}] $patch_file]
        return
    }
    if {[catch {uplevel #0 [list source $patch_file]} e]} {
        ::vstatus::set_msg [format [::vi18n::mc {patch error: %s}] $e]
        return
    }
    ::vstatus::set_msg [::vi18n::mc {patches reloaded}]
}
