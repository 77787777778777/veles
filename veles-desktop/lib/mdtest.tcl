# veles-desktop/lib/mdtest.tcl — assertions for the markdown renderer.
#
# Run:  wish veles-desktop/lib/mdtest.tcl   (needs a DISPLAY; Tk text
#       widgets are the thing under test, so there is no headless path)
#
# Exists because clicking a link in the desktop app did nothing for as
# long as links have existed: the <Button-1> binding passes `@x,y`
# SCREEN COORDINATES, and _link_click treated that string as a text
# index. Nothing in the Tcl side was tested, so it went unnoticed.

set _dir [file dirname [file normalize [info script]]]
source [file join $_dir safeexec.tcl]
source [file join $_dir paths.tcl]
source [file join $_dir i18n.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir media.tcl]
source [file join $_dir markdown.tcl]

set failed 0
set passed 0
proc ok {name cond} {
    # `cond` arrives as an unevaluated expression (braced at the call
    # site), so evaluate it in the caller's scope.
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name"
        incr ::failed
    }
}

# On a phone the real opener fires a VIEW intent through AndroWish's
# `borg` — and still refuses what the allow-list refuses. Before the stub
# below replaces it, with a `borg` that records instead of launching.
puts "\nlinks on a phone open with an intent"
namespace eval ::vstatus {}
proc ::vstatus::set_msg {m} {}
# NOTHING here may launch a real opener. The real `open_url` runs in this
# block, and if its phone branch ever regresses it falls through to
# xdg-open — which hands the URL to the browser in the developer's own
# session over the session bus, Xvfb or not (a mutation run opened
# https://example.org/a in the USER's browser, 2026-10-11). So the spawn
# is recorded, never made, and the test says if one was attempted.
rename ::vsafe::spawn ::vsafe::_real_spawn
set ::spawned {}
proc ::vsafe::spawn {cmd args} { lappend ::spawned [list $cmd {*}$args] ; return 1 }
set ::borg_calls {}
proc ::borg {args} { lappend ::borg_calls $args ; return "" }
ok "an https link opens through borg's VIEW intent" {
    [::vmd::open_url https://example.org/a] == 1
    && $::borg_calls eq {{activity android.intent.action.VIEW https://example.org/a {}}}
}
set ::borg_calls {}
ok "…and a javascript: link is refused before it reaches Android" {
    [::vmd::open_url {javascript:alert(1)}] == 0 && $::borg_calls eq {}
}
ok "…and no desktop opener was started on a phone" {
    $::spawned eq {}
}
rename ::borg {}
rename ::vsafe::spawn {}
rename ::vsafe::_real_spawn ::vsafe::spawn

# Stub the browser so tests never actually launch one.
set ::opened {}
proc ::vmd::open_url {url} { lappend ::opened $url; return 1 }
namespace eval ::vstatus {}
proc ::vstatus::set_msg {m} {}

# `bbox` needs the widget MAPPED, so this test must have a window — but
# it must never appear on, or steal focus from, a real desktop. Run it
# under Xvfb (scripts/tcl_tests.sh does that); refuse to run against a
# live DISPLAY unless explicitly forced.
if {[info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne ""} {
    # caller accepts a visible window
} elseif {![info exists ::env(XVFB_RUNNING)]} {
    puts "SKIP: refusing to map a window on a real display."
    puts "      run scripts/tcl_tests.sh (uses Xvfb), or set"
    puts "      VELES_TK_TEST_FORCE=1 to accept a visible test window."
    exit 0
}
wm geometry . 900x600+0+0
# Never take focus from whatever the user is doing.
catch { wm attributes . -topmost 0 }
text .t -width 80 -height 30
pack .t
update

puts "markdown link tests"

# 1. [text](url) is clickable and resolves to its own URL.
.t configure -state normal
.t delete 1.0 end
::vmd::render .t "see \[the docs\](https://example.com/a) for more"
update
set idx [lsearch -exact [split [.t get 1.0 end] ""] x]
# Click in the middle of the link label.
set link_idx ""
foreach tag [.t tag names] {
    if {[string match "mdlink_*" $tag]} {
        set r [.t tag ranges $tag]
        if {[llength $r] >= 2} { set link_idx [lindex $r 0] }
    }
}
ok "link got its own tag" {$link_idx ne ""}
if {$link_idx ne ""} {
    # Convert the text index to a screen coordinate, then feed the proc
    # the SAME @x,y form the real binding does.
    lassign [.t bbox $link_idx] bx by bw bh
    set ::opened {}
    ::vmd::_link_click .t "@[expr {$bx + 1}],[expr {$by + 1}]"
    ok "click on \[text\](url) opens the url" \
        {[lindex $::opened 0] eq "https://example.com/a"}
}

# 2. A bare URL autolinks.
.t configure -state normal
.t delete 1.0 end
::vmd::forget_links .t
::vmd::render .t "raw https://example.com/bare here"
update
set link_idx ""
foreach tag [.t tag names] {
    if {[string match "mdlink_*" $tag]} {
        set r [.t tag ranges $tag]
        if {[llength $r] >= 2} { set link_idx [lindex $r 0] }
    }
}
ok "bare url is linkified" {$link_idx ne ""}
if {$link_idx ne ""} {
    lassign [.t bbox $link_idx] bx by bw bh
    set ::opened {}
    ::vmd::_link_click .t "@[expr {$bx + 1}],[expr {$by + 1}]"
    ok "click on a bare url opens it" \
        {[lindex $::opened 0] eq "https://example.com/bare"}
}

# 3. Two links on ONE line resolve independently — the old line-number
#    map could only remember one per line.
.t configure -state normal
.t delete 1.0 end
::vmd::forget_links .t
::vmd::render .t "\[one\](https://example.com/1) and \[two\](https://example.com/2)"
update
set tags {}
foreach tag [lsort [.t tag names]] {
    if {[string match "mdlink_*" $tag]} { lappend tags $tag }
}
ok "two links on one line get two tags" {[llength $tags] == 2}
if {[llength $tags] == 2} {
    set urls {}
    foreach tag $tags {
        set r [.t tag ranges $tag]
        lassign [.t bbox [lindex $r 0]] bx by bw bh
        set ::opened {}
        ::vmd::_link_click .t "@[expr {$bx + 1}],[expr {$by + 1}]"
        lappend urls [lindex $::opened 0]
    }
    ok "each link opens its own url" \
        {[lsort $urls] eq {https://example.com/1 https://example.com/2}}
}

# 4. Clicking ordinary text opens nothing.
.t configure -state normal
.t delete 1.0 end
::vmd::forget_links .t
::vmd::render .t "just prose, no links at all"
update
lassign [.t bbox 1.2] bx by bw bh
set ::opened {}
::vmd::_link_click .t "@[expr {$bx + 1}],[expr {$by + 1}]"
ok "plain text click opens nothing" {[llength $::opened] == 0}

# 5. Links survive text being inserted ABOVE them — the exact case the
#    line-number map got wrong.
.t configure -state normal
.t delete 1.0 end
::vmd::forget_links .t
::vmd::render .t "\[deep\](https://example.com/deep)"
update
.t insert 1.0 "a\nb\nc\nd\ne\n"
update
set link_idx ""
foreach tag [.t tag names] {
    if {[string match "mdlink_*" $tag]} {
        set r [.t tag ranges $tag]
        if {[llength $r] >= 2} { set link_idx [lindex $r 0] }
    }
}
if {$link_idx ne ""} {
    lassign [.t bbox $link_idx] bx by bw bh
    set ::opened {}
    ::vmd::_link_click .t "@[expr {$bx + 1}],[expr {$by + 1}]"
    ok "link still resolves after lines are inserted above" \
        {[lindex $::opened 0] eq "https://example.com/deep"}
} else {
    ok "link still resolves after lines are inserted above" 0
}

# ── theme mapping (engine JSON → ::vt::T) ────────────────────────────
# 4. A link INSIDE EMPHASIS must still be a link.
#
# `**see [docs](url)**` rendered as the raw markdown — brackets, parens
# and URL — for as long as the renderer has existed. The bold span
# starts before the link, so it wins _inline's earliest-match race, and
# the bold branch then inserted its whole inner span as one plain
# md_bold run without looking inside it. Model output bolds links
# constantly.
puts "\nlinks nested inside emphasis"

proc link_tags {w} {
    set out {}
    foreach tag [lsort [$w tag names]] {
        if {[string match "mdlink_*" $tag] && [llength [$w tag ranges $tag]] >= 2} {
            lappend out $tag
        }
    }
    return $out
}
proc url_of {w tag} {
    set r [$w tag ranges $tag]
    lassign [$w bbox [lindex $r 0]] bx by bw bh
    set ::opened {}
    ::vmd::_link_click $w "@[expr {$bx + 1}],[expr {$by + 1}]"
    return [lindex $::opened 0]
}
proc fresh_md {w text} {
    $w configure -state normal
    $w delete 1.0 end
    ::vmd::forget_links $w
    ::vmd::render $w $text
    update
}

fresh_md .t "**bold \[inside\](https://example.com/inbold) more**"
ok "a link inside **bold** is parsed" {[llength [link_tags .t]] == 1}
ok "…the markdown syntax is gone from the text" \
    {![string match {*](http*} [.t get 1.0 end-1c]]}
ok "…the label is what shows" {[string match "*inside*" [.t get 1.0 end-1c]]}
ok "…and it opens the right url" \
    {[url_of .t [lindex [link_tags .t] 0]] eq "https://example.com/inbold"}
ok "…while the surrounding text stays bold" {[llength [.t tag ranges md_bold]] >= 2}

fresh_md .t "*italic \[inside\](https://example.com/inital) more*"
ok "a link inside *italic* is parsed" {[llength [link_tags .t]] == 1}
ok "…and opens the right url" \
    {[url_of .t [lindex [link_tags .t] 0]] eq "https://example.com/inital"}
ok "…while the surrounding text stays italic" {[llength [.t tag ranges md_italic]] >= 2}

fresh_md .t "**bold with `code` inside**"
ok "inline code inside bold is parsed" {[llength [.t tag ranges md_code]] == 2}
ok "…and the backticks are gone" {![string match "*`*" [.t get 1.0 end-1c]]}

fresh_md .t "**bold https://example.com/inbare end**"
ok "a bare url inside bold is linkified" {[llength [link_tags .t]] == 1}
ok "…and opens" {[url_of .t [lindex [link_tags .t] 0]] eq "https://example.com/inbare"}

# The plain cases must not have regressed by the recursion.
fresh_md .t "**just bold**"
ok "plain bold still has no link tags" {[llength [link_tags .t]] == 0}
ok "…and is still bold" {[llength [.t tag ranges md_bold]] == 2}
fresh_md .t "**a *b* c**"
ok "italic nested in bold still renders as bold-italic" \
    {[llength [.t tag ranges md_bolditalic]] == 2}

puts "\ntheme mapping tests"

namespace eval ::vapi {}
proc ::vapi::get_json {path} {
    if {$path ne "/api/themes"} { error "unexpected $path" }
    return [dict create themes [list [dict create \
        name "testtheme" bg "#010203" panel "#111213" panel_hi "#212223" \
        hair "#313233" pink "#ff00aa" cyan "#00ffee" purple "#aa66ff" \
        gold "#ffcc00" green "#00ff88" red "#ff3355" \
        text "#eeeeee" dim "#888888"]]]
}

array set ::vt::T {}
ok "engine theme is adopted" {[::vt::from_engine testtheme ::vt::T] == 1}
ok "text → fg"        {$::vt::T(fg) eq "#eeeeee"}
ok "dim → fg_dim"     {$::vt::T(fg_dim) eq "#888888"}
ok "panel → bg_panel" {$::vt::T(bg_panel) eq "#111213"}
ok "hair → hairline"  {$::vt::T(hairline) eq "#313233"}
ok "roles derive from accents" \
    {$::vt::T(user) eq "#00ffee" && $::vt::T(reasoning) eq "#aa66ff" \
     && $::vt::T(tool) eq "#ffcc00"}
ok "unknown theme is not adopted" {[::vt::from_engine nope ::vt::T] == 0}

# A gateway that is down must fall back, not crash.
proc ::vapi::get_json {path} { error "connection refused" }
ok "engine unreachable → falls back cleanly" {[::vt::from_engine testtheme ::vt::T] == 0}

# ── markdown images ─────────────────────────────────────────────────
#
# USER (spot test, 2026-08-25): "The xorg logo seems to only render in
# the preview but not in the transcript". There was no image branch in
# the inline parser at all — `![alt](path)` matched the LINK pattern one
# character in, so the `!` was emitted as literal text and the picture
# became a clickable link labelled with its alt text.
#
# These need a real widget (an embedded image is not text), which is why
# they live here and not in the mdspec table.
puts "\nmarkdown images"

# The theme tests above deliberately leave ::vt::T half-populated, and
# ::vmedia's artifact card reads real palette entries from it. Put a
# whole theme back rather than letting a missing colour look like a
# broken image.
catch { ::vt::palette_synthwave ::vt::T }

# A real PNG to point at. Tk can write one out itself, so the test does
# not depend on a fixture file existing in the tree.
set ::imgdir [file join [expr {[info exists ::env(TMPDIR)] && $::env(TMPDIR) ne ""
    ? $::env(TMPDIR) : "/tmp"}] mdtest-img-[pid]]
file mkdir $::imgdir
set ::imgfile [file join $::imgdir dot.png]
set _p [image create photo -width 8 -height 8]
$_p put "#ff0000" -to 0 0 8 8
$_p write $::imgfile -format png
image delete $_p

proc embedded_images {w} { return [llength [$w image names]] }

puts "\nmarkdown table geometry"
text .tabletest -width 80 -height 12
pack .tabletest
.tabletest configure -state normal
.tabletest delete 1.0 end
set table_input [string map [list "\\n" "\n"] {| # | Result | State |\n|---|---|---|\n| 1 | **Title**<br>body line | ready |}]
::vmd::render .tabletest $table_input
update
set table_tags {}
foreach tag [.tabletest tag names] { if {[regexp {^md_table[0-9]+$} $tag]} { lappend table_tags $tag } }
ok "a multiline cell still has a real table tag" {[llength $table_tags] == 1}
set state_index [.tabletest search -exact ready 1.0 end]
lassign [.tabletest bbox $state_index] state_x state_y
set result_index [.tabletest search -exact Result 1.0 2.0]
lassign [.tabletest bbox $result_index] result_x result_y
set body_index [.tabletest search -exact Title 1.0 end]
lassign [.tabletest bbox $body_index] body_x body_y
ok "the neighboring cell is on the first body row" {
    [.tabletest index "$state_index linestart"] eq [.tabletest index "$body_index linestart"]
    && [lindex [.tabletest dlineinfo $state_index] 1] == [lindex [.tabletest dlineinfo $body_index] 1]
}
set state_header_index [.tabletest search -exact State 1.0 2.0]
lassign [.tabletest bbox $state_header_index] state_header_x state_header_y
# bbox is a glyph's INK box, not the display line's box. Under the
# sandbox's Noto fonts bold Title starts at y=41 while ordinary ready
# starts at y=42 on the SAME row. Comparing those values made a sound
# grid fail only in the Nix check; dlineinfo is the line geometry.
ok "the neighboring cell starts at the same x as its own header" {$state_x == $state_header_x}
ok "the table has explicit stops for its two tab-separated columns" {
    [llength [.tabletest tag cget [lindex $table_tags 0] -tabs]] == 4
}
set continuation_index [.tabletest search -exact {body line} 1.0 end]
if {$continuation_index ne ""} {
    lassign [.tabletest bbox $continuation_index] continuation_x continuation_y
} else {
    lassign [list {} {} {} {}] continuation_x continuation_y
}
ok "the wrapped continuation occupies the second cell, after one tab" {
    $continuation_index ne "" && [.tabletest count -chars "$continuation_index linestart" $continuation_index] == 1
}
ok "the wrapped continuation has its own row" {$continuation_y > $body_y}
set table_text [.tabletest get 1.0 end-1c]
ok "the wrapped row is not converted into a bullet record" {![string match "*1. Title*" $table_text]}
ok "the other column is not relabelled as prose" {![string match "*State:*" $table_text]}
ok "escaped pipes stay inside their cell" {[::vmd::_table_cells {| a \| b | c |}] eq {{a | b} c}}

# A long cell must wrap at the width the CURRENT font actually needs.
# A byte-for-byte expectation here hard-coded the host's line break and
# went red under the Nix sandbox's Noto metrics even though the grid was
# correct. Assert the invariant, including the continuation's tab stop.
.tabletest delete 1.0 end
::vmd::forget_links .tabletest
set wide_cell {This is a deliberately long explanation that must wrap within its own cell rather than turn the table into a stack of records.}
set wide_input "| Source | Explanation |\n|---|---|\n| one | $wide_cell |"
::vmd::render .tabletest $wide_input
update
set wide_tags {}
foreach tag [.tabletest tag names] { if {[regexp {^md_table[0-9]+$} $tag]} { lappend wide_tags $tag } }
set wide_text [.tabletest get 1.0 end-1c]
set wide_index [.tabletest search -exact {This is} 1.0 end]
# The continuation's first WORD is stable even when the renderer's
# chosen wrap splits the longer phrase used in the old fixture.
set wide_continue [.tabletest search -exact {turn} "$wide_index +1l" end]
lassign [.tabletest bbox $wide_index] wide_body_x wide_body_y
set wide_continue_ok 0
if {$wide_continue ne ""} {
    # `turn` may be several words into the continuation. Its line's
    # first cell character is the actual tab-stop probe. Do not compare
    # its ink box with the bold header's, whose bearings can differ.
    set wide_col_start [.tabletest index "$wide_continue linestart+1c"]
    lassign [.tabletest bbox $wide_col_start] wide_continue_x wide_continue_y
    set wide_continue_ok [expr {$wide_continue_x == $wide_body_x}]
}
# Restrict the text-preservation assertion to the body, not the header,
# rule, or final blank lines. Whitespace may change at a wrap; words may
# neither disappear nor change order.
set wide_body [string trim [string map [list "\n" " " "\t" " "] \
    [.tabletest get $wide_index end-1c]]]
set wide_body [regsub -all {\s+} $wide_body { }]
ok "a wide table still has a real table tag" {[llength $wide_tags] == 1}
ok "a wide table wraps its long cell" {$wide_index ne "" && $wide_continue ne ""}
ok "the wrapped cell starts in its own column" {
    $wide_index ne "" && [.tabletest get "$wide_index -1c" $wide_index] eq "\t"
}
ok "the wide continuation stays in its own column" {
    $wide_continue ne "" && [.tabletest get "$wide_col_start -1c" $wide_col_start] eq "\t" && $wide_continue_ok
}
ok "the wide continuation has its own display row" {
    $wide_continue ne "" && [.tabletest index "$wide_continue linestart"] ne [.tabletest index "$wide_index linestart"]
}
ok "the wide table has the expected tab stops" {
    [llength $wide_tags] == 1 && [llength [.tabletest tag cget [lindex $wide_tags 0] -tabs]] == 2
}
ok "the wide cell keeps every word in reading order" {
    [regexp {^This is a deliberately long explanation that must wrap within its own cell rather than turn the table into a stack of records\.\s*$} $wide_body]
}
ok "the wide table is not a stacked record" {
    ![string match "*Explanation:*" $wide_text] && ![string match "*1. This is*" $wide_text]
}

destroy .tabletest

.t configure -state normal
.t delete 1.0 end
::vmd::render .t "before !\[a red dot\]($::imgfile) after"
ok "a local image is DRAWN, not linked" {[embedded_images .t] == 1}
ok "…and the ! is not left as literal text" \
    {![string match "*!*" [.t get 1.0 end]]}
ok "…and the alt text is not inserted as prose" \
    {![string match "*a red dot*" [.t get 1.0 end]]}
ok "…while the surrounding text survives" \
    {[string match "*before*" [.t get 1.0 end]]
     && [string match "*after*" [.t get 1.0 end]]}

# file:// is how the agent refers to something it just wrote.
.t configure -state normal
.t delete 1.0 end
::vmd::render .t "!\[dot\](file://$::imgfile)"
ok "a file:// image is drawn too" {[embedded_images .t] == 1}

# A remote one stays a link: rendering it would mean the transcript
# fetches whatever URL a model wrote.
.t configure -state normal
.t delete 1.0 end
::vmd::render .t "!\[remote\](https://e.example/i.png)"
ok "a remote image is NOT fetched" {[embedded_images .t] == 0}
ok "…it is a link instead" {[llength [.t tag ranges md_link]] > 0}

# A path that is not there must say WHICH path. Alt text alone is
# indistinguishable from prose, and it is the one thing that does not
# tell you what is missing.
.t configure -state normal
.t delete 1.0 end
::vmd::render .t "!\[gone\](/no/such/file.png)"
set _txt [.t get 1.0 end]
ok "a missing image names the path" {[string match "*/no/such/file.png*" $_txt]}
ok "…and says it is missing" {[string match "*missing image*" $_txt]}
ok "…rather than drawing nothing silently" {[embedded_images .t] == 0}

# An image inside bold — the nested-construct path, which is where the
# link branch has been wrong before.
.t configure -state normal
.t delete 1.0 end
::vmd::render .t "**bold !\[d\]($::imgfile) end**"
ok "an image nested in bold still draws" {[embedded_images .t] == 1}

.t configure -state normal
.t delete 1.0 end
catch { ::vmedia::forget .t }
file delete -force $::imgdir

puts "\nmdtest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
