# veles-desktop/lib/brand.tcl — synthwave brand banner.
#
# A canvas strip across the top of the window: gradient night sky with
# stars, a retro sun sliced by horizon lines, mountain silhouettes, a
# perspective grid, a periodic shooting star, and a neon wordmark.
# Carpenter Brut album-cover energy, drawn with core Tk canvas
# primitives (no image assets, no deps).
#
# THEME-AWARE (2026-08-11): every color comes from the active ::vt::T
# palette (or a blend toward the background), so switching themes
# re-skins the banner too. ::vt::use_theme calls ::vbrand::redraw after
# applying a palette.
#
# The canvas redraws on <Configure> so the sun/grid track the window
# width; the drawing is ~60 rows of 1px rects — negligible. A shooting
# star sweeps every ~8 seconds (canceled on destroy).

namespace eval ::vbrand {}

proc ::vbrand::hex {r g b} {
    format "#%02x%02x%02x" [expr {int($r)}] [expr {int($g)}] [expr {int($b)}]
}

# Interpolate between two #rrggbb colors; t in [0,1] toward c2.
proc ::vbrand::blend {c1 c2 t} {
    scan $c1 "#%2x%2x%2x" r1 g1 b1
    scan $c2 "#%2x%2x%2x" r2 g2 b2
    set r [expr {int($r1 + ($r2 - $r1) * $t)}]
    set g [expr {int($g1 + ($g2 - $g1) * $t)}]
    set b [expr {int($b1 + ($b2 - $b1) * $t)}]
    ::vbrand::hex $r $g $b
}

proc ::vbrand::redraw {} {
    variable T
    variable shoot_after
    if {![array size ::vt::T]} { return }
    set w .brand
    if {![winfo exists $w]} { return }
    # A redraw is also what brings the streaks back after the phone's
    # background stop (shoot_stop): any visible change already redraws.
    if {![info exists shoot_after] || $shoot_after eq ""} {
        set shoot_after [after 8000 ::vbrand::shoot]
    }
    $w delete all
    # Drawn in desktop pixels. On a phone they are dp: the drawing is made
    # at the size in dp and scaled up by the density at the end, or the
    # 20 pt title ran off a 58 px banner (lib/android.tcl).
    #
    # NOT named `k`: the grid loop below counts its rows in `k`, and a
    # scale-by-the-row-counter threw the whole drawing off the canvas
    # (2026-10-01, laptop and phone alike: gradient and stars at 3x,
    # everything else below the bottom edge).
    set u [_unit]
    set W [expr {int([winfo width $w] / $u)}]
    if {$W < 40} { return }
    set H [expr {int([winfo height $w] / $u)}]
    set horizon [expr {int($H * 0.62)}]
    set bg      $::vt::T(bg)
    set pink    $::vt::T(pink)
    set cyan    $::vt::T(cyan)
    set purple  $::vt::T(purple)
    set red     $::vt::T(red)
    set gold    $::vt::T(gold)

    # ── night sky: background easing into the accent horizon glow ──
    for {set y 0} {$y < $horizon} {incr y} {
        set t [expr {double($y) / double($horizon)}]
        set t [expr {$t * $t}]
        $w create rectangle 0 $y $W [expr {$y + 1}] \
            -fill [blend $bg $pink $t] -outline ""
    }
    # ground below the horizon
    $w create rectangle 0 $horizon $W $H -fill $bg -outline ""

    # ── stars: fixed pseudo-random scatter in the upper sky ──
    # Deterministic seed so the field is stable across redraws.
    set seed 7778777
    for {set i 0} {$i < 28} {incr i} {
        set seed [expr {($seed * 1103515245 + 12345) & 0x7fffffff}]
        set sx [expr {$seed % $W}]
        set seed [expr {($seed * 1103515245 + 12345) & 0x7fffffff}]
        set sy [expr {$seed % ($horizon - 6)}]
        set seed [expr {($seed * 1103515245 + 12345) & 0x7fffffff}]
        set size 1
        if {($seed & 7) == 0} { set size 2 }
        $w create rectangle $sx $sy [expr {$sx + $size}] [expr {$sy + $size}] \
            -fill [blend $::vt::T(fg_dim) $bg 0.35] -outline ""
    }

    # ── retro sun, half-submerged on the horizon ──
    set cx [expr {int($W * 0.72)}]
    set sy [expr {$horizon - 1}]
    set R [expr {min(16, int($H * 0.30))}]
    set R [expr {$R > 10 ? $R : 10}]
    $w create oval [expr {$cx - $R}] [expr {$sy - $R}] \
        [expr {$cx + $R}] [expr {$sy + $R}] \
        -fill $red -outline $gold -width 1
    # horizontal slice lines (the classic looking-through-the-sun look)
    for {set dy 0} {$dy <= $R} {incr dy 4} {
        set half [expr {int(sqrt(double($R * $R - $dy * $dy)))}]
        if {$half < 1} { continue }
        $w create line [expr {$cx - $half}] [expr {$sy - $dy}] \
            [expr {$cx + $half}] [expr {$sy - $dy}] -fill $bg -width 2
        if {$dy > 0} {
            $w create line [expr {$cx - $half}] [expr {$sy + $dy}] \
                [expr {$cx + $half}] [expr {$sy + $dy}] -fill $bg -width 2
        }
    }

    # ── perspective grid: cyan verticals, purple horizontals ──
    set vpx $cx
    set vpy $horizon

    # ── mountain silhouettes flanking the sun ──
    set mtn [blend $bg $purple 0.22]
    # Left range
    set mx0 [expr {$cx - 120}]
    set mx1 [expr {$cx - 40}]
    $w create polygon \
        $mx0 $vpy \
        [expr {$mx0 + 30}] [expr {$vpy - 18}] \
        [expr {$mx0 + 55}] [expr {$vpy - 8}] \
        [expr {$mx0 + 75}] [expr {$vpy - 24}] \
        $mx1 $vpy \
        -fill $mtn -outline $purple -width 1
    # Right range
    set rx0 [expr {$cx + 25}]
    set rx1 [expr {$cx + 110}]
    $w create polygon \
        $rx0 $vpy \
        [expr {$rx0 + 25}] [expr {$vpy - 14}] \
        [expr {$rx0 + 50}] [expr {$vpy - 26}] \
        [expr {$rx0 + 72}] [expr {$vpy - 10}] \
        $rx1 $vpy \
        -fill $mtn -outline $purple -width 1

    set grid_v [blend $cyan $bg 0.55]
    set grid_h [blend $purple $bg 0.55]
    for {set x [expr {$cx - 150}]} {$x <= [expr {$cx + 150}]} {incr x 26} {
        set dx [expr {$x - $vpx}]
        set ex [expr {$vpx + $dx * 2.4}]
        $w create line $x $vpy $ex $H -fill $grid_v -width 1 -tags grid
    }
    set k 0
    for {set y [expr {$horizon + 5}]} {$y < $H} {incr k; set y [expr {$horizon + 5 + $k * $k * 2}]} {
        set half [expr {($y - $vpy) * 1.7}]
        $w create line [expr {$vpx - $half}] $y [expr {$vpx + $half}] $y \
            -fill $grid_h -width 1 -tags grid
    }

    # ── wordmark with a neon glow (draw dim copies offset, then main) ──
    set fam [font actual TkDefaultFont -family]
    set glow [list $fam 20 bold]
    set sub  [list $fam 9 normal]
    set wx 22
    set wy [expr {int($H * 0.42)}]
    set glow_col [blend $pink $bg 0.35]
    foreach {dx dy} {1 0 -1 0 0 1 0 -1 1 1 -1 -1} {
        $w create text [expr {$wx + $dx}] [expr {$wy + $dy}] -text [::vi18n::mc {▲ VELES}] \
            -fill $glow_col -font $glow -anchor w
    }
    $w create text $wx $wy -text [::vi18n::mc {▲ VELES}] -fill $pink -font $glow -anchor w
    $w create text $wx [expr {$wy + 20}] -text [::vi18n::mc {neon impressionism}] \
        -fill $::vt::T(fg_dim) -font $sub -anchor w

    # hairline accent under the banner
    $w create line 0 [expr {$H - 1}] $W [expr {$H - 1}] -fill $pink -width 1
    if {$u != 1.0} { $w scale all 0 0 $u $u }
}

# Screen pixels per desktop pixel: the density on a phone, else 1.
proc ::vbrand::_unit {} {
    set k 1.0
    catch { set k [expr {double($::vandroid::density)}] }
    return $k
}

# ── shooting star: a 3-frame streak every ~8s, upper-right ──
proc ::vbrand::shoot {} {
    variable shoot_after
    set shoot_after ""
    set w .brand
    if {![winfo exists $w]} { return }
    set u [_unit]
    set W [expr {int([winfo width $w] / $u)}]
    set H [expr {int([winfo height $w] / $u)}]
    set horizon [expr {int($H * 0.62)}]
    set x0 [expr {$W - 10}]
    set y0 [expr {int($H * 0.10)}]
    set dx -28
    set dy 14
    for {set f 0} {$f < 4} {incr f} {
        set x [expr {$x0 + $dx * $f}]
        set y [expr {$y0 + $dy * $f}]
        set tail [expr {$x - $dx * 2}]
        set tail_y [expr {$y - $dy * 2}]
        set tag "shoot$f"
        $w delete $tag
        $w create line $tail $tail_y $x $y -fill $::vt::T(cyan) -width 1 -tags $tag
        if {$u != 1.0} { $w scale $tag 0 0 $u $u }
    }
    after 350 {catch {.brand delete shoot0 shoot1 shoot2 shoot3}}
    set shoot_after [after 8000 ::vbrand::shoot]
}

# The phone, backgrounded (lib/android.tcl): the streaks stop; the
# first redraw back in the foreground starts them again.
proc ::vbrand::shoot_stop {} {
    variable shoot_after
    if {[info exists shoot_after] && $shoot_after ne ""} {
        catch { after cancel $shoot_after }
        set shoot_after ""
    }
    catch { .brand delete shoot0 shoot1 shoot2 shoot3 }
}

proc ::vbrand::build {} {
    set w .brand
    catch { destroy $w }
    canvas $w -height [expr {int(round(58 * [_unit]))}] -highlightthickness 0 -bd 0 \
        -background $::vt::T(bg)
    bind $w <Configure> {::vbrand::redraw}
    bind $w <Destroy> {set ::vbrand::dead 1}
    after idle ::vbrand::redraw
    # The streaks' one timer, held in shoot_after: a bare `after` here
    # ran beside the one the first redraw arms, two streaks every 8 s,
    # and shoot_stop (the phone's background) could cancel only one.
    variable shoot_after
    if {[info exists shoot_after] && $shoot_after ne ""} {
        catch { after cancel $shoot_after }
    }
    set shoot_after [after 4000 ::vbrand::shoot]
}