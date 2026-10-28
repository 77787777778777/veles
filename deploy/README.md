# veles on the VPS (vps.example.org)

The VPS is the pool's hub: it holds the arbiter, runs IRC, Telegram and
cron, and is always on. The laptop and the phone are the other two nodes
(docs/pool.md). This file is how the VPS side is built, staged, switched
over to and kept up to date. The files it names are in `deploy/vps/`.

## The layout

Three loginless system users, each with its own rootless podman, homes
and container storage on `/dev/sdb` (`/srv/pool`, a bind mount of
`/var/lib/gitea/veles-pool` — the root disk has no room), and a fourth
for slopdrop, a system service:

| user | runs | reaches |
|---|---|---|
| `veles` | the gateway container (`veles-gateway`) | the internet; the other nodes' A2A (10.0.30.2/.3:9900); slopdrop (10.0.30.1:8080); the signer's API (10.0.30.1:9911); searxng-tor (10.0.30.1:8889) and Tor's SOCKS port (10.0.30.1:9050); the sandbox socket |
| `veles-sbx` | `podman system service` on `/run/veles-sbx/podman.sock`, every sandbox, and `searxng-tor` | the internet, IPv4 TCP through Tor only |
| `veles-signer` | the ticket signer container (`veles-signer`) | the internet only (Telegram, IRC) |
| `slopdrop` | `slopdrop.service`: the channels' Tcl interpreter and its web bridge on 10.0.30.1:8080 | the internet, IPv4 TCP through Tor only (its `http` through Tor's SOCKS port, a circuit per network+nick); the gateway API's Bluesky reads (127.0.0.1:8642); its state repo on this host's sshd |

- **The gateway's container is veles' "host" zone**: the owner's terminal
  runs in it (busybox, git, ssh, curl, rg, tcl), not on the VPS.
- **Sandboxes are not nested in it.** They run beside it as `veles-sbx`,
  and the gateway drives them through that user's podman socket
  (`CONTAINER_HOST`): a user env's `/work` is a named volume, and files go
  in and out with `podman cp`. Nesting would have to get past Ubuntu's
  `kernel.apparmor_restrict_unprivileged_userns`.
- **The gateway cannot touch the signer** — its key, its tokens, its
  process: another user, its files 0700, no socket. It can only reach
  the signer's API, `POST /v1/sign`, like the other two nodes do.
- **The firewall** is the nft table `inet veles` (`veles.nft`, loaded by
  `veles-nft.service`, nothing else in the ruleset touched). Rootless
  networking (pasta) opens host sockets as the container's owner, so
  `meta skuid` confines each user to the column above. Input to 9900 and
  9911 is WireGuard (10.0.30.0/24 on wg0) only.
- **Ports**: 9900 (A2A) and 9911 (the signer) on 10.0.30.1; 8642 (the
  desktop API) on 127.0.0.1, reached through an SSH tunnel; 113 (ident)
  on every address; 9040 (Tor's TransPort) on 127.0.0.1; 8889
  (searxng-tor) and 9050 (Tor's SOCKS port) on 10.0.30.1, for the
  gateway only (dropped on wg0).
- **Sandboxes leave through Tor** (`tor/torrc`, the instance
  `velessbx`). The box has one IPv4 and one IPv6 (OVH gives the VPS a
  single /128; the /56 on ens3 is shared), and the owner's IRC client
  uses both, so a sandbox must not leave from either. The nft table
  hands every IPv4 TCP connection a `veles-sbx` socket opens to Tor's
  TransPort and refuses everything else: UDP, ICMP, IPv6, local and
  private addresses. DNS stays with the host resolver. Pulls and image
  builds as `veles-sbx` go through Tor too.
- **Other people's web traffic leaves through Tor too**
  (PLAN-challenge-relay.md D5, D6; `[web] others = "proxy"` in the
  gateway's config). Their searches go to `searxng-tor`, a SearXNG
  running as `veles-sbx` (so through the TransPort) with the engines
  that answer Tor (`searxng-tor/settings.yml`). Their fetches go through
  the SOCKS port of a second Tor instance, `velesweb` (`tor/torrc-web`),
  one circuit per session (`IsolateSOCKSAuth`). Their browser (a page
  put to a person, and since D3 every `browser_*` call of theirs) runs
  in that session's own box (the sandbox image carries Chrome, Xvfb and
  agent-browser). The owner's traffic stays with the laptop, which says
  `others = "refuse"` and `others_via = "<this node's id>"`, so an
  owner's session switched to `/egress tor` on the desktop is sent here.
- **Ident** (`identd/`): the owner's IRC client runs on this box, and so
  do the sandboxes, so both reach IRC from the same address — and an
  unverified `~ident` is anyone's to claim. `veles-identd` answers ident
  queries for the owner's Unix user only (`wrath` → `hastur`), so the
  owner's connections carry a verified ident and a sandbox's cannot. It
  is a small script on purpose: one query per sandboxed, throwaway
  process, no config files, at most 8 at once.

## Setting the host up

```sh
scp -r deploy/vps user@vps.example.org:veles-deploy
ssh user@vps.example.org sudo sh veles-deploy/setup.sh            # users, storage, socket, firewall
ssh user@vps.example.org sudo sh veles-deploy/setup.sh --with-units   # + the two quadlets
```

Idempotent: run it again after changing anything in `deploy/vps/`. The
sandbox image is built as `veles-sbx` from `sandbox/Dockerfile`
(`scripts/build-sandbox-image.sh`, run in that user's home).

## Reboots

Everything comes back on its own:

- **System units**, all enabled: `veles-nft` (at sysinit, before the
  network), `tor@velessbx`, `tor@velesweb` (after `wg-quick@wg0`),
  `veles-identd.socket`, `wg-quick@wg0`, `slopdrop` (which requires
  `veles-nft`: it never runs without its fence).
- **Linger** starts the user managers of `veles`, `veles-sbx`,
  `veles-signer` and the owner; their units are the quadlets
  (`WantedBy=default.target`, `searxng-tor` among them), veles-sbx's
  podman socket. slopdrop was the owner's user unit until 2026-10-13,
  so anything that escaped its interpreter was the owner, sudo
  included.
- **Order**: a drop-in (`veles-user-manager.conf`) puts the veles
  users' managers after the firewall table and wg0, and keeps them down
  if the table is missing: no container runs unconfined.
- **Retries**: the gateway and the signer publish on 10.0.30.1; if wg0
  is late, podman fails at once and `Restart=always` tries again in
  10 s. slopdrop binds 10.0.30.1 too, and exits 0 when it can't, so its
  unit is `Restart=always`.
- **slopdrop** (`slopdrop/`): its build stays the owner's, in
  `~/slopdrop` (`git pull --ff-only && cargo build --release --features
  frontend-web`), and `sudo sh slopdrop/slopdrop.sh update` installs it
  into `/srv/pool/slopdrop` (bin/ and tcl/ root's, config.toml root's and
  readable by slopdrop, state/ slopdrop's) and restarts it. Limits are the
  cgroup's: `MemoryHigh=512M`, `MemoryMax=768M`, no swap, one CPU, 256
  tasks. Its state repo pushes with a deploy key of its own (gitea
  `wrath/shaniqua-smeggdrop`, key "slopdrop service user"), never the
  owner's SSH key.
- **Demon stays down**: restart policy `no`, which root's
  `podman-restart.service` skips.

## Images

Built on the laptop by nix, never on the VPS:

```sh
scripts/build-image.sh        # result-images/veles-{agent,signer}-0.1.0-<commit>.tar.gz
scripts/push-image.sh         # sha256-checked, loaded into veles' / veles-signer's podman, tagged :current
scripts/vps-test.sh           # a scratch gateway as veles: health, and `veles doctor --sandbox` inside it
```

The gateway image carries the podman CLIENT only (`podman-remote`).

### Modules (in-process platform dylibs, R4)

A platform module (IRC is the first) is a Rust dylib the running gateway
loads, supervises and unloads — a module update is a hot reload, no image
rebuild, no gateway restart. The image carries the runtime and the
modules in `$out/lib`; installing or updating one on the VPS:

```sh
scripts/push-plugin.sh irc   # builds with the gateway (a module built
                             # alone links a different runtime and is
                             # refused), key-checks, installs content-
                             # addressed under <data>/modules/irc/,
                             # then asks the gateway to rescan, or to
                             # reload when the rescan did not start it
```

`<data>/modules/<name>/` holds `<sha256>.so` per build plus a `current`
symlink, so a running gateway's mapped copy is never rewritten. The
journal and the notes carry every load/reload/kept-mapped; a module that
cannot unload (a pinned library) is reported kept-mapped, never unmapped
live. The laptop is the dev box (no IRC/telegram/cron there); the VPS
gateway is where modules run.

## Configuration

- **The gateway** (`/srv/pool/veles/data` = `/opt/data`): `config.toml` is
  the laptop's shared config with the node-local sections replaced by
  `vps-node.toml.template` filled in; `.env` holds the provider keys, the
  A2A tokens (`A2A_PEER_TOKENS=laptop:…,phone:…`), `VELES_SIGNER_TOKEN`,
  the slopdrop tokens, and at the switch the Telegram and IRC secrets.
  Both mode 600. Secrets are copied as filtered files, never printed.
- **The signer** (`/srv/pool/veles-signer/signer` = `/opt/signer`):
  `signer.toml` from `signer.toml.template`, `signer.env`, and the key
  `vps-signer.key`; its passphrase is the podman secret
  `veles-signer-passphrase`, so a restart needs nobody. The owner makes
  both, at a terminal (the passphrase is typed, never sent):

  ```sh
  ssh -t user@vps.example.org 'cd / && sudo runuser -u veles-signer -- env \
      XDG_RUNTIME_DIR=/run/user/$(id -u veles-signer) podman run --rm -it \
      --userns=keep-id:uid=1000,gid=1000 -e VELES_HOME=/tmp/home \
      -v /srv/pool/veles-signer/signer:/tmp/veles-signer \
      localhost/veles-signer:current ticket keygen vps-signer'
  ssh -t user@vps.example.org 'cd / && sudo runuser -u veles-signer -- sh -c \
      "stty -echo; printf \"passphrase: \"; read -r p; stty echo; echo; \
       printf %s \"\$p\" | XDG_RUNTIME_DIR=/run/user/\$(id -u) \
       podman secret create veles-signer-passphrase -"'
  ```

  `keygen` writes to `veles-signer/` in `VELES_HOME`'s PARENT, so for
  that one run the volume is mounted at `/tmp/veles-signer` with
  `VELES_HOME=/tmp/home`, and the key lands in the volume's top, where
  `signer run --dir /opt/signer` reads `<key>.key`. (A user's first
  container from an image takes minutes: rootless keep-id copies the
  image's layers once.) Its `[cluster.ticket_signers.vps-signer]` lines go
  into every gateway's config.

## Running

The units are rootless quadlets under each user's
`~/.config/containers/systemd/`, started by its user manager (linger):

```sh
sudo systemctl --user -M veles@ start veles-gateway
sudo systemctl --user -M veles-signer@ start veles-signer
sudo systemctl --user -M veles@ status veles-gateway
sudo journalctl _UID=$(id -u veles) -f
```

## Updating

```sh
scripts/build-image.sh gateway
for m in irc slopdrop telegram atproto-cmd lang-js lang-racket lang-lisp; do
    scripts/push-plugin.sh --no-reload "$m"
done
scripts/push-process-plugin.sh --no-reload atproto
scripts/push-image.sh gateway
ssh user@vps.example.org sudo systemctl --user -M veles@ restart veles-gateway
```

The modules go first. A new core changes the build key, and a gateway
refuses every module built for another core ("core changed"), so a
restart onto an image whose modules are not installed brings up no
platform (2026-10-10, 2–4 min). `--no-reload` leaves the running
gateway on its mapped copies until the restart. `push-image.sh` checks
this: it refuses to tag :current while a module on the VPS lacks the
image's key, and lists them (ALLOW_STALE_MODULES=1 overrides). The
list above is every module the VPS runs; `atproto-cmd` was missing from
it until 2026-10-21, when a push stopped on exactly that check.

Check `session_runs` is empty first (a restart ends a running turn).
The push makes a throwaway container of the new image as its user:
under keep-id, a new image's first container copies every layer (two
minutes for the gateway), longer than the unit's 90 s start timeout, so
the restart must not be the first.

## The switch from Demon

Done 2026-10-02. Demon (`hermes`, rootful) held 0.0.0.0:8642 and :9900
until then; it is stopped with its restart policy set to `no` (a reboot
must not hand it 9900 before the gateway), and its image and
`/root/.hermes` are kept until 2026-10-09 for rollback:

1. restore the `*.bak-pre-vps` config and `.env` on the laptop and the
   phone;
2. `sudo systemctl --user -M veles@ stop veles-gateway`;
3. `sudo podman start hermes`.

The switch was one window: stop the laptop's gateway and the phone's;
the VPS's final config and `.env` in place; start it (it took the
arbiter, Telegram, both IRC networks and cron at boot); start the laptop
and the phone on configs naming it; once gossip converged, restart the
two once more to arm their fences. PENDING.md has the record. Two things
that move with IRC and are easy to miss:

- **Published media.** Until the switch the laptop's
  `veles-media-sync.{path,timer}` rsync its media dir into
  `/var/www/veles-media` with `--delete`. Run it one last time, disable
  both units, and only then `setup.sh --take-media`: left running, the
  laptop's next sync deletes everything the VPS publishes.
- **Turning IRC off on a node.** `IRC_ENABLED` in `.env` wins over
  `[platforms.irc] enabled`, and the shared config says `true`, so a node
  that must not run IRC sets `IRC_ENABLED=false` — removing the variable
  turns it ON.

## The laptop

The laptop keeps one thing of its own here: `deploy/laptop/`, the local
SearXNG that `web_search`'s first rung uses (the quadlet, its settings,
the idle watch that stops it after 15 min of quiet). `bash
deploy/laptop/setup.sh` installs it as the owner's user, no root. It is
in git since 2026-10-21 because the home deletion of 2026-10-19 took
the original, which was not.
