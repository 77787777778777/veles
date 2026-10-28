# veles-desktop/lib/mediatest.tcl — an attachment is a thing you can use.
#
# USER: "model-to-user images would also be great, and attachments/
# artifacts in general. I didn't mean just model-to-user MEDIA."
#
# Before ::vmedia the transcript could inline a PNG or a GIF, at native
# size, and for everything else wrote one dead line:
#
#     📎 media: /home/user/shot.jpg
#
# — no size, no way to open it, save it or preview it, and a JPEG (the
# commonest artifact of the lot) never showed at all. A 1280×1577 photo
# that DID decode shoved the transcript sideways.
#
# These tests pin the three things that fixed:
#
#   * breadth  — tkimg's decoders are actually reachable, so a .jpg is a
#                picture and not a filename;
#   * fitting  — an oversized image is subsampled to the display cap
#                BEFORE it goes in;
#   * actions  — anything we cannot draw still gets a card with Open /
#                Save as… / Preview / Copy path bound to real commands.
#
# The regression these guard against is silent: a stale extension list,
# a lost `package require`, or a card whose buttons are decoration would
# all still "render something". So the assertions are on the decoded
# geometry, on the tag bindings, and on the click actually firing.
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

# A failing ::vmedia::load returns "", and `image width ""` THROWS —
# which aborts the script before it prints a tally, and CI reads a
# missing tally as a broken run rather than as the specific regression.
# So every assertion about a photo goes through these.
proc imw {im} { if {$im eq ""} { return -1 } ; if {[catch {image width $im} w]} { return -1 } ; return $w }
proc imh {im} { if {$im eq ""} { return -1 } ; if {[catch {image height $im} h]} { return -1 } ; return $h }
proc imfree {im} { catch { image delete $im } }

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

source [file join $_dir safeexec.tcl]
source [file join $_dir paths.tcl]
source [file join $_dir i18n.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
namespace eval ::vstatus {
    variable last ""
    proc set_msg {m} { set ::vstatus::last $m }
}
namespace eval ::vpanes {
    variable previewed ""
    variable shown ""
    proc open_file_preview {p} { set ::vpanes::previewed $p }
    proc show {t} { set ::vpanes::shown $t }
}

source [file join $_dir media.tcl]

# ── fixtures ─────────────────────────────────────────────────────────
# Written to a scratch dir, never into the repo: an un-ignored build
# artifact in a flake repo rides `src = ./.` into /nix/store.
set tmp [file join [expr {[info exists ::env(TMPDIR)] ? $::env(TMPDIR) : "/tmp"}] \
    "veles-mediatest-[pid]"]
file mkdir $tmp

# A deliberately oversized PNG — the display cap is 560×460, so this
# must come back subsampled or the "fits the pane" claim is empty.
set big_png [file join $tmp big.png]
set gen [image create photo -width 1400 -height 1100]
$gen put "#ff00aa" -to 0 0 1400 1100
$gen write $big_png -format png
set small_png [file join $tmp small.png]
set gen2 [image create photo -width 40 -height 30]
$gen2 put "#00ffaa" -to 0 0 40 30
$gen2 write $small_png -format png

# A .webp that is really PNG bytes. Used ONLY for the no-converter
# branch (::vmedia::convertible is stubbed off for those cases) — a real
# webp is built further down, where the decoder is exercised for real.
set webp [file join $tmp shot.webp]
file copy -force $small_png $webp

set txt [file join $tmp notes.md]
set fh [open $txt w] ; puts $fh "# hello" ; close $fh
set zip [file join $tmp bundle.zip]
set fh [open $zip w] ; puts -nonewline $fh "PK\x03\x04not-really" ; close $fh
set missing [file join $tmp nope.png]

wm geometry . 900x600+0+0
text .log -wrap word
pack .log
update idletasks

puts "the decoders we claim are the decoders we have"
set have [::vmedia::have_img]
ok "have_img answers without throwing"   {$have in {0 1}}
ok "PNG is renderable with or without Img" {[::vmedia::renderable $::small_png]}
ok "a .webp is an image"                  {[::vmedia::is_image $::webp]}
ok "…that Tk cannot read itself"          {![::vmedia::decodes_natively $::webp]}
ok "a .zip is not an image at all"        {![::vmedia::is_image $::zip]}
ok "…and reads as an archive"             {[string match "*archive*" [::vmedia::label_for $::zip]]}
# .psd is in the image list with no converter anywhere — the honest
# "nothing can read this" case, distinct from "the reader is missing".
set psd [file join $tmp layers.psd]
file copy -force $small_png $psd
ok "a format with no converter is not renderable" {![::vmedia::renderable $::psd]}
ok "…and its label says no decoder"       {[string match "*no decoder*" [::vmedia::label_for $::psd]]}

# tkimg is in the flake, so on the gate this branch is the real one. Off
# the gate (bare wish) the suite must still be honest rather than red.
if {$have} {
    set jpg [file join $tmp photo.jpg]
    $gen2 write $jpg -format jpeg
    ok "JPEG is renderable when Img loaded" {[::vmedia::renderable $::jpg]}
    ok "…and actually decodes"              {[::vmedia::load $::jpg] ne ""}
    ok "…labelled JPEG, not JPG"            {[::vmedia::label_for $::jpg] eq "JPEG image"}
} else {
    puts "  (Img absent — JPEG assertions skipped, see flake.nix tclPackages.tkimg)"
    ok "without Img, a JPEG is honestly un-renderable" \
        {![::vmedia::renderable [file join $::tmp x.jpg]]}
    ok "…and says so in its label" \
        {[string match "*no decoder*" [::vmedia::label_for [file join $::tmp x.jpg]]]}
    ok "…rather than pretending"  {[::vmedia::load [file join $::tmp x.jpg]] eq ""}
}

puts "\nan oversized image is fitted before it goes in"
set im [::vmedia::load $big_png]
ok "the big PNG decodes"                 {$im ne ""}
ok "…and is capped to the display width" {[imw $im] <= 560}
ok "…and to the display height"          {[imh $im] <= 460}
ok "…without collapsing to nothing"      {[imw $im] > 100}
imfree $im
set im2 [::vmedia::load $small_png]
ok "a small image is left alone"         {[imw $im2] == 40 && [imh $im2] == 30}
imfree $im2
set im3 [::vmedia::load $big_png 200 0]
ok "an explicit cap is honoured"         {[imw $im3] <= 200}
ok "…and a zero height means no height cap" {[imh $im3] > 0}
imfree $im3

puts "\nwhat cannot be decoded still gets a usable card"
.log delete 1.0 end
::vmedia::insert .log $webp
set body [.log get 1.0 end]
ok "no image was embedded for a webp"    {[llength [.log image names]] == 0}
ok "the card names the file"             {[string match "*shot.webp*" $body]}
ok "…says what it is"                    {[string match "*WebP*" $body]}
ok "…and how big"                        {[string match "*B *" $body] || [string match "*KB*" $body]}
foreach a {Open "Save as…" "Copy path"} {
    ok "…and offers $a"                  {[string match "*$a*" $::body]}
}

puts "\nthe card's actions are bound to real commands"
set act_tags {}
foreach t [.log tag names] { if {[string match "vmedia_a*" $t]} { lappend act_tags $t } }
ok "every action is its own tag"         {[llength $act_tags] >= 3}
set bound 1
foreach t $act_tags { if {[.log tag bind $t <Button-1>] eq ""} { set bound 0 } }
ok "…and every one has a click binding"  {$bound}

# Copy path is the action with no side effects outside the app, so it is
# the one we can fire end-to-end.
set copy_tag ""
foreach t $act_tags {
    if {[lindex $::vmedia::act($t) 0] eq "copypath"} { set copy_tag $t }
}
ok "the Copy path action is registered"  {$copy_tag ne ""}
clipboard clear
::vmedia::_click $copy_tag
ok "…and clicking it copies the path"    {[clipboard get] eq $::webp}
ok "…and says so"                        {[string match "*copied path*" $::vstatus::last]}

puts "\npreview is offered only when the preview pane can do something"
.log delete 1.0 end
::vmedia::card .log $txt
ok "a markdown file offers Preview"      {[string match "*Preview*" [.log get 1.0 end]]}
.log delete 1.0 end
::vmedia::card .log $zip
ok "an archive does not"                 {![string match "*Preview*" [.log get 1.0 end]]}
.log delete 1.0 end
::vmedia::card .log $small_png -preview 0
ok "…and -preview 0 suppresses it"       {![string match "*Preview*" [.log get 1.0 end]]}

.log delete 1.0 end
::vmedia::card .log $txt
set ptag ""
foreach t [.log tag names] {
    if {[string match "vmedia_a*" $t] && [lindex $::vmedia::act($t) 0] eq "preview"} { set ptag $t }
}
set ::vpanes::previewed ""
::vmedia::_click $ptag
ok "clicking Preview routes to the preview pane" {$::vpanes::previewed eq $::txt}
ok "…and brings the pane up"             {$::vpanes::shown eq "preview"}

puts "\na file that is not there says so, and offers nothing"
.log delete 1.0 end
::vmedia::insert .log $missing
set body [.log get 1.0 end]
ok "the card says not found"             {[string match "*not found*" $body]}
ok "…and offers no actions"              {![string match "*Save as*" $body]}
ok "…and embeds no image"                {[llength [.log image names]] == 0}

puts "\na renderable image is drawn AND captioned"
.log delete 1.0 end
::vmedia::insert .log $small_png
ok "the picture is embedded"             {[llength [.log image names]] == 1}
set body [.log get 1.0 end]
ok "…with the file name under it"        {[string match "*small.png*" $body]}
ok "…and Save as… beside it"             {[string match "*Save as*" $body]}

puts "\nforget drops the stray photos and the actions"
# The leak is GLOBAL Tk image objects, not the widget's embed list: a
# named photo survives `delete 1.0 end` and is never garbage-collected,
# so a transcript rebuilt on every source-toggle grew one per attachment
# per rebuild. The FIX turned that rebuild cost into a feature: photos
# that came through ::vmedia::load are CACHED and must survive forget —
# re-decoding every attachment on every session switch was the "lot of
# cpu time" complaint. Everything else forget created is still freed.
set owned [.log image names]
ok "the transcript owns a photo"          {[llength $owned] == 1}
# A photo NOT from the cache — the old behaviour must still hold for
# anything forget alone owns.
set stray [image create photo]
.log image create end -image $stray
::vmedia::forget .log
ok "…and forget frees an uncached photo" {[lsearch -exact [image names] $stray] < 0}
set survivors 0
foreach im $owned { if {$im in [image names]} { incr survivors } }
ok "…but keeps the cache-owned one (that IS the rebuild cache)" \
    {$survivors == 1}
set left 0
foreach t [.log tag names] { if {[string match "vmedia_a*" $t]} { incr left } }
ok "…and leaves no orphan action tags"    {$left == 0}
# The cache itself is bounded, and a reset clears it.
::vmedia::cache_reset
set after_reset 0
foreach im $owned { if {$im in [image names]} { incr after_reset } }
ok "…and cache_reset frees them all" {$after_reset == 0}

puts "\nformats with no Tk handler go through a converter"
#
# USER: "couldnt we add a webp decoder? surely there is a library, even
# if it's not core." tkimg has no webp/heic/avif/svg reader and no Tcl
# extension provides one, so ::vmedia shells out to dwebp / heif-dec /
# resvg and caches the PNG. These pin the cascade end to end: a REAL
# webp, decoded by the real binary, cached, and re-served from cache.

# An SVG needs no encoder to build — it is text — so it is the one
# converter case that is always constructible here.
set svg [file join $tmp diagram.svg]
set fh [open $svg w]
puts -nonewline $fh {<svg xmlns="http://www.w3.org/2000/svg" width="240" height="160"><rect width="240" height="160" fill="#ff00aa"/><circle cx="120" cy="80" r="50" fill="#00ffcc"/></svg>}
close $fh

# Point the cache at the scratch dir: a test must not write into the
# user's VELES_HOME, and it must not depend on what is already there.
set ::env(VELES_HOME) [file join $tmp home]

set have_resvg [expr {[lindex [auto_execok resvg] 0] ne ""}]
if {$have_resvg} {
    ok "an SVG is renderable via its converter" {[::vmedia::renderable $::svg]}
    ok "…without Tk being able to read it"      {![::vmedia::decodes_natively $::svg]}
    ok "…and the label drops the caveat"        {[::vmedia::label_for $::svg] eq "SVG image"}
    set png [::vmedia::transcode $svg]
    ok "…it converts to a real PNG"             {$::png ne "" && [file isfile $::png]}
    ok "…inside the media cache, not beside the source" \
        {[string match "*[file join cache media]*" $::png]}
    set im [::vmedia::load $svg]
    ok "…and load returns a photo for it"       {$::im ne ""}
    ok "…of the right size"                     {[imw $::im] == 240}
    imfree $im

    # A second call must NOT re-run the converter: that is the whole
    # point of the cache, and a transcript rebuild renders every
    # attachment again.
    #
    # Comparing paths or mtimes proves nothing — a re-run writes the same
    # path, inside the same one-second mtime tick. So the cached file is
    # stamped with content the converter would never produce; if it comes
    # back the cache was used, and if it is gone the converter ran.
    set fh [open $png a] ; puts -nonewline $fh "VELES-CACHE-SENTINEL" ; close $fh
    set marked [file size $png]
    set again [::vmedia::transcode $svg]
    ok "a second render reuses the cached PNG"  {$::again eq $::png}
    ok "…without re-running the converter"      {[file size $::again] == $::marked}

    # Editing the source must invalidate it, or you look at a stale
    # picture forever. mtime is set explicitly rather than slept for: a
    # suite that pauses a second per cache assertion stops being run.
    set fh [open $svg w]
    puts -nonewline $fh {<svg xmlns="http://www.w3.org/2000/svg" width="60" height="60"><rect width="60" height="60" fill="#00ff00"/></svg>}
    close $fh
    file mtime $svg [expr {[file mtime $svg] + 60}]
    set third [::vmedia::transcode $svg]
    ok "…but editing the source re-converts"    {$::third ne "" && $::third ne $::png}
    set im [::vmedia::load $svg]
    ok "…and the new picture is the new size"   {[imw $::im] == 60}
    imfree $im
} else {
    puts "  (resvg absent — SVG conversion skipped, see flake.nix imageDecoders)"
    ok "without resvg an SVG is not renderable" {![::vmedia::renderable $::svg]}
    ok "…and says the decoder is not installed" \
        {[string match "*not installed*" [::vmedia::label_for $::svg]]}
}

# The real thing the user asked about. cwebp ships with libwebp, so
# wherever dwebp exists an encoder does too and the fixture is real.
if {[lindex [auto_execok cwebp] 0] ne "" && [lindex [auto_execok dwebp] 0] ne ""} {
    set real_webp [file join $tmp real.webp]
    exec -ignorestderr cwebp -quiet $big_png -o $real_webp
    ok "a real webp is renderable"          {[::vmedia::renderable $::real_webp]}
    ok "…labelled WebP with no caveat"      {[::vmedia::label_for $::real_webp] eq "WebP image"}
    set im [::vmedia::load $real_webp]
    ok "…decodes to a photo"                {$::im ne ""}
    ok "…scaled to the display cap like any other image" \
        {[imw $::im] <= 560}
    imfree $im

    # And it inlines: a webp in a reply is a picture now, not a card.
    .log delete 1.0 end
    ::vmedia::insert .log $real_webp
    ok "…and inlines in the transcript"     {[llength [.log image names]] == 1}
    ::vmedia::forget .log
} else {
    puts "  (libwebp absent — webp conversion skipped, see flake.nix imageDecoders)"
    ok "without dwebp a webp is not renderable" {![::vmedia::renderable $::webp]}
}

puts "\na converter that is absent is not a capability"
# The distinction the card has to make: nobody can read this, versus the
# reader is not installed HERE. Force the second case by pointing the
# rule at an executable that cannot exist.
set ::vmedia::converters [dict replace $::vmedia::converters .webp {veles-no-such-decoder {IN OUT}}]
array unset ::vmedia::exe_cache
ok "a missing converter is not renderable" {![::vmedia::renderable $::webp]}
ok "…and the label says not installed"     {[string match "*not installed*" [::vmedia::label_for $::webp]]}
ok "…and transcode explains which"         {[::vmedia::transcode $::webp] eq ""}
ok "…in the error, by name"                {[string match "*WEBP decoder installed*" $::vmedia::last_error]}
.log delete 1.0 end
::vmedia::insert .log $webp
ok "…and it falls back to a card"          {[llength [.log image names]] == 0}
ok "…that still offers Open"               {[string match "*Open*" [.log get 1.0 end]]}

puts "\nthe cache does not grow forever"
set cdir [::vmedia::cache_dir]
file mkdir $cdir
for {set i 0} {$i < 140} {incr i} {
    set f [file join $cdir "filler$i.png"]
    set fh [open $f w] ; puts -nonewline $fh "x" ; close $fh
}
ok "the cache is over the keep limit" {[llength [glob -nocomplain -directory $cdir *.png]] > 128}
::vmedia::_cache_prune
ok "…and pruning brings it back down" {[llength [glob -nocomplain -directory $cdir *.png]] <= 128}

puts "\nsizes read like sizes"
ok "bytes"      {[::vmedia::human_size 512] eq "512 B"}
ok "kilobytes"  {[::vmedia::human_size 4096] eq "4 KB"}
ok "megabytes"  {[::vmedia::human_size 1572864] eq "1.5 MB"}

catch { image delete $gen }
catch { image delete $gen2 }
catch { file delete -force $tmp }

puts "\nmediatest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
