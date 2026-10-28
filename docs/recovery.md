# Recovering a node whose home was lost

A node of a pool keeps almost nothing only on itself. Its sessions are
in its peers' replicas of its store, its memory in theirs, its config in
the config log every node keeps. When its home is gone — deleted,
reinstalled, a new disk — `veles recover` puts it back from ONE peer,
with one command on the bare machine and one on the arbiter.

What you need: the veles binary on the bare machine, a way to run one
command on the arbiter (usually the VPS), and the network between them
(the WireGuard tunnel, if the pool uses one — it lives outside veles'
home and normally survived).

## 1. A binary on the bare machine

Every install is a source build (docs/install.md). From any copy of the
repository — GitHub, the VPS's mirror (`git clone vps.example.org:veles.git`),
or a bundle on another node:

```sh
nix build github:77787777778777/veles     # or: nix build .  in a clone
./result/bin/veles version
```

Without nix, `scripts/install/install.sh` (Debian, Arch, …) or
`scripts/install/windows.ps1`. Do not run the setup wizard and do not
start a gateway: the recovery writes the home, and refuses while a
gateway answers on it.

## 2. On the arbiter: a recovery invite

The arbiter (`[cluster] arbiter`; the VPS in a three-node pool) mints it
for the member by the name it links it under:

```sh
veles pool invite laptop --recover          # draws the QR, prints how to use it
veles pool invite laptop --recover --text   # the text only, for pasting
```

On the VPS the gateway runs in a container:
`podman exec veles-gateway veles pool invite laptop --recover --text`
(as the `veles` user, deploy/README.md).

It names the node it re-admits — the id this arbiter binds that peer to
(its gossip, its store offers, or its registry row; `--node <id>` when it
cannot tell) — and refuses a name it does not link. It is good for ten
minutes, once, and holds credentials: show it only on your own screen.

## 3. On the bare machine: one command

```sh
veles recover 'veles-pool:1:{…}' --as laptop
```

(the text, a file holding it, or a photo of the QR). It shows what it
will write and asks; `--yes` skips the question. In order, it:

1. **takes the old identity back** — node.toml gets the id the invite
   names; `--as` (the id, 8+ characters of it, or the name) must match.
   Without `--as` it refuses: a new node joins with a plain invite
   (`veles pool invite <new name>`; `veles recover` with a plain invite
   joins as a new node, says so, and pulls only what the pool shares —
   memory and config — never the old node's sessions, and no skills: a
   recovery window opens only for a recovery invite);
2. **re-links the pool with new tokens** — the redeem replaces the
   arbiter's tokens for this node, and the arbiter introduces it to every
   member with a fresh pair (the hand rotation of 2026-10-19, minted);
3. **pulls its sessions** — a snapshot of the peer's replica of this
   node, in 4 MiB parts, checked by sha256; store offers from this node
   the peer had not applied go on top; then every row goes back into
   `state.db` under its own id. The store log continues after the
   furthest replica cursor any member reports, so the pool's replicas
   keep taking this node's new sessions;
4. **pulls its memory** — everything the peer has, plus this node's own
   offers still waiting in the peer's inbox;
5. **pulls its config** — the whole config log the peer holds, this
   node's own history among it, merged into config.toml;
6. **copies skills and commands** — the peer's own (skills are per node:
   these are the peer's, not a copy of yours), only where no file is;
7. **asks the vault to admit it** — new vault keys, and a join request
   in the peer's vault inbox.

It ends with three lists: what came back, what did **not** (and why), and
what you do next.

Pull from another member than the arbiter with `--from <name>` (it must
hold your replica: `veles replicas list` there). Leave steps out with
`--skip store,files`. Run it again without the invite —
`veles recover --from vps` — to repeat the pulls over the links the
first run wrote: every step is idempotent (rows already here stay,
files already here are not overwritten, the config import skips what it
has).

## 4. By hand, after

The run prints these with the values filled in.

- **The vault.** On the peer (or any member), after reading the
  fingerprint off the recovered machine's screen:
  `veles vault admit <node id> --fingerprint <fp>`. It re-encrypts every
  secret for the new key and pushes the vault on its next sync tick.
  Then on the recovered node: `veles vault apply --inbox --trust <root
  fingerprint>` (the root's fingerprint as `veles vault status` on a
  member prints it) and `veles vault status` — every secret readable.
- **Members that did not answer** the introduction (a phone asleep, a
  workstation off) still hold the OLD tokens. On the arbiter, for each:
  `veles pool introduce laptop <member>`.
- **Secrets that were never in the vault** (`.env` lines `veles vault
  import` never took): enter them again.
- **Node-local config**: `[node]` (name, static_ip, caps, overrides),
  `[cluster]` flags and lists beyond what the arbiter's introduction wrote
  (`storesync_auto`, `memsync_auto`, `sandbox_home`, `local_memory_globs`,
  `failover`), `[tui]`, and the machine keys of shared sections
  (`configsync::NODE_KEYS`: the A2A bind host is restored from the
  invite; `sandbox.backend`/`image`/`mounts`, the `web.*` backends and
  the IRC media dir are not).
- **Plugins**: the run lists the peer's; install them again through the
  plugin installer (code is never pulled from a peer).
- **Outside veles**: the ticket signer's key (`~/.config/veles-signer`),
  ssh and WireGuard keys, and the service unit. On NixOS the gateway's
  unit pins a PATH without a shell; the 2026-10-19 recovery needed a
  drop-in (`~/.config/systemd/user/veles-gateway-dev.service.d/override.conf`
  with `VELES_SHELL=/run/current-system/sw/bin/bash` and a wider PATH).
  The laptop's local SearXNG (the quadlet, its settings, the idle watch)
  is outside veles too: `bash deploy/laptop/setup.sh` puts it back (the
  originals had never been in git before 2026-10-21).
- Then start the gateway, and check `veles doctor`, `veles nodes`,
  `veles replicas status` and `veles vault status`.

## What a recovery never brings back

Nothing outside the lost home held these: the session rows the peer's
replica had not applied yet; node-local tables (approvals, always-allow
rules, cron runs, triggers, the inbound ledger); memory paths that never
leave their node (`reference/nodes/**`, `[cluster] local_memory_globs`);
replicas of OTHER nodes this node held (each sender re-sends them on its
next push); and the data-dir trees no sync carries — `notes/`,
`scripts/`, `apps/`, `app-data/`, `plans/`, `certs/`, `media/`. Only
`veles backup` has those: keep one off the machine.

## Limits

- The store step refuses a `state.db` that was written to after the loss
  under ids the replica also uses (a fresh store numbers from 1 again):
  move it aside and run again. Renumbering those rows is an open item.
- Only the arbiter mints a recovery invite, so the arbiter itself cannot
  be recovered this way yet.
- Measured on the real pool (2026-10-20): the laptop rebuilt from the
  VPS over WireGuard — 2,308 sessions, 73,756 messages, 149 memory
  paths, 137 config keys, 768 skill files — in 212 s, of which the
  700 MB replica in 167 parts was most; every link rotated and the
  vault's join request waiting on the arbiter, as this page says. The
  same store on a loopback rehearsal took 169 s, and a second run
  changed nothing.
- A message that looks like a credential never reaches a replica (the
  replica's own rule), so it does not come back either: 22 of 73,622 in
  that run. The account does not count them yet.
