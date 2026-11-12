# Fleet, extension development, and selected-text translation

`PLAN-command-center-dev-translation.md` is the active execution plan. This
page describes the first shipped workflows and their current limits.

## Instances: automatic cluster inventory

The Instances tab reads the attached gateway's `/api/fleet` inventory and adds
a **Cluster registry** group below manually configured instances. Rows include
version, normalized load, heartbeat/staleness, capabilities and replica details
when the registry provides them. The API reads the existing node registry and
exposes an explicit field allowlist, not credentials or signing configuration.

Entering the tab asks the gateway at once, so every node is on screen without
a click and a node that went down while the tab was not shown reads stale on
your return; after that the list refreshes every 15 seconds. A refresh changes
rows in place: a group you closed stays closed, and the selection, the list's
scroll position and your place in the detail pane stay where you left them.
Version, uptime, load and the platforms (or, for a registry row, how it is
managed) each have a column; in a narrow window, such as a phone in portrait,
the columns at the right give way so the names stay whole. The controls under
the list wrap into as many rows as the window needs, so every action is on
screen at the 760x480 minimum and on a phone.

A cluster-only row has no implicit SSH lifecycle target. Add a managed instance
explicitly before using start, stop, restart or deployment operations. This
prevents a registry name from being mistaken for an SSH host. Existing SSH
controls remain asynchronous. The detail pane of a registry row bound to a
saved instance with an SSH host and unit names them ("SSH lifecycle: through
saved instance …").

Start, stop and restart of a saved instance that has a **node id** first ask
that instance's gateway which node it is (its `/api/fleet` `self` row). A
gateway that answers as another node, or with no node identity, is refused,
and the status bar names both ids. A stopped gateway cannot answer: **start**
goes ahead and the binding is checked once the gateway is healthy; **stop**
and **restart** of a gateway that does not answer (a hung one is why you
restart it) ask you first. The check goes to the instance's URL; it cannot
prove that the SSH host is the same machine.

### Health

Each registry row's dot shows the node's health, not only its heartbeat age:
grey means **stale** (no heartbeat within `[cluster] stale_after_secs`), red
means **failing** (it still heartbeats, but a sync push to it or the last probe
failed), and green means fresh. The detail pane shows the heartbeat age and the
last probe. **Probe** asks the attached gateway to fetch the node's public A2A
card from the URL in your `[a2a_agents]` entry for that node. The probe never
dials the registry's self-reported endpoint and sends no bearer. A failed probe
marks the node failing only until the next good exchange with it: a gossip
exchange or a sync pass of any kind that succeeds after the probe clears it
(and the change back to fresh is an alert like any other). The chat equivalent
is `/fleet probe <node-id>` (owner only).

### Sync

The detail pane lists every sync kind the node takes part in. Store sync uses
`[cluster] storesync_peers`. Memory, config and vault replication use
`memsync_peers`. For each kind the pane shows:

- the peer name, and whether the automatic loop runs that kind;
- the cursor against this node's high-water mark ("N behind"), or for the
  vault, whether the peer already holds this vault. For config, "behind"
  counts only what a push would send: this node's own replicated rows past
  the peer's cursor, not the rows received from other peers nor the
  credential-shaped values that are never offered, so a peer in sync reads
  0 behind;
- the last pass this gateway saw, automatic or explicit, with its error;
- the automatic loop's backoff, if any (failures and the next try).

On this gateway's own row, the pane also counts the received offers waiting in
each inbox.

**Sync now** pushes the kinds you tick to the selected node. **Apply
received** (on this gateway's own row only) drains the inboxes you tick: only
the store drain, and only when something waits in it, starts ticked. Both
go through the same core calls, cursors and backoff keys as the automatic loops,
and they dial the operator's `[a2a_agents]` URL and bearer. A store push only
lands in the peer's inbox, and a store apply writes only per-sender replicas,
so neither asks for confirmation. A config or vault offer, in either direction,
and a memory merge into this node change data. The desktop asks before it sends
them, and the gateway refuses them without `confirm: true`. Config changes to a
security section still wait for the owner. The same operations are available
as `/fleet sync <node-id> [store|memory|config|vault] [confirm]` and
`/fleet apply [kind] [confirm]` (owner only).

### Managing nodes

- A registry row that a saved instance is bound to (through that instance's
  **node id**) says `managed via <instance>` instead of `registered`.
- **Attach** on a registry row asks the bound gateway's own `/api/fleet` for
  its `self` node. Attach is refused unless that node is the selected one. A
  typed node id is never trusted on its own.
- With a registry row selected, **Add…** opens pre-filled with the node's name
  and node id. You enter the desktop URL and the SSH host yourself. The
  registry endpoint is the A2A address, not the desktop API, so it is never
  guessed into these fields.
- The instance editor refuses a node id that the attached registry does not
  know. Clearing the field removes the association.
- **Forget node…** removes a stale registry row after confirmation. Use it
  for a retired machine. The gateway refuses to forget itself or a node that
  is still heartbeating. Forgetting is fleet-wide: the gateway keeps a
  tombstone (node id and the time it was forgotten) and sends it with its
  gossip. A 0.2.0 peer that receives it drops the row, passes the tombstone
  on, and refuses any copy of the row whose heartbeat is not newer than the
  forget, so a third peer cannot gossip the node back. A peer older than
  0.2.0 ignores tombstones and keeps listing the node, but the 0.2.0 nodes
  do not take it back from it. If the machine itself comes back and
  heartbeats, it is registered again and the tombstone is dropped.
  Tombstones are kept for 90 days, and a node does not take or pass on an
  older one. A receiving node applies the same rule as the forgetting one:
  it drops only a node that is stale by its own `stale_after_secs`, and
  never itself; a tombstone stamped ahead of its clock is held to its
  current time. The chat equivalent is `/fleet forget <node-id> confirm`,
  and `veles nodes forget <id>` also forgets fleet-wide, with the same
  refusals (this node, a node still heartbeating).

### Restart, config, bulk actions and the audit log (0.2.2)

These are asked over the cluster link, like Logs… and Metrics: the
attached gateway dials the node's `[a2a_agents]` entry, never the
registry's endpoint. Only the owner can ask (the desktop token, or a chat
caller holding `all`). The node answers only a peer named in its
`[cluster] memsync_peers` (the list config sync already uses for "who may
push config to me"), never during a lockdown, and logs every request in
its `a2a_audit.jsonl` (who asked for what, and why it refused). Each
request is its own JSON-RPC method, `veles/fleet-manage`: a node older
than 0.2.2 answers that it does not know it ("restart and config over
the cluster link need 0.2.2 or later"), never as a chat turn.

- **Restart** on a registry row restarts that node's gateway through
  whatever supervises it. The node exits with code 76 and its supervisor
  starts it again: a systemd unit whose `Restart=` is `on-failure` or
  `always` (read from systemd, and only when the gateway is the unit's
  main process), the VPS container (its unit declares
  `VELES_SUPERVISOR=container`; `Restart=always` brings it back),
  `veles-gw` (the Windows logon task's launcher), or the Android app's
  gateway service. A node that knows no supervisor says so and stays up:
  started by hand, by the desktop, from a terminal, or in a container that
  did not declare its unit. The tab first asks the node how it is
  supervised and what is running, asks you once (twice if turns are
  running: desktop turns resume from their saved history after the
  restart; connector, A2A, mail, cron and subagent turns do not), then
  waits up to two minutes for the node to answer with a new boot, and
  says "back after N s" or that it did not come back. A saved instance's
  Restart is still ssh + `systemctl --user`. Chat: `/fleet status
  <node>`, `/fleet restart <node> [force] confirm` (it waits for the node
  too).
- **Config…** reads and writes one key of that node's `config.toml`, under
  config sync's rules and no others. Only the keys config sync carries
  can be read or written: a node-local key (`[node]`, `[cluster]`,
  `[a2a_agents]`, `[tui]`, any section config sync does not know) is
  refused, and so is a table that holds one (`web`, `platforms.a2a.extra`:
  name a key under it). A node whose owner turned `[cluster]
  configsync_auto` off decides every config change from another node, so
  it takes no write from another node's command center either: push
  config, and its owner applies it there. A key in a held section (`[acl]`, approvals, sandbox, hooks,
  plugins, MCP servers) can be read but not written from another node:
  edit it on the gateway you are on and push config (Sync now, config),
  and the receiving node holds it until its owner applies it
  (`/configsync apply`). A credential (a value shaped like a key or token,
  or a key named like one) is never shown or written. A write names the
  value it replaces (Read first), so a write decided on an old value, or
  a replayed one, is refused. The node writes it like an edit made there
  (comments kept, `config.toml.bak` first, refused if the result would not
  load), and config sync then carries it to the nodes that sync config
  with it. Chat: `/fleet config get <node> <key>`, `/fleet config set
  <node> <key> <value> confirm`, `/fleet config unset <node> <key>
  confirm`.
- **Probe all…** and **Sync all…** (a kind: store, memory, config or vault)
  ask the gateway for their targets (every other node it can dial; every
  node that is a configured peer for the kind), list them in one
  confirmation, and run only after it. Chat: `/fleet bulk probe [confirm]`,
  `/fleet bulk sync <kind> [confirm]`.
- **Audit log…** lists every command-center action this gateway was asked
  for (desktop or chat): when, who, what, which node, and how it went
  (`fleet_audit.jsonl` in the veles home, the newest 2000 kept). Chat:
  `/fleet audit [n]`.

A restart or config write carries a request id and its time: a node
refuses one more than 120 s from its own clock, and one it has already
received. A restart also names the boot it was decided for, so it cannot
be replayed after the node restarted.

### Deployment consistency

**Drift…** (and `/fleet drift`) also says whether every node runs the
same point release at the same commit. It compares each node's version
and commit, the version of each module and process plugin among the
nodes that load it, each provider driver's source hash, and every user
plugin that shadows a bundled one (with a fingerprint of each side). A
node that differs from the others is flagged with what differs (a user
copy with other content than the bundled one is always flagged); without
a majority, every node that disagrees is.

What a platform or a role loads does not decide the verdict. A module
that is not on every node (Racket and Lisp are not built for Windows, a
phone has its own set) is named with the nodes that lack it, and the
fleet is consistent all the same. A module standing by for a singleton
lease reports no version and is not compared.

The commit of a published build is the development commit its source
tree was exported from: the published tree names it in `SNAPSHOT_OF`,
and the build reports it beside its own revision. A published build and
a build of the development tree at that commit are the same build to
this check. A build that does not know its source (a build of a modified
tree) reads "revision not reported" and agrees with nothing.

"The fleet is consistent" is said only when every node was compared and
none differs; a node that could not be asked (stale, no `[a2a_agents]`
entry, older than 0.2.2) makes it "unknown". The verdict of the last
check is in every registry row's detail and in `/fleet list`.

### Observing nodes

These controls work on any registry row, including this gateway's own.
For this gateway, the attached gateway answers itself. For any other node,
it calls the node's A2A server with the `veles/fleet` JSON-RPC method,
using the URL and bearer from your `[a2a_agents]` entry for that node. The
registry endpoint is never dialed. The node answers only if the asking
gateway is named in its `[cluster] memsync_peers` (the list it already
uses for gossip and the sync inboxes), and it answers nothing while in
lockdown. A node older than 0.2.0 answers "method not found", and the
desktop reports that it predates these requests. The desktop API that
starts each request is owner-authenticated, and so is the `/fleet`
command.

- **Logs…** opens a viewer with that node's recent gateway log: the last
  50, 100 (the default), 200 or 500 lines, at all levels or at least
  info, warn or error. The lines come from the gateway's in-memory log
  ring, which holds the last 500 lines; older lines are in
  `journalctl --user -u veles-gateway` or `gateway.log` on that machine.
  Each line is redacted on the node before it is sent: registered secret
  values become `NAME=<REDACTED>`, and credential-shaped strings (API key
  prefixes, JWTs, bearer tokens, email addresses) are replaced as well.
- **Metrics** asks the node now and adds a block to the detail pane: CPU
  core count and 1/5/15-minute load (also as a share of the cores), memory
  available and total, free space on the filesystem that holds the data
  home, IO pressure (Linux PSI: the share of time some task waited on IO
  over 10 s, 1 min and 5 min), host and gateway uptime, the version and
  source revision, and the work in flight (turns running in that gateway,
  by surface, and background tasks, by kind). A figure the machine cannot
  provide is listed as not available, not shown as zero. These are not
  heartbeat fields: they are fetched only when you ask. Windows keeps no
  load average: a Windows node reports its CPU busy share since its
  previous sample (from `GetSystemTimes`; the heartbeat samples every
  gossip interval), its memory (`GlobalMemoryStatusEx`), free disk and
  host uptime, and no IO pressure. The heartbeat's load figure on Windows
  is the same busy share, so placement ranks a Windows node by its CPU. A
  macOS node reads memory from `hw.memsize` and `host_statistics64` and its
  uptime from `kern.boottime`; it has no IO pressure figure either.
- **Drift…** compares every registry node that is not stale and has an
  `[a2a_agents]` entry with this gateway, and opens a report. A node that
  is stale, has no entry, or cannot be reached is listed with the reason
  it was not compared. The report
  lists each version with the nodes that run it, and for each node whether
  its version or source revision differs from this gateway's. For
  replicated config it shows which keys differ, which exist on only one
  side, and whose config-log rows one side has more of (the per-author
  high-water marks). Values are never sent: each node hashes every
  replicated key's value with a nonce chosen for this check, and only the
  key names and hashes are compared. A node that is not one of this
  gateway's memsync peers is marked as such, because its config is not
  synced with this gateway. The detail pane also names version drift
  among the live registry nodes without asking anyone.
- **Gossip now** runs a registry exchange with every `[cluster]
  gossip_peers` entry immediately, instead of waiting for the next tick of
  the gossip cadence, and reports which peers answered. This gateway's own
  row shows the last exchange with each gossip peer, automatic or
  explicit, and lists the nodes forgotten fleet-wide.

The chat surfaces have the same requests as `/fleet logs <node-id>
[lines]`, `/fleet metrics <node-id>`, `/fleet drift` and
`/fleet gossip [peer]` (owner only).

`/fleet list` prints the inventory the tab reads (the `/api/fleet` body),
so the two cannot disagree. Every caller with the status capability gets
each node with its health state: fresh, stale or failing. The owner also
gets why a node is stale or failing, one line per sync kind (the peer,
automatic or manual, the cursor and how far behind, the last pass, the
backoff), the offers waiting in this node's inboxes, a version drift,
the nodes forgotten fleet-wide, and the recent health alerts with where
they go. The last passes, the backoff and the alerts are kept by the
gateway process. A surface that runs in its own process (`veles chat`,
the TUI) lists only what that process has seen, and says so.

### Alerts

Every gateway watches the health of the registry nodes on the gossip
cadence. Each change to stale, to failing, or back to fresh is recorded
once, after the new state has held for `[notify]
fleet_alert_debounce_secs` (default 120 s). A node that fails one sync and
recovers before then is not reported. The first time a gateway sees a node
counts as a baseline, not a change. The detail pane lists a node's recent
changes, and this gateway's own row lists them for every node. They are
kept in memory: the last 50, since this gateway started.

With `[notify] fleet_alerts = true`, each change is also sent to the
owner's home chat, which is where `[notify] ask_relay` puts questions
(`[platforms.telegram.home_channel] chat_id`). The ask relay must be on.
Because `[notify]` replicates, the switch is normally on for every node
at once. To send each change once, only the node whose gateway runs the
Telegram adapter sends it; the other nodes record that it was not sent
from them. Each node checks for the adapter when it reports a change,
not once at startup. When the node that runs the adapter itself goes
stale, the alert is sent only if the Telegram lease has moved to another
node by the time that node reports the change; otherwise no node sends
it. The Instances tab of the other gateways still shows the change. The
detail pane's "Health alerts" heading says which applies to the attached
gateway: sent from this gateway, sent by the gateway that runs the
Telegram adapter, or not sent and why (for example `[notify] ask_relay` is
off, or no home chat is named).

```toml
[notify]
ask_relay = true
fleet_alerts = true
fleet_alert_debounce_secs = 120
```

The desktop also shows each new alert in its window, on every tab: a
notice under the tab bar for ten seconds and in the status bar (it asks
the attached gateway every 30 s; the alerts it already had when the
desktop started are not replayed). The Instances list has a **Health
alerts** group with a row per recent alert; choosing one selects its node.
`/fleet alerts` lists them in chat, and `/fleet forgotten` the nodes
forgotten fleet-wide.

### Fleet deployment (not yet verified on a real node)

**Fleet deploy…** is enabled on a cluster row whose node is bound to a saved
instance (its `node_id`, SSH `host` and `unit`). It opens a dialog that builds
a manifest and a health preflight for a signed GitHub release (tag, exact
commit, workflow commit), then runs each mutating step separately and only
after a confirmation: **Stage canary** (signed preview on the target's
installer), **Restart + verify canary** (install, `systemctl --user restart`
over SSH, then health must report the exact reviewed commit), **Continue to
next target**, **Roll back selected target**, **Finish rollback**, and
**Abandon untouched operation** (closes an operation in which nothing was
installed and cancels its pending signed plan). Each operation has a local
journal under the desktop data directory (`fleet-deployments/`) that holds no
credentials; **Resume unfinished operation** reloads the oldest one. The
release tag starts as the version the attached gateway runs (its own registry
row) until you type another.

The saved instance must prove it is the selected node. **Build manifest +
preflight**, and again **Stage**, **Restart + verify** and **Roll back**, ask
the instance's own gateway (its URL and token, the link every installer call
uses) for its `/api/fleet` `self` row. A gateway that answers as another node,
reports no node identity, or cannot be asked is refused before anything is
staged, installed or restarted, and the refusal names both node ids. The
operation's state does not change, so it can be retried or abandoned.

Limits: a target's core must already be installer-managed, so that the
installation can be rolled back; the mutating steps run in a `tclsh` worker
process, so the dialog is not available on Android; the target gateway accepts
signed bundles on Linux only. The workflow has been exercised against loopback
test gateways only, never against a real node, so it is not yet a claimed
feature.

Logs, metrics, drift, alerts and Gossip now (above) have been exercised
between two throwaway loopback gateways only, not on the real fleet.

## Dev: extensions first

The Dev toolbar's **New extension** action previews the exact manifest and
source of a scaffold (Tcl, TypeScript, Racket, Lisp, or an out-of-tree Rust
plugin) before writing a user plugin directory. The Tcl scaffold registers a
`post_llm_call` hook and loads as written; the TypeScript scaffold also
writes `tsconfig.json` and `veles.d.ts` (the typed `veles` object the
lang-js host provides), so the language server and `tsc` know the host API.
The Rust scaffold is a `cdylib` crate on the published `veles-plugin-abi-sdk`
whose `cargo build --release` output is the manifest's `entry`: build it,
then reload it in Manage extensions (the plugin directory's history never
records `target/`). Creating an extension loads it through its own tier at
once (Tcl, or the lang-js, lang-racket or lang-lisp host) and the dialog
says what the gateway answered: "Created and loaded", or "Created, not
loaded" with the reason (no host for that language on this gateway, the
load's own error, a Rust crate not built yet). A first extension on a fresh
install needs no gateway restart: the gateway makes `plugins/` at boot,
so the `::veles::` API stub (hover, definition) and hot reload are there
from the start. A plugin that fails to load is reported and skipped
without stopping the plugins after it. **Extension guide** explains
tool/hook/service/provider boundaries and every tier (modules and process
plugins start from the command line), gives hook, tool, service and provider
examples that load as shown, lists the language server the gateway has for
each language, and opens the plugin author guide (`docs/plugins.md`) and the
extension API reference (`docs/extending.md`) as the gateway serves them, so
they open in every build and against a remote gateway.

Opening veles' own source in the editor (a checkout, the bundled plugins,
the installed desktop) says that an extension is usually the better change,
and the first save of such a file asks whether to draft the change as an
extension instead: accepting opens New extension and leaves the edit
unsaved in the buffer; saving again patches the file.

### Language servers and Check

The editor opens the gateway's files, including remote instances, and the
gateway runs the language server for the file's type: the copy veles ships
first, then the gateway's `PATH`:

| files | server | project root |
|---|---|---|
| `.tcl` `.tm` | `tcl-lsp`: the Linux packages (nix package, `.tar.zst` bundle, AppImage) ship it; the Windows zip and the APK do not (build it from <https://github.com/77787777778777/tcl-lsp> on Linux or macOS) | the repository (nearest `.git`, `Cargo.toml` or `flake.nix`) |
| `.ts` `.mts` `.js` `.mjs` | `typescript-language-server --stdio` | nearest `tsconfig.json`/`jsconfig.json`/`package.json` inside the extension, else the extension directory |
| `.rs` | `rust-analyzer` | the cargo workspace that lists the crate, else the crate |
| `.c` `.h` (`.cpp` …) | `clangd` (or `ccls`) | nearest `compile_commands.json`/`compile_flags.txt`/`.clangd`, else the build or extension directory |
| `.rkt` | `racket -l racket-langserver` | nearest `info.rkt`, else the extension directory |
| `.lisp` `.asd` | `cl-lsp` | nearest `.asd`, else the extension directory |

`VELES_LSP_<LANGUAGE>` in the gateway's environment (`VELES_LSP_LISP`,
`VELES_LSP_RUST`, …) replaces the server command for that language. Each
language family keeps its own server, so switching tabs between a Tcl plugin
and a Rust plugin crate reuses both. When the gateway has no server for a
file, the editor keeps syntax highlighting and says which program to install
on the gateway and how (for Tcl, on a build without tcl-lsp: where to get
it, and that Check still works); a server that exits is reported with its
own last words.
Hover, go to definition, completion, references, rename and diagnostics are
the server's; positions are converted between the editor's characters and
the server's units (tcl-lsp counts UTF-8 bytes, tsserver UTF-16), and a
request a server cannot answer yet (rust-analyzer while it loads the
workspace says "content modified") is asked again. Without a
`tsconfig.json` a TypeScript extension's `veles` global is unknown to the
server, and the editor says so.

The editor's toolbar wraps to the pane's width: the extension actions, the
path entry (type a path, then Return or Open), Save and pop-out on its first
row, the buffer actions after them and the **guts ▾** menu (the desktop's own
source files), so every control is reachable at any window size and on a
phone. **Check** is also C-c C-k in the editor. The REPL and the inspector
start at the height they ask for and the editor takes the rest, until a
sash is dragged.

**Check** runs the file type's checker on the gateway, on the saved file:
tcl-lsp diagnostics where the gateway has tcl-lsp, else Tcl `info complete` on the
engine's own tclsh (`VELES_TCLSH`, then `PATH`, then beside the gateway's
executable as in the Windows zip, then `TCL_LIBRARY`'s Tcl),
`tsc --noEmit` (the extension's `tsconfig.json`, else the host declaration),
`node --check`, `cargo check` for a Rust crate, `clang -fsyntax-only` for C,
and reader-only checks for Racket and Common Lisp (with `racket` or ECL on
the gateway, else with the reader of the lang-racket or lang-lisp engine the
gateway runs: nothing is expanded or evaluated). `cargo check` compiles the crate without producing the plugin,
so the crate's and its dependencies' build scripts and procedural macros do
run; the others do not run the extension. A missing checker is reported as
unavailable, never as clean. Save, reload from Manage extensions, and test
the smallest hook first.

### AI assist

**AI assist** works on four levels: **Ask a design question** (no file has
to be open; with one open, its extension's manifest and file list go along,
not its source), **Change this file**, **Change this extension's files** (a
change set across the extension, new files included), and **Explain the
diagnostics** of the open file. **New extension → Generate with AI…** writes
a whole extension from its purpose, starting from the scaffold for the chosen
language. Every request is a direct completion without tools, sessions or
file access, and is refused when any text in it — the question, the buffer,
the extension's files, the manifest, the diagnostics or the description —
contains a registered secret or a credential-shaped value. Changes are shown
as diffs and written only by an explicit apply: a change set is revalidated
file by file and written whole or not at all (a file edited since the preview
stops it), and a generated extension is created only if its directory still
does not exist. Files outside the user plugin directory (core or repository
source) get no AI changes; the pane offers to draft the change as a new
extension instead, with the request as its purpose.

### Contribute

**Contribute** (Manage extensions) lists every user extension (copy a bundled
one into your user plugin directory first). **Select all extensions** ticks
every extension's sources in every language (manifests, Tcl, TypeScript and
JavaScript with their `tsconfig.json`, Racket, Lisp, a Rust crate's
`Cargo.toml` and `src/`, C sources and build files, READMEs and licenses),
nested directories included, and never local state, build output or hidden
files. Review the redacted diff and secret scan, and run the non-evaluating
checks (Tcl `info complete` on the engine's tclsh, `node --check`,
`tsc --noEmit` — an npm `tsc.cmd` on Windows included; Racket, Lisp, Rust
and C are reported as unchecked here). One pull request carries at most 100
files per extension and 1000 in all; more extensions than that go in a
second one. **Prepare** clones the chosen
repository (the veles upstream by default; normally your fork) into an
isolated worktree and commits every selected extension in one branch, each
under `plugins/examples/<name>/` (the directory is editable). A path that
already exists there is replaced only after a second confirmation. The commit
uses the gateway's ambient git identity (`user.name`, `user.email`); a
gateway account without one is told so, and what to set, before anything
is cloned, and a failed git step reports git's own words.
**Push** is a separate, confirmed, non-force push with your own git
configuration:
over HTTPS your credential helper (e.g. `gh auth setup-git`) answers, over
`ssh://` or `user@host:path` your SSH setup, and your `url.<base>.insteadOf`
rewrites apply. **Open PR** is a third confirmed step that runs
`gh pr create` against `77787777778777/veles` by default, naming a fork branch
as `owner:branch`. Once the PR is open its branch name is free again and the
isolated clone is removed. The whole sequence is exercised against an authenticating
local git server and a stub `gh`, not yet against GitHub.

## Selection translation

Select text in the transcript, the composer, a dialog, an overlay, the Dev
editor or any other text or entry field, then right-click and choose
**Translate selection**, or press **F11**. Fields with their own context
menu add the entry to it; the rest get a small menu with **Translate
selection** and **Copy**. The popup accepts a target language, translates
asynchronously, reports the actual provider/model, and supports retry and
copy. It does not replace source text or create a chat session. TUI,
Telegram and IRC use the same direct completion through
`/translate [--to <language>] -- <text>`.

The translator is its own route, independent of the chat chain. With no
`[translation]` section, veles uses the first `[[custom_providers]]` entry
(in config order) whose model list contains `deepseek-v4.1-flash-1017`, the
default translator model. A provider that already lists it, such as an
Alibaba Model Studio token plan, therefore needs no extra setup. Otherwise,
or to choose another route, name both halves:

```toml
[translation]
provider = "your-configured-provider"   # "custom:<name>" works too
model = "your-translator-model"
target_language = "German"              # optional
```

The provider must exist in `[[custom_providers]]`. It does not need to be the
`[model]` primary or a `[[fallback_providers]]` entry, and translation works
without a `[model]` section. Its credentials stay in the existing secret
configuration, and its `extra_body` and `reasoning_effort` apply. The translator
never comes from the current chat session's model override, and no
arbitrary paid fallback is used. A `[translation]` that names only one of
provider or model is reported as an error rather than completed by a guess.
Without a usable route, the command and the popup say what to set. Without a
target language, the desktop uses its own display language and the chat
surfaces use the reader's locale. Input is limited to 16,000 characters,
and no tools, memory or session history are included. The offline UI and
routing tests do not establish network translation quality, which depends on
the chosen model.
