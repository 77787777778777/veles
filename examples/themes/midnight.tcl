# Example user theme for veles-desktop (Tcl/Tk).
# Copy to ~/.config/veles/themes/<name>.tcl with a proc named
# ::vt::palette_<name> that array-sets the ACTIVE palette variable.
# The desktop discovers it automatically (Ctrl-K → Themes). Every key
# is a "#rrggbb" hex string; the synthetic keys (pink/cyan/purple/gold/
# green/red) drive the banner + status accents.
proc ::vt::palette_midnight {a} {
    upvar 1 $a T
    array set T {
        bg        #0b0f1a
        bg_panel  #131a2e
        bg_input  #070b14
        bg_hover  #1f2a4a
        fg        #e6ebff
        fg_dim    #7a86a8
        hairline  #2a3a5a
        accent    #ff5c8a
        accent_fg #0b0f1a
        user      #4dd0e1
        assistant #e6ebff
        reasoning #9d8cff
        tool      #ffd166
        system    #ff5c5c
        ok        #4ade80
        warn      #ffd166
        select_bg #1f2a4a
        pink      #ff5c8a
        cyan      #4dd0e1
        purple    #9d8cff
        gold      #ffd166
        green     #4ade80
        red       #ff5c5c
    }
}