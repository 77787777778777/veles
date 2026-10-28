# Access control, sandboxes and context ceilings

Everything here is manageable from **every** surface — `config.toml`,
`/acl` · `/sandbox` · `/limits` · `/lockdown` on IRC, Telegram, the TUI
and the REPL, the desktop's ACL and Sandboxes overlays, and
`veles acl|sandbox|limits|lockdown` in a shell. One implementation
(`frontends/commands.rs`) answers all of them.

## One command language

There is no separate IRC command dialect. The adapter's prefix is a
plain **sigil**: `.acl list` is `/acl list`, same name, same tier, same
implementation, and it exists only because IRC clients eat a leading
`/`. Anything the shared dispatcher owns is forwarded to it; a name
nobody implements is answered with "unknown command" rather than fed
to the model as a prompt.

Four commands stay in the adapter, because the gateway serialises a
chat behind a lock a running turn holds and these cannot wait for it:
the ask verbs (`asks`, `answer`, `approve`, `deny` — the turn that is
running is the one waiting for the answer), the ACL family (what you
reach for while something is going wrong), and `irc` / `tcl`, which
need the live connection. Same names, same tiers; only the code path
differs. `IRC_IMMEDIATE` is that list and a test keeps it honest.

`.agent <verb>`, which had its own verb table and its own drifting
copies of `/acl`, `/todos`, `/checkpoints` and `/restore`, is gone
(USER, 2026-09-09: *"Uniform user interface is good."*). Typing it
answers with the new spelling.

The sigil is configurable, which doubles as the migration path: set
`command_prefix = ".agent "` and the two-word form keeps working
verbatim, because the prefix is stripped before the name is read.
`command_prefix = "!"` gives `!acl list` for a channel that already
has another bot on `.`.

Two IRC-only names survive, declared as such: `.irc`
(`status | history … | nick`) for the connection, and `.tcl` for the
channel evaluator. `/status` means the SESSION's status on every
surface, which is why the connection report could not keep that name.

## The identity is the hostmask

There is **no NickServ/Services gating of any kind**. On a network with
Services an account is one more identity a rule may name, and a
stronger one when it exists; it is never required for anything.

What identifies a sender is `nick!ident@host` as seen on the wire. What
tells two senders apart downstream — sessions, sandboxes, ask binding,
`/cancel` — is the principal's **subject**, resolved in this order:

1. the `[[acl.users]]` name that matched (`user:<name>`), so one human
   is one subject across every network and platform they match;
2. the verified account id (`irc:wrath`, `telegram:77`);
3. the observed mask (`irc:libera/wrath!~w@host`);
4. `anon:<chat>` when the platform told us nothing at all.

A mask-shaped id is deliberately **not** stuffed into the `id` field:
`id` takes part in exact-spec matching, and a string containing `!` and
`@` pasted into a rule would be read as a mask *pattern* and match
nothing. `/acl` therefore prints the **mask** as the pasteable spec for
a mask-derived principal.

Nick-only masks (`vic!*@*`) are legal and remain spoofable — a nick is
the most transferable thing on IRC. `/acl explain` says so, and the
desktop's roster menu does a WHOIS before it offers one.

## Precedence is specificity, never declaration order

The **narrowest matching rule wins**, regardless of where it was
declared or which source it came from — including over a broader ban.
A tie at equal specificity resolves to the **lower** role (fail-closed).

Where a rule came from is the **last** thing compared — after identity,
specificity, scope and role. So a grant in the runtime store is not a
weaker copy of a line in `config.toml`: a grant scoped to one network
beats an unscoped line for the same identity, and a grant at a lower role
beats a higher one, because both comparisons happen before source is ever
consulted. `/acl list` therefore reports a *standing* for each runtime
grant — whether the store copy decides anything, and if not, which file
rule outranks it — instead of assuming the file wins.

Specificity is compared as the tuple `(host, ident, nick)` — host
first, because it is the segment a client can least choose — with each
segment scored:

| segment | score |
|---|---|
| exact literal | `1000 + len` |
| glob with literals (`*.example.com`) | `100 + literal chars` |
| `re:` regex | `200` |
| bare `*` or absent | `0` |

An exact identity spec (`irc:wrath`, `telegram:77`, `user:wrath`)
outranks every mask. Scope narrows further: a rule naming a channel
beats one naming only a network, which beats one naming only a
platform.

So this config gives the three answers the owner wanted, in any
declaration order:

```toml
[acl]
[[acl.entries]]
identity = "*!*@*network-default-masking"   # everyone
role     = "user"

[[acl.entries]]
identity = "me!me@1234-network-default-masking"
role     = "owner"

[[acl.entries]]
identity = "jimmy!asshole@444-network-default-masking"
role     = "guest"
```

`/acl explain jimmy!asshole@444-network-default-masking` prints the
winner, its specificity and source, and the runners-up it beat. That is
the tool that makes this auditable; reach for it before editing a rule.
A scoped rule needs the scope in the question before it can match:
`/acl explain <id|mask> [network] [channel] [platform]`, either
positionally or as `--network` / `--channel` / `--platform` — both forms
give the same answer, and a flag `explain` does not know is refused by
name rather than read as somebody's network.

## Roles

| role | what it adds |
|---|---|
| `banned` | nothing — no turn, no command, dropped at the transport |
| `guest` | **read-only, inside the sandbox**: chat, read files, `/status` |
| `user` | + write files, shell, web, the browser, tools — still sandbox-only |
| `trusted` | + memory, skills, speaking as the bot in its own buffer |
| `admin` | + operator work (join/part/kick/mode), cron, background |
| `owner` | everything, including `grant`/`revoke` |

Guests deliberately do **not** get `web`: a fetch costs money and is an
SSRF surface. They get the filesystem of their shared box and the
channel, nothing else.

Everyone below the owner who has `web` or `browser` uses it the way
`[web] others` says on the node that runs the call (PLAN-challenge-relay
D3, D5, D6). Their searches and fetches go through the node's proxy (Tor
on the pool's VPS), and their `browser_*` calls drive a browser in their
own sandbox box there. On a node that does not proxy their traffic the
browser is refused, so it never runs as the owner's browser or from the
owner's address. That includes admins, who had `browser` before
2026-10-13 and drove the owner's browser with it. The owner's own
session can choose that path too (`/egress tor`), to see what other
people see; by default it uses the owner's address.

The owner can also join someone else's session as a second person in it
(`/join`, owner-only; see docs/session-mail.md). A line the owner writes
there runs with the owner's rights, and its questions are asked of the
owner only. That person's own lines keep that person's rights.

## `[acl]` reference

```toml
[acl]
owner        = ["irc:wrath", "me!me@host"]   # exact ids or masks
default_role = "guest"                        # unmatched senders

[acl.roles]                                   # exact-identity pins
"telegram:77" = "trusted"

[[acl.entries]]                               # masks and scopes
identity = "*!*@gateway/veles"
role     = "user"
network  = "libera"        # optional
channel  = "#veles"        # optional
platform = "irc"           # optional
extra_caps = ["web"]       # optional
deny_caps  = ["shell"]     # optional
max_context_tokens = 120000 # optional, per-rule ceiling

[[acl.users]]                                 # one human, many identities
name       = "wrath"
role       = "owner"
identities = ["irc:wrath", "telegram:77", "wrath!*@gateway/veles"]

[acl.limits]
default_max_context_tokens = 250000   # any non-admin
[acl.limits.by_role]
guest = 150000
admin = 0                              # 0 = unlimited
owner = 0
```

`[acl]` is **live**: it is re-read when the file changes, and a role
change takes effect on the caller's next turn. No restart, and no
frozen session principal.

With **no `[acl]` section at all** the ACL is dormant and resolves
every sender as owner. That is the documented single-user posture, and
it is why a **command line on IRC refuses outright** on a bot with no
boundary declared: "declare a boundary" is the answer, not "identify
to services". Both spellings are covered — the sigil and a literal
`/name` — because after the fold they are the same thing; before it,
only the sigil path checked, so `/restore 1` from a stranger on an
undeclared bot resolved them as the owner. Four reports stay open even
then (`help`, `caps`, `asks`, `irc`): what the bot is, what commands
exist, what is pending, and the caller's own level.

## A role held by ticket

An identity the agent itself can speak as must not be the owner. The
Bluesky test account is the case this was built for (2026-10-11): the
agent holds its password to run the tests, so whatever that account may
do, the agent may do. Give it a standing role and a ticket role:

```toml
[[acl.users]]
name        = "tester"
role        = "user"       # what it holds, always
ticket_role = "owner"      # what ONE call may be raised to, by ticket
identities  = ["atproto:did:plc:5gpaqik336k7xsb6duz2x6k2"]
```

It stands at `role`. A call above that runs at `ticket_role` only on a
[ticket](pool.md#tickets) for exactly that call, signed on a device the
gateway does not hold. The headless signer puts the call to you on its
own Telegram bot or IRC nick, or you paste a ticket you signed
yourself. Two things ask:

- **a tool call** the standing role is refused: the agent loop asks for
  a ticket instead of refusing. Plan mode and a plugin's veto still stop
  it first. The call runs at the role the ticket grants, never above
  `ticket_role`, and only if that role passes the tool's own gate.
- **one command**, sent as `/sudo <command>`, for example
  `/sudo /tcladmin …`. It takes a built-in command only, never a
  prompt. The command runs at the granted role, and the session keeps
  its standing role. A refusal of a command above the standing role says
  how to ask. On a public feed `/sudo` passes the command filter for
  this caller alone, and the raised command is judged again as the
  granted role.

Each ticket is good for one call and ten minutes. A decline, a timeout,
or a ticket for any other call, arguments or session runs nothing, and
the reply says which.

`role` stays the standing role on purpose. A node too old to know
`ticket_role` ignores the key and reads plain `user`. It never reads an
owner. `/acl explain` shows such a rule as `user, owner by ticket`.

## Runtime grants, and folding them back into the file

`/acl grant` and `/pair` write the **runtime store** (`state.db`), not
`config.toml`. That is deliberate — a grant made from a chat line while
something is going wrong should not need an editor, and a temporary one
can expire by itself — but it leaves two sources, and the store copy is
the one a `state.db` reset takes away.

`/acl list` merges them the way resolution does and says, per grant,
whether it decides anything:

```
runtime grants the STORE decides (lost if the store is reset — `/acl promote` writes them into the file):
  `bob!*@libera/host` on libera = user (granted 1789059011)
    no rule in the file names this identity — the store is the only source
```

The reason is one of: nothing in the file names the identity; the file's
line is scoped differently, and scope is compared before source; the file
names it at a higher role, and the lower role wins the tie; or the grant
is **inert** — expired, unparseable, or outranked by a file rule at the
same scope with an equal-or-lower role — in which case the listing says
so and dropping the copy changes nothing.

`/acl promote` (owner) writes the deciding grants into the file and drops
the store copies, so the file becomes the one source instead of the source
that happens to agree today:

```
/acl promote [--dry-run|-n] [--include-expiring] [--users] [--prune]
```

- The write goes through the same comment-preserving editor `veles config
  fill` uses: ordering, comments and every other value survive, a `.bak`
  is written beside the file, and because `[acl]` is live the change is
  in effect immediately — the report says so rather than implying it.
- Each appended entry carries `# was a runtime grant (granted <ts>)` and
  the scope it was granted with.
- `--dry-run` prints the exact lines and writes nothing.
- **It refuses on a box with no `[acl]` at all.** Writing the first entry
  is what turns dormant mode (every sender the adapter allows resolves as
  owner) into curated mode (every identity the file does not name falls
  to `default_role`). A one-line edit that locks the operator out of
  their own bot is not a side effect a tidy-up command gets to have;
  declare the boundary first.
- A grant that has **not expired** stays in the store: a config entry
  cannot expire, so promoting one would quietly make a temporary grant
  permanent. `--include-expiring` does it anyway and says so.
- An inert grant is never written — there is nothing to say. `--prune`
  drops those copies, which is the same tidy-up in the other direction:
  same decision, one source.

Promote appends, and it does not reconcile — but it no longer appends
blindly. Every candidate is compared with the same function resolution
uses, precedence key included, so a grant the file only *partly* covers is
written and the report names the line that shadows it: which callers each
side keeps, and which edge decided it (exact identity over mask, mask
specificity, scope, role, and only then source). Two lines that genuinely
disagree — the downgrade case — are still a human edit, because merging
them means reading both; the difference is that the report says so instead
of leaving the operator to find the second line later.

Runtime **users** (`/acl user save`) fold in with `--users`, as
`[[acl.users]]` tables through the same editor and with the store copies
dropped. A name the file already carries with a different role or
different identities is refused rather than written: two tables sharing a
name would leave resolution merging two roles for one subject.

`/acl export --write` is `promote --include-expiring --users` — owner-only,
one writer into `config.toml`, and the report says which command it ran.
Plain `/acl export` still only prints.

Every frontend carries the control, not just the command line: the
desktop's ACL overlay previews through `POST /api/acl/promote` and gates
the write on that preview being current (changing an option afterwards
re-runs the dry run and tells you), and the TUI's `/acl` picker lists the
file's entries and users beside the store's, with the dry run and the write
as rows that run the shared command.

## Context ceilings

USER, 2026-09-09: *"The maximum context length for any non-admin user
is 250k tokens. Anything above that ask to compact or start a new
session"*, and *"maximum context for guests is 150k"*.

Over the ceiling the turn is **refused**, with a reply naming the
number and the two ways out (`/compact` archives the head, `/new`
starts fresh). It is not silently compacted: nothing implicit.

Admin and owner are uncapped by default. A per-identity override rides
the winning rule (`max_context_tokens` on an entry, user or grant).
`/limits` reads the table on any surface and `/limits set <role|default>
<tokens>` changes it live (admin); the runtime value wins over the file
until `/limits clear` hands it back.

## Spend limits

A context ceiling caps **one conversation**. It says nothing about how
many conversations a caller may have, so a channel of strangers each
entitled to a 250k turn, with nothing limiting how many turns they take,
is an unbounded bill. The only brake that existed was IRC's
`inbound_max_per_window`, which counts *lines* and was built to stop
flooding — it knows nothing about money.

Three brakes, all per **caller** (the ACL subject), all in
`[acl.limits]`, all live-reloaded:

| knob | what it bounds | when it fires |
|---|---|---|
| `default_max_tokens_per_day` + `tokens_per_day_by_role` | the bill: prompt + completion tokens over a rolling 24h | after the calls it measures — it is the ceiling, not the flood brake |
| `default_max_turns_per_hour` + `turns_per_hour_by_role` | turns started, rolling 1h | **before any provider call**, so a flood costs nothing |
| `max_concurrent_turns` | turns in flight at once | on arrival; refused, never queued |

```toml
[acl.limits]
default_max_tokens_per_day = 2000000   # 0 = unlimited
default_max_turns_per_hour = 60
max_concurrent_turns       = 2         # 0 = unbounded
[acl.limits.tokens_per_day_by_role]
guest = 200000
[acl.limits.turns_per_hour_by_role]
guest = 20
```

Admin and owner resolve to unlimited on every axis by default, and an
owner surface (no principal) is never on a budget at all — so nothing
changes for the operator. An explicit per-role entry outranks even the
admin exemption: a number written for `admin` was meant.

Notes worth knowing:

- **Tokens are counted per subject, not per session.** One caller holds
  one session per chat, so a session-keyed sum could not answer "what
  has this person cost". The `usage` table carries the subject; rows
  written before anyone was identified carry NULL and are charged to
  nobody.
- **Cached reads still count.** They are cheaper, not free, and a budget
  that ignored them would be gameable by keeping one prefix warm.
- **`max_concurrent_turns` is not the turn lock.** The gateway
  serializes one session (one person in one chat), so two people in one
  channel run side by side; this is the same person in several chats at
  once, which no session lock can see. In a shared room, a line for a
  different session than the previous one opens with its owner's nick
  (`a: …`).
- The turn-rate ledger is a **true rolling window** (the timestamps
  still inside it, and nothing else), not an hourly bucket — a bucket
  admits twice the limit across a boundary.
- **Commands are not turns.** `/status` and `/limits` cost no provider
  call and consume no budget.
- `/limits` shows all three tables plus the caller's own reading against
  them. `/limits set [context|tokens-per-day|turns-per-hour|
  concurrent-turns] <role|default> <n>` changes any of them live, and
  the runtime value binds the **enforcement**, not only the reports.

## Sandboxes

Below admin, tools run **inside a container, always** — routed and
enforced by the framework, not by the model. The model is *told* where
it is (`[env: sandbox <name>]` on every result, and a prompt note), and
that is awareness, not authority.

| caller | default zone |
|---|---|
| role < admin over a connector | sandbox, forced |
| role ≥ admin over a connector | sandbox, opt out per call (`host: true`) |
| no principal (desktop/TUI/REPL/CLI, cron, A2A) | host |

Two knobs flip those defaults:

```toml
[sandbox]
admin_default_zone = "sandbox"   # or "host" — connector admins/owners
local_default_zone = "host"      # or "sandbox" — everything with no principal
```

### What the fence actually is

"Sandbox" is a specific set of properties, not a mood. A user-env
container gets:

- **one mount**: its own workspace at `/work`. `[[sandbox.mounts]]` and
  `[sandbox] extra_args` are **not** applied here — those belong to the
  general-purpose sandbox in `sandbox.rs`, which is a different
  mechanism. So there is no host path in a user env but the one veles
  created for it.
- `--cap-drop ALL` (under the default `capabilities = "drop"`) and
  `--security-opt no-new-privileges`.
- memory **and swap** capped at `memory_mb`.
- a network namespace of its own — see below.
- a fixed hostname, so an env-side execution is identifiable as one.

File operations cross the fence by `podman cp`, never through the bind,
so a symlink planted in the workspace resolves inside the container.

#### The network posture

```toml
[sandbox]
user_env_network = "isolated"   # default; or "none", or "host"
```

- **`isolated`** (default) — the box gets its own network namespace with
  working outbound. The host's `127.0.0.1` is *not* the box's. The
  outbound half has a precondition: a rootless podman needs `pasta` or
  `slirp4netns`, and with neither installed it still creates the box —
  with no network at all, and nothing publishable into it. veles warns
  once when that is this host and `veles doctor --sandbox` counts it as a
  problem; it does not refuse to run, because a helper-less host should
  not fence everyone out of a working box.
- **`none`** — no network at all.
- **`host`** — the host's own namespace. **This hands every routed
  caller the host's loopback**, and loopback is where veles' own
  operator API lives: `desktop_api` binds `127.0.0.1` and serves ACL
  edits, sandbox binds and limits. That API now authenticates every
  route but `/api/health` with a bearer token it mints by default, so
  this is no longer the one-curl promotion to owner it was — but the
  host's loopback also carries A2A, whatever database or hermes instance
  is running, and every other daemon that trusts being unreachable, and
  a fence whose safety depends on enumerating them is not a fence. Set
  this only if you know why you want it.

`network = false` still means no network whatever `user_env_network`
says — it is the untrusted posture and it wins. An unrecognised value
falls back to `isolated`, the tighter answer.

Changing the posture needs a gateway restart (the section is read once
at startup). The **containers** need nothing: a box whose namespace no
longer matches the configured posture is treated as stale and rebuilt on
its next use, so tightening the knob reaches boxes that already exist
rather than only the ones created afterwards.

#### The fence is also the approval

`approvals.mode = "smart"` screens risky tool calls with an aux model
and asks a human when the judgement is close. In a **fenced** box that
gate is the wrong instrument: it was asking somebody to approve
`rm -rf ./build` inside a container that gets thrown away, and on IRC
the prompt goes to the channel where only the caller can answer it — so
the gate asked the stranger to approve the stranger.

A `Sandbox` zone whose box is fenced therefore **skips** the gate and
says so, as an `⚖ approval skipped` note naming the box and how to get
the gate back. The note is in the transcript. IRC and Telegram leave
every `⚖` note out of the chat unless that platform's
`[mechanisms.platforms.<platform>]` sets `verbosity = "normal"` or
`"loud"`. Fenced means all of: netns not the host's, capabilities
dropped, no `[[sandbox.mounts]]`, no `extra_args`. Open any of those and
the full gate returns. `approvals.mode = "manual"` also keeps it: asking
to be asked about everything is a decision the engine does not overrule.

`Zone::Host` is untouched — an admin working outside the fence gets the
same gate as always.

### The zone arguments

A tool call says where it wants to run with **`host`**, and which box
with **`sandbox`**:

| argument | meaning |
|---|---|
| *(nothing)* | the framework decides — the table above |
| `"host": true` | the host, if the caller may have it |
| `"host": false` | this call is fenced, said out loud |
| `"sandbox": "<name>"` | that named box, if the caller may reach it |

The boolean is `host`, not `sandbox`, on purpose (USER, 2026-09-09:
*"we should fail closed and default to sandbox, `host: true` should be
the flag to execute on host"*). Absence means the fence; leaving it
takes a word. A name beats `host: true` when a call carries both, and
anything malformed is read as *nothing said* — a typo must never open
the fence.

Below admin the `host` argument is ignored and **stripped before
dispatch**, so a transcript never records a host run that did not
happen. `"sandbox": true|false` — the pre-2026-09-09 spelling — still
parses with its original polarity, because a skill or a memory written
against the old schema asking for the fence must not silently get less
of one.

### Which box

The implicit box is **one per conversation**, created on first use:

* `chan:<platform>/<network>/<chat>` — everyone talking to the bot in
  `#foo` shares `#foo`'s box, which is what makes collaborative work
  possible at all;
* `dm:<subject>` — a query or DM gets a private one;
* `shared` — a global box that exists to be assigned by name.

Explicit assignments override both, resolved by the same specificity
engine the ACL uses:

```toml
[[sandbox.envs]]                 # declare one
name      = "quarantine"
image     = "localhost/veles-sandbox:1"
memory_mb = 512
network   = false
note      = "guests only"

[[sandbox.assign]]               # "these callers, in this place, use that box"
identity = "jimmy!*@*"    # mask, exact id, or user:<name>   (optional)
network  = "libera"                                        # optional
channel  = "#veles"                                        # optional
platform = "irc"                                           # optional
role     = "guest"                                         # optional
sandbox  = "quarantine"
```

At least one selector is required — a bind with none would capture
everyone, and is refused.

A caller's **reachable set** is their resolved default plus every box
any matching bind names. Naming anything outside it is a framework
denial (a tool-result row the model reads), never a silent fallback.
`role >= admin` may name any box that exists.

Nothing about this is IRC-shaped: `platform`, `chat_id` and `direct`
are filled by every connector, so "guests on bluesky are read-only in
their own box" is one rule.

### Sharing one is concurrent

Two people in one channel are two sessions pointed at one container,
and their calls run **at the same time** inside it — there is no
per-box queue and none is wanted (a channel where one person's
`cargo build` froze everyone else's `ls` would be worse than no shared
box at all). Two consequences worth knowing:

* **Writes race.** The stale-write ledger is what stops the silent
  half of it: a `write_file` to a path this session read is refused
  when the bytes changed underneath, so the loser of a race is told
  rather than overwritten.
* **The box is bounded.** `[sandbox] max_concurrent_exec` (default
  `4`, `0` = unbounded) caps how many container `exec`s one box runs
  at once, because a burst against one container's memory cap kills
  whichever process is unlucky. A call that cannot get a slot within
  30 seconds is **refused** and names the box, the limit and the knob;
  it is never queued silently. `/sandbox show <name>` prints the
  limit. Background shells escape the bound by design — their exec
  returns as soon as the runner is detached, and they are accounted by
  task id.

`user_env_idle_stop_secs` (default 3600) stops an idle container; the
workspace is a host directory and survives, so the next call starts it
again with everyone's files where they were — and with what was
installed into the box (apt, pip), which lives in the container itself.
A box is rebuilt only when its workspace binding or its network posture
no longer fits the config; a newer sandbox image reaches an existing box
only through `purge` (which keeps the workspace).

### Managing them

`/sandbox where` (open — a caller is entitled to know which box their
own files are in), then `list`, `show <name>`, `create <name>`,
`assign`, `unassign`, `drop <name>`, `use <name>`, `purge <name>
[--work]`. Declaring, assigning and purging are `cmd-admin`; `use` is
checked against the reachable set, which is a tighter gate than any
tier.

`drop` forgets a declaration. `purge` removes the container. `purge
--work` deletes the **workspace**, which holds people's files — a
separate decision on purpose, and the desktop confirms it.

## Lockdown

The emergency brake: **connectors are refused, the local frontends
carry on.**

```
/lockdown              # status, open to anyone
/lockdown on [reason]   # cmd-admin, from ANY surface
/lockdown off           # only from a LOCAL surface
```

| caller | `on` | `off` |
|---|---|---|
| IRC / Telegram / A2A | admin+ | **refused, any role** |
| desktop / TUI / REPL / `veles lockdown` | yes | yes |

The asymmetry is the feature. Engaging has to be reachable from the
place the trouble is — a channel, from a phone — or the brake is
useless. Lifting must not be, because an attacker who reached an admin
mask would simply lift it, and the brake would then be worth exactly
what that mask is worth. Being at the desktop, the TUI or the REPL is
different evidence in kind: not a role in a config file but access to
the machine veles runs on.

**What stops.** Every inbound connector message: no turns, no
commands, no tool calls, no ask answers — whatever the sender's role,
an owner over IRC included. Turns already running in connector
sessions are cancelled when it engages, because refusing only the
*next* message leaves the current one calling tools. Each sender is
told once and then met with silence; a refusal per line would make a
locked-down bot the loudest thing in the channel. `/lockdown` itself
still answers, so someone who was refused can find out why.

**What does not.** The local frontends, and cron: scheduled jobs keep
running and keep delivering, including to a connector. They are the
owner's own instructions, authored before the lockdown, and stopping
them is `/crons pause` (`/cron` is the alias).

**Where it lives.** Two sources, ORed. `/lockdown on` writes a record
to the store, so it survives a gateway restart — a brake a crash-loop
releases is not a brake. `[acl] lockdown = true` declares one in
config.toml, and **no frontend can lift that one**: the file is the
operator's word, so lifting it means editing it. The desktop's ACL
overlay shows which kind is engaged and hides its lift button for the
config-declared one.

The bot stays connected and stays in its channels throughout. Leaving
the network is a different decision, and a noisy one.

## Command tiers

`builtin_cap` gives every command a tier, and a missing arm is a test
failure rather than an open door. Four commands are `cmd-status` at
the name and tiered **inside**: `/acl`, `/sandbox`, `/limits` and
`/lockdown` each answer a question about the caller in their bare form
and manage the rules in their subcommands. One tier for the whole name
would either lock the self-report away from the person it is about or
hand `grant` to them — and `/lockdown off` needs a condition no
capability can express at all (a local surface), which is why the verbs
are checked in the command rather than at the name.

Tools are gated the same way and **fail closed**: a tool nobody
classified is owner-only. An adapter whose tool gates itself per action
declares that (`acl::declare_self_gated`), and the owner may classify a
plugin tool in config:

```toml
[tool_caps]
wordcount = "web"
```

Only tools with no builtin classification may be named there — a config
that could rewrite `terminal`'s cap would make the builtin table
advisory.
