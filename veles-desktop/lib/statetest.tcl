# veles-desktop/lib/statetest.tcl — the desktop state blob has a schema.
#
# `desktop_state.json` (in the data dir) holds what the UI owns: theme, keymap,
# window geometry, sash positions, pinned sessions, composer history.
# FIVE places read it, merged one key and wrote it back — each with its
# own Tcl→JSON guesser, and the three guessers disagreed about what a
# list is.
#
# That was not theoretical. From the live state file on this machine:
#
#   "input_history": {"/mechanisms": {"Use":"zai","an":"xwayland", …},
#                     "/model": {"/model":"ox"}, …}
#
# A list of prompts became an OBJECT. ::vsidebar::to_json treats any
# even-length list as a dict — `dict size` succeeds on every even-length
# list — so each prompt's own words were paired into keys and values and
# the composer history was destroyed. Which encoder ran depended on
# which setting you happened to change last: picking a theme rewrote the
# pins and the history through the THEME code's serializer, which quoted
# every value as a string.
#
# So: one writer, encoding by a declared type per key, and a read that
# repairs what the old ones mangled. These tests pin both, and the
# round-trip that the guessers could not survive.
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

namespace eval ::vstatus { variable last "" ; proc set_msg {m} { set ::vstatus::last $m } }
source [file join $_dir api.tcl]
source [file join $_dir vstate.tcl]

# A stand-in gateway: the blob lives in a variable, and the encoder's
# output is decoded the way the real server would decode it.
namespace eval ::vapi {
    variable stored "{}"     ;# the RAW document, as the gateway stores it
    variable posted ""
    variable fail_post 0
    # The state blob lives IN the gateway; merge/all talk to it. The
    # suite attaches a synthetic instance so the real unattached guards
    # (api.tcl) let those calls through — exactly what an attached
    # desktop does.
    variable current "stub"
    variable instances
    array set instances {stub {url http://127.0.0.1:8642 token {}}}
    proc unattached {} { variable current ; return [expr {$current eq ""}] }
    proc get {path} {
        variable stored
        variable fail_get
        if {[info exists fail_get] && $fail_get} { error "no gateway" }
        return $stored
    }
    proc post {path body} {
        variable posted $body
        variable fail_post
        if {$fail_post} { error "connection refused" }
        return "{}"
    }
}

# Build the document the way the SERVER would: an array for a list, an
# object for the shape the old encoder produced. Writing raw JSON in the
# fixtures is the point — the bug is about which of the two was stored.
proc store {json} { set ::vapi::stored $json }
proc json_array {items} {
    set parts {}
    foreach i $items { lappend parts "\"[::vapi::json_escape $i]\"" }
    return "\[[join $parts ", "]\]"
}

# ── encoding by declared type, not by guess ─────────────────────────
puts "the encoder knows what each key is"

ok "a string key is a JSON string" \
    {[::vstate::encode theme "synthwave"] eq "\"synthwave\""}
ok "a number key is a bare number" \
    {[::vstate::encode composer_height 3] eq "3"}
# The theme writer used to quote EVERY value, so the height came back as
# the string "3" — which is what the live file actually holds.
ok "…even when it arrives as a string" \
    {[::vstate::encode composer_height "3"] eq "3"}
ok "a non-numeric number falls back to a string" \
    {[::vstate::encode composer_height "tall"] eq "\"tall\""}
# The guesser's failure mode for a number key: anything it can read as a
# multi-element list becomes a JSON ARRAY. A height is never an array,
# whatever junk a previous encoder left in the slot.
ok "…and junk with a space stays a string, not an array" \
    {[::vstate::encode composer_height "3 4"] eq "\"3 4\""}
ok "…and an emptied value stays a string" \
    {[::vstate::encode composer_height ""] eq "\"\""}

# THE one. A list of prompts is an array of strings, always — never an
# object, whatever its length or content happens to look like.
set hist {"one" "two"}
set hist [list "check the deploy" "fix the thing"]
ok "a two-element history is an array" \
    {[::vstate::encode input_history $hist] eq "\[\"check the deploy\", \"fix the thing\"\]"}
ok "…and so is a one-element one" \
    {[::vstate::encode input_history [list "only"]] eq "\[\"only\"\]"}
ok "…and an empty one"    {[::vstate::encode input_history {}] eq "\[\]"}
# An even number of one-word entries is exactly what the old guesser
# mistook for a dict.
ok "…and an even count of single words" \
    {[::vstate::encode input_history [list a b c d]] eq "\[\"a\", \"b\", \"c\", \"d\"\]"}
ok "pins are an array too" \
    {[::vstate::encode pins [list s1 s2]] eq "\[\"s1\", \"s2\"\]"}

# A quote or a brace in a prompt must not break the document.
ok "a quoted prompt is escaped" \
    {[::vstate::encode input_history [list "say \"hi\""]] eq "\[\"say \\\"hi\\\"\"\]"}

# A key nothing declares still round-trips rather than vanishing.
ok "an unknown key still encodes" {[::vstate::encode future_thing "x"] ne ""}

# ── merge writes the whole blob, encoded once ───────────────────────
puts "\nmerging one key does not rewrite the others"

store "\{\"theme\": \"synthwave\", \"pins\": [json_array {s1 s2}], \"input_history\": [json_array {"one prompt" "two prompt"}], \"composer_height\": 3\}"
ok "the merge reports success" {[::vstate::merge [dict create theme amber]]}
set out $::vapi::posted
ok "the changed key is written"  {[string match "*\"theme\": \"amber\"*" $::out]}
# This is the regression: changing the THEME used to re-encode the
# history and the pins through the theme code's string-quoting loop.
ok "…and the history is still an array" \
    {[string first {"input_history": ["one prompt", "two prompt"]} $::out] >= 0}
ok "…the pins too"               {[string first {"pins": ["s1", "s2"]} $::out] >= 0}
ok "…and the height is still a number" {[string match "*\"composer_height\": 3*" $::out]}

set ::vapi::fail_post 1
ok "a failed write is reported, not swallowed" \
    {![::vstate::merge [dict create theme amber]]}
ok "…with the reason"  {[string match "*connection refused*" $::vstate::error]}
set ::vapi::fail_post 0

# ── a stalled gateway must not cost the blob its keys ────────────────
puts "\na merge during a failed read refuses rather than wipes"

# USER 2026-09-20: "something simply reset my keybindings to normie"
# (and again mid-session: "the resets to normie keep happening,
# presumably during leaky testing"). `all` answers {} both for a first
# run and for a gateway too busy to answer; merging into the SECOND {}
# shipped a blob holding ONLY the new keys — keymap, theme, layout,
# everything else erased by one sash move during a stall.
store "\{\"theme\": \"synthwave\", \"keymap\": \"emacs\", \"composer_height\": 3\}"
set ::vapi::fail_get 1
ok "the merge refuses while the read fails" \
    {![::vstate::merge [dict create sash0 42]]}
ok "…and says why" {[string match "*no gateway*" $::vstate::error]}
set ::vapi::fail_get 0
ok "the stored document kept every key it had" \
    {[string first "keymap" $::vapi::stored] >= 0
     && [string first "synthwave" $::vapi::stored] >= 0}
# The healthy path is unchanged: a read that succeeds merges over it.
ok "a merge once the gateway answers again succeeds" \
    {[::vstate::merge [dict create sash0 42]]}
ok "…carrying the OLD keys alongside the new one" \
    {[string first "\"keymap\": \"emacs\"" $::vapi::posted] >= 0
     && [string first "\"sash0\": 42" $::vapi::posted] >= 0}

# ── repairing what the old encoders wrote ───────────────────────────
puts "\nreading back what the guessers mangled"

# The exact shape from the live file: a list of prompts that came back
# as an object, so Tcl sees {key value key value} where the values are
# themselves multi-word.
# The exact shape from the live file: an OBJECT where an array belongs.
store {{"input_history": {"/mechanisms": {"Use":"zai","an":"xwayland","and":"report"},
                          "/model": {"/model":"ox"},
                          "search for the thing": {"desktop":"app","image":"to"}},
        "theme": "synthwave"}}
set st [::vstate::all]
set h [dict get $st input_history]
ok "the corrupt history becomes a flat list" {[llength $::h] == 3}
ok "…keeping the entries that survived as keys" \
    {[lsearch -exact $::h "/mechanisms"] >= 0
     && [lsearch -exact $::h "search for the thing"] >= 0}
ok "…and it says what it repaired" {[llength [::vstate::repairs]] == 1}
ok "…naming the key"               {[string match "input_history*" [lindex [::vstate::repairs] 0]]}
ok "an undamaged key is left alone" {[dict get $st theme] eq "synthwave"}

# Repaired, it must survive the next write as an array.
::vstate::merge [dict create theme amber]
ok "the repaired history writes back as an array" \
    {[string first {"input_history": [} $::vapi::posted] >= 0}
ok "…and never as an object again" \
    {[string first {"input_history": \{} $::vapi::posted] < 0}

# A HEALTHY history must not be mistaken for a corrupt one. This is the
# assertion that stops the repair from eating real data.
# THE assertion that stops the repair from eating real data. Two
# multi-word prompts are shape-identical to a two-pair object once
# parsed; only the raw document can tell them apart.
foreach good [list \
    {} \
    [list "one"] \
    [list "one" "two"] \
    [list a b c d] \
    [list "check the deploy" "fix it"] \
    [list "a b" "c d"] \
] {
    store "\{\"input_history\": [json_array $good]\}"
    set back [dict get [::vstate::all] input_history]
    ok "a healthy history survives: [llength $good] entries" {$::back eq $::good}
    ok "…with nothing reported repaired"  {[llength [::vstate::repairs]] == 0}
}

# A number that came back as a string is coerced, not passed through.
store {{"composer_height": "3"}}
ok "a stringly number reads as a number" \
    {[dict get [::vstate::all] composer_height] eq "3"}
store {{"composer_height": "tall"}}
ok "…and a junk one is dropped"  {[dict get [::vstate::all] composer_height] eq ""}

# A gateway that is not there is a first run, not an error.
set ::vapi::fail_get 1
ok "an unreachable gateway reads as empty" {[::vstate::all] eq {}}
ok "…without throwing"                     {![catch { ::vstate::all }]}
set ::vapi::fail_get 0
store "not json at all"
ok "a corrupt document reads as empty"     {[::vstate::all] eq {}}

# ── counts: the inverse shape problem ───────────────────────────────
#
# `strings` is corrupt when the raw document shows an object. `counts`
# — the coach's per-command tally — is corrupt when it shows an ARRAY.
# Same rule in both directions, and the same reason it has to be read
# off the RAW text: after parsing, a two-key object and a four-element
# array are both a four-element Tcl list, so no amount of squinting at
# the parsed value can tell them apart. That is the exact mistake that
# shredded input_history.
puts "\ncounts"

store {{"coach_seen": {"::vsidebar::new_session": 2, "::vchat::toggle_source": 3}}}
set cs [dict get [::vstate::all] coach_seen]
ok "an object of counts reads back"      {[dict get $::cs ::vsidebar::new_session] == 2}
ok "…all of it"                          {[dict size $::cs] == 2}

store {{"coach_seen": ["a", "b", "c", "d"]}}
ok "an array-shaped tally is discarded, not read as pairs" \
    {[dict get [::vstate::all] coach_seen] eq {}}

store {{"coach_seen": {"a": 2, "b": "banana"}}}
set cs [dict get [::vstate::all] coach_seen]
ok "a non-numeric count is dropped"      {![dict exists $::cs b]}
ok "…and its healthy neighbour survives" {[dict get $::cs a] == 2}

# Round trip: what the encoder writes must read back identically.
store {{}}
::vstate::put coach_seen [dict create ::vpanes::hide 1 {::vpanes::show dev} 3]
store $::vapi::posted
set cs [dict get [::vstate::all] coach_seen]
ok "a tally round-trips through the encoder" \
    {[dict get $::cs ::vpanes::hide] == 1
     && [dict get $::cs {::vpanes::show dev}] == 3}
ok "…as a JSON object, not an array" \
    {[string first "\"coach_seen\": \{" $::vapi::posted] >= 0}

# A command with a space in it is one key, not two.
ok "a multi-word command stays one key" {[dict size $::cs] == 2}

# ── pairs: string -> string, values left alone ──────────────────────
#
# The tab layouts. Same object-or-array rule as `counts`, but the
# values are opaque: one escaped JSON string per tab, whose contents
# are tabs.tcl's business. That is deliberate — a nested object per tab
# would hand the shape problem straight back to whoever parses it, and
# the shape problem is why this schema exists.
puts "\npairs"

store {{"tab_layout": {"chat": "sashes 210 focus .sidebar.list", "dev": "sashes 40"}}}
set tl [dict get [::vstate::all] tab_layout]
ok "an object of pairs reads back"    {[dict size $::tl] == 2}
ok "a multi-word value stays one value" \
    {[dict get $::tl chat] eq "sashes 210 focus .sidebar.list"}

store {{"tab_layout": ["chat", "dev"]}}
ok "an array-shaped layout is discarded" {[dict get [::vstate::all] tab_layout] eq {}}

store {{}}
::vstate::put tab_layout [dict create chat "sashes 210" dev "sashes 40"]
ok "…and it round-trips as an object" \
    {[string first "\"tab_layout\": \{" $::vapi::posted] >= 0}
store $::vapi::posted
ok "…with both entries intact" \
    {[dict size [dict get [::vstate::all] tab_layout]] == 2}

puts "\nstatetest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
