# The slopdrop bridge

Two bots' worth of capability from one bot in the channel.

## Why

veles' own `tcl` is `sandbox::execute` on a temp file: safe, and
stateless. Every proc a channel defines dies with the process, so the
channel can never build anything. [slopdrop](https://github.com/pillowtrucker/slopdrop)
is the complement — one persistent, git-backed interpreter with years of
accumulated procs and a rollback command — and having both has
historically meant two bots sitting in one channel.

The bridge goes both ways:

- **veles → slopdrop**: `tcl`, `.tcl`, `/tcl` evaluate in slopdrop's
  interpreter instead of the local throwaway one.
- **slopdrop → veles**: a native `ai <prompt>` Tcl command asks a veles
  agent, over A2A.

Either half works without the other. Unconfigured, nothing changes.

## veles → slopdrop

### slopdrop's side

```toml
[web]
bind_address = "127.0.0.1"     # anything else REQUIRES a token; enforced at bind
port = 8080

[[web.tokens]]
token = "…32 bytes from openssl rand -hex 32…"
admin = false                   # the bridge's ordinary token
name  = "veles-bridge"

[[web.tokens]]
token = "…a different 32 bytes…"
admin = true                    # only an owner-tier veles caller gets this
name  = "veles-bridge-owner"
```

Run it headless: `slopdrop config.toml --web`.

> **Do not run `--irc --web` against one state directory.** The web
> frontend builds its own `TclThreadHandle`, so that is two interpreters
> writing one git repo. Headless is the supported shape here.

### veles' side

```toml
[slopdrop]
enabled         = true
url             = "http://127.0.0.1:8080"
token_env       = "SLOPDROP_TOKEN"        # the admin = false one
admin_token_env = "SLOPDROP_ADMIN_TOKEN"  # omit to give nobody the admin path
timeout_ms      = 45000                   # > slopdrop's own eval timeout
```

with the two values in `$XDG_CONFIG_HOME/veles/.env`.

Since 2026-10-09 the bridge is an
in-process MODULE, `crates/veles-slopdrop` exported by
`crates/veles-module-slopdrop`, loaded by the module host from
`<data>/modules/slopdrop/` (`scripts/push-plugin.sh slopdrop` on the VPS,
`scripts/install-module-dev.sh slopdrop` for the laptop's dev gateway).
Core knows none of it: with the module loaded `tcl` goes to the bridge,
without it `tcl` is the local sandbox again, a public `/tcl` fails
closed, a Slopdrop trigger row refuses with a reason, and the mirror's
desktop routes (`/api/plugins/slopdrop/{status,sync,save}`) answer 503.
The section above is read by the module from config.toml and re-read
when the file changes. Its state (the ledger, the verdict store, the
mirror) lives in `<data>/module-state/slopdrop/`; the first start moves
the old `<data>/slopdrop/` tree there and keeps it as
`slopdrop.pre-module`.

### Who the call is made as

Two questions, and keeping them apart is the whole design:

| | decides |
|---|---|
| **which command you typed** | whether the privileged path is *asked for* |
| **your veles role** | whether you may *have* it |

So `tcl` is slopdrop's ordinary interpreter **for everybody, owner
included**, and `tcladmin` asks for the privileged one and is refused
below owner. That is slopdrop's own `tcl` / `tclAdmin` split, and it is
the split not to flatten.

It was flattened once, briefly, and the live symptom is worth recording:
mapping owner → admin on *every* call meant an owner's plain
`tcl expr {6*7}` went out as an admin request, slopdrop's own
`privileged_users` check refused it, and the owner became the single
caller who could not use the command.

It is *offered*, never asserted. slopdrop re-checks every admin request
against its own `privileged_users` regardless, and an owner whose call
could not be made as admin (no `admin_token_env`) is told so rather than
quietly demoted.

### The one non-obvious knob

veles sends the caller's **principal subject** as slopdrop's `user`
field, so slopdrop's git history names the person in the channel rather
than recording every commit as the bridge. slopdrop then builds its
hostmask as `<user>!web` — which is *not* an IRC-shaped mask. So
`privileged_users` needs the subject form:

```toml
[security]
privileged_users = ["irc:libera/wrath!*"]   # not "wrath!*@…"
```

Run `/acl` in the channel to read the exact subject veles will send.

### From a turn

`tcl_state_eval` is the same bridge reachable by the model, tiered at
`Cap::Shell` — the same tier the command answers to. Deliberately not
`tcl_eval`, which is `Cap::All`: that one drives the gateway's *own*
in-process interpreter, while this one is somebody else's behind an HTTP
boundary and cannot reach veles at all.

## The public surface (Bluesky): the TOS firewall

On IRC the answer goes to the channel and the worst case is an over-long
drawing. On Bluesky the reply is a PUBLIC post under the bot's account,
and slopdrop's state is a decade of channel-built procs — some of which a
public feed must not carry. So a `/tcl` mention on the atproto frontend
is NOT posted raw. The slopdrop module's `check::decide_public` (through
core's `tcleval::PublicTclReply` slot; `safety::shape_for_public` does the
shaping) decides what the post may carry:

- **Safety**, keyed by slopdrop's content hash (`invoked_hash`, reported
  on `/api/eval` for the stored proc the line named). A prebaked
  per-hash verdict lives in `$XDG_DATA_HOME/veles/slopdrop/safety.tsv`
  (`safe` / `borderline` / `unsafe`). `borderline` and `unsafe` are
  never posted inline — the post says the output is withheld and links
  the full result. An UNKNOWN hash (a redefined proc, or ad-hoc code,
  which is hashed by its own text) fails CLOSED: withheld, published,
  and the hash is appended to `safety.pending` for the next
  re-classification sweep.
- **Length**: a `safe` result past ~two posts (~560 chars) is elided —
  head + a link to the full output.

Either way the FULL result (never just the elided part) is published to
**our own PDS as blobs** — `com.atproto.repo.uploadBlob` on the vlpds the
atproto adapter already drives. Blobs are content-addressed bytes keyed by
CID with NO repo record and NO firehose fan-out, so the output lives in
the PDS island without entering the public ATProto data stream (USER's
steer, 2026-10-06). Both a raw markdown blob and a rendered HTML blob are
uploaded; the post links the rendered page at
`com.atproto.sync.getBlob?did=…&cid=…`, a public no-auth endpoint. The
rendering runs the output through an mIRC control-code renderer (`\x03`
color, `\x02` bold, …) so the formatting a drawing carried over IRC shows
as actual color — the flat post drops it. Raw HTML in the source is
dropped by the renderer, never served live.

The public link needs the PDS's PUBLIC hostname (the adapter posts through
a loopback), so one knob names it:

```toml
[platforms.atproto]
public_url = "https://veles.7778777.online"   # for /tcl blob links
```

Unset, a withheld/elided result says the result could not be hosted rather
than posting a dead link.

The prebake classifier is a batch of `veles chat -q` one-shots
(`deepseek-v4.1-flash-1017` on the personal plan; the undated release was
retired as the default 2026-10-23), one verdict per proc body,
written into `safety.tsv` by hash. It re-runs over `safety.pending`.

## Tcl on Bluesky: the post, facets and embeds

USER, 2026-10-10: *"special tcl variables and procs to manipulate posts
and facets at atproto/bsky level"*. A `/tcl` line that came from a
Bluesky post arrives with that post, and its code can shape the reply
beyond plain text. slopdrop's `tcl/bsky.tcl` is the reference; in short:

| | |
|---|---|
| `$::bsky::active` | 1 on Bluesky, 0 everywhere else |
| `$::bsky::post` | the post: `uri cid text did handle created langs facets embed reply` |
| `$::bsky::thread` | the posts above it, oldest first, same shape |
| `bsky::text`, `author`, `links`, `tags`, `mentions`, `facets`, `images`, `is_reply` | readers over a post dict |
| `bsky::facet_text`, `byteslice`, `strip_facets` | facets are UTF-8 BYTE offsets; these turn them into characters |
| `bsky::link text url`, `tag`, `mention`, `quote`, `card`, `image`, `lang`, `newpost` | writers: return the visible text and record what it means |
| `bsky::profile`, `getpost`, `getthread`, `search`, `feed`, `followers` | reads as the bot (`bsky::query`) |
| `bsky::art text ?-alt a? ?-theme dark\|light?` | attach a drawing as a picture (see "Text art as pictures") |
| `bsky::label val …` | self-label the reply: `nudity`, `sexual`, `porn`, `graphic-media` |
| `bsky::parent ?post?`, `bsky::root ?post?` | the post above, the thread's first post, as post dicts |
| `bsky::quotewith post ?lambda? ?arg …?` | quote a post and answer it: the lambda gets its text |
| `bsky::magick source ?-alt a? op …` | an image macro: a picture of the thread changed by ops (see "Image macros") |
| `bsky::caption source top ?bottom?` | the classic image macro: words written over a picture of the thread |
| `bsky::scheme program ?-alt a?` | a program in the base library's image language; returns its handle `magick:N` (see "The base library's image language") |
| `bsky::picture ?source?` | the URL of a post's first picture, by default the post above |
| `bsky_get_parent`, `bsky_get_root`, `bsky_quote`, `bsky_art`, `bsky_magick`, `bsky_caption`, `bsky_image` | the same as plain global procs (`bsky_image` is `bsky::picture`) |
| `magick_scheme`, `magick`, `magick_id` | the base library's image procs, drawn again (`tcl/magick.tcl`) |

Off Bluesky the writers render the IRC way (`docs <https://…>`), so one
stored proc works on both. Off Bluesky `bsky_get_parent` is the
channel's previous line (not the caller's), as a post dict.

The USER's line, 2026-10-12, quotes the post above and answers it with
the fedora goon saying its words, as a picture:

```tcl
/tcl bsky_quote [bsky_get_parent] {{t} fedoragoonsay $t}
```

**The thread is the channel log.** For one Bluesky evaluation the
thread's posts are `::slopdrop_log_lines(bluesky)` as `{ts handle did
text}`, so the procs that read the log (`irclog`, `mock`, `quote`,
`lastlink`, `s///`) read the thread. It is cleared after the evaluation
and never committed. A Bluesky evaluation keeps up to 200 output lines,
not `max_output_lines` (art is tall).

**What the reply may carry** is decided here, in the slopdrop module
(`crates/veles-slopdrop/src/bsky.rs`, `validate_spec`), never by the
code that recorded it: mentions only of the caller and the thread's own
participants (a proc must not notify strangers from the bot's account),
up to 8 tags and 20 links, one quote OR card, up to 3 languages, and
images only from the owner's `/tcladmin` (a public reply must not carry
pictures from an arbitrary URL; an empty alt is written by the vision
model). The spec rides `meta["bsky.spec"]` only on a post that goes out
inline; a withheld or blocked post is a note and a link, nothing more.
The output check's people gate withholds any `@handle` in the text,
except exactly the `@<handle>` of each mention the spec may carry, taken
from its validated handle and never from the text a proc recorded. Until
2026-10-11 it withheld those too, so `bsky::mention` never went out.
`bsky::newpost` marks a post boundary with a form feed; the output
check reads it as a newline (it withholds control characters as art)
and the mark returns only on an inline post whose text is the one the
check passed. The atproto plugin (`crates/veles-atproto/src/rich.rs`)
places each recorded span by its text on the FINAL post, so the byte
offsets are the post's own.

**Quotes** (2026-10-12). A quote notifies the
quoted post's author, and the account quoting is the bot's, so:

- Anyone but the configured owner may quote only a post of the thread
  they are in (the post that asked, its parent and root, the posts above
  it) or one of their own, by at-uri. A bsky.app link is the owner's only.
- Anyone but the owner gets one quote per post and three per quoted
  author a day (`quotecap.rs`, `<state>/quotes.json`). Over the limit, the
  quote is dropped and the reply says why.
- "The owner" means a configured `[acl]` with this sender as owner. With
  no `[acl]` every sender resolves as owner, which never counts here.
  A trigger's answer is never the owner's, whoever's event fired it.
- The atproto plugin reads the target as the bot before quoting it. It
  drops the quote, with a note, when the author turned quoting off (a
  postgate), a block stands between the bot and the author, a moderation
  service's label is on the post or its author, or the author gave it an
  adult self-label (`porn`, `sexual`, `nudity`, `graphic-media`, `gore`).
  The author's other self-labels describe rather than warn: until
  2026-10-12 the `bot` label on an automated account's profile refused
  every quote of it.

**Attachment markers.** A `MEDIA:/path` line is how a turn attaches a
file, after the gateway's media gate vetted it. A `/tcl` output is
somebody's code and never went through that gate, so the policy keeps
every such line as text (`MEDIA :/path`), a form feed counting as a line
start. The atproto plugin also ignores markers on any reply the policy
shaped (one naming a ledger entry or a hosted result). Before 2026-10-12
an output line `MEDIA:/srv/…/x.png` passed the output check and the
plugin uploaded that file from its own disk.

**Text art as pictures** (2026-10-12). A
Bluesky post is 300 graphemes in a proportional font, so IRC art (mIRC
colour, ANSI, box, block and braille drawings, the `*say` family) means
nothing as its text. The module draws it in the IRC font instead
(`crates/veles-textart`: GNU Unifont, cell backgrounds, a whole-number
scale toward 1400 px wide, an indexed PNG under Bluesky's 1,000,000-byte
image limit) and the atproto plugin attaches the PNG as it was drawn,
with its alt text and its aspect ratio.

- **What is a drawing** (`art::is_art`): any mIRC colour, reverse video
  or ANSI escape; otherwise two or more lines that use a drawing
  character, have more symbols than letters, or carry a run of four of
  one symbol. One line of anything stays text.
- **What a reply draws**: the output itself when it is a drawing (the
  post's text is then only veles' notes, such as `(state: …)`), and each
  `bsky::art` the line recorded, at most four images with the line's
  other images.
- **The raw output** reaches the policy in veles' own spec record
  (`kind = "_raw"`, the output and the notes apart), because the
  answer's text is cut and noted. A record of a `_` kind that the code
  made is dropped as a forgery.
- **The owner's drawing** (a configured `[acl]` owner, as for quotes) is
  shown to the `[vision]` route once (`art::classify`, a strict JSON
  verdict; it is kept by the PNG's hash in `<state>/art/seen.json`).
  What the model sees self-labels the post: `sexual_explicit` is `porn`,
  `sexual` is `sexual`, `nudity` is `nudity`, `gore` is `graphic-media`.
  The reply is held for a one-tap question when it sees a hate symbol, a
  slur, a threat, a minor or personal info, or when the check fails or
  answers in no verdict shape. An approved drawing (its text on the
  allowlist) is never held; a deleted one (its text on the blocked list)
  is a note, as for text.
- **Anyone else's drawing** (P5) passes the public checks first, then
  the image check with the stricter set. The first that decides
  withholds it and asks the owner, without showing it to the model:
  1. the bridge gives no proc index (fail closed);
  2. the line reaches the bad list. Tracked procs are followed through
     their bodies, as for text. A base-library proc (`/api/procs?base=1`)
     is judged by its own verdict and its body is read only for the
     stamps it names, never for its calls. A stamp is reached the way
     slopdrop's handler draws it: `<x>say`, `<x>think` and `stampsay -s
     <x>` reach `ST<x>`, so `nazisay hi` reaches `STnazi` with no proc
     of that name. `stampsay` or `stampthink` with no `-s` draws a random
     stamp, so a line or a tracked body that calls it is on the list;
  3. the typed line names a denylisted word (folded, with the acrostic);
  4. the words the drawing shows fail the word checks: links and contact
     details (read from the text as shown, since its words drop the
     `://`), the denylist, then the scorer.

  The image check then withholds on everything the owner's holds on,
  plus harassment and self-harm. Nudity, sexual content and gore post
  with their self-label, as for the owner. A drawing the owner approved
  before skips the public checks and is never withheld.
- **A quote by anyone else** (P5) of a post in the thread is dropped with
  a note when that post's text fails the word checks, because the bot's
  account re-posts what it quotes. The rest of the reply goes out. Their
  own post from elsewhere is theirs to quote.
- **Long text as a picture** (P6). A Bluesky reply over the inline cap
  goes out as before, its head and the link to the full output, and now
  also carries the whole text drawn as one picture: word-wrapped at 72
  columns, cut at the grid's 123 rows, its alt text the text (2000
  characters, then a note pointing at the link). The picture goes only
  when the text it shows passes the word checks: links and contact
  details, the denylist, and the scorer, which reads at most 512 tokens
  and refuses a longer text, so it scores the text in pieces of 600
  characters cut at line ends. A text that fails keeps the reply to its
  head and the link; why is in the journal (`long text kept to its
  head`). It takes a picture slot only when one is left. A withheld long
  text keeps its whole text drawn, unchecked (the owner's question shows
  it), and an approve reposts its head and link with the picture. The
  head is 220 characters, so the head, the 29-character note and the
  41-character short link fit one 300-grapheme post; at 260 the link
  always went to a second post.
- **Image macros** (P6; anyone's since the wrap-up round). `bsky_caption
  [bsky_get_parent] TOP BOTTOM` writes over the parent's picture;
  `bsky_magick source op …` runs a chain of ops on it: `{resize W H}`
  (0 keeps the shape), `{scale PCT}`, `{rotate 90|180|270}`, `{flip}`,
  `{flop}`, `{crop X Y W H}`, `{border PX ?COLOUR?}`, `{blur SIGMA}`,
  `{sharpen}`, `{grayscale}`, `{invert}`, `{caption TOP ?BOTTOM?}`, at
  most 10. The source is a post dict (its first picture), the at-uri of
  a thread post, or the URL of a picture the thread shows. slopdrop
  records `{kind magick src ops alt}`, each op's words joined by U+001F
  and the ops by U+001E. The module fetches only a URL the post context
  lists in `embed.images`, matched exactly, over HTTPS, no redirect, at
  most 8 MB, and decodes at most 10,000 px a side, scaled to 2000. The
  caption is GNU Unifont, bold, white with a black outline, wrapped at
  spaces, three lines at most at each end. The PNG is cut down by
  quarters until it fits the 1,000,000-byte limit. Then it is checked,
  labelled or held like the owner's drawing, and its alt text is the
  code's, else the image check's description. Anyone else's macro
  (USER, 2026-10-12) passes the checks their drawings do: the words it
  writes through the word checks, then the image check's stricter set.
  `scale` goes straight to the size that fits (never a larger picture in
  between).
- **The base library's image language** (wrap-up round). The old bot's
  image procs (`mirror_left`, `magick-concat`, `image-border_scheme`,
  `macify2`, `recurse_eyes`, …) sent programs to magick.buttes.org,
  which is gone. slopdrop's `tcl/magick.tcl` rebinds `magick_scheme`,
  `magick` and `magick_id` after the base library loads
  (`::magick_dsl::revive`, which `tcl_wrapper.rs` runs after the state;
  a tracked proc of the same name stays). `magick_scheme` records
  `{kind magick scheme <program> id <n> alt}` and returns the handle
  `magick:<n>`; the old `magick CMD SOURCE ARG …` becomes a program;
  `magick_id` returns what it is given. A later program names an earlier
  one by its handle, and the module draws only the programs no other one
  names (at most 16 programs a reply) and leaves the handles out of the
  post's text. `bsky_image ?post?` gives a picture's URL:
  `/tcl mirror_left [bsky_image]`. The module (`scheme.rs`) reads the
  subset the library writes: `let` (each binding sees the ones before,
  as the library assumes), `if`, `begin`, `and`/`or`/`not`, arithmetic
  and comparisons, names folding case (R5RS: `recurse_eyes` binds `I`
  and reads `i`), and `image`, `width`, `height`, `resize`,
  `resize_to_fit`, `scale`, `crop`, `rotate` (any angle, clockwise, the
  canvas grown with transparent corners), `flip`, `flop`, `grayscale`,
  `invert`, `blur`, `sharpen`, `border`, `composite`, `overlay`,
  `annotate` (size, x, y, colour, text, in the IRC font), `caption`,
  `canvas` and `noop`. There is no `lambda` or `define`, and a drawing is
  bounded: 20,000 steps, 40 image operations, 24 million pixels made, 64
  levels of nesting, 4 pictures of the thread. `(image x)` takes a
  picture of the thread, a handle, a program as text, or one of the
  library's blank canvases on dead hosts (`white100x100.png`,
  `trans100x100.png`, `whitepixel.png`, `1blackpixel.png`), which are
  made here; any other URL fails the drawing with a note, so procs whose
  pictures lived on dead hosts (`qwantz`, `macify2`) still fail. The
  words `annotate` and `caption` write pass the word checks. Off Bluesky
  `magick_scheme` is an error: nothing draws there.
- **The base library's verdicts** come from `scripts/slopdrop-prebake.py`
  (P5): the worst names by hand, the random-stamp procs, every base proc
  rated by a cheap text model, and every `ST*` stamp drawn and shown to
  the image check once, a stamp also by its source (the worse verdict
  wins). Its rows are merged into the VPS
  `module-state/slopdrop/safety.tsv` after a backup. A proc that is only
  adult gets no row, so its picture posts with a self-label. The model
  is noisy both ways, so two passes follow. `--recheck` re-rates the
  model's own `unsafe` rows in small batches and keeps a row only when
  it agrees. `--fixups` applies two rules: a slur or hate stem in the
  name forces `unsafe`, and a pure stamp wrapper (`<x>say` whose body
  is `stampsay -s <x> [join $args]`) takes its stamp's verdict. Neither
  touches the owner's `approved` or `blocked` rows.
- **Text beside a picture** goes through the output check as text; a
  text that is withheld withholds its pictures with it.
- **Alt text**: the code's (`bsky::art -alt`), else the model's, else
  `Text art: <the words it shows>`; a drawing cut to the 248 × 123 cell
  grid says so.
- The PNGs stay in `<state>/art/` for a week, so a held reply's question
  and its approve read the same file.
- **A held picture is asked about with the picture** (P4): the ledger
  entry keeps each drawing (its PNG's hash, alt text, size and its
  text's hash), the labels, the rest of the code's spec and the text
  beside it. The owner's question carries the first picture, which
  Telegram sends as a photo above it, and says how many there are and
  their labels. The hosted copy shows the pictures above the output.
  One requester is asked about at most five withheld replies an hour;
  the rest wait, still listed and deletable.
- **Approve reposts the picture**, not its text: the kept PNGs, their
  alt text and labels, the code's quote or links and the text beside
  them, through the gateway's bus under the request. The withheld post
  comes down when the repost's delivery receipt arrives, never before.
  Approving allowlists each drawing's text, so the same drawing goes
  out next time; deleting blocks it. A drawing nobody checked
  (anyone's) is shown to the image check on approve, so it carries the
  warning it calls for.

**Reads** go through the bot's AppView session (search needs a signed-in
viewer: the public AppView answers it 403). Each Bluesky evaluation gets
a grant good for 5 reads, handed to slopdrop with the post and revoked
when the answer is back; slopdrop keeps it in Rust (`src/bsky.rs`), so
Tcl can use it but not read it. The reads arrive on this module's OPEN
route, `/api/plugins/slopdrop/open/bsky-read`, the one module surface
outside the desktop API's bearer gate (the challenge pages' shape: a
credential of its own); it allows only public profile, post, feed,
graph and search reads, never notifications, preferences or chat:

```toml
[slopdrop]
bsky_read_url = "http://127.0.0.1:8642"   # where SLOPDROP reaches this gateway's API
```

Unset, `bsky::query` answers that no read is available, and nothing
else changes.

**Events.** `bind LIKE|REPOST|FOLLOW|QUOTE|REPLY|MENTION bluesky proc`
binds like the IRC six (argument shapes in `tcl/triggers.tcl`;
`$::bsky::event` holds the whole event). The atproto plugin forwards its
unread notifications to the `bsky.event` service this module provides
(only the atproto plugin may call it); trigger ROWS scoped
`platform = atproto` decide, with their cooldowns, caps and the bridge
budget (`/triggers add … platform=atproto network=atproto channel=bluesky
event=LIKE action=slopdrop`); each answer passes the same public-reply
policy as `/tcl`, classified by the hash of the proc that wrote it, and
is posted through the gateway under the post it is about. A FOLLOW's
answer is never posted (there is no post to put it under).

## slopdrop → veles

### slopdrop's side

```toml
[veles]
enabled        = true
url            = "http://127.0.0.1:9900"    # veles' A2A server
token          = "…"                        # or token_env = "VELES_A2A_TOKEN"
timeout_ms     = 120000
max_per_eval   = 2      # model calls one evaluation may make
max_per_minute = 10     # …and per minute, across everybody
```

This registers one native command:

```tcl
ai <prompt> ?context?
```

`context` groups a conversation on veles' side; it defaults to a single
shared thread so follow-ups work.

It is native Rust rather than a `tcl/*.tcl` file, and that is not
gratuitous: `SafeTclInterp` is **one** interpreter with the dangerous
commands removed, not a master/safe pair with aliases across a boundary.
There is no variable, namespace or proc that user Tcl cannot read and
rewrite — so a Tcl bridge would have to keep veles' URL and token
*inside* that interpreter, where the first thing a channel does is
`puts $::veles::token` and the second is to point the URL at a listener
and make the bot POST its own credential there.

The budgets exist because an `ai` call spends the veles operator's model
tokens and any proc can make one.

### veles' side

```
A2A_PEER_TOKENS=slopdrop:<the same token>
```

The **name** on that line is the identity veles' ACL resolves, so what
the bridge may do is one line of config:

```toml
[acl.roles]
"a2a:slopdrop" = "user"
```

`user` is also the default when no rule names the peer — an A2A peer
arrived holding a credential the operator minted, so "unlisted" means
"role unstated", not "a stranger walked in". A rule raises it to
`trusted` or removes it with `banned`.

veles already wraps A2A inbound text as untrusted external input and
defangs it, which is the right posture for a prompt that came from a
channel.

## Verifying it

```sh
# slopdrop's API refuses what it should
curl -s -o /dev/null -w '%{http_code}\n' -XPOST $SLOP/api/eval \
  -H 'content-type: application/json' -d '{"code":"expr {1+1}"}'      # 401

# the bridge, from the channel
tcl expr {6 * 7}            # 42, and (state: …) when it changed something
tcl set x 100
tcl set x                   # 100 — a LATER message, so state persisted
tcladmin info commands exec # refused below owner, by name

# the other direction, from inside Tcl
tcl ai {Reply with exactly the word PONG and nothing else.}
```

Both halves were driven end to end against a local ergo on 2026-09-11. Since
2026-09-15 the bridge is LIVE in production: the `forward_slopdrop`
triggers landed and fire (`5c376ff`, `78ec3d9`, `1a17fdd`), and the
slopdrop state mirror/editor (a materialized mirror on veles' disk,
with the conflict-checked save the interpreter alone can honor) went
live 2026-09-16 (`e63468a`).
