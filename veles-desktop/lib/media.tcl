# veles-desktop/lib/media.tcl — one renderer for every attached file.
#
# USER: "model-to-user images would also be great, and attachments/
# artifacts in general. I didn't mean just model-to-user MEDIA."
#
# What existed: `_render_media_item` in chat.tcl inlined a PNG or a GIF
# and, for everything else, wrote one dead line —
#
#     📎 media: /home/user/shot.jpg
#
# — with no way to open it, save it, preview it or even learn how big it
# is. A JPEG screenshot, the commonest artifact of the lot, was a string.
# And an inlined PNG went in at its native size, so a 1280×1577 photo
# shoved the whole transcript sideways.
#
# So this module owns three things the two call sites (the transcript and
# the preview pane) were each half-doing:
#
#   1. DECODING breadth. Core Tk 8.6 reads PNG/GIF/PPM and nothing else.
#      tkimg ("Img") adds JPEG, TIFF, BMP, ICO, PCX, TGA, XPM, SGI, SUN.
#      It is in the flake, but `package require` is still guarded: the
#      desktop has to keep working when someone runs main.tcl against a
#      bare wish.
#
#   2. FITTING. Images are subsampled to a display cap before they go in.
#
#   3. AN ARTIFACT CARD for everything that cannot be decoded — the name,
#      what it is, how big, and *actions*: Open · Save as… · Preview ·
#      Copy path. A .webp or a .pdf the agent produced is now a thing you
#      can do something with rather than a path you have to select and
#      paste into a terminal.
#
# Card actions are tagged text runs rather than embedded ttk buttons: the
# transcript is a read-only text widget that gets rebuilt wholesale, and
# tagged runs are how everything else clickable in this app (links, code
# blocks, tool-call expanders, note expanders) already works.

namespace eval ::vmedia {
    # -1 = not tried yet, 1 = Img loaded, 0 = absent. Tried once: a
    # failed `package require` is not cheap enough to repeat per image.
    variable img_pkg -1

    # Display cap for an inlined image. Wide enough to read a screenshot,
    # narrow enough that the transcript still wraps like a transcript.
    variable max_w 560
    variable max_h 460

    # Per-card tag counter — tags are per-item because each card's
    # actions close over its own path.
    variable seq 0

    # tag -> {action path}, so a click knows what it is acting on
    # without parsing the line back out of the widget.
    variable act

    # Decoded photos, keyed "path|mtime|maxw|maxh" -> photo name.
    # Session switches re-render both transcripts; without this every
    # replay re-decoded every attachment. See `load` for the rules.
    variable photo_cache {}

    # ── formats with no Tk decoder at all ───────────────────────────
    #
    # USER: "couldnt we add a webp decoder? surely there is a library,
    # even if it's not core."
    #
    # There is, but not as a Tk image handler: tkimg 2.1 has no webp,
    # heic, avif or svg reader, and there is no maintained Tcl extension
    # that does. What every one of these formats DOES have is a small,
    # boring command-line decoder that writes a PNG — which Tk reads
    # natively. So the cascade is: can Tk read it? no — can something on
    # PATH turn it into a PNG? yes — cache that and read the PNG.
    #
    # Each rule is {executable {argument template}}, where IN and OUT are
    # substituted. `exec` takes a list, so no shell and no quoting hazard
    # from a path the model chose; paths are normalised to absolute so a
    # leading "-" can never be read as a flag.
    variable converters {
        .webp {dwebp    {IN -o OUT}}
        .svg  {resvg    {IN OUT}}
        .svgz {resvg    {IN OUT}}
        .heic {heif-dec {IN OUT}}
        .heif {heif-dec {IN OUT}}
        .avif {heif-dec {IN OUT}}
    }

    # Converters run SYNCHRONOUSLY, from inside a transcript render, so a
    # pathological input must not be able to sit there spinning. Tcl has
    # no exec timeout and a `vwait` here would re-enter the event loop
    # mid-render, so the guard is on the input instead: past this we do
    # not even try. A real screenshot is orders of magnitude under it.
    variable max_convert_bytes 41943040

    # exe -> resolved path or "" — auto_execok is a PATH walk, and this
    # is asked once per attachment per render.
    variable exe_cache
}

# Is tkimg available? Cached. Never throws.
proc ::vmedia::have_img {} {
    variable img_pkg
    if {$img_pkg >= 0} { return $img_pkg }
    set img_pkg [expr {[catch { package require Img }] ? 0 : 1}]
    return $img_pkg
}

# Extensions core Tk can decode on its own.
proc ::vmedia::_core_exts {} { return {.png .gif .ppm .pgm .pnm} }

# Extensions tkimg adds. Deliberately NOT including .webp/.heic/.avif:
# tkimg 2.1 has no decoder for those, and claiming otherwise would put us
# straight back to a broken inline image instead of an honest card.
proc ::vmedia::_img_exts {} {
    return {.jpg .jpeg .jpe .tif .tiff .bmp .ico .pcx .tga .xpm .xbm .sgi .sun .ras}
}

# Every extension we recognise AS AN IMAGE, decodable or not — the card
# for a .webp should say "WebP image", not "file".
proc ::vmedia::_all_image_exts {} {
    return [concat [_core_exts] [_img_exts] {.webp .heic .heif .avif .jxl .svg .psd}]
}

proc ::vmedia::_ext {path} { return [string tolower [file extension $path]] }

proc ::vmedia::is_image {path} {
    return [expr {[_ext $path] in [_all_image_exts]}]
}

# Can Tk read this file itself — core handlers or tkimg's?
proc ::vmedia::decodes_natively {path} {
    set e [_ext $path]
    if {$e in [_core_exts]} { return 1 }
    if {$e in [_img_exts]}  { return [have_img] }
    return 0
}

# Is there a converter on PATH that turns this into something Tk reads?
proc ::vmedia::convertible {path} {
    return [expr {[_converter_for $path] ne ""}]
}

# The rule for this extension, IF its executable is actually installed.
# A rule whose binary is missing is not a capability — reporting it as
# one would put a picture-shaped hole where the artifact card should be.
proc ::vmedia::_converter_for {path} {
    variable converters
    variable exe_cache
    set e [_ext $path]
    if {![dict exists $converters $e]} { return "" }
    lassign [dict get $converters $e] exe argv
    if {![info exists exe_cache($exe)]} {
        set exe_cache($exe) [lindex [auto_execok $exe] 0]
    }
    if {$exe_cache($exe) eq ""} { return "" }
    return [list $exe_cache($exe) $argv]
}

# Can we actually put pixels on the screen for this file, here, now?
proc ::vmedia::renderable {path} {
    if {[decodes_natively $path]} { return 1 }
    return [convertible $path]
}

# A human label for the kind of thing this is.
proc ::vmedia::label_for {path} {
    set e [string trimleft [_ext $path] "."]
    if {$e eq ""} { return "file" }
    set up [string toupper $e]
    switch -- [_ext $path] {
        .jpg - .jpeg - .jpe { set up "JPEG" }
        .tif - .tiff        { set up "TIFF" }
        .webp               { set up "WebP" }
        .heic - .heif       { set up "HEIF" }
        .svg - .svgz        { set up "SVG" }
    }
    if {[is_image $path]} {
        if {[renderable $path]} { return "$up image" }
        # Two different things, and a user chasing this needs to know
        # which: nothing can read this format, versus the reader for it
        # is simply not on PATH here.
        if {[dict exists $::vmedia::converters [_ext $path]]} {
            return "$up image (decoder not installed)"
        }
        return "$up image (no decoder)"
    }
    switch -- [_ext $path] {
        .txt - .md - .markdown - .log { return "$up text" }
        .tcl - .rs - .py - .sh - .c - .h - .js - .ts - .lua { return "$up source" }
        .json - .toml - .yaml - .yml - .xml { return "$up data" }
        .pdf  { return "PDF document" }
        .zip - .tar - .gz - .xz - .zst - .7z { return "$up archive" }
        .mp4 - .mkv - .webm - .mov { return "$up video" }
        .mp3 - .ogg - .wav - .flac - .opus { return "$up audio" }
    }
    return "$up file"
}

proc ::vmedia::human_size {n} {
    if {$n >= 1048576} { return [format "%.1f MB" [expr {$n / 1048576.0}]] }
    if {$n >= 1024}    { return "[expr {$n / 1024}] KB" }
    return "$n B"
}

# Load PATH as a Tk photo, subsampled to fit within maxw × maxh.
#
# Returns the image name, or "" with the reason in ::vmedia::last_error.
# Tk's `-subsample` is integer-only, so the result is at-most-cap rather
# than exactly-cap — which is the right trade here: it is one C memcpy
# pass, and a transcript redraw must not stall on resampling.
proc ::vmedia::load {path {maxw ""} {maxh ""}} {
    variable max_w
    variable max_h
    variable last_error ""
    if {$maxw eq ""} { set maxw $max_w }
    if {$maxh eq ""} { set maxh $max_h }
    if {![file isfile $path]} { set last_error "not found" ; return "" }
    if {![is_image $path]} { set last_error "not an image" ; return "" }
    # Switching between two sessions re-renders both transcripts, and
    # every replay used to DECODE every attachment again — that is the
    # "lots of cpu time" the session switch costs. The decode result is
    # a function of (path, mtime, maxw, maxh), so cache it keyed on
    # exactly that; an edited file has a new mtime and a new entry.
    variable photo_cache
    set mtime 0
    catch { set mtime [file mtime $path] }
    set ck "$path|$mtime|$maxw|$maxh"
    if {[dict exists $photo_cache $ck]} {
        set im [dict get $photo_cache $ck]
        # Still a live image? (A rebuild that deletes widget images must
        # never touch the cache, but belt and braces.)
        if {[catch { image width $im }]} {
            dict unset photo_cache $ck
        } else {
            return $im
        }
    }
    # Img registers its handlers on load; without it `image create photo`
    # on a JPEG fails with "couldn't recognize data".
    have_img
    # webp / heic / avif / svg: no Tk handler exists for any of them, so
    # decode through a cached PNG. transcode() owns the "why not" text,
    # including the case where the format is convertible in principle but
    # its converter is not installed here.
    if {![decodes_natively $path]} {
        set converted [transcode $path]
        if {$converted eq ""} { return "" }
        set path $converted
    }
    if {[catch { set im [image create photo -file $path] } e]} {
        set last_error $e
        return ""
    }
    set iw [image width $im]
    set ih [image height $im]
    if {$iw <= 0 || $ih <= 0} { catch { image delete $im } ; set last_error "empty image" ; return "" }
    set f 1
    if {$maxw > 0 && $iw > $maxw} { set f [expr {int(ceil(double($iw) / $maxw))}] }
    if {$maxh > 0 && $ih > $maxh} {
        set fy [expr {int(ceil(double($ih) / $maxh))}]
        if {$fy > $f} { set f $fy }
    }
    if {$f > 1} {
        set small [image create photo]
        $small copy $im -subsample $f $f
        image delete $im
        set im $small
    }
    dict set photo_cache $ck $im
    _photo_evict
    return $im
}

# Cap the decode cache so a long-lived window cannot collect every
# attachment it ever saw. Oldest entries go first; the evicted image is
# deleted only when no widget shows it — `image inuse` is the truth,
# and deleting a visible photo tears a hole into the transcript.
proc ::vmedia::_photo_evict {} {
    variable photo_cache
    while {[dict size $photo_cache] > 24} {
        set ck [lindex [dict keys $photo_cache] 0]
        set im [dict get $photo_cache $ck]
        dict unset photo_cache $ck
        if {![catch { image inuse $im } used] && !$used} {
            catch { image delete $im }
        }
    }
}

# Names this module owns (a rebuild that sweeps widget images must skip
# them, or the cache hands out a dead name on the next replay).
proc ::vmedia::cached_images {} {
    variable photo_cache
    return [dict values $photo_cache]
}

# Drop the cache — an attach teardown: the pictures belonged to the
# previous gateway's world of sessions.
proc ::vmedia::cache_reset {} {
    variable photo_cache
    foreach im [dict values $photo_cache] { catch { image delete $im } }
    set photo_cache {}
}

# ── converting what Tk cannot read ──────────────────────────

# Where converted PNGs live: `media/` under the cache root — so
# ~/.cache/veles/media, or <VELES_HOME>/cache/media when a home is forced.
# Regenerable by definition (every entry is a conversion of a file that
# still exists), one directory the user can delete, and never beside the
# original: the source may be read-only, on a mount, or somewhere we have
# no business writing.
#
# This used to hardcode ~/.veles/cache/media, which made the desktop the
# one thing still writing into the pre-split home: every launch recreated
# the tree the gateway's migration had just cleaned, and re-armed its
# collision check against a directory that held nothing but this cache.
proc ::vmedia::cache_dir {} {
    return [file join [::vpaths::cache_dir] media]
}

# Deterministic name for one source file's conversion.
#
# Keyed on the path AND its mtime, so editing or replacing the original
# produces a different cache entry rather than serving a stale picture.
# crc32 is built into Tcl 8.6 (no tcllib dependency in a code path the
# transcript needs) and collisions here are harmless: a wrong hit would
# have to match path, mtime and basename at once.
proc ::vmedia::_cache_path {path} {
    set stem [file rootname [file tail $path]]
    set stem [string map {" " _ / _} $stem]
    if {[string length $stem] > 40} { set stem [string range $stem 0 39] }
    set mt 0
    catch { set mt [file mtime $path] }
    set key [format %08x [zlib crc32 $path]]
    return [file join [cache_dir] "$stem-$key-$mt.png"]
}

# Convert PATH to a PNG and return that PNG's path, or "" with the reason
# in ::vmedia::last_error.
proc ::vmedia::transcode {path} {
    variable last_error ""
    variable max_convert_bytes
    set rule [_converter_for $path]
    if {$rule eq ""} {
        set fmt [string trim [string toupper [_ext $path]] "."]
        # Two different failures, and the distinction matters: nobody can
        # read this, versus the reader is simply not installed here.
        variable converters
        set last_error [expr {[dict exists $converters [_ext $path]]
            ? "no $fmt decoder installed"
            : "no decoder for $fmt"}]
        return ""
    }
    if {![file isfile $path]} { set last_error "not found" ; return "" }
    set size 0
    catch { set size [file size $path] }
    if {$size > $max_convert_bytes} {
        set last_error "[human_size $size] is too large to convert"
        return ""
    }
    set out [_cache_path $path]
    # A hit is only a hit if it is newer than the source; _cache_path
    # already folds mtime in, so this is the belt to that braces.
    if {[file isfile $out] && [file mtime $out] >= [file mtime $path]} {
        return $out
    }
    if {[catch { file mkdir [cache_dir] } e]} {
        set last_error "cannot write the media cache: $e"
        return ""
    }
    lassign $rule exe argv
    set cmd [list $exe]
    foreach a $argv {
        switch -- $a {
            IN  { lappend cmd [::vsafe::arg $path] }
            OUT { lappend cmd [::vsafe::arg $out] }
            default { lappend cmd $a }
        }
    }
    # 2>@1 rather than -ignorestderr: dwebp and heif-dec narrate on
    # SUCCESS ("Decoded x.webp. Dimensions: … Now saving…"), and Tcl
    # turns any stderr output into an error. Merging it into the captured
    # result both silences that chatter — it would otherwise land on the
    # app's terminal mid-render — and keeps it available in $e when the
    # exit status says something really did go wrong.
    if {[catch { exec {*}$cmd 2>@1 } e]} {
        catch { file delete -force $out }
        # Converter chatter is long and useless in a transcript; the last
        # line is where the actual complaint is.
        set last_error [lindex [split [string trim $e] "\n"] end]
        if {$last_error eq ""} { set last_error "conversion failed" }
        return ""
    }
    if {![file isfile $out] || [file size $out] == 0} {
        catch { file delete -force $out }
        set last_error "the converter produced nothing"
        return ""
    }
    _cache_prune
    return $out
}

# Keep the cache from growing without bound: a session that scrolls past
# a hundred screenshots should not leave a hundred PNGs in VELES_HOME
# forever. Oldest first, and only when there is something to do.
proc ::vmedia::_cache_prune {{keep 128}} {
    set dir [cache_dir]
    if {![file isdirectory $dir]} { return }
    set files [glob -nocomplain -directory $dir *.png]
    if {[llength $files] <= $keep} { return }
    set stamped {}
    foreach f $files {
        set mt 0
        catch { set mt [file mtime $f] }
        lappend stamped [list $mt $f]
    }
    set stamped [lsort -integer -index 0 $stamped]
    set drop [expr {[llength $stamped] - $keep}]
    foreach entry [lrange $stamped 0 [expr {$drop - 1}]] {
        catch { file delete -force [lindex $entry 1] }
    }
    return
}

# ── the artifact card ────────────────────────────────────────────────

# Insert PATH into text widget W: the picture if we can draw it, an
# artifact card if we cannot.
#
#   -tagprefix  extra tag applied to everything inserted, so a caller
#               that rebuilds selectively can find its own runs
# The single run-insert primitive for media. Every byte ::vmedia writes
# into a text widget goes through here, so "how a media run is written"
# is decided in ONE place (the transcript's ::vchat::_run and markdown's
# ::vmd::_emit are the sibling chokepoints). Callers hold the widget
# writable; this is a pure pass-through, so it adds no state toggling.
proc ::vmedia::_emit {w text {tags {}}} {
    $w insert end $text $tags
}

proc ::vmedia::insert {w path args} {
    array set o {-tagprefix "" -maxw "" -maxh ""}
    array set o $args
    if {[renderable $path] && [file isfile $path]} {
        set im [load $path $o(-maxw) $o(-maxh)]
        if {$im ne ""} {
            $w image create end -image $im -padx 4 -pady 3
            _emit $w "\n"
            card $w $path -tagprefix $o(-tagprefix) -caption 1
            return $im
        }
        # Fell through: a .png that is not really a PNG, a truncated
        # download. The card says what went wrong instead of nothing.
    }
    card $w $path -tagprefix $o(-tagprefix)
    return ""
}

# The one-line card, on its own.
#
#   -caption  the picture is already above it, so this is a caption with
#             actions rather than the whole story
#   -preview  offer the Preview action (off inside the preview pane,
#             where it would just re-preview what you are looking at)
proc ::vmedia::card {w path args} {
    variable seq
    variable act
    variable last_error
    array set o {-tagprefix "" -caption 0 -preview 1}
    array set o $args
    set prefix $o(-tagprefix)
    set inlined $o(-caption)
    set exists [file isfile $path]
    set name [file tail $path]
    if {$name eq ""} { set name $path }

    set base [list status]
    if {$prefix ne ""} { lappend base $prefix }

    if {!$exists} {
        _emit $w "📎 $name — not found: $path\n" $base
        return
    }

    # `file size` throws on a path that vanished between the isfile
    # check and here, or that we cannot stat. A card is painted from
    # inside the transcript render — a throw there kills the whole
    # message, so an unknown size is a "?", never an error.
    set size "?"
    catch { set size [human_size [file size $path]] }
    if {$inlined} {
        _emit $w "   $name · $size · " $base
    } else {
        # A file we could have drawn but did not — a truncated PNG, a
        # .png that is really something else — has to say so. A .webp has
        # nothing to explain: label_for already reads "(no decoder)".
        set why ""
        if {[renderable $path] && [info exists last_error] && $last_error ne ""} {
            set why " · cannot display: $last_error"
        }
        _emit $w "📎 $name · [label_for $path] · $size$why · " $base
    }

    set actions [list Open open]
    lappend actions "Save as…" save
    if {$o(-preview) && [_previewable $path]} { lappend actions Preview preview }
    lappend actions "Copy path" copypath

    set first 1
    foreach {text action} $actions {
        if {!$first} { _emit $w " · " $base }
        set first 0
        incr seq
        set tag "vmedia_a$seq"
        set act($tag) [list $action $path]
        _emit $w $text [concat $base [list $tag]]
        $w tag configure $tag -foreground $::vt::T(cyan) -underline 1
        $w tag bind $tag <Button-1> [list ::vmedia::_click $tag]
        $w tag bind $tag <Enter> [list $w configure -cursor hand2]
        $w tag bind $tag <Leave> [list $w configure -cursor xterm]
        $w tag raise $tag
    }
    _emit $w "\n" $base
    return
}

# Preview is only offered when the preview pane can do something real
# with the file — offering it for a .zip would be a button that shows you
# "binary — not previewable".
proc ::vmedia::_previewable {path} {
    if {[renderable $path]} { return 1 }
    return [expr {[_ext $path] in
        {.txt .md .markdown .mdown .mkd .mdwn .log .tcl .rs .py .sh .c .h
         .js .ts .lua .json .toml .yaml .yml .xml .csv .diff .patch}}]
}

proc ::vmedia::_click {tag} {
    variable act
    if {![info exists act($tag)]} { return }
    lassign $act($tag) action path
    switch -- $action {
        open      { open_external $path }
        save      { save_as $path }
        preview   { preview $path }
        copypath  {
            clipboard clear
            clipboard append $path
            ::vstatus::set_msg [format [::vi18n::mc {copied path: %s}] $path]
        }
    }
    return
}

# Hand the file to the desktop. Detached, because a viewer that takes two
# seconds to start must not freeze the transcript.
proc ::vmedia::open_external {path} {
    if {![file exists $path]} {
        ::vstatus::set_msg [format [::vi18n::mc {no such file: %s}] $path]
        return 0
    }
    if {$::tcl_platform(platform) eq "windows"} {
        set start [auto_execok start]
        if {$start ne "" && ![catch { exec {*}$start "" [::vsafe::arg $path] & }]} {
            return 1
        }
    }
    foreach opener {xdg-open open} {
        if {[auto_execok $opener] ne ""} {
            # `file exists` above already proves this is a real path, so
            # it cannot be an exec operator — but it can begin with `-`
            # and become a flag to the opener, and routing it through
            # `spawn` is what lets uitest assert no bare `exec` with a
            # non-literal argument survives anywhere in the desktop.
            if {![::vsafe::spawn $opener $path]} {
                ::vstatus::set_msg [format [::vi18n::mc {could not open %s}] [file tail $path]]
                return 0
            }
            ::vstatus::set_msg [format [::vi18n::mc {opened %s}] [file tail $path]]
            return 1
        }
    }
    ::vstatus::set_msg [::vi18n::mc {no xdg-open — path copied instead}]
    clipboard clear
    clipboard append $path
    return 0
}

proc ::vmedia::save_as {path} {
    if {![file exists $path]} {
        ::vstatus::set_msg [format [::vi18n::mc {no such file: %s}] $path]
        return 0
    }
    set dest [tk_getSaveFile -initialfile [file tail $path] \
        -title [::vi18n::mc {Save attachment as}]]
    if {$dest eq ""} { return 0 }
    if {[catch { file copy -force -- $path $dest } e]} {
        ::vstatus::set_msg [format [::vi18n::mc {save failed: %s}] $e]
        return 0
    }
    ::vstatus::set_msg [format [::vi18n::mc {saved to %s}] $dest]
    return 1
}

proc ::vmedia::preview {path} {
    if {[catch { ::vpanes::open_file_preview $path } e]} {
        ::vstatus::set_msg [format [::vi18n::mc {preview failed: %s}] $e]
        return 0
    }
    catch { ::vpanes::show preview }
    return 1
}

# Drop every image embedded in W, and forget the card actions that
# pointed into it. Named Tk images are not garbage-collected: a
# transcript that is rebuilt on every source-toggle leaks one photo per
# attachment per rebuild without this.
proc ::vmedia::forget {w} {
    variable act
    if {[winfo exists $w]} {
        # The decode cache OWNS its photos across rebuilds — deleting
        # them here would turn every session switch into a full
        # re-decode, which is the exact cost the cache exists to kill.
        set keep {}
        variable photo_cache
        if {[info exists photo_cache]} { set keep [dict values $photo_cache] }
        foreach im [$w image names] {
            if {[lsearch -exact $keep $im] >= 0} { continue }
            catch { image delete $im }
        }
        foreach t [$w tag names] {
            if {[string match "vmedia_a*" $t]} {
                catch { $w tag delete $t }
                catch { unset act($t) }
            }
        }
    }
    return
}
