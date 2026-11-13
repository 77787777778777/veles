# Tcl in a channel

What `tcl …` in a channel runs, how to look inside it, and where its
commands come from. Written for the people who ask the bot about it, and
for the bot, which reads this page with its `veles_docs` tool.

## Which interpreter answers

- **Without the slopdrop bridge**, `tcl <code>` (also `.tcl` and `/tcl`)
  runs in veles' own sandbox: a fresh interpreter for every call. Nothing
  a call defines survives it.
- **With the bridge** (`[slopdrop]` in config.toml and the slopdrop module
  loaded) the same commands evaluate in slopdrop's interpreter instead:
  one persistent, git-backed interpreter that holds the procs and
  variables the channel has built up, with a rollback. slopdrop is a
  separate program (<https://github.com/pillowtrucker/slopdrop>) that
  veles talks to over HTTP; it cannot reach into veles. `tcladmin` asks
  for slopdrop's privileged interpreter and is refused below owner.
  `docs/slopdrop-bridge.md` (page `slopdrop-bridge`) has the whole
  arrangement.

Which of the two a channel has shows at once: define a proc, call it in
the next line. If it is still there, the interpreter is slopdrop's.

## Looking inside

Everything below is plain Tcl and works for anyone who may use `tcl` in
the channel:

| to find | type |
|---|---|
| the procs whose name matches | `tcl info procs *stock*` |
| every command, native ones included | `tcl info commands *stock*` |
| a proc's code | `tcl info body stock2` |
| a proc's arguments | `tcl info args stock2` |
| the namespaces | `tcl namespace children ::` |
| what a namespace holds | `tcl info procs ::stock::*` |
| where a name resolves | `tcl namespace which -command stock::chart` |
| a variable | `tcl set name` (an array: `tcl array get name`) |

Helpers such as `inspect` or `fip` are procs the channel defined
itself; `tcl info body inspect` shows what one does.

From a turn, the bot reads the same interpreter with its
`tcl_state_eval` tool, as the person who asked: the same door as the
`tcl` command, at the same permission. (`tcl_eval` is something else:
veles' own gateway and desktop interpreter, for the owner only. Being
refused that one says nothing about `tcl_state_eval`.)

## When the code is not Tcl

Two signs that a command is native, compiled into slopdrop and not
written in the channel:

- `info commands` lists it and `info procs` does not;
- `info body` shows a proc that only passes its arguments on (an
  `uplevel #0 … eval` of the same name).

Its implementation is then in slopdrop's source, not in the interpreter
and not in veles. What veles' own tree records about slopdrop is how it
is deployed beside a gateway (`deploy/vps/slopdrop/`), and that answers
some questions by itself. On that deployment slopdrop's stock commands
(`stock::quote`, `price`, `detail`, `history`, `chart`) call Yahoo's
chart API through a filtering proxy that lets them reach
`query1.finance.yahoo.com` and `query2.finance.yahoo.com` on port 443 and
nothing else (`stock-proxy.filter`, `stock-proxy.service`); every other
connection slopdrop makes leaves through Tor. A channel proc that fetches
some other host (an old `download.finance.yahoo.com` quote URL, say)
fails for that reason, whatever the host itself does.

## What the bot can and cannot see

- The channel's procs and variables: yes, with `tcl_state_eval`.
- veles' own source and documentation: yes, built into the binary
  (`veles_docs`: `grep`, `file`, `page`, `query`).
- slopdrop's source: not in the binary. It is public at the address
  above, and a caller whose role includes web access can have the bot
  read it there.
- The host's files: no. A caller who is not the owner works in a sandbox
  container, and the bot's tools for them run there.
