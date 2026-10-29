# The Android app

USER, 2026-09-30: *"an apk contains the necessary dependencies and
installs a system service in android, then we push through adb and
ideally at some future point release this in a store"*.

The app is the gateway: `veles gateway run` inside a foreground service,
so the phone is a node of the pool (PLAN-android.md, PLAN-compute-pool).
It is also the desktop frontend: the same `veles-desktop` Tcl/Tk tree as
on a laptop, on AndroWish's Tk (PLAN-android.md phase C). The launcher
shows two entries, "Veles" (the desktop) and "Veles gateway" (the
service's controls and log).

    nix build .#veles-apk               # result/veles-{debug,release}-unsigned.apk
    nix run .#android-install           # sign with this machine's key, adb install
    nix run .#android-emulator-smoke    # the whole lifecycle on an x86_64 emulator

**A desktop edit costs seconds, not a cross-build.** The engine is
built only from what compiling it reads (flake.nix `engineSrc`), and
the Java in a step of its own (`androidCode`), so editing a `.tcl` file
under `veles-desktop/`, `android/desktop.tcl`, an engine catalog or
this README repacks the APK around both: 15 s, against 219 s for the
engine's cross-build (arm64, this machine), then
`android-install` as before. An edit under `crates/` is still the
engine's cross-build. Precommit runs `scripts/android-rebuild-scope.sh`,
which fails if such an edit ever reaches anything else, or if the
engine `include_str!`s a file `engineSrc` leaves out.

## What is in the APK

| Path in the APK | What it is |
|---|---|
| `lib/arm64-v8a/libveles.so` | the engine: the release `veles`, an executable despite its name |
| `lib/arm64-v8a/libtcl8.6.so` | Tcl, which the engine links |
| `lib/arm64-v8a/libtclsh.so` | `tclsh`, for the execute_code tool (`VELES_TCLSH`) |
| `assets/runtime.zip` | Tcl's script library, the engine's 47 catalogs, and the bundled Tcl plugins (`plugins/tcl-anthropic-driver`) |
| `lib/arm64-v8a/lib{tcl,tk,SDL2,freetype,main}.so` | the desktop's Tcl/Tk: AndroWish's, built from source |
| `assets/tcl8.6`, `assets/sdl2tk8.6`, `assets/json` | their script libraries, and tcllib's `json` |
| `assets/veles-desktop`, `assets/app/main.tcl` | the desktop, and `android/desktop.tcl`, which starts it |
| `classes.dex` | the six Java classes below, and AndroWish's |
| `lib/arm64-v8a/libulsh.so` | `ulsh`, the shell that runs a script in the userland package's Alpine (below); `VELES_SHELL` when that package answers |

### The userland package: a second APK

`chat.janitor.veles.userland` (`android/userland/`,
`nix build .#veles-userland-apk`, `nix run .#android-install-userland`)
is a separate app at targetSdk **28**, the last target whose SELinux
domain may execute its own data files: it carries a distro the gateway
(targetSdk 36) cannot. Adopted 2026-10-20 after its spike —
PLAN-phone-userland.md T3 has the gates, docs/phone.md "The userland
package" the install, the sandboxes and the checks. arm64-v8a only,
signed with the gateway's sideload key.

| Path in the APK | What it is |
|---|---|
| `lib/arm64-v8a/libproot.so` | Termux's proot fork, static musl, 16 KB pages, no NEEDED |
| `lib/arm64-v8a/libproot_loader.so` | proot's loader, unbundled (the broker sets `PROOT_LOADER`) |
| `lib/arm64-v8a/libveles_userland.so` | the exec broker (`crates/veles-userland`): sessions handed over by the service on `@veles-userland-handoff` (this app's own uid only), and the abstract socket `@veles-userland` for the gateway's uid (which Android 16 does not let the gateway connect to); a session runs in `files/rootfs`, or in a named sandbox, `files/sandboxes/<name>` (`--sandboxes`), which it makes from a copy of the rootfs on first use |
| `assets/rootfs.tar` | Alpine 3.24.2 aarch64 minirootfs (ustar), unpacked once to `files/rootfs` |
| `classes.dex` | `UserlandService` (foreground, wake lock: unpack, `resolv.conf`, run the broker; exported, started by the gateway's bind), `Sessions` (its Binder: `openSession`), `Rootfs` (`files/rootfs`, `files/shared`, `files/sandboxes`), `MainActivity` |

**The permission.** The userland declares
`chat.janitor.veles.userland.permission.OPEN_SESSION` at `signature`
level and guards `UserlandService` with it; the gateway's manifest asks
for it, so only an app signed with the same key may bind the service.
Android grants it when the gateway is installed after the userland (an
install of veles that came first goes without it until veles is
installed again: `dumpsys package chat.janitor.veles | grep OPEN_SESSION`).

**The transport** (PLAN-phone-userland.md T3 §4; measured on the
SM-S942B in T3 §2 R1b-bis). The two apps' SELinux domains may not
connect to each other's sockets, but Binder may carry a descriptor
between them. The gateway's `UserlandRelay.java` binds the service once
and serves `@veles-userland-local` to its own processes; each `ulsh`
(`ULSH_RELAY`) gets a session there: `Sessions.openSession` makes a
socketpair, hands the broker one end and returns the other, which the
relay passes to ulsh and does not keep. ulsh then speaks the broker's
protocol on it, its stdio riding as `SCM_RIGHTS`. The gateway's
`Userland.java` runs `ulsh -c 'exit 0'` that way when the engine starts
and, when it exits 0, points `VELES_SHELL` at `libulsh.so`; every shell
the engine spawns then runs in Alpine, and Android's (or Tier 1's bash)
is the fallback.

**The sandboxes** (PLAN-phone-userland.md T3 design item 4).
`[sandbox] backend = "userland"` — the phone's default while the userland
answers (`auto`; `backend = "native"` opts out) — makes the phone's
user-env boxes the broker's named sandboxes (a ulsh with `ULSH_SANDBOX` = the box's key,
and no environment of the engine's) and runs the Tcl fence with the
rootfs's tclsh; the engine reaches a box's files by exec only, as under
a remote podman. Isolated files, not a podman fence: same uid, the
phone's network, no memory cap — so the approval gate stays on for them.
docs/phone.md "Sandboxes in the userland" has the config and the checks.

**Why an executable is called `lib*.so`.** Since Android 10, an app may
execute files only from its native library directory, never from its own
data directory. Only files named `lib*.so` under `lib/<abi>/` are
extracted there, and only with `android:extractNativeLibs="true"`. It is
the documented way to ship an executable, and Play accepts it.

Nothing else is needed on the phone: no Termux, no root. The engine's
libraries are bionic's own (libc, libm, libdl, liblog, libz) and
libtcl; the desktop's add Android's own graphics and system libraries
(libandroid, libGLESv1_CM, libGLESv2, libstdc++). The flake's build
fails if any other one appears, or a segment is not aligned to 16 KB.

## What runs on the phone

- **GatewayService.** A foreground service of type `specialUse`,
  showing a notification with a Stop button. It runs the engine and
  starts it again when it crashes: after 1 s, doubling to a minute, and
  back to 1 s after five minutes of uptime. Stop sends SIGTERM (the
  gateway's graceful shutdown) and SIGKILL after 10 s.
- **Idle.** The phone's gateway is for the person holding it — chat
  networks and cron run on the VPS — so the engine runs with
  `--exit-when-idle 300`: five minutes after anyone last used it (a
  desktop request, an inbound message, a turn in flight) it stops on
  its own, exit 75, and the service ends with it. No timers, no
  sockets, no radio; the phone's battery stats showed the gateway
  drawing as much as the charger supplied before this (2026-10-04).
  Opening the app starts it again. A reboot and an update start
  nothing: there is no boot receiver.
- **MainActivity**, "Veles gateway" in the launcher. Start, Stop, and
  the tail of the engine's log.
- **Engine.** The layout and environment:
  - `files/home`: `VELES_HOME`, which holds config.toml, .env, state.db
    and everything else.
  - `files/runtime`: unpacked from runtime.zip once per install;
    `VELES_BUNDLED_PLUGINS` names its `plugins/`, the read-only bundled
    Tcl plugin tier (docs/plugins.md, "Bundled plugins").
  - `files/gateway.log`: the engine's stdout and stderr, with one old
    copy kept at 4 MB.
  - Environment: `SSL_CERT_DIR` at Android's CA store; `TZ` and
    `VELES_LOCALE` from the phone's settings; `NO_COLOR`, since the
    log is shown as text.

## The desktop frontend

- **DesktopActivity**, "Veles" in the launcher. AndroWish's activity
  (`tk.tcl.wish.AndroWish`, Tk drawn through SDL2), in a process of its
  own (`:desktop`): Tcl's `exit` ends the process it runs in, and the
  gateway's service must not go with it. Opening it starts the gateway,
  as `veles gui` does on a laptop.
- **android/desktop.tcl** (`/assets/app/main.tcl`: AndroWish mounts the
  APK at `/assets` and runs that file). It sets `VELES_HOME` to the
  gateway's home, waits up to 30 s for the gateway, reads the desktop
  API's token from its `.env`, and sources the desktop's `main.tcl`.
- **veles-desktop/lib/android.tcl**, the one place the desktop knows it
  is on a phone (a no-op elsewhere, tested by `androidtest`):
  - one desktop pixel is one Android dp, in fonts (`tk scaling`) and in
    the sizes the desktop gives in pixels (dialogs, tree rows);
  - the window is the screen, without the desktop's own title strip;
  - a fresh install's instance registry holds "phone", the gateway in
    the same app, and attaches to it;
  - the composer gets its own row, and in portrait the sidebar a third
    of the width;
  - hover tips go to the status bar: a tap is an Enter with no Leave.
- **The keyboard.** From Android 15 an app is edge to edge and the
  window no longer shrinks for the keyboard (a fullscreen one never
  did). DesktopActivity pads the content by the keyboard and the
  navigation bar, so SDL's surface shrinks and Tk lays the desktop out
  above the keyboard.
- **AndroWish** is built from its 2024 release source (NDK r27, API 26,
  16 KB pages), only what the desktop needs, with five patches in
  `android/androwish/`: its Java on the platform API instead of a
  prebuilt support library; its Tk resolving an unknown font family as
  fontconfig does (the desktop's `{TkDefaultFont 11}` was ~10 px); the
  device's clipboard left alone when a Tk selection goes away; a long
  press that survives a finger drifting (`touch.patch`, below); and
  dialog titles in the phone's script (`title-font.patch`: the frame
  draws them itself, in a font with no CJK). Its Tk also takes the
  desktop's own Tk patch, `nix/tk-kinsoku.patch`, so Chinese and
  Japanese lines break between characters, never before 。 or after 「.
- **Other gateways.** The Instances tab adds any gateway by URL and
  token; the registry is the app's (`files/.config/veles-desktop`),
  never the gateway's. From the emulator the laptop is `10.0.2.2`;
  from a phone, its WireGuard or LAN address.

**By touch** (fde6e60, 8b130ad, 6470bb8, 7b2a783, and the command
tree):

- **⋮, every command.** The ⋮ at the end of the tab strip opens every
  command the desktop has, by category, a tap at a time
  (`lib/cmdtree.tcl`): the palette's actions, everything a key does in
  any keymap mode, the tabs, and the slash commands. Its first row is
  the palette's search; an empty session's welcome line holds the same
  button. On a laptop each row names its key. A slash command that
  needs an argument is written into the composer, keyboard up.
- **Portrait** stacks the regions and makes the session list a drawer
  over the chat (☰); picking a session closes it. Landscape keeps them
  side by side. Which one is the display's shape, not the window's, so
  the keyboard coming up does not turn a phone sideways.
- **Back** closes the menu, the drawer or the window on top (in the
  command tree, a level first), and otherwise sends the app to the
  background; it never quits.
- **Dialogs** are fitted in dp inside what the keyboard leaves, with
  AndroWish's frame as their one title bar, and their text wraps inside
  them; the setup wizard's steps scroll and keep the field being typed
  into in sight.
- **Rows of buttons wrap** (the Instances strip, the chat bar, the tab
  strip, the sidebar's); buttons, check marks, tree triangles, menus
  and list rows are sized in dp. No label names a key chord.
- **A finger** drags read-only text (the transcript, the command tree)
  instead of selecting it, and a tap opens a link or runs a row only if
  it did not move. Two fingers do not zoom the window.
- **A long press** is the right button, as AndroWish makes it, but held
  for Android's own touch and hold delay and allowed to drift up to
  twice the device's touch slop before it counts as a drag
  (`touch.patch`; DesktopActivity hands both values to Tk). AndroWish
  cancelled it on any motion at all, so a held finger scrolled instead.
- **Selecting text.** A long press on the transcript (any read-only
  text with a menu) selects the word under the finger; moving the
  finger before it lifts carries the selection a word at a time,
  scrolling at the edges; a long press on either end moves that end a
  letter at a time. Lifting opens the menu, with Copy first; a tap
  clears the selection.
- **Help** (⋮ → Help, or F1) on a phone is the touch controls; the
  keyboard's help is a button on it.
- **Notifications** (8b130ad): an agent's question, and a desktop turn
  that finished, while the desktop is not on the screen; a tap opens
  the session (and clears the question's).
- The soft keyboard opening at start on the emulator is its PS/2
  keyboard device; a phone without one does not open it.

The desktop API listens on the phone's `127.0.0.1:8642`, as it does
everywhere. From the laptop:

    adb forward tcp:18642 tcp:8642
    adb shell run-as chat.janitor.veles cat files/home/.env    # the bearer token

A config goes onto the phone the same way (the debug APK only;
`run-as` needs a debuggable app):

    adb shell run-as chat.janitor.veles sh -c 'cat > files/home/config.toml' < config.toml

## The phone's toolbox (PLAN-phone-userland.md T1)

The arm64 APK carries a static toolbox beside the engine, each program
named like a library because Android extracts only `lib*.so` from
`lib/<abi>/`. Engine.java (`TOOLS`, `linkTools`) links each by its real
name into `files/bin`, which is **first** on the gateway's PATH, ahead
of `/system/bin` (HANDOFF pitfall 77):

| on PATH | ships as | flake.nix |
|---|---|---|
| `bash` (VELES_SHELL, SHELL) | `libbash.so` | `velesBashAndroid` |
| busybox's applets (`ash`, `awk`, `vi`, `less`, `wget` …) | `libbusybox.so` | `velesBusyboxAndroid` |
| `git` (+ `libexec/git-core` helpers) | `libgit.so`, `libgit_remote_http.so` | `velesGitAndroid` |
| `rg` (also VELES_RG) | `librg.so` | `velesRgAndroid` (both ABIs) |
| `jq` | `libjq.so` | `velesJqAndroid` |
| `curl` | `libcurl_cli.so` | `velesCurlCliAndroid` |
| `sqlite3` | `libsqlite3_cli.so` (`libsqlite3.so` is the engine's library) | `velesSqliteCliAndroid` |
| `ssh`, `scp` | `libssh.so`, `libscp.so` | `velesSshAndroid` |

Each is nixpkgs' static musl package relinked with 16 KB pages and
passes the same two guards (`checkAndroidStatic`: no NEEDED at all,
every LOAD at 0x4000). The x86_64 emulator gets none of them (static
musl dies under its app filter) and keeps mksh. Static musl cannot
resolve names here: git and curl go out through the engine's loopback
CONNECT proxy (git by configuration in the environment, curl by
`CURL_HOME/.curlrc`, both rewritten per start with that start's token),
and ssh reaches hosts by address (the pool's tailnet IPs). ssh answers
"who am I" from HOME/USER, since Android has no `/etc/passwd`.

## Not on the phone

- **A package manager.** At targetSdk 36 the app's SELinux domain may
  not execute anything in its own storage, so nothing can be installed
  next to the toolbox: no apt/apk/pip, no python, no node. The
  `veles-userland` package at targetSdk 28 is the way past this
  (PLAN-phone-userland.md T3, adopted 2026-10-20): with it installed
  the terminal runs in Alpine, where `apk add` works, and the prompt
  stops listing this.
- **A container runtime.** Sandboxed calls run on `[cluster]
  sandbox_home`, or (Tier 2) on a peer that advertises one — or, with
  the userland answering (the default; `[sandbox] backend = "native"`
  opts out), in its proot guests on the phone itself (above).
- **`/usr/bin/env`.** A `#!/usr/bin/env bash` script fails outside the
  userland; run scripts as `bash file` (inside Alpine it is there).
- systemctl, sudo, a desktop (xdg-open, notify-send).

In the pool these are other nodes' tools: `[tools.home]` by hand, or the
automatic homes of PLAN-phone-userland.md T2 (docs/pool.md).

**ripgrep IS on the phone** (2026-10-01): `velesRgAndroid` in flake.nix
cross-builds nixpkgs' ripgrep 15.2.0 for both ABIs with the same NDK
environment and guards as the engine (only system libraries NEEDED,
16 KB-aligned LOAD segments), and the APK carries it as
`lib/<abi>/librg.so`. Engine.java points `VELES_RG` at it, which
`search_files`'s host path runs (`filetools.rs rg_path`); it also puts
`files/bin/rg` — a symlink to the same file — on the gateway's PATH, so
`rg` works in terminal commands. That link works because of what
Android forbids: since Android 10 an app may not execute a file in its
own writable storage (SELinux gives it no execute on app data), and a
symlink there is only a name. The kernel follows it and checks the
target, librg.so in the native library directory, which the app may
execute, as it executes the engine. Measured on the emulator (16 KB
kernel) from inside the app, not through `run-as` (another SELinux
domain): an agent turn's terminal command `rg --version` prints ripgrep
15.2.0 (the smoke test's `rg-on-the-agent-s-own-path`). The sandbox
zone's rg is the container's own and is deliberately unaffected.

## How it is built

`flake.nix`, `velesAndroid` and `velesApk`. There is no Gradle; the
SDK's own tools (aapt2, javac, d8, zipalign) are called in one nix build,
so nothing resolves dependencies over the network and the build is
reproducible.

- The APK is **unsigned** in the store. A signing key does not belong in
  a world-readable path, and an unsigned build is what a store's
  reproducibility check compares.
- Every string but English comes from `strings.tsv`, turned into
  `res/values-*/` by `scripts/android-strings.tcl`. The build fails when a
  string lacks a language or a placeholder differs, the desktop's rule.
- The app needs API 26 (Android 8); it targets 36 (Android 16), which
  Play requires of new apps and updates.
- Native code is linked for 16 KB pages, which Play requires. The smoke
  test boots a 16 KB page kernel to prove it.

## Toward a store

- **The app id**, `chat.janitor.veles`, is permanent once a store has
  it. Changing it now costs only a reinstall.
- **Keys.** The sideload key (`~/.config/veles-android/sideload.p12`,
  made by `android-install`) is for this machine's installs. A store gets
  its own upload key, and Play re-signs with its own (Play App Signing).
  Android installs an update only over the same signature, so moving a
  phone from the sideload build to a store build means uninstalling,
  which deletes `files/home`: back it up first.
- **Play** takes an app bundle (`.aab`), not an APK. `nix build
  .#veles-aab` produces it, unsigned, from the SAME payload as the APK
  (one `androidPayload` derivation feeds both, so they cannot drift):
  proto-format aapt2 link, the module zip bundletool wants
  (`manifest/`, `res/`, `resources.pb`, `dex/`, `lib/`, `assets/`), and
  `android/BundleConfig.json`, which turns off bundletool's
  uncompressed native libraries. Left on, the split APKs it makes for
  a device say `extractNativeLibs=false`, the libraries stay in the
  APK, and the engine, an executable run from the extracted library
  directory, is not on disk: a store install's gateway never started
  (measured 2026-10-01, while a universal APK from the same bundle,
  built differently, passed every check). `nix run
  .#android-aab-smoke` proves the flow end to end: bundletool makes the
  debug bundle into the split APKs it would serve the emulator, and the
  full smoke test runs against those. Two gotchas,
  both measured: bundletool's embedded aapt2 cannot execute on NixOS
  (exit 127 out of /tmp), so `build-apks` takes `--aapt2=` pointing at
  the SDK's; and the smoke needs the DEBUG bundle, since `run-as`
  reaches the app's files only when the package is debuggable. The
  `specialUse` foreground service needs the declaration form's
  justification, and the text in the manifest's
  `PROPERTY_SPECIAL_USE_FGS_SUBTYPE` is its draft.
- **F-Droid** builds from source. The nix build is the reproducible
  recipe their "reproducible builds" check can compare against a signed
  upload.
- `versionCode` (flake.nix) is raised by hand for each store release.
## Two nodes

The phone is a fleet peer, not the only gateway. Part F of
PLAN-pool-round.md is the two-node deployment: the phone and a laptop
(or the VPS) sharing sessions, with the laptop running the tools a
phone cannot (web and browser) and holding the arbiter.

`nix run .#android-pool-rehearsal` proves the whole thing without a
phone: scripts/f-partf-emulator.sh boots the emulator as the "phone"
node, runs a scratch gateway on this machine as the "laptop" node (its
home in /dev/shm, ports 198xx-199xx, the mock provider for the model),
and checks that gossip converges across the boundary, that a phone
session replicates to the laptop and a laptop session to the phone,
that a phone session's `web_extract` runs ON THE LAPTOP (`[tools.home]`),
that stopping the phone's gateway lets the laptop take its session over
once the node lease expires (and that the phone yields on return, one
writer at a time), and that a ticketed elevated call works across the
boundary with a key each side trusts.

### The config pair

Two files per side. The ids are what the cluster registry knows; the
peer NAMES are what the config lists. Replace the addresses with the
WireGuard ones.

**laptop** `~/.config/veles/config.toml`:

```toml
[node]
endpoint = "http://<laptop-wg-addr>:9900"   # what the phone dials

[a2a_agents.phone]
url = "http://<phone-wg-addr>:9900"
# The token the LAPTOP presents when it dials the phone: its OWN
# A2A_BEARER_TOKEN, which the phone's A2A_PEER_TOKENS names as
# `laptop:<it>`. (The d1 convention: [a2a_agents.X].auth.token is what
# you send; A2A_PEER_TOKENS is what peers send you.)
auth = { type = "bearer", token = "<laptop's A2A_BEARER_TOKEN>" }

[platforms.a2a]
enabled = true

# This node's own bind address — node-local, configsync never ships it.
[platforms.a2a.extra]
host = "<laptop-wg-addr>"

[cluster]
arbiter = "<laptop node id>"   # a prefix is enough, if unique
gossip_peers = ["phone"]
memsync_peers = ["phone"]
task_peers = ["phone"]
storesync_peers = ["phone"]
storesync_auto = true
failover = true

[cluster.ticket_signers.laptop]
key = "ed25519:<laptop's public key>"
```

**laptop** `~/.config/veles/.env`:

```
A2A_BEARER_TOKEN=<the laptop's own inbound token>
A2A_PEER_TOKENS=phone:<phone's token>
VELES_DESKTOP_HOST=0.0.0.0
VELES_CHALLENGE_URL=http://<laptop-wg-addr>:8642
```

**phone** `files/home/config.toml` — the same shape, mirrored, plus the
tool homes (this is the half that makes the phone useful: web and
browser calls leave for the laptop):

```toml
[node]
endpoint = "http://<phone-wg-addr>:9900"

[a2a_agents.laptop]
url = "http://<laptop-wg-addr>:9900"
# The phone presents ITS OWN A2A_BEARER_TOKEN when it dials the laptop.
auth = { type = "bearer", token = "<phone's A2A_BEARER_TOKEN>" }

[platforms.a2a]
enabled = true

# This node's own bind address — node-local, configsync never ships it.
[platforms.a2a.extra]
host = "<phone-wg-addr>"

[cluster]
arbiter = "<laptop node id>"
gossip_peers = ["laptop"]
memsync_peers = ["laptop"]
task_peers = ["laptop"]
storesync_peers = ["laptop"]
storesync_auto = true
failover = true

[tools.home]
"web_*" = "laptop"
"browser_*" = "laptop"

[cluster.ticket_signers.laptop]
key = "ed25519:<laptop's public key>"
```

**phone** `files/home/.env`:

```
A2A_BEARER_TOKEN=<the phone's own inbound token>
A2A_PEER_TOKENS=laptop:<laptop's token>
DESKTOP_BEARER_TOKEN=<seeded so the API does not mint one over this file>
```

**Where each listener binds.** Every listener binds 127.0.0.1 unless
its host is named, and a host is honoured only with a token in place
(`resolve_bind_host`): `[platforms.a2a.extra] host` for the A2A port (or
`A2A_HOST` in the environment or `.env`, which beats it), `VELES_DESKTOP_HOST`
for the desktop API. The rehearsal never sets them because its adb
tunnels land on loopback; over WireGuard each side's A2A port must
listen on its WireGuard address, or the other cannot dial it. That
address may not exist yet when the gateway starts (a phone booting
before its VPN): a specific IPv4 address is bound with `IP_FREEBIND`, so
the listener is in place at once and takes connections as soon as
WireGuard is up. Prefer it to `0.0.0.0`, which also puts the port on
the mobile network. Leave the phone's desktop API on loopback: its own
desktop and its notifications reach it at 127.0.0.1.

**The challenge relay page** (`VELES_CHALLENGE_URL`) is served by the
gateway whose browser met the challenge, and with `[tools.home]` above
that is the laptop. So the laptop's desktop API listens beyond loopback
(`VELES_DESKTOP_HOST=0.0.0.0`; it keeps its bearer token, and the page
needs only the random token in its link), and the link names the
laptop's WireGuard address. Both are read from `.env` as well as the
environment, which on the phone is the only channel there is
(Engine.java's environment is a fixed list).

### The commands, on the real phone

```sh
# 1. WireGuard up on both; the phone can reach the laptop and back.
# 2. The laptop's key, so both sides trust the same signer:
printf '<passphrase>\n<passphrase>\n' | veles ticket keygen laptop
#    it prints ed25519:<pub>; that string goes in BOTH configs'
#    [cluster.ticket_signers.laptop], and its key dir stays beside the
#    laptop's config dir (never inside a gateway home).

# 3. Install the app (the debug APK — run-as needs it):
nix run .#android-install

# 4. Push the phone's config pair:
#    The quotes are for the PHONE's shell: adb joins its arguments
#    with spaces, so the redirect must reach the device inside them.
adb shell run-as chat.janitor.veles mkdir -p files/home
adb shell "run-as chat.janitor.veles sh -c 'cat > files/home/config.toml'" < phone-config.toml
adb shell "run-as chat.janitor.veles sh -c 'cat > files/home/.env'" < phone.env
#    (write node.toml first if you want the id fixed ahead of the boot,
#     as the rehearsal does; otherwise the first boot mints one.)

# 5. Start it: "Veles gateway" in the launcher, Start (Stop and Start
#    again if it was already running, so it reads the new files). The
#    service is not exported, so a phone's adb cannot start it; the
#    rehearsal can only because the emulator's adb runs as root.
#    Then check both registries name both nodes:
veles nodes list
#    The engine is an executable named lib*.so, so a CLI call on the
#    phone runs that file. Its directory is random per install — read it
#    out of the running engine's own environment (VELES_TCLSH is beside
#    it). This is exactly what the rehearsal's phome helper does:
#    (run-as: only the app may read its process's environment.)
NATIVE=$(adb exec-out run-as chat.janitor.veles cat /proc/$(adb shell pidof libveles.so)/environ \
         | tr '\0' '\n' | sed -n 's|^VELES_TCLSH=||p' | xargs dirname)
adb shell run-as chat.janitor.veles sh -c "'cd files/home && HOME=\$PWD \
    VELES_HOME=\$PWD LD_LIBRARY_PATH=$NATIVE NO_COLOR=1 \
    $NATIVE/libveles.so nodes list'"
```

### Three nodes (since 2026-10-02)

The real phone is now one of three: the laptop, the phone, and the VPS,
which holds the arbiter, Telegram, IRC and cron (deploy/README.md). The
phone's config pair gains, beside the above:

- `[a2a_agents.vps]` (url `http://10.0.30.1:9900`) and `vps` in every
  `*_peers` list; `arbiter` names the VPS's id;
- one token per direction: `[a2a_agents.X].auth.token` is what the
  phone presents to X, and its `A2A_PEER_TOKENS=laptop:…,vps:…` is what
  each of them presents to it;
- `sandbox_home = "<laptop id>"`: the phone has no podman, so its
  sandboxed calls run in a box on the laptop (a set `sandbox_home` wins;
  with none, and the userland answering, they run in the userland's own
  guests, the phone's default and a weaker fence — docs/phone.md);
- `memsync_auto = true`, and `signers = ["http://10.0.30.1:9911"]` with
  `VELES_SIGNER_TOKEN` in `.env`: the headless signer on the VPS;
- `[tools.home] "web_*"` as an order, `["<laptop id>", "<vps id>"]`.

docs/pool.md has the whole shape, and how to move the arbiter (one
window, then one more restart of the laptop and the phone).

### What differs on the emulator

Everything above, except the addresses and the binds: the rehearsal
uses `adb forward` (laptop → phone) and `adb reverse` (phone → laptop)
instead of WireGuard, so `127.0.0.1` works in both directions and every
listener stays loopback-bound. On a real phone those two become the two
WireGuard addresses, the A2A listeners bind them
(`[platforms.a2a.extra] host`, above),
and the ports are the same.
