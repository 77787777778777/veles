# Session mail — one session talks to another

Veles sessions run in several processes at once: the gateway (its chats
and the desktop's sessions), every `veles chat` TUI or REPL, and their
delegate children. Session mail lets one of them put text into another,
where it runs as that session's next turn — the way Claude Code's agents
message each other. A session on the laptop can ask the phone's session
what is on its screen; a long-running worker can tell the chat you are
in that it is done; a shell script can hand a running session a job.

## Sending

From a session, the agent uses the `sessions` tool (deferred, so it finds
it through `tool_search`):

- `list` — the sessions some process on this machine holds open or is
  running a turn in.
- `send` — `to` is a session id, a unique id prefix, or an exact title;
  `text` is the message. `wait_secs` (up to 600) waits for the answer.
- `check` — the state and answer of a mail it sent, by mail id.

From a shell:

```sh
veles session list
veles session send <id|prefix|title> "the message" [--wait 60]
veles session mail <mail id>
```

`--wait` prints the answer, exits 2 when the turn failed and 3 when no
answer came in time.

## What the receiving session sees

The text arrives as the session's next turn, after a provenance line the
model reads: it is mail from session “title” (id), written by another
agent or program, not by the user. The turn's final answer is the mail's
reply — returned to a sender that waits, and kept for `check` otherwise.
To say more, the receiver sends mail of its own.

The desktop paints it as `✉ message from session <id>` with the first
line in view; the TUI labels it `✉ mail from <id>`; the REPL prints
`✉ mail from <id>:` above it.

## Rules

- **Rights.** The turn a mail starts runs with the lower of the sender's
  and the receiver's rights, for the whole turn: the receiving session's
  own person writing meanwhile does not lift it. A Telegram guest's
  session can never make the owner's session use owner tools. The `sessions` tool itself is
  owner-only (`[tool_caps] sessions = "<cap>"` widens it). From a shell,
  `veles session send` is the owner — unless `VELES_SESSION_PRINCIPAL`
  names someone (a delegate child shelling out), whose rights the mail
  then carries.
- **Delivery.** The process holding the session runs the mail at once
  when it is idle, or at the end of the turn it is running. A session
  nobody holds keeps its mail queued until it is next opened: when a
  desktop window opens it, a TUI or REPL resumes it, or its chat's next
  message arrives. Mail is never run by a process that does not hold the
  session, because who the session acts for is unknowable then.
- **Who holds a session.** The gateway holds its chats; the desktop holds
  the sessions it has an agent for and the ones a window shows; a TUI
  holds the session on its screen; a REPL holds its own (it prints a line
  when mail arrives, and the next Enter — an empty one does it — runs it).
  A one-shot `veles chat -q` holds nothing.
- **Loops.** Mail sent from inside a mail turn continues that exchange
  one hop further; an exchange stops at 6 hops. One session sends at
  most 30 mails a minute. A waited send back to a session that is itself
  waiting on this exchange is refused: it would deadlock until both time
  out, and the final answer reaches the waiting session anyway.
- **Chat-backed sessions.** A mail into a Telegram or IRC session runs
  there and its answer is posted to that chat, as a cron wake's is. On a
  threaded platform (Bluesky) the answer is posted in the session's own
  thread.

## The owner joins someone's session

USER, 2026-10-12: "is there any way I can chime in and attach
(non-exclusive) sessions of other users? If I want to be the second
conversation partner in a user's session?" The owner can be a second
person in a conversation the agent has with someone else, without taking
it over. In any chat the gateway serves (IRC, Telegram), the owner
writes:

- `/join` — the sessions this gateway holds whose person is not the
  owner, newest first, labelled `[alice in #chan]` or `[bob on telegram]`.
- `/join <name|id>` — joins one, by session id, an id prefix of four
  characters or more, or the person's name.
- `/leave` — stops.

While the owner is joined, a plain line they write in that chat goes into
the joined session, not their own:

- It appears in that person's chat as `<owner> (owner): …`.
- It runs as a turn of that session with the OWNER's rights. Its
  approvals and questions are asked in the owner's chat, of the owner
  only; the session's person cannot answer them. If the owner has left
  by the time it runs, nobody is asked and an approval is refused.
- The model reads who said what: the owner's line comes with a note that
  the owner wrote it and the session's person did not, and while the
  owner is joined, the person's own lines name the person.
- What the session says (progress, replies, and the person's own lines)
  is copied to the owner's chat behind that label.

The person keeps talking as before; neither one locks the other out, and
the person's own turns keep the person's rights. The owner's slash
commands stay the owner's own; only plain lines go into the joined
session. A line written while that session runs a turn waits and runs
after it.

The turn keeps the session's own system prompt. The owner's memory is
not loaded into a conversation the other person reads.

A line is session mail whose `session_mail` row has `kind = 'join'` (its
`session_queue` row is ordinary mail). Only a row written on this node,
by the owner's own principal, raises the turn. A relayed
`join` row, or one anybody else wrote, runs as ordinary mail, at the
lower of both rights.

Limits:

- A join lives in the gateway's memory. A gateway restart ends it, and
  the owner joins again.
- Only sessions the same gateway holds can be joined: not a desktop
  session, and not another node's.
- The owner's line is held to the session person's media and rate
  limits, not the owner's.

## How it works

The mail is a `kind='mail'` row in the receiver's `session_queue` — the
queue every surface already drains at its turn boundaries — and a
`session_mail` row with the sender, the rights, the exchange and the
answer. The queued text starts with `[veles mail <id>]`; `Agent::run_turn`
recognises it only when that mail is addressed to this session and the
text is exactly what was queued, strips the header, clamps the rights,
and records the answer.

Each process that holds sessions binds `<data dir>/run/<pid>.sock`
(0700, same-uid peers only) and answers `open_sessions`, `nudge` and
`mail_done`. Presence is never written to the database; a socket whose
process died is removed by the next process that finds it.

## Between pool nodes

A session on another node is addressed `<node>/<session>`, where
`<node>` is the name this node gives it in `[a2a_agents]` (or a registry
node's name or id prefix). `sessions list remote=true` and `veles session
list --remote` show each node's live sessions that way; a bare session
id that only a node's replica knows is sent to that node too. A mail sent
there has the id `<node>/<mail id>`, and `check` asks that node for it.

```sh
veles session send phone/20261011_101010_abcdef "what is on the screen?" --wait 120
veles session mail phone/m1a2b3c4d5e6f7a8b
```

The hop is one A2A message (`[veles session mail]`, framed like the other
cluster protocols). The receiving node:

- answers only peers in its `[cluster] task_peers`, and refuses all of it
  during a lockdown;
- caps the sender's rights at what the peer holds there — `a2a:<peer>` in
  its ACL, a User unless the owner named it higher. What the sender held
  on its own node can only lower that. So an owner session on the laptop
  mailing the phone gets a User-level turn on the phone, with tools in a
  sandbox, unless the owner gave the laptop more;
- keeps the exchange's hop count, so the 6-hop limit holds across nodes;
- writes provenance naming the node, and tells the receiving agent to
  answer at `<node>/<session>`.

Both nodes need A2A on (`[platforms.a2a] enabled = true`), each other in
`[a2a_agents]` with a bearer token, and each other in `task_peers`.
