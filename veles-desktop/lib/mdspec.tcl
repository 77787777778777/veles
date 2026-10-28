# veles-desktop/lib/mdspec.tcl — THE MARKDOWN SPEC, as data.
#
# USER: "maybe it would be good if we ported over some comprehensive
# test suite and spec for markdown and programmed against that instead
# of doing ad-hoc firefighting with this in the future".
#
# This file is the source of truth for what the desktop's markdown
# renderer does. `mdspectest.tcl` runs every case against the real
# ::vmd::render. Adding a construct means adding a case here FIRST.
#
# ── why not CommonMark's spec.json verbatim ─────────────────────────
#
# CommonMark's 652 examples assert HTML. This renderer produces TAGGED
# TEXT in a Tk widget — there is no HTML to compare against, and much of
# the spec (raw HTML blocks, link reference definitions, setext
# headings, entity references, nested block containers) is deliberately
# out of scope for a chat transcript. Importing the file would give a
# suite that is ~80% red by design, which teaches nothing.
#
# So each case cites the CommonMark section it comes from, and every
# construct we do NOT implement is recorded as a `gap` that asserts
# TODAY's behaviour. A gap is not a failure — it is a promise that the
# behaviour will not change silently. When one gets implemented, its
# entry moves from `gap` to `case`.
#
# ── quoting convention ──────────────────────────────────────────────
#
# EVERY argument is brace-quoted, so `[`, `$` and `\` are literal —
# markdown is full of all three and double quotes turn `[a](b)` into a
# command substitution. Newlines are written as the token <NL>, which
# the runner expands. That keeps every case on one readable line and
# removes the whole class of quoting bug from the spec.
#
#   case {name} {CommonMark ref} {input} {expected text} {tag want …}
#   gap  {name} {CommonMark ref} {input} {expected text} {tag want …}
#
# `expected text` is the ENTIRE buffer after rendering, so a stray
# newline or a swallowed marker is caught rather than ignored.
# Each {tag want} pair asserts the text covered by `tag`, concatenated
# across its ranges in document order, equals `want`. Empty asserts the
# tag is absent.

namespace eval ::mdspec {
    variable cases {}
}
# <NL> is a newline, <BS> a backslash and <TAB> a tab. All three need a
# token: a brace string cannot end in a backslash (it would escape the
# closing brace), markdown is full of the first two, and a literal tab
# in the expected text of a table case is invisible in a diff.
proc ::mdspec::_nl {s} {
    return [string map [list <NL> "\n" <BS> "\\" <TAB> "\t"] $s]
}
proc ::mdspec::case {name ref input text {spans {}}} {
    variable cases
    lappend cases [list supported $name $ref [_nl $input] [_nl $text] $spans]
}
proc ::mdspec::gap {name ref input text {spans {}}} {
    variable cases
    lappend cases [list gap $name $ref [_nl $input] [_nl $text] $spans]
}
# Spans are compared after the same expansion.
proc ::mdspec::spans {args} {
    set out {}
    foreach {t v} $args { lappend out $t [_nl $v] }
    return $out
}

# ── 4.8 paragraphs, 4.1 thematic breaks ─────────────────────────────
::mdspec::case {plain paragraph} {CM 4.8} {hello world} {hello world<NL>} \
    [::mdspec::spans md_bold {} md_link {}]
::mdspec::case {two paragraphs keep the blank line} {CM 4.8} {one<NL><NL>two} {one<NL><NL>two<NL>}
::mdspec::case {thematic break from dashes} {CM 4.1} {---} {────────────────────────────<NL>} \
    [::mdspec::spans md_hr {────────────────────────────<NL>}]
::mdspec::case {thematic break from asterisks} {CM 4.1} {***} {────────────────────────────<NL>}
::mdspec::case {thematic break from underscores} {CM 4.1} {___} {────────────────────────────<NL>}

# ── 4.2 ATX headings ────────────────────────────────────────────────
::mdspec::case {h1} {CM 4.2} {# Title} {Title<NL>} [::mdspec::spans md_h1 {Title<NL>}]
::mdspec::case {h2} {CM 4.2} {## Title} {Title<NL>} [::mdspec::spans md_h2 {Title<NL>}]
::mdspec::case {h3} {CM 4.2} {### Title} {Title<NL>} [::mdspec::spans md_h3 {Title<NL>}]
::mdspec::case {heading keeps inline formatting} {CM 4.2 + 6.4} \
    {# Title with **bold**} {Title with bold<NL>} [::mdspec::spans md_bold {bold}]
::mdspec::case {heading with a link} {CM 4.2 + 6.6} \
    {# See [docs](https://e.example)} {See docs<NL>} [::mdspec::spans md_link {docs}]
::mdspec::gap {h4 and deeper are not headings} {CM 4.2 - only 1-3 implemented} \
    {#### Title} {#### Title<NL>} [::mdspec::spans md_h3 {}]
::mdspec::gap {a heading needs a space after the hashes} {CM 4.2} \
    {#Title} {#Title<NL>} [::mdspec::spans md_h1 {}]

# ── 4.5 fenced code blocks ──────────────────────────────────────────
::mdspec::case {fence without a language} {CM 4.5} \
    {```<NL>code here<NL>```} {code here<NL><NL>} [::mdspec::spans md_codeblock {code here<NL>}]
::mdspec::case {fence with an info string} {CM 4.5} \
    {```tcl<NL>set x 1<NL>```} {· tcl<NL>set x 1<NL><NL>} \
    [::mdspec::spans md_fencelang {· tcl<NL>} md_codeblock {set x 1<NL>}]
::mdspec::case {markdown inside a fence stays literal} {CM 4.5} \
    {```<NL>**not bold** [not a link](u)<NL>```} {**not bold** [not a link](u)<NL><NL>} \
    [::mdspec::spans md_bold {} md_link {}]
::mdspec::case {an unclosed fence still terminates} {CM 4.5} \
    {```<NL>runaway} {runaway<NL><NL>} [::mdspec::spans md_codeblock {runaway<NL>}]
# CM 4.5 construct family, xenia report 2026-09-12:
#  · an opener may be indented up to 3 spaces  (the old opener was
#    column-0 only, so the "inside a numbered list" fence was dead)
#  · the closer matches the OPENER's tick count, with optional trailing
#    spaces  (the old closer was fixed-length 3, so a 4-tick fence
#    closed early and its real closer opened a runaway block)
::mdspec::case {an indented fence is a fence} {CM 4.5 indentation} \
    {   ```<NL>   code<NL>   ```} {   code<NL><NL>} [::mdspec::spans md_codeblock {   code<NL>}]
::mdspec::case {the closer matches the opener tick length} {CM 4.5} \
    {````<NL>a<NL>```<NL>b<NL>````} {a<NL>```<NL>b<NL><NL>} \
    [::mdspec::spans md_codeblock {a<NL>```<NL>b<NL>}]
::mdspec::gap {a closer with trailing text never closes} {CM 4.5} \
    {```<NL>a<NL>``` done<NL>more} {a<NL>``` done<NL>more<NL><NL>} \
    [::mdspec::spans md_codeblock {a<NL>``` done<NL>more<NL>}]

# ── 5.1 block quotes ────────────────────────────────────────────────
::mdspec::case {block quote} {CM 5.1} {> quoted} {quoted<NL>} \
    [::mdspec::spans md_quote {quoted<NL>}]
::mdspec::case {block quote keeps inline formatting} {CM 5.1 + 6.4} \
    {> quoted **bold**} {quoted bold<NL>} [::mdspec::spans md_bold {bold}]
::mdspec::gap {a nested block quote is flattened} {CM 5.1 - no nesting} \
    {> > deep} {> deep<NL>} [::mdspec::spans md_quote {> deep<NL>}]

# ── 5.2 / 5.3 lists ─────────────────────────────────────────────────
::mdspec::case {bullet with a dash} {CM 5.2} {- item} {• item<NL>} \
    [::mdspec::spans md_bullet {• }]
::mdspec::case {bullet with an asterisk} {CM 5.2} {* item} {• item<NL>}
::mdspec::case {bullet with a plus} {CM 5.2} {+ item} {• item<NL>}
::mdspec::case {nested bullet is indented} {CM 5.2} \
    {- top<NL>  - nested} {• top<NL>    • nested<NL>}
::mdspec::case {ordered list keeps its number} {CM 5.3} {1. first} {1. first<NL>}
::mdspec::case {ordered list keeps the author numbering} {CM 5.3} {3. third} {3. third<NL>}
::mdspec::case {list items keep inline formatting} {CM 5.2 + 6.6} \
    {- see [docs](https://e.example)} {• see docs<NL>} [::mdspec::spans md_link {docs}]
::mdspec::case {unchecked task item} {GFM task lists} {- [ ] todo} {☐ todo<NL>}
::mdspec::case {checked task item} {GFM task lists} {- [x] done} {☑ done<NL>}

# ── 6.1 code spans ──────────────────────────────────────────────────
::mdspec::case {code span} {CM 6.1} {a `code` b} {a code b<NL>} \
    [::mdspec::spans md_code {code}]
::mdspec::case {two code spans on a line} {CM 6.1} {`one` and `two`} {one and two<NL>} \
    [::mdspec::spans md_code {onetwo}]
::mdspec::case {markdown inside a code span stays literal} {CM 6.1} \
    {`**not bold**`} {**not bold**<NL>} [::mdspec::spans md_bold {} md_code {**not bold**}]
# CM 6.1: a span opens on a BACKTICK RUN and closes on the next run of
# the SAME length. The old matcher could only strip one tick per side,
# so a triple run rendered as `` around the tagged span (xenia report,
# 2026-09-12). The three here were red before the run-matcher landed.
::mdspec::case {a triple-tick span is delimited by equal runs} {CM 6.1} \
    {look: ```block``` done} {look: block done<NL>} [::mdspec::spans md_code {block}]
::mdspec::case {a two-tick run is a code span delimiter too} {CM 6.1} \
    {a ``b`` c} {a b c<NL>} [::mdspec::spans md_code {b}]
::mdspec::case {one space inside is stripped when both sides have one} {CM 6.1} \
    {` code `} {code<NL>} [::mdspec::spans md_code {code}]

# ── 6.4 emphasis and strong emphasis ────────────────────────────────
::mdspec::case {strong} {CM 6.4} {**bold**} {bold<NL>} [::mdspec::spans md_bold {bold}]
::mdspec::case {emphasis with asterisks} {CM 6.4} {*ital*} {ital<NL>} \
    [::mdspec::spans md_italic {ital}]
::mdspec::case {strong and emphasis on one line} {CM 6.4} {**b** and *i*} {b and i<NL>} \
    [::mdspec::spans md_bold {b} md_italic {i}]
::mdspec::case {emphasis nested in strong} {CM 6.4} {**a *b* c**} {a b c<NL>} \
    [::mdspec::spans md_bolditalic {b} md_bold {a  c}]
::mdspec::case {underscores are not emphasis mid-word} {CM 6.4 left-flanking} \
    {snake_case_name} {snake_case_name<NL>} [::mdspec::spans md_italic {}]
::mdspec::gap {single underscores do not emphasise} {CM 6.4 - _ not implemented} \
    {_ital_} {_ital_<NL>} [::mdspec::spans md_italic {}]

# ── 6.6 links, 6.9 autolinks ────────────────────────────────────────
::mdspec::case {inline link} {CM 6.6} {[label](https://e.example/p)} {label<NL>} \
    [::mdspec::spans md_link {label}]
::mdspec::case {link at the start of a line} {CM 6.6} \
    {[label](https://e.example) rest} {label rest<NL>} [::mdspec::spans md_link {label}]
::mdspec::case {two links on one line} {CM 6.6} \
    {[a](https://e.example/1) [b](https://e.example/2)} {a b<NL>} \
    [::mdspec::spans md_link {ab}]
::mdspec::case {bare url autolinks} {CM 6.9} \
    {see https://e.example/x now} {see https://e.example/x now<NL>} \
    [::mdspec::spans md_link {https://e.example/x}]
::mdspec::case {a url with underscores survives} {CM 6.9} \
    {https://e.example/a_b_c} {https://e.example/a_b_c<NL>} \
    [::mdspec::spans md_link {https://e.example/a_b_c} md_italic {}]
::mdspec::case {link inside strong} {CM 6.4 + 6.6} \
    {**see [docs](https://e.example) now**} {see docs now<NL>} \
    [::mdspec::spans md_link {docs} md_bold {see  now}]
::mdspec::case {link inside emphasis} {CM 6.4 + 6.6} \
    {*see [docs](https://e.example) now*} {see docs now<NL>} \
    [::mdspec::spans md_link {docs} md_italic {see  now}]
::mdspec::case {code span inside strong} {CM 6.1 + 6.4} \
    {**run `cmd` now**} {run cmd now<NL>} [::mdspec::spans md_code {cmd} md_bold {run  now}]
::mdspec::case {bare url inside strong} {CM 6.4 + 6.9} \
    {**at https://e.example/z end**} {at https://e.example/z end<NL>} \
    [::mdspec::spans md_link {https://e.example/z}]
::mdspec::gap {reference-style links are not resolved} {CM 6.6 / 4.7} \
    {[label][ref]} {[label][ref]<NL>} [::mdspec::spans md_link {}]
::mdspec::case {angle autolink} {CM 6.9} \
    {<https://e.example/a>} {https://e.example/a<NL>} \
    [::mdspec::spans md_link {https://e.example/a}]
# A REMOTE image is a link to the image. Fetching whatever URL a model
# happened to write would make the transcript issue network requests on
# the model's say-so, which is a different decision from "render
# markdown". A LOCAL one is drawn — see mdtest's "markdown images"
# section, which needs a real widget and so cannot live in this table.
#
# Was a `gap` until 2026-08-25: there was no image branch at all, so the
# `!` came out as literal text and the picture became a link labelled
# with its alt text. USER: "The xorg logo seems to only render in the
# preview but not in the transcript".
::mdspec::case {a remote image is a link to it} {CM 6.7} \
    {![alt](https://e.example/i.png)} {alt<NL>} \
    [::mdspec::spans md_link {alt}]


# ── GFM tables ──────────────────────────────────────────────────────
#
# Wide or multiline cells wrap inside their own columns. They never
# become stacked records: the next column keeps its tab stop.
#
# The header rule is as wide as the content IN CHARACTERS, not pixels,
# so the rendering does not depend on the host's fonts.
::mdspec::case {table renders as an aligned grid} {GFM tables} \
    {| Name | Age |<NL>|---|---:|<NL>| Ada | 36 |} \
    {Name<TAB>Age<NL>──────────<NL>Ada<TAB>36<NL><NL>} \
    [::mdspec::spans md_bold {NameAge} md_hr {──────────<NL>} md_codeblock {}]
::mdspec::case {table cells keep their inline formatting} {GFM tables + CM 6.6} \
    {| Tool | Docs |<NL>|---|---|<NL>| **veles** | [docs](https://e.example/d) |} \
    {Tool<TAB>Docs<NL>────────────<NL>veles<TAB>docs<NL><NL>} \
    [::mdspec::spans md_link {docs} md_bold {ToolDocsveles}]
::mdspec::case {an empty cell keeps its column} {GFM tables} \
    {| a | b | c |<NL>|---|---|---|<NL>| 1 |  | 3 |} \
    {a<TAB>b<TAB>c<NL>─────────<NL>1<TAB><TAB>3<NL><NL>} \
    [::mdspec::spans md_bold {abc}]
::mdspec::case {a table needs no header row} {GFM tables} \
    {| 1 | 2 |<NL>| 3 | 4 |} {1<TAB>2<NL>3<TAB>4<NL><NL>} \
    [::mdspec::spans md_bold {} md_hr {}]
# `<br>` is a line break inside the same grid cell, not a fallback to
# stacked records. The separator follows display-column widths; tabs
# are nonprinting, so its length need not equal the buffer's labels.
::mdspec::case {a cell with <br> keeps its column and wraps in place} {GFM tables} \
    {| # | Result | State |<NL>|---|---|---|<NL>| 1 | **Title**<br>body line | ready |} \
    {#<TAB>Result<TAB>State<NL>─────────────────────<NL>1<TAB>Title<TAB>ready<NL><TAB>body line<TAB><NL><NL>} \
    [::mdspec::spans md_bold {#ResultStateTitle} md_bullet {}]
# Long-cell wrapping depends on the actual font and widget width, so its
# exact line break is intentionally NOT a text-spec expectation. The
# real-widget geometry suite (mdtest) pins the full cell text, wrapping,
# tab structure, column stops, and absence of stacked-record fallback.

::mdspec::case {the alignment row is never printed} {GFM tables} \
    {| a |<NL>|:-:|<NL>| x |} {a<NL>────<NL>x<NL><NL>} \
    [::mdspec::spans md_codeblock {}]

# ── deliberately out of scope ───────────────────────────────────────
::mdspec::gap {setext headings are not implemented} {CM 4.3} \
    {Title<NL>=====} {Title<NL>=====<NL>}
::mdspec::gap {indented code blocks are not implemented} {CM 4.4} \
    {    indented} {    indented<NL>} [::mdspec::spans md_codeblock {}]
::mdspec::gap {raw HTML is passed through as text} {CM 4.6 / 6.8} \
    {<b>hi</b>} {<b>hi</b><NL>}
::mdspec::gap {hard line breaks are not implemented} {CM 6.12} \
    {a  <NL>b} {a  <NL>b<NL>}
# The backslashes survive but the asterisks are still taken as emphasis,
# so `\*x\*` renders as `\x\` in italics. Recorded as it actually is.
::mdspec::gap {backslash escapes are not implemented} {CM 2.4} \
    {<BS>*not emphasis<BS>*} {<BS>not emphasis<BS><NL>} \
    [::mdspec::spans md_italic {not emphasis<BS>}]

# ── regressions this spec exists to keep fixed ──────────────────────
::mdspec::case {REGRESSION a bolded link is still a link} {user report} \
    {**bold [inside](https://e.example/b) text**} {bold inside text<NL>} \
    [::mdspec::spans md_link {inside}]
::mdspec::case {REGRESSION bold containing italic is not literal} {user report} \
    {**a *b* c**} {a b c<NL>} [::mdspec::spans md_bold {a  c}]
