# A pool of nodes: tickets, the headless signer, re-homed tools, bot challenges

A veles pool is two or more gateways that know each other: a laptop and
a phone, say. They share memory and sessions, a session can move or be
taken over, and a tool call can run on the node that should run it.

What a node may ask another to do is limited on purpose. The point is
not secrecy between your own machines but the blast radius of one of
them being compromised: a node that has been taken over must not be able
to make the others do anything it likes.

## What a peer may do without asking

A call from another node runs here at the **lower** of the caller's
level and this node's `User` role (ACL), with `User`'s capabilities and
sandbox routing. On top of that, a peer may use the browser tools and
read files on this host (outside veles' own state and the secret paths
the read jail fences). If you later change what `User` may do, this
changes with it.

Everything else — a host shell, owner-only tools, anything above `User` —
needs a **ticket**.

## Tickets

A ticket authorizes exactly one call: this tool, with these arguments,
from that node to this one, in this session, as this role, for ten
minutes, once. It is an Ed25519 signature made on a **device**, never by
a gateway: gateways hold only public keys and cannot sign anything.

Any trusted key signs, including one on the machine of the node that
asks. A compromised gateway already runs whatever it likes on its own
machine; what a ticket stops is it reaching the others at once. It holds
no key, so every raised call waits for you to read the call and sign it.
And with two devices, the survivor's own key is the one left to approve
a takeover when the other device is gone.

Make a key on each device that should be able to sign:

```sh
veles ticket keygen phone        # asks for a passphrase twice
```

It prints the public key and the lines to add on every node that should
accept its tickets:

```toml
[cluster.ticket_signers.phone]
key = "ed25519:…"
```

`[cluster]` is local to each node: config sync never carries it, so a
peer cannot add a signer to your trust list.

When a call needs a ticket, whoever is attached to the session is asked:

- **desktop**: a dialog that shows the call and signs it with a key on
  that machine (the passphrase goes to a `veles ticket sign` child on
  stdin, never on its command line), or takes a pasted ticket;
- **TUI / REPL**: paste the ticket;
- **IRC / Telegram**: the gateway posts the request; sign it on any
  key-holding device (`veles ticket sign '<request>'`) and send the
  `veles-ticket:1.…` line back as a plain message — the emergency path —
  or let the headless signer ask you (below).

An identity that holds a role BY TICKET (`ticket_role` in `[acl]`, see
[acl.md](acl.md#a-role-held-by-ticket)) uses the same tickets for calls
on its own node: the origin and the target are both this node.

## The headless signer (IRC and Telegram)

A chat client cannot unlock a key, so `veles signer` does it for them.
It is its own process with its own key, its own Telegram bot and/or IRC
nick — never the gateway's — and it signs only when **you** say yes on
that bot or nick.

Run it on the device whose key it holds; any node may ask it, the one
beside it included. Its directory is the key directory `veles ticket
keygen` wrote (`~/.config/veles-signer`):

`signer.toml`

```toml
key  = "laptop"                  # laptop.key in this directory
listen = "10.0.0.1:9911"         # where the gateways reach it

[telegram]
owner_id = 123456789             # your Telegram user id: only your press counts
chat_id = -1001234567890         # optional: show requests in this group (your ops chat) instead of the DM

[irc]
server = "irc.libera.chat"
nick = "wrath-signer"
owner_nick = "wrath"
owner_account = "wrath"          # your services account: only a logged-in reply counts
```

`signer.env` (chmod 600)

```sh
SIGNER_GATEWAY_TOKENS=phone:<a long random token>
SIGNER_TELEGRAM_TOKEN=<its OWN bot's token>
# SIGNER_IRC_PASSWORD=<SASL password for its nick>
```

```sh
veles signer run                 # asks for the key's passphrase
# or, as a service: SIGNER_PASSPHRASE_FILE=/run/credentials/…/pass veles signer run
```

Open a chat with its bot once (`/start`) so it may write to you.

On each gateway that should ask it (here, the phone):

```toml
[cluster]
signers = ["http://10.0.0.1:9911"]
```

and `VELES_SIGNER_TOKEN=<the same token>` in that gateway's `.env`.

When a call needs a ticket, the gateway asks the signer at the same time
as the session's own surfaces. The signer shows you the call in full and
waits for **Sign** / **Decline** (Telegram) or `yes CODE` / `no CODE`
(IRC). The first answer anywhere decides; a decline on the signer is a
decline.

The strong form runs the signer as its own Unix user, so the gateway
cannot read its key file or its tokens at all: the VPS runs it that way,
in its own container (deploy/README.md).

## Re-homed tools

A tool can run on another node — web fetches from the laptop's
residential IP, the browser where there is a display:

```toml
[tools.home]
"web_*"     = ["laptop", "vps"]   # the laptop's IP, else the vps
"browser_*" = "laptop"            # one node: never falls back
```

or for one session: `/tools home web_* laptop` (`/tools home web_* -`
clears it; a session rule names one node). The call passes every gate
on the calling node first, then runs on the other under the rules above.

A list is an order. A node is passed over only when it is
**unreachable** — its heartbeat stale, or the connection never opened —
because then nothing can have run there. A node that answered and
refused is the answer: a call never shops for a node that allows it.
This node in the list means "here, at that position". The result says
where it ran and which nodes were passed over and why. With one node
and that node down, the call fails and says so; it never quietly runs
here instead.

Tools that act on the session itself (todo, memory, skills, delegation)
never leave. The peers must be in each other's `[cluster] task_peers`.
`[tools.home]` replicates with the rest of `[tools]`, so name nodes the
same way everywhere (an id prefix works when a node's name is its
hostname).

### Automatic homes

Each node advertises what it has — its OS and which of a fixed list of
programs it finds on PATH at start (bash, git, rg, jq, curl, ssh,
sqlite3, tclsh, podman, systemd-nspawn, firecracker, jail,
agent-browser, edge-tts, xclip/xsel/wl-paste, python3, node, proot) —
in its registry row, which gossip carries like the rest. `veles nodes`
prints them; `veles nodes whoami` prints this node's. Override the probe
where it is wrong:

```toml
[node.caps]
tools = ["bash", "rg", "podman"]   # replaces the probe's list
os = "android"
```

With no `[tools.home]` rule for a tool, a call whose program this node
lacks goes to a live `[cluster] task_peers` node that advertises one,
least loaded first, and the result says `[<tool> ran on node <id>]`:

| tool | needs one of |
|---|---|
| `search_files` | rg |
| `browser_*` | agent-browser |
| `text_to_speech` | edge-tts (not with `[agent] tts_provider` set) |
| `clipboard_*` | xclip, xsel, wl-paste |
| `execute_code` | tclsh |
| `terminal` | bash, sh |

A rule wins: `"search_files" = "local"` (or a session's `/tools home
search_files local`) keeps it here whatever this node lacks. `[tools]
auto_home = false` turns automatic homes off. A node that advertises
nothing (an older build) is never chosen, and a node that cannot say
what it has itself runs everything here. `delegate_task` takes `needs =
["podman"]` the same way: `gpu`/`any` choose among nodes that have them
all, a named node without one is an error naming it, and `needs` alone
means `any`.

## A node without a sandbox

A phone has no container runtime, so a call that belongs in a sandbox
cannot run there: `execute_code{sandbox}`, or an IRC, Telegram or peer
caller's terminal and file tools. Name a node that has one:

```toml
[cluster]
sandbox_home = "<laptop id>"
```

Those calls (terminal, read_file, write_file, patch, search_files and
execute_code, when their zone is a sandbox) go there as peer calls, and
land in a box of their own on that node, never on its host. An explicit
`[tools.home]` rule still wins. A background shell is refused by name:
shells live in one process, and the home keeps none for another node.

Without `sandbox_home`, a node that advertises no runtime (podman,
systemd-nspawn, firecracker, jail) sends those calls to the least loaded
task peer that does (the automatic homes above; `[tools] auto_home =
false` or a `[tools.home]` rule for the tool keeps them here). With no
such peer the call fails, naming the missing runtime and this setting.

## Memory replicates on its own

```toml
[cluster]
memsync_auto = true
memsync_peers = ["laptop", "vps"]
```

Each node pushes its memory revisions to every `memsync_peers` entry, on
the store sync's cadence, past a cursor per peer that moves only once
the peer has stored them; a peer that was down gets the backlog. The
receiver applies them at once: new paths and fast-forwards land. A path
both sides changed is a **conflict**: the local content stays, the offer
stays in the inbox (`veles memory inbox`), and `veles memory apply
--inbox` is where you pick. Revisions travel on, so memory reaches a
node whose only link is through another. Without the setting, sync is
by hand: `veles memory push <peer>`, then `veles memory apply --inbox`
there.

## The owner's questions follow you to Telegram

`[notify] ask_relay` sends an approval, a clarify or a failover question
nobody answered to your Telegram home chat. In a pool the Telegram
adapter runs on whichever node holds its lease. A question born
elsewhere, say on the laptop's desktop, asks the arbiter who that is
and goes there over A2A. That node posts it in its own home chat (never
a chat the asker names), and only for a peer in its `task_peers`. The
first answer anywhere decides, and the other legs close. Secrets are
never relayed.

The home chat is the ops chat (2026-10-21). Point
`[platforms.telegram.home_channel] chat_id` at a private group with the
bot and your DM stays a working session: relayed questions go to the
group, so does cron output with `deliver = "home"` (the default for a
job made in a chat whose platform has a home chat; `deliver = "origin"`
keeps it in the chat), and so do the signer's tickets once
`[telegram] chat_id` in signer.toml names the same group. A plain reply
in the home chat answers the question waiting there, no reply-to and no
@mention needed, and starts no turn; with no question waiting a plain
message starts nothing either, since the home chat keeps the same
@mention gate as any group (the ops chat and the chat sessions split
cleanly, 2026-10-21). The adapter names the home chat at boot, beside
the door.

The questions a node holds for you about sync (a config change to a
security section, a memory conflict, a vault join) go the same way, and
they wait for you even when the node that asked has stopped. A phone
stops five minutes after its last use, usually long before you answer.
The node that posted the question keeps it open and keeps your answer;
the next time the phone runs, it asks again in the same round, takes
the answer, and acts on it. You are asked once per round, however often
the phone restarts in between. A button on a question that is no longer
open says "no longer waiting".

## A three-node pool: the VPS as the hub

The laptop, the phone, and an always-on VPS that holds the arbiter,
Telegram, IRC and cron (deploy/README.md builds it). Each node lists
the other two; one token per direction (`[a2a_agents.X].auth.token` is
what this node sends X, `A2A_PEER_TOKENS` what each peer sends here).
The phone's half:

```toml
[a2a_agents.laptop]
url = "http://10.0.30.2:9900"
auth = { type = "bearer", token = "<phone→laptop>" }

[a2a_agents.vps]
url = "http://10.0.30.1:9900"
auth = { type = "bearer", token = "<phone→vps>" }

[cluster]
arbiter = "<vps id>"
gossip_peers = ["laptop", "vps"]
memsync_peers = ["laptop", "vps"]
task_peers = ["laptop", "vps"]
storesync_peers = ["laptop", "vps"]
storesync_auto = true
memsync_auto = true
failover = true
signers = ["http://10.0.30.1:9911"]   # the headless signer beside the VPS
sandbox_home = "<laptop id>"           # the phone only

[cluster.ticket_signers.vps-signer]
key = "ed25519:…"
[cluster.ticket_signers.laptop]
key = "ed25519:…"

[tools.home]
"web_*" = ["<laptop id>", "<vps id>"]
"browser_*" = "<laptop id>"
```

and in its `.env`: `A2A_PEER_TOKENS=laptop:<laptop→phone>,vps:<vps→phone>`,
`VELES_SIGNER_TOKEN=<its token at the signer>`; and in its own
config.toml, `[platforms.a2a.extra] host = "10.0.30.3"` (node-local: configsync
never ships it; `A2A_HOST` in the environment or `.env` still overrides it).
A node that must not run IRC sets `IRC_ENABLED=false` (the `.env` wins
over the shared config's `enabled = true`), and only the VPS's `.env`
holds `TELEGRAM_BOT_TOKEN`.

**Moving the arbiter** to a fresh node is one window: stop the others,
start the new arbiter, start the others with its id as `arbiter`. Wait
until every `veles nodes list` names all three, then restart the laptop
and the phone once more. They booted while the new arbiter's registry
was empty, so their fences refused; the restart arms them, and each
then holds its node lease there. `nix run .#pool-three-rehearsal` is
this whole pool on one machine.

**Accepted limits.** When the VPS is down, IRC, Telegram, cron and
dreaming stop, and no node can take a session over: the arbiter and
those duties share one process. Turns on the laptop and the phone go
on. Skills and plugins stay per node. The phone's sandboxed calls fail
while the laptop is down: `sandbox_home` names one node.

**Config sync is automatic** (`[cluster] configsync_auto`, on by
default, with `memsync_peers` as the peer list). A node notices an edit
to its own `config.toml` on the next sync tick and pushes the changed
keys to its peers, which apply them as they arrive. A key that describes
the machine stays on it even inside a shared section: the IRC
`media_public_dir`; `web.backends`, `searxng_url`, `searxng_wake_cmd`
and the `others*` egress keys; and the fence itself — `sandbox.backend`,
`image`, `kernel_path`, `extra_args` and `mounts` (a Windows node's
`native` must not become the hub's) — while the sandbox *policy*
(memory, network, capabilities, zones) is shared
(`configsync::NODE_KEYS`). Two kinds of change wait instead:

- a change to `acl`, `approvals`, `sandbox`, `hooks`, `plugins` or
  `mcp_servers` — who may do what, what runs without asking, and what
  code is loaded;
- an offer whose keys two nodes wrote within two minutes of each other
  (a conflict; the later write is put in the file, the decision is yours).

The desktop's status line counts what waits. `/configsync` lists it,
`/configsync apply` lands it and `/configsync drop` sets it aside — the
answer for a change that is right on its own node and wrong here (a
peer's `[sandbox]` for its own runtime). A default route that should
differ per node belongs in `[node.overrides]`, which never travels.
Before a first sync between two nodes, `veles configsync apply
--dry-run <offer>` shows what would change. An offer that could not be
applied when it came (an older binary parked it) gets another try on
the next sync tick after the gateway starts, or when the next one
arrives.

**The vault travels the same way.** A `veles secret set` (or any vault
write, admit or revoke) goes to every `memsync_peers` peer that lacks
it, on the next sync tick, as the whole vault file: it is small, and
what a peer already has is skipped (the receiver records the rows each
sender holds, so nothing is sent back). A peer that already has the
vault merges it on arrival; a row still counts only when a member
signed it. Two steps stay yours, because each needs a fingerprint read
off the other machine: admitting a node (`veles vault admit <node>
--fingerprint …`) and a node's first sync (`veles vault apply --inbox
--trust <root fingerprint>`). Until then its inbox keeps only the
newest offer from each peer. `veles vault push <peer>` still works by
hand.

**The phone yields.** Its gateway runs `--exit-when-idle 300` (the
APK's GatewayService): five minutes after anyone last used it — a
desktop request, an inbound message, a turn in flight — the engine
stops, the service with it, and opening the app starts it again. A
reboot or an update starts nothing (no boot receiver). The desktop,
backgrounded, stops its polls, the banner's animation and every
session's event stream, so an idle phone holds no socket and wakes no
radio. Peers see it as away: their gossip says so once, and their
pushes to it back off. When it answers gossip again they clear that
backoff, so what it is owed goes on their next sync tick while it is
up, not after a wait that outlasts its five minutes. A gateway that
stops when idle never asks for the cron lease, nor stands by for a
Telegram or IRC lease; a node that stays up holds those (the laptop
stays the VPS's standby). It fires its own cron jobs regardless — cron
state is per node, and nothing else would — and its service's alarm
starts it for the next one (docs/phone.md "The next cron job"). Backgrounded, the desktop's own runtime
(AndroWish's SDL and Tk) looks for events 4 times a second instead of
polling hundreds of times. Its battery draw
before this matched the charger's supply (2026-10-04).

**Sync offers are compressed** (`enc:z` in the fence: base64 over a
DEFLATE stream; a 500-row chunk is ~4.7× smaller). A node from before
it cannot read one and refuses it ("payload is not base64"); the
sender then sends that peer the plain layout until it restarts, so a
pool upgraded one node at a time keeps syncing.

## Joining by invite (a QR code)

A new machine — a phone, most often — joins the whole pool from one
invite, instead of hand-written tokens on every side.

1. **On the arbiter**, `veles pool invite <name>` (the name the pool will
   call the newcomer: a-z, 0-9, -). It refuses on any other node. It
   needs `[node] endpoint` set: the newcomer dials the arbiter there.
   - With `[pool.wireguard]` set, the hub also mints the newcomer's
     tunnel. It generates a key pair and the next free address in the
     subnet, queues the hub's side for the root helper, and prints the
     WireGuard app's code first.
   - Without it, `--url http://<its address>:9900` says how the pool
     will reach the newcomer.
   - The veles code follows. `--png <dir>` also writes both codes as
     pictures; `--text` prints only the invite's text, for pasting.
2. **On the newcomer**: on a phone, use the Phone page's *Join a pool*
   and the camera (docs/phone.md). Anywhere else, run `veles pool join
   '<text>'` (or a file holding it, or a photo of the code). Joining shows
   what it will write and asks first.
3. The newcomer's first call **redeems** the invite at the arbiter with
   its new bearer token. Until then that token opens nothing but this
   one call. The arbiter writes its own side of the link, then
   **introduces** the newcomer to every member in its peer lists: a
   fresh token pair per member, sent over the link it already has. A
   member takes an introduction only from its `[cluster] arbiter`, over
   the link it dials the arbiter at. The arbiter answers with the
   members, and the newcomer writes its side of every link. It also
   writes `[cluster] arbiter`, the signers, `[node] endpoint`,
   `[platforms.a2a] enabled` and `[platforms.a2a.extra] host` (its bind
   address, from the invite URL; node-local), and seeds its registry with
   the arbiter and the members, so its first boot finds them.
4. Restart the newcomer, so its A2A server starts.

An invite holds credentials. It is good for 10 minutes (`--ttl`), once.
Show it only on your own screen. `veles pool invites` lists them, and
`--revoke <name>` drops an open one and queues its tunnel's removal.

A join introduces the newcomer only to the members that answer at that
moment; an ephemeral node that is off then (a phone asleep, a
workstation powered down) never gets the link, and the two see each
other only through the hub. **`veles pool introduce <a> <b>`, on the
arbiter**, closes that gap later: it mints one fresh token pair and
sends each member its side over the arbiter's own link, exactly as a
join does. Both must answer. The same command **rotates** a pair's
tokens: a member that already has the peer, at the same url, takes the
new tokens in place of the old (a peer at another url is refused — an
introduction never re-points a link). A gateway accepts the new tokens
at once (`.env` is read per call); restart one whose peer lists gained
a name, so it gossips with its new peer.

What joining does **not** widen: what a peer may do. A newcomer is a
User on every node (see "What a peer may do without asking") until you
name it higher or a ticket says more. Vault admission stays your own
step: `veles vault admit`, with a fingerprint read off the other machine.
`pool` is one of the sections the agent's `config` tool may read but
never write.

**The hub's WireGuard** (`[pool.wireguard]` on the arbiter):

```toml
[pool.wireguard]
subnet = "10.0.30.0/24"
address = "10.0.30.1"              # the hub's own address in it
endpoint = "vps.example.org:51820"    # where peers dial the hub
public_key = "<wg show wg0 public-key>"
# interface = "wg0", keepalive = 25 (the defaults)
```

The gateway cannot touch `wg0`: it runs rootless. It queues each new peer
in `<data dir>/pool/wg-pending/`. On the VPS, `setup.sh --with-wg-peers`
installs `veles-wg-peers`, a root path unit that applies the queue.
It accepts only a valid key and a /32 inside the subnet that no peer
already holds, so a gateway cannot take over another node's address. It
reads no symlinks and rewrites the interface's config from what it
parsed. `scripts/test-wg-peers.sh` pins those refusals.

## A node whose home was lost

A member keeps little only on itself: its sessions are in its peers'
replicas of its store, its memory in theirs, its config in every node's
config log. On the arbiter, `veles pool invite <name> --recover` mints a
recovery invite for that member — the node id this arbiter binds the
name to, the url it links it at. On the bare machine, `veles recover
'<invite>' --as <name>` takes that id back, re-links the whole pool with
new tokens (the redeem rotates the arbiter's side, the introduction each
member's), folds its sessions back from the peer's replica, pulls its
memory and config, copies the peer's skills and commands where none are,
and sends the vault a join request a member admits. It ends by listing
what it could not restore. The whole procedure, from a bare machine:
[docs/recovery.md](recovery.md).

The peer answers a recovery ask only for the node it binds the asker to,
and each kind of data only to the list that already carries it
(`storesync_peers` for the replica, `memsync_peers` for memory and
config); skills and commands, which no sync carries, only for a day after
a recovery invite re-linked that peer.

## Bot challenges (captchas)

A page behind a "verify you are human" check — Cloudflare's interstitial
or Turnstile, reCAPTCHA, hCaptcha, DataDome, PerimeterX, or a generic
gate — is put to you to solve for the agent, wherever you are talking to
it. The agent never tries to solve one itself.

- `web_extract` does not hand the model the challenge page as if it were
  the page: it opens the same URL in a browser and puts the challenge to
  a person itself. An owner's session uses the agent's browser; anyone
  else's uses the guest browser, behind its own proxy, with no cookies of
  the owner's.
- A browser tool that lands on a challenge asks the people on the
  session instead of answering at once. You see the page as a picture and
  act on it; each action returns a fresh picture, until the check is gone
  or you say `solved` or `give up`. The model then gets the real page,
  told whether you solved it or it cleared on its own — or, if you gave
  up or nobody answered, the challenge and why.

Where you answer:

| surface  | what you get |
|----------|--------------|
| desktop  | a dialog with the picture: click on it where you would click on the page; Type, Enter, Tab; Solved, Give up, and Open a window where the browser's node has a display |
| Telegram | the picture, the question, and a link to the relay page |
| IRC      | the question and a link to the relay page |
| TUI, CLI | the question; answer `click X Y`, `type TEXT`, `key Enter`, `solved`, `give up` |

**The relay page** is a small web page the gateway serves for as long as
the relay runs, at `/challenge/<token>` on its desktop API listener: the
picture, the same buttons, a click on the picture sent in the page's own
pixels. Its random token is its only credential (a chat client has no
bearer token to send) and it is gone the moment the relay ends. The link
names `http://127.0.0.1:<port>` unless you set `VELES_CHALLENGE_URL` to
the address your phone reaches the gateway at — over WireGuard, say:

```sh
VELES_CHALLENGE_URL=http://10.8.0.1:9900
```

The page is in English only for now.

**A real window.** Where the node running the browser has a display,
*Open a window* restarts the browser headed with its cookies and storage,
you solve the check there and answer `solved`, and it goes back to
headless with what you earned. The state it carries across lives in the
data directory (mode 0700) only for that moment.

**With a re-homed browser** (`"browser_*" = "laptop"`), the pictures and
the clicks travel to the laptop and the question stays with the session:
a session on the phone gets the laptop's browser and residential IP, and
the check still reaches you on the phone.

**The browser does not say it is automated.** Both browsers launch
without Chrome's automation flag (`navigator.webdriver` is false) and move
the pointer like a hand: Cloudflare turned down every solve in a browser
that said it was automated. `[web] browser_webdriver = true` puts the flag
back for a site that treats a declared automaton better; the browser
relaunches when it changes, and the page it had open is gone.

**A page left unsolved is not asked about again** in the same session for
30 minutes — given up on, unanswered, or out of time. The model is told
so, and that it should tell you or find the content elsewhere.

Limits: a relay gives up after 40 rounds or 15 minutes, and each question
after 10 minutes unanswered.

## Downloads (PDFs)

`web_extract` reads web pages; a PDF it downloads instead, and so does a
relay whose solved page turns out to be one (the browser fetches it from
inside the page, with the cookies the solve earned). The file lands where
the session's commands run — its sandbox at `/work/downloads/<name>`, or
`~/.cache/veles/downloads/` on the host for a session that runs there —
and the model is told the path and to read it with `pdftotext`. The
sandbox image carries `pdftotext` and `file`.

A file is downloaded on whichever node runs the tool (the laptop, with
`"web_*" = "laptop"`) and brought to the session's node with the call's
answer: up to 8 MiB inside it, a larger one in 8 MiB parts the session's
node pulls after it and checks against its sha256. A download may be up
to 128 MiB, and has five minutes to arrive.
