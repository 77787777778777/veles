# Installing veles

## Quick install (prebuilt release)

**Linux** (x86_64; aarch64 when that release has one) — no root, compiler
or host Tcl needed; the archive carries its own runtime:

```sh
curl -fsSL https://raw.githubusercontent.com/77787777778777/veles/master/scripts/install/install.sh | sh
curl -fsSL .../install.sh | sh -s -- --version v0.1.0 --require-signature
```

**Windows 10/11** (x64), in PowerShell — the portable zip (Tcl/Tk,
tcllib, sqlite and the C++ runtime beside `veles.exe`):

```powershell
irm https://raw.githubusercontent.com/77787777778777/veles/master/scripts/install/install.ps1 | iex
$env:VELES_VERSION = 'v0.1.0'; irm .../install.ps1 | iex
```

Then `veles setup` (terminal) or `veles gui` (the desktop app walks you
through it). The AppImage and the Windows zip also run without installing.

What the installers do: download the release's `SHA256SUMS` and the one
archive for this machine, refuse on any checksum mismatch, and — when
`cosign` is on PATH — verify `SHA256SUMS.sigstore.json` against this
repository's release workflow (keyless Sigstore, GitHub OIDC) before
anything is unpacked. Without cosign they print the exact `cosign
verify-blob` command and continue on the checksum; `--require-signature`
(`-RequireSignature`, `VELES_REQUIRE_SIGNATURE=1`) makes the signature
mandatory. Linux installs into `~/.local/share/veles/install/releases/<v>-<rev>`
with a `current` link and a `~/.local/bin/veles` launcher; Windows into
`%LOCALAPPDATA%\Programs\veles\releases\<v>-<rev>` with a `current`
junction on the user PATH. Re-running upgrades in place and keeps the
previous release; `~/.config/veles` / `%APPDATA%\veles` are never touched.
`--version` / `VELES_VERSION` pick a release (default: the latest);
`VELES_REPOSITORY` names another repository.

Android: the gateway and userland APKs are attached to the release by
the maintainer, signed with the same certificate, listed in
`SHA256SUMS-android` (docs/phone.md).

## From source

Veles links `libtcl` through bindgen, so a source build is bound to the
Tcl the host actually has (the prebuilt archives carry their own):

| host | path |
| --- | --- |
| NixOS | the flake's `nixosModules.default` — `services.veles` |
| Debian sid · Ubuntu · Arch · Gentoo · FreeBSD | `scripts/install/install.sh --from-source` |
| Windows 10/11 (native, not WSL) | `scripts/install/windows.ps1` — see [Windows](#windows) |

Both end the same way, and this is the part that matters:

```sh
veles doctor            # config, provider route chain, store, Tcl
veles doctor --sandbox  # resolves the fence AND runs a script through it
```

`doctor --sandbox` is not a readout. It resolves `[sandbox]` through the
same `backend_from` the engine uses, reports the binary and every
artifact that backend needs, then executes `puts [expr {6*7}]` through
the fence and checks for `42`. Both commands exit non-zero on failure,
so an install script can use them as its verdict — which is exactly what
`scripts/install/common.sh` does.

It exists because nothing else answered "does the fence work *here*". The
five backends shipped 2026-09-03 with argv-pinning tests and no runtime
evidence; the first time the nspawn backend was actually run it failed
three times in a row on defects no test could have seen.

## Where things live (XDG)

Since 2026-09-04 veles follows the XDG base directories:

| kind | path | contents |
| --- | --- | --- |
| config | `$XDG_CONFIG_HOME/veles` → `~/.config/veles` | `config.toml`, `.env`, `secrets.env`, `vault/` (the secrets vault — see below), `themes/`, `commands/`, `agents/`, `plugins/`, `skills/`, `failover-scripts/` |
| state | `$XDG_DATA_HOME/veles` → `~/.local/share/veles` | `state.db` (sessions and memory — `veles memory`), `checkpoints/`, `media/`, `certs/`, `backups/`, `logs/` |
| cache | `$XDG_CACHE_HOME/veles` → `~/.cache/veles` | regenerable working files |

veles does not read `~/.veles`, the single directory it used before the
split. The migration that moved it into these three roots ran from
2026-09-04 and was removed on 2026-09-26; if a `~/.veles` is still there,
`veles doctor` names it and you can check what is in it and delete it.

The Tcl desktop resolves the same three roots (`veles-desktop/lib/paths.tcl`
mirrors them), so themes, skills, plugins and the hot-patch file are read
from and written to the same places the engine uses.

`VELES_HOME` still forces a single-directory layout, and always
will — the OCI image sets `VELES_HOME=/opt/data` against a bind-mounted
volume, and every test fixture points it at a tempdir. Both want exactly
one directory.

## First run: the setup wizard

A fresh install has no `config.toml`. Two wizards write the first one,
and they are the SAME wizard — the block builders, the config emitter,
the backup rule and the `0600` secrets rule all live in
`veles_core::setup`, so the two frontends cannot emit different TOML
(the drift that once shipped a `key_env` misspelled `api_key_env` and an
IRC block named `[platforms.ircx]`, which the running build never read).

```sh
veles setup        # headless: provider, model, telegram, IRC, ACL posture
veles gui          # desktop: the wizard opens on first run, or via the
                   # palette ("Setup wizard…") / `/setup` any time
```

Both walk the same steps and both **preview before writing**: the exact
`config.toml` bytes are shown (as a diff when the file exists), and the
write refuses if the file changed after the preview was read. A fresh
install gets a whole new `config.toml` (`0600`). An existing one is
**edited in place**: only the sections you answer change, a skipped step
leaves its section exactly as it is, comments and everything the wizard
does not own survive, and the previous text is kept as `config.toml.bak`.
Re-running the wizard and changing nothing changes no byte.

The provider key, the Telegram bot token and an IRC SASL password are
typed into hidden fields (the desktop) or prompts that do not echo
(`veles setup`), and go into the **encrypted vault** — never into
`config.toml`, never into a chat. `config.toml` names each by its
variable (`key_env`); a key pasted into the NAME field is refused without
being echoed. A field left empty keeps whatever is stored, and the
finish screen lists what is still unset, with `veles secret set NAME`
(or `/secret set NAME` on the desktop) to set it later. A stored provider
key is used by the next request, no restart needed. An existing `.env`
(the gateway mints `DESKTOP_BEARER_TOKEN` into it at first boot) only
gains commented reference lines for names still unset, so a live line is
never touched.

The desktop sends key values only to a gateway that accepts them from
it: its own machine, or a network listed in that gateway's
`[vault] trusted_networks` (e.g. a WireGuard subnet). Otherwise the
wizard shows where to type the key instead of a field.

With Telegram or IRC switched on and no `[acl]` section, anyone who
reaches the bot is treated as owner until `TELEGRAM_ALLOWED_USERS` /
`IRC_ALLOWED_USERS` names who may talk to it. The wizard says so and puts
the variable in `.env`; choosing *Curated* access with an owner closes
the door instead. An owner is `telegram:<numeric id>`,
`irc:<services account>`, or a `nick!user@host` mask (written as an owner
`[[acl.entries]]` row).

The desktop wizard auto-opens on first run only: it fires when the engine
reports `config.toml` is absent (`GET /api/setup/state`) **and** this
desktop has no `setup_done` marker. That marker lives in
`desktop_state.json`, not `config.toml` — a config you copy between
machines must not carry a machine-local "wizard done" flag. An operator
with a hand-written config never sees the wizard pop up.

The desktop writes through three loopback, bearer-gated endpoints
(`/api/setup/{state,preview,apply}`); `apply` reloads the live config
handle, so the change is in effect for new sessions immediately, and
platform adapters (telegram, IRC) on the next `veles gateway restart`. If
that reload fails, the finish screen says so.

## Secrets: the vault

Keys live in `~/.config/veles/vault/`: `identity.txt` (this node's age
identity) and `signing.key` stay on the machine; `vault.json` holds one
armored age file per value, encrypted to every node admitted to the
vault, and every row is signed by the node that wrote it.

```sh
veles secret set NAME               # prompts without echo
veles secret list                   # names and what each may be used for
veles secret policy NAME --shell off --hosts api.example.com
veles vault import                  # copy .env / secrets.env credentials in
```

What the model may do with each key is its **policy**. A provider key
works in commands as `$NAME` (the value is injected when the command runs
and scrubbed from the output) and through `api_request`, which sends it
only to that provider's own hosts. A bot token, an IRC password or a
bearer token is for veles itself and never reaches a command. Guests get
no secrets at all. The model asks for a missing key with
`secret_request`: a hidden field opens on the desktop (or the TUI says to
run `veles secret set`), and the model only learns whether it was set.

Another node joins by fingerprint: `veles vault join --push <member>`
on it, then `veles vault admit <node> --fingerprint …` on the member,
and `veles vault apply --inbox --trust <root fingerprint>` on the new
node once the member's vault has reached its inbox (the gateways send
it on their next sync tick; `veles vault push` does it by hand). Both
need each other in `[cluster] memsync_peers`. From then on every vault
write travels by itself (`[cluster] configsync_auto`). `veles vault
revoke` removes a node and lists the keys it could read, to be replaced
at their providers. There is no recovery key: losing every node's
identity loses the vault. `age -d -i ~/.config/veles/vault/identity.txt`
opens a record by hand.

A value exported in the environment (a systemd `EnvironmentFile`, the
shell) still outranks the vault.

## NixOS

```nix
{
  inputs.veles-agent.url = "github:77787777778777/veles";
  # …
  modules = [
    inputs.veles-agent.nixosModules.default
    {
      services.veles = {
        enable = true;
        scope = "user";           # systemd USER unit, your own XDG dirs
        gateway.enable = true;
        gateway.devTree = "veles-agent";      # relative to $HOME; adds the dev unit
        sandbox.backend = "firecracker";
        sandbox.network = false;  # firecracker has no tap devices
      };
    }
  ];
}
```

### Two units: the build you trust, and the tree you are editing

Under `scope = "user"` the module installs **both** gateway units:

| unit | runs | pinned by nix |
| --- | --- | :-: |
| `veles-gateway` | `${package}/bin/veles` | yes |
| `veles-gateway-dev` | `%h/<devTree>/target/<profile>/veles` | **no** |

There is no mode option and **neither unit is wanted by anything**:
`gateway.enable` installs them, and you pick one with the command whose
whole job is to make that symlink.

```sh
systemctl --user start veles-gateway              # just this session
ln -s /etc/systemd/user/veles-gateway.service \
      ~/.config/systemd/user/default.target.wants/    # and at every login
systemctl --user disable veles-gateway            # undoes that symlink
```

Not `systemctl --user enable`: like most NixOS units these are `static`.
A unit only gets an `[Install]` section from `wantedBy`, which would
also plant the root-owned symlink described below — so `enable` warns
that there is no installation config and merely copies the unit into
your directory, where it shadows rather than enables. (`systemctl --user
add-wants default.target <unit>` works too, but leaves a stray
`default.target` symlink in `~/.config/systemd/user` as well.)
`disable` undoes a hand-made symlink cleanly.

That is not just a preference. A `wantedBy` from the module lands in
`/etc/systemd/user/default.target.wants/`, which is root-owned and
nix-managed — `systemctl --user disable` cannot remove it, so a login
would start the production gateway on top of whichever one you were
actually using. `enable` writes into
`~/.config/systemd/user/default.target.wants/`, which is yours.

Run one at a time: both hold the same pid file and the same ports.

`devTree` is relative to `$HOME` and reaches the unit as systemd's `%h`,
so no username appears anywhere and the unit is correct for whoever
starts it. An absolute path (a tree in `/srv`, say) is used as given.

The dev unit exists only when `gateway.devTree` is set, and only under
`scope = "user"` — the system unit runs as a dedicated user with
`ProtectHome = "tmpfs"` and could not see a tree in `/home` anyway. It
runs a binary nix did not build and does not own: **every `cargo build`
in that tree replaces the running daemon's executable.** That is what
the dev unit is for, and why it has its own name — a machine you depend
on should be on `veles-gateway`. If the tree has not been built, the
unit fails with a message saying so rather than systemd's bare
`status=203/EXEC`.

A hand-written `~/.config/systemd/user/veles-gateway.service` from
before this module SHADOWS the one the module installs — a unit file in
the user's own directory wins — so delete it, or a rebuild will appear
to do nothing.

`scope = "user"` runs veles as you, against your own directories, and
reaches root only through `sudo` — the workstation posture.
`scope = "system"` is the same unit as a hardened system service with a
dedicated user and `/var/lib/veles`.

Choosing `sandbox.backend` **builds and wires the artifact that backend
needs**: `nspawn` gets a tclsh rootfs tree, `firecracker` gets a guest
kernel and an ext4 rootfs, and `[sandbox] kernel_path`/`image` are set
to those store paths. A config that names an artifact nobody built is
the failure this coupling exists to prevent.

## Debian sid · Ubuntu · Arch · Gentoo · FreeBSD

```sh
scripts/install/install.sh --from-source                    # detects the OS
scripts/install/install.sh --from-source debian             # or force one
VELES_PREFIX=/usr/local scripts/install/install.sh --from-source
VELES_SCOPE=system      scripts/install/install.sh --from-source   # system unit, not user
VELES_SKIP_DEPS=1       scripts/install/install.sh --from-source   # build + verify only
```

The script installs dependencies, builds from this tree, installs the
binary and a service unit (systemd on Linux, `rc.d` on FreeBSD), then
verifies by running. A missing REQUIRED dependency is fatal; a missing
optional one is **named**, with the capability it costs — a dependency
that quietly did not install becomes a link error twenty minutes into a
cargo build, or a feature that silently is not there.

Native packaging metadata, for anyone who would rather build a package:

```
packaging/debian/debian/     control, rules (dh-cargo-style source build)
packaging/arch/PKGBUILD
packaging/gentoo/app-misc/veles-agent/veles-agent-0.1.0.ebuild
packaging/freebsd/           Makefile, pkg-plist, pkg-descr, files/veles.in
```

None of it is published anywhere: no PPA, no AUR, no ports submission.

### What each OS can fence with

| OS | native | podman | nspawn | firecracker | jail |
| --- | :-: | :-: | :-: | :-: | :-: |
| NixOS | ✓ | ✓ | ✓ (root) | ✓ | — |
| Debian / Ubuntu | ✓ | ✓ | ✓ (root) | ✓ | — |
| Arch | ✓ | ✓ | ✓ (root) | ✓ | — |
| Gentoo | ✓ | ✓ | ✓ (root) | ✓ | — |
| FreeBSD | ✓ | — | — | — | ✓ (root) |
| Windows | ✓ (Job Object limit) | later (Podman Desktop / WSL2) | — | — | — |

- **native** is the safe-interp fence: a real `tclsh` on the host with
  no `exec`, `open`, `socket`, `load` or `source`, plus `RLIMIT_AS` and
  a timeout. Available everywhere, and the default.
- **podman** is defense in depth — the same safe-interp driver inside a
  rootless container. Two postures: hardened (`network = false`,
  `capabilities = "drop"`) and dev (`network = true`, `capabilities =
  "default"`, so `apt`/`pip`/`npm` work inside `sandbox/Dockerfile`'s
  ubuntu image).
- **nspawn** and **jail** need root. On a user-level install that means
  `sudo`, or it means using `native`.
- **firecracker** needs `/dev/kvm`, a guest kernel and a rootfs. On
  NixOS the module builds both; elsewhere see `nix/sandbox-artifacts.nix`
  for what they must contain — the guest init's whole contract is:
  mount `/dev/vdb` at `/work`, run `tclsh /work/driver.tcl`, print
  `VELES_SANDBOX_EXIT:<n>` last.

## Installation profiles and host components

`veles install profile` installs veles in one of four named shapes, and
`veles install host-component` adds what a public host needs: an own
PDS, a media site, a WireGuard pool hub or member. Neither asks you to
write JSON: `--template` prints a commented TOML answers file to fill in.

```sh
veles install profile --template server > server.toml        # desktop | server | container-host | nixos
veles install host-component --template media > media.toml   # pds | media | pool-hub | pool-member
```

Every command previews first: without `--yes` it prints the whole plan
(files with their contents, units, commands, the privileged script) and
changes nothing. An unknown or misspelt key, or an answer the chosen
profile does not use, is refused rather than ignored.

`--core` names the unpacked release bundle: the directory holding
`BUNDLE-INFO.json` and `bin/veles`. For the root handoff, unpack it
somewhere every account may read, e.g. `/opt/veles-<version>`. Run as
root, the bundle mounts its closure on `/nix` in a private mount namespace
(util-linux `unshare`); on a host without Nix that leaves an empty `/nix`
directory behind as the mount point.

### Who runs what

| profile / component | runs as | applied by |
| --- | --- | --- |
| `desktop` | you | `veles install profile --answers desktop.toml --core <bundle> --yes` |
| `nixos` | you | prints a `services.veles` snippet; nothing is applied and `/etc/nixos` is never edited |
| `server`, `container-host`, every host component | root | `--yes` only **stages** a reviewed request; `sudo <bundle>/bin/veles install profile-handoff --request <printed path> --yes` applies it |

The handoff rebuilds the review under a lock and refuses it when the
request, an artifact or the target changed since the preview: an edited
request file, other bytes at a reviewed plugin path, a changed client
config. Secrets (a provider key named by `key_env`, the PDS's S3 keys)
are asked for, hidden, by the step that uses them; an environment
variable of the same name is used instead when set.

The Tk wizard (Setup → Installation profile) previews and applies the
`desktop` profile and previews and stages `server`, PDS and
container-host requests; it never runs the root handoff.

### server

```sh
sudo useradd --system --create-home --shell /usr/sbin/nologin veles
veles install profile --answers server.toml --core /opt/veles-0.1.0 --yes
sudo /opt/veles-0.1.0/bin/veles install profile-handoff --request <printed path> --yes
sudo systemctl enable --now veles-gateway
sudo -u veles env VELES_HOME=/var/lib/veles /var/lib/veles/install/current/bin/veles doctor
```

On the host: `/var/lib/veles` (VELES_HOME: `config.toml`, `.env`,
`install.toml`, `install/releases/<version>` and the `current` link),
owned by the named account; `/etc/systemd/system/veles-gateway.service`
(`User=`, `VELES_HOME`, `gateway run`). The installer never enables or
starts the unit. The provider key named by `key_env` is asked for, hidden,
by the handoff (or taken from the same-named environment variable) and
written to `/var/lib/veles/.env`. To repair or update an installed
server, prepare the request as root (`sudo …/bin/veles install profile
--answers server.toml --core … --yes`): its VELES_HOME is private to the
service account. (A re-run with the same answers is meant to leave the
unit and `config.toml` unchanged; that is not yet shown on a host.)

Rollback: when a step after the first write fails, `config.toml`,
`.env`, `install.toml` (and its `.bak`), the `current` link and the unit
file are restored, and the release and the directories the run created
are removed. Ownership changes and the account itself are not reversed.

### container-host

The confined layout: a rootless gateway account and a sandbox account
on one storage root, their user managers (linger, subordinate ids), the
sandbox's Podman socket behind a group-owned directory, a shared network
that every quadlet joins, and the gateway quadlet, from one reviewed
root script that the preview prints. Images are named by digest only
(`skopeo inspect docker://<image>` prints it); moving tags such as
`latest` are refused. The script checks for podman, uidmap,
passt/slirp4netns and dbus-user-session and installs them only with
`packages.install_missing = true`.

```sh
veles install profile --answers container-host.toml --yes
sudo veles install profile-handoff --request <printed path> --yes
sudo systemctl --user -M veles@ start veles-gateway
```

Rollback: when the script stops, every file it renders (units, tmpfiles,
drop-ins, quadlets, the firewall) is restored, or removed when it was
new. Accounts, linger, subordinate id ranges, packages and units the
script enabled are not reversed, and the error says so.

### nixos

`veles install profile --answers nixos.toml --yes` prints the snippet.
Put it in your configuration and run `nixos-rebuild` yourself.

### Host components

```sh
veles install host-component --answers media.toml --yes
sudo veles install profile-handoff --request <printed path> --yes
```

The components of one request are applied as one transaction. Each host
file, link and directory is captured just before it is written; on any
failure they are restored, or removed when they were new, and the
service effects already performed are compensated in reverse order:
nginx is reloaded on the restored configuration, systemd is reloaded,
and units the run enabled are disabled again. Generated private keys and
pulled images are kept on purpose, and the error says so. DNS is never
changed; point the records at the host first.

- **media**: `/etc/nginx/sites-available/veles-media.conf` plus its
  enabled link, the ACME webroot and the publication directory (an
  existing one must already belong to the gateway account; nothing in it
  is touched). The port-80 vhost is enabled first, certbot obtains the
  certificate by HTTP-01, then the TLS vhost replaces it, and a probe
  file is fetched back over HTTPS from this host. The
  `[platforms.irc] media_public_*` keys are printed for the gateway's
  config.
- **pds**: two runs. The first creates the secrets (the S3 keys you are
  asked for; a KEK and tokens generated here), pulls the pinned vlpds
  image as the gateway account, wraps a PLC rotation key with it and
  writes the quadlet and the gateway ordering drop-in; nothing starts.
  Back up `/srv/veles/vlpds/secrets/kek` and `plc-rotation.key` offline,
  set `backup_confirmed = true`, and run again for HTTP-01 TLS,
  activation and a public health check.
- **pool-hub** (on the arbiter): `/etc/wireguard/wg0.conf` with a new key
  pair (an existing one is never rotated), wg-quick, and the root helper
  that applies the peers `veles pool invite` queues
  (`/usr/local/sbin/veles-wg-peers` and its path unit). The report prints
  the `[pool.wireguard]` section with the hub's public key. The gateway's
  data directory must be outside `/home` (the helper runs with
  ProtectHome), so use a `server` profile. Until the pool has peers, an
  arbiter's A2A listens only on 127.0.0.1 unless its `.env` sets
  `A2A_BEARER_TOKEN`; the report says so.
- **pool-member**: on the arbiter, `veles pool invite <name>
  --wireguard-file <file>` writes the newcomer's WireGuard config; copy
  it over, name it in the answers, apply, then `veles pool join '<invite
  text>'` as the gateway's account.


### What was run on a real host, and what was not (2026-10-28)

Disposable Debian 13 (trixie) genericcloud VMs under QEMU/KVM, 2 vCPU,
user-mode networking plus a private VM-to-VM link. The core was the
nix-built release bundle of a pre-release commit with the 0.1.0 CLI,
runtime library (debug build) and launcher swapped in; a clean 0.1.0
bundle has not been run on these VMs.

| what | result |
| --- | --- |
| `server`: template → preview → stage → root handoff | applied; `/var/lib/veles` owned by the account; unit installed, not enabled |
| `server`: `systemctl enable --now veles-gateway` | active; the gateway runs as the account |
| `server`: `veles doctor` as the account | passes; the provider key given to the handoff is in `/var/lib/veles/.env` |
| `server`: preparing a second run as root | staged; **the second root handoff did not finish inside the test window, so re-run idempotency on a host is not shown** |
| `container-host` with a locally built gateway image pushed to a local registry (standing in for the release registry), pinned by digest | script applied; accounts, linger, subordinate ids, sandbox socket, `veles-net`; the gateway quadlet starts and `/api/health` answers |
| `container-host`, script failing mid-way (user manager not yet up, then `~/.config` ownership) | the error names what is not reversed; the rendered files were restored (no quadlet left) |
| `pds` first run | vlpds image pulled as the gateway account, PLC key wrapped by vlpds, secrets owned by the mapped container uid, quadlet written, nothing started |
| `pds` with `backup_confirmed`, ACME failing | vhost and link removed, nginx reloaded and valid, quadlet and secrets kept, the gateway still healthy |
| `media`, ACME failing | the operator's nginx state restored byte-for-byte; new directories removed |
| `media` with a certificate already present (a test CA trusted on the VM) | TLS vhost enabled, probe file fetched back over HTTPS |
| `pool-hub` → `veles pool invite --wireguard-file` → `pool-member` on a second VM → `veles pool join` | hub key generated, wg-quick up, the queued peer applied by the helper; handshake and ping both ways; the join redeemed the invite (the arbiter lists it as joined, the newcomer names the arbiter). The arbiter's own `nodes list` had not yet shown the newcomer 15 s after its gateway started |
| `nixos` | the emitted snippet evaluates inside a throwaway NixOS configuration on the build host (not applied anywhere) |

Not verified, and why:

- **Public ACME (HTTP-01) success, the PDS activation and its public
  health check**: they need a real domain whose DNS points at the host
  and, for the PDS, a real S3 bucket. On the VMs certbot failed (Let's
  Encrypt refused the example mail address before any challenge); only
  the failure and rollback paths, and the TLS path with an existing
  certificate, ran.
- **The Tk wizard** was exercised by its test suite against a mocked API
  (installtest) and by the API's own tests, not by hand on a VM.

## What has actually been run

Proven on the development machine, 2026-09-04, each by executing a
script through the fence and getting `42` back:

| fence | posture | result |
| --- | --- | --- |
| native | safe interp on the host | `result: OK (42)` |
| nspawn | `network=false`, `capabilities=drop`, under sudo | `result: OK (42)` |
| firecracker | microVM, guest kernel + ext4 rootfs | `result: OK (42)` |
| podman | `network=false`, `capabilities=drop` | `result: OK (42)` |
| podman | `network=true`, `capabilities=default` | `result: OK (42)` |
| jail | FreeBSD 15.0-RELEASE, `network=false`, under sudo | `result: OK (42)` |

**All five.** Getting there cost fourteen defects, not one of which was
visible to a test that reads code. nspawn: `--drop-capability=ALL` is a
hard parse error (it wants lowercase `all`), the container got no `PATH`
so `execv(tclsh)` failed with `/usr/bin/tclsh` present, and nspawn
refuses a rootfs without `/usr`. jail, the same shape on the other OS:
`ip4.disable` is not a parameter jail(8) knows (`ip4=disable` is), the
interpreter was hardcoded as `tclsh` on the one platform whose tcl
package installs only `tclsh8.6`, and the whole command was packed into
a single `command=…` string, which jail treats as one filename.

Each of those three jail defects had a passing unit test pinning the
broken shape — an argv checked against itself.

And the installer matrix, all four container targets:

| target | result |
| --- | --- |
| Debian sid | PASS |
| Ubuntu 24.04 | PASS |
| Arch | PASS |
| Gentoo stage3 | PASS |

Arch and Gentoo had failed since these scripts were written, with 47
`no field 'refCount' on type '&Tcl_Obj'` errors. The cause is libclang
22: `bindgen` 0.64, which `clib` 0.2.4 pins, cannot parse with it and
silently makes every record containing an anonymous union opaque.
Debian and Ubuntu were passing because they are still on clang 21 and
18. `vendor/clib` raises the floor to bindgen 0.72 — see
`vendor/clib/WHY.md`, which also records two earlier explanations of the
same errors that were wrong.

`sandbox/README.md` records the firecracker and nspawn artifact
contracts.

## Windows

Native, on the desktop ("the dom0"), not inside WSL — a node that can
post a toast, open a window and drive the browser the person sees.
One codebase: the engine's platform layer
(`crates/veles-core/src/platform/`) answers the Windows questions —
processes, the local pipe, private files, the module loader — and
nothing else in the tree knows which OS it is on.

```powershell
pwsh -ExecutionPolicy Bypass -File scripts\install\windows.ps1
pwsh ... windows.ps1 -SkipDeps          # build + verify only
pwsh ... windows.ps1 -Prefix D:\veles    # elsewhere than %LOCALAPPDATA%\Programs\veles
pwsh ... windows.ps1 -WithWireGuard     # also install the WireGuard app (a pool node)
pwsh ... windows.ps1 -SkipDeps -PortableZip out\veles.zip   # build the release zip instead
```

The script installs the dependencies with `winget` (Git, rustup,
CMake, LLVM for bindgen, NASM, ripgrep; the Visual Studio Build Tools
with the C++ workload when no MSVC is found), **builds Tcl/Tk 8.6.16
from source** into `<prefix>\tcl` (the same doctrine as everywhere: the
binary is bound to the Tcl it ships) with **tcllib 2.0** beside it (the
desktop's `json` package), fetches **sqlite.org's
`sqlite3.dll`** and makes its import library (rusqlite links the shared
sqlite on every target — `crates/veles-core/Cargo.toml` says why — and
Windows has no system one), builds veles and the modules with the
`winrelease` profile (release without LTO: thin LTO inlines code from
crates inside `veles_runtime.dll` into `veles.exe`, which then names
the DLL's statics without the import indirection PE needs — LNK2001 on
`GLOBAL_PANIC_COUNT`; root `Cargo.toml` says so), installs them,
registers the gateway and ends by running it.

| what | where |
| --- | --- |
| `veles.exe`, `veles-gw.exe`, `veles_runtime.dll`, `std-*.dll`, `sqlite3.dll`, `tcl86t.dll`, `tk86t.dll`, `zlib1.dll` | `<prefix>\bin` (on the user PATH) |
| Tcl/Tk, `tclsh.exe`, `wish.exe`, tcllib | `<prefix>\tcl` |
| sqlite.org's `sqlite3.dll` + the import library the build links | `<prefix>\sqlite` |
| the desktop app, the bundled skills | `<prefix>\veles-desktop`, `<prefix>\skills` |
| config: `config.toml`, `.env`, `node.toml`, `vault\` | `%APPDATA%\veles` |
| state: `state.db`, `modules\`, `gateway.log`, `run\` | `%LOCALAPPDATA%\veles` |
| cache | `%LOCALAPPDATA%\veles\cache` |

`VELES_HOME` still forces one directory, as on every platform; an XDG
variable, when set to an absolute path, still wins.

**How the gateway runs.** `veles gateway install` registers a **logon
task** (Task Scheduler, `veles-gateway`, interactive, limited
privileges) that runs `veles-gw.exe`, a tiny GUI-subsystem launcher
that starts `veles.exe gateway run` without a console window and exits.
The gateway therefore lives in your desktop session — toasts, Tk apps,
a headed browser and cua-driver all need that — and starts whenever you
log on. A Windows *Service* (session 0, no desktop) is a later option.
The task is registered from an XML definition
(`%LOCALAPPDATA%\veles\gateway-task.xml`, kept for comparison): that is
the one form a normal, non-elevated shell may register a logon task in
(the `/SC ONLOGON` command line is "Access is denied" without an
administrator), and it sets no execution time limit (the command-line
default would end the gateway after 72 hours) and no battery rule.
The launcher waits for the gateway and exits with its code, so the
task runs as long as the gateway does and its `RestartOnFailure`
(every minute, five times) brings back a gateway that died — systemd's
`Restart=on-failure` — while a clean stop exits 0 and stays stopped.
`veles gateway status | stop | restart | uninstall` work as elsewhere;
a stop asks the gateway over its local pipe first (there is no SIGTERM
to send), then ends it.

**Modules** are `veles_module_<name>.dll` files under
`%LOCALAPPDATA%\veles\modules\<name>\<hash>.dll`, with `current` a
one-line file naming the hash (no symlink). `veles plugins
install-module <name> <file>` installs one (`<name>` is the name the
module declares — `echo-module` for the echo module); the same command
works on Linux. The Racket CS and Common Lisp hosts are not built on Windows yet;
echo, irc, telegram, slopdrop and lang-js are.

**What is absent, said plainly** (the prompt tells the model the same):
podman user boxes and every container fence — the Tcl fence is the
native safe interpreter under a Job Object memory limit; the Xvfb
drawing path — the browser draws on the desktop itself, headless when
told to; `systemctl`, `xdg-open`, `notify-send` — the device tool's
`open` and `notify` use `start` and a PowerShell toast instead. The
model's shell is PowerShell 7 (then Windows PowerShell, then cmd;
`VELES_SHELL` overrides). The bundled `windows-node` skill
(`skills/windows-node/SKILL.md`) says the same at length for the model;
a skill's `platforms: [windows]` frontmatter line is what makes it exist
on this node and on no other (any skill may carry one; without it a
skill is everywhere).

**The build host and the gate.** `scripts/win/check.sh [host] [step]`
copies the working tree to a Windows machine over ssh and runs `cargo
check`, `clippy`, `build`, `test` or `all` there (`scripts/win/check.bat`;
`all` is clippy + build + the unit tests of core, frontends, cli, the
plugin SDK and types, ~15 min warm). `VELES_WIN_HOST=<host>
scripts/precommit.sh` adds `all` to the gate; a host that does not
answer is skipped, not red.

## Proving the installers

Every one of these is **on demand**. None runs from `precommit.sh`, from
a hook, or from `nix flake check` — a full matrix run is about an hour
and several GB of container overlay, and the NixOS VM test boots qemu.

```sh
scripts/install/test-matrix.sh          # debian, ubuntu, arch, gentoo — containers
scripts/install/test-matrix.sh --clean  # drop the cached images and volumes
scripts/sandbox_vm_check.sh local       # native, nspawn, firecracker, podman — here
scripts/sandbox_vm_check.sh freebsd     # the jail fence, in the ci-freebsd VM
nix build .#nixos-module-test           # the module, proven by booting it
nix flake check                         # the fast gate: build, clippy, fmt, tests
scripts/install/test-install.sh         # install.sh vs a loopback fixture release (needs socat)
scripts/release/install-smoke.sh ARCHIVE # the real archive via install.sh on debian/fedora/arch
scripts/win/release-zip.sh [host]        # the Windows release zip, built + proven on the build host
```

The matrix caches the dependency step as a per-distro image after its
first success and shares one cargo-registry volume, so a second run goes
straight to the build; `VELES_MATRIX_FRESH=1` forces the dependency step
again after editing `deps/<distro>.sh`. `target/` is deliberately not
cached (4–6 GB per distro); `VELES_MATRIX_CACHE_BUILD=1` asks for it.

Measured on the Debian arm, 2026-09-04: **~9½ minutes cold, 135 seconds
warm**, for one 2.93 GB image and a shared registry volume.
`test-matrix.sh --clean` gives that back, and reports only what it
actually removed — `podman rmi -f` exits 0 for an image that was never
there, so a cleanup that asks nothing would claim to free four images it
never had.

The VM arms use the libvirt domains `ci-freebsd` (192.168.122.60) and
`ci-linux` (192.168.122.50) when they exist, falling back to booting the
qcow2 by hand. Prefer the domains: the fallback uses qemu user
networking, so the guest lands on 10.0.2.x with no cloud-init seed and
the provisioned accounts look like they do not exist.

Containers for the four Linux distros because the question they answer —
"does the dependency step reach a working `veles doctor`?" — needs no
PID 1 and no KVM. VMs for what containers cannot answer: systemd units,
nspawn, firecracker, and FreeBSD (which has no container path at all).

Gentoo's arm uses the binhost (`--getbinpkg`); a from-source toolchain
build is hours and the binhost is what Gentoo users run. Set
`VELES_GENTOO_SOURCE=1` for the full source build.
