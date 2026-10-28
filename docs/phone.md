# Setting veles up on a phone

Typing TOML on a touch keyboard is nobody's idea of setup. On a phone,
veles is configured two ways: by asking the agent, and on the Phone page.

## By asking: the `config` tool

Say what you want ("compact the conversation sooner", "turn the Telegram
bot off") and the agent uses its `config` tool (deferred, found through
`tool_search`):

- `get {path}` shows a key or a whole table, its default, and when a
  change takes effect: at once, for new sessions, or after a gateway
  restart.
- `set {path, value}` and `unset {path}` change one key. **You are asked
  first**, every time, with the key, its value now and the value it would
  get. Nothing is written until you allow it, and a question you do not
  answer changes nothing.

The write goes through the running gateway, so its live settings reload
with it. Your comments and key order in `config.toml` survive, the old
file is kept as a backup, and a change that would leave the file
unreadable is refused.

What the agent will not do here:

- **Credentials.** A key named like one (`token`, `api_key`, `password`,
  `secret`) or a value shaped like one is refused. The agent asks for it
  with `secret_request` instead: you type it privately and it goes to the
  vault, never through the chat. `get` shows a credential as set, not its
  value.
- **Who may reach the agent, and what runs unasked.** `acl`, `tool_caps`,
  `approvals`, `sandbox`, `cluster`, `node`, `pool`, `vault`, `hooks`,
  `mcp_servers`, `platforms.a2a` and `platforms.desktop` can be read but
  not written by the agent. One tap on "allow" is too thin a gate for
  them: a page the agent read could talk it into asking. Change these
  yourself, in the desktop's settings or in `config.toml`.

The tool is the owner's alone (ACL class `All`): in someone else's chat,
the person answering "allow" would not be you.

## By touch: All settings

⋮ → Settings → **All settings** (on a laptop too) lists every option this
veles reads, one section at a time, with a search across all of them. Each
option gets the control its kind wants: a switch, a number, a text, a
comma-separated list. Its default is named under it, and ↺ puts the
default back (removes the key). A change is written when you leave the
field or press Enter, through the same route as above, and the page says
whether it is in effect now, applies to new sessions, or needs a gateway
restart.

The form comes from the gateway (`GET /api/config/form`), built from the
same structs that read `config.toml`, so a new option appears on its own.
Each option's kind is read off its default by the TOML library. An option
with no default that you never set has no kind to read: what you type is
sent as `raw`, and the gateway reads it as a TOML literal (`5` stays a
number). Arrays of tables (`[[custom_providers]]`) and credentials are not
on the page.

## By touch: the Phone page

On a phone, ⋮ → Settings → **Phone: permissions, battery, storage**. One
row per thing that decides whether veles works on this phone, each with
the button that changes it in Android's own settings:

| row | what it tells you | the button |
|---|---|---|
| Notifications | whether Android shows veles' notifications | the app's notification settings |
| Running in the background | whether Android lets veles run when you are not looking | Android's one-tap "allow" dialog (the app's page on a phone without it) |
| Reading notifications, for triggers | whether veles may read other apps' notifications | Android's notification access switch for veles |
| Start with the phone | whether the engine starts when the phone does | turns it on or off (veles' own switch) |
| Using the phone, for the agent | whether the agent may use the phone (the sideload build only) | Android's accessibility switch for veles |
| Location | precise, approximate or off | asks for it; the app's page once it is precise |
| Network | Wi-Fi, mobile (and which), or offline | — |
| Free space | free and total storage | — |
| Engine | when the engine started, or that it is not running | — |

Below the rows: what the agent can do with this phone right now (the
`device` actions, see [devices.md](devices.md)), and a button to open
veles' own page in Android's settings.

Some makers stop apps beyond Android's own battery rules: Xiaomi, Redmi,
POCO, Tecno, Infinix, itel, OPPO, realme, OnePlus, vivo, Huawei, Honor,
Meizu, ASUS, Lenovo, Nokia. On their phones the page says so, and what to
allow in veles' app settings (starting by itself, running in the
background).

When you come back from Android's settings, the page redraws by itself:
the app writes what only Android can tell it (`files/phone-status`, in
the app's private storage) every time veles comes back to the screen.

## Joining the pool from the phone

The Phone page's **Join a pool** reads an invite from the machine that
holds the pool's arbiter (`veles pool invite <name>` there, see
[pool.md](pool.md)). When the hub mints WireGuard tunnels, it shows two
codes: scan the first with the WireGuard app, then the second here. The
camera takes a picture, veles reads the code, and the page says which
pool it would join, under what name, and where the pool will reach the
phone. Nothing changes until you tap Join. Without a camera, paste the
code's text instead. After joining, close veles and open it again, so its
A2A server starts and the pool can reach the phone.

## Reacting to the phone

The engine on a phone stops five minutes after anyone last used it (the
battery is the point). Three things start it again besides opening the
app:

- **A notification a trigger listens to.** With Reading notifications
  on, veles reads what other apps post. A trigger on a phone's
  notification runs a turn of its own:

  ```text
  /triggers add mom event=NOTIFICATION channel=com.whatsapp match=contains pattern=Mom action=prompt arg="Tell me what Mom wants, in one line."
  ```

  `channel` is the app's package (a glob: `com.bank.*`), `pattern` is
  matched against the notification's title and text. The turn runs in
  the trigger's own session (titled "Trigger: mom"), and its answer comes
  back as a veles notification. It runs as `trigger:mom`, a User: a
  notification is other people's words, and the message is quoted to the
  agent as data under your instruction. Name `trigger:mom` in `[acl]` to
  give it more. A trigger without an hourly cap runs at most 30 turns an
  hour; `cooldown=` and `cap=` set your own.
- **The next cron job.** When the engine stops, it leaves `wake.json` in
  its home (the packages a trigger listens to, the next cron job's time),
  and the app sets an alarm for that time. With "Alarms & reminders"
  allowed in Android's settings the alarm is on time; without, Android
  may run it a few minutes late (grant it once:
  `adb shell appops set chat.janitor.veles SCHEDULE_EXACT_ALARM allow`,
  or Settings → Apps → Special app access → Alarms & reminders). A phone
  in a pool fires its own jobs this way too (since 2026-10-21): a
  gateway that stops when idle never asks for the pool's cron lease — a
  node that stays up holds that — but cron jobs are per node and nothing
  else fires the ones made on the phone, so it fires them itself and the
  alarm starts it for the next. Before, such a phone stood by for nothing
  and its jobs never ran. Measured 2026-10-21: the engine stopped at
  10:57, the alarm started a new one at 11:00:00, and the job ran within
  a second; the next two alarms, each set by the engine on its way out,
  fired at 11:10 and 12:10. Through a night off USB (2026-10-21/22) the phone sat in deep doze for twelve hours and the hourly job fired every hour, within a second, without taking the phone out of doze. Removing the last job leaves no alarm; removed while the engine is stopped, the alarm it already set fires once more, starts an engine that finds nothing, and that engine stops five minutes later with nothing armed. A `.tcl` job on the phone cannot `clock format` yet (the
  bundled runtime lacks msgcat); `clock seconds` works.
- **The phone starting**, when Start with the phone is on.

A notification from an app no trigger listens to starts nothing. While
the engine runs, every notification also reaches plugins (the
`device_event` hook, [extending.md](extending.md)) and the `power`
events (plugged in, unplugged) reach triggers on `event=POWER`
(`channel=connected` or `disconnected`).

Android starts the engine from the background only when veles may run
in the background (the Phone page's row above). If it refuses, veles
says so in a notification, at most once an hour.

The agent's `device` tool also gets two actions from the app's service,
which work with the window closed: `notifications` (what other apps
notified lately, newest first, only with Reading notifications on) and
`battery`. `files/idle-secs` (a number of seconds, 10 to 86400) changes
how long the engine runs unused before it stops.

## Letting the agent use the phone

In the sideload build (not the one from a store), veles carries an
accessibility driver: with it on, the
agent's `computer_use` tool sees what is on the screen and taps, types,
scrolls and presses Back or Home, in any app. It is off until you turn
it on, on the Phone page (Using the phone, for the agent), in Android's
accessibility settings. If Android says the setting is restricted (it
does for an app installed from a file), open veles' app page, then ⋮ →
Allow restricted settings, and try again.

- Only the owner's agent can use it: on a phone `computer_use` refuses
  every session but the owner's, and the driver answers only veles on
  the phone, with the gateway's own token.
- While the agent is using the phone, a notification says so. Its Stop
  turns the driver off; turn it on again in the same place.
- `capture` lists what can be acted on, numbered, and saves a screenshot
  with the numbers drawn on it (for `vision_analyze`); `click`, `type`,
  `scroll` and `drag` take a number or a screen position; `key` is back,
  home, recents, notifications, quick_settings or enter; `list_apps` and
  `focus_app` open an app by its name.
- The driver is declared an accessibility tool, so Android shows it the
  controls it protects from other apps too: Airplane mode in Settings,
  for one. That is why it is off until you turn it on, and why only your
  own agent may use it. If Android ever keeps a screen from it, its
  `capture` says so instead of coming back empty; Back works, and
  opening the screen again usually gives one it can read.

On an arm64 phone (nearly every phone), veles also carries busybox and
git, on the terminal's path beside rg:

- busybox's commands that Android lacks (awk, vi, less and more), after
  Android's own;
- git, for local repositories (plugin rollback, the Review pane) and over
  HTTPS (git installs). git is built without Android's libc and cannot
  look up names there, so the engine resolves them for it through a
  proxy of its own on the phone's loopback, which only veles' git can use.

The x86_64 build (the emulator) has neither: there Android stops these
programs as they start.

## The userland package

*Since 2026-10-20 the agent's shell, background shells, monitors and
`.md` commands run in Alpine on the phone, and so do the phone's
sandboxes (tested on an SM-S942B, Android 16).*

Android lets an app that targets a recent Android run only the programs
it shipped, never ones it downloaded or unpacked, so veles itself can
have no package manager. The userland is a second app,
**Veles userland** (`chat.janitor.veles.userland`), built for an older
Android target that may still run what it unpacked: it carries an
Alpine Linux and runs programs in it for veles, through proot, so
`apk add` works on the phone. veles stays the app it was; when the
userland answers, the agent's shell commands run in Alpine instead of
in Android's shell.

- **Two APKs, one key.** Both come from this flake and are signed with
  the same key (`nix run .#android-install`,
  `nix run .#android-install-userland`): the release key when
  `~/.config/veles-android/release.p12` exists, this machine's sideload
  key otherwise (`scripts/android-install.sh` says which). The userland serves only a
  veles signed like itself, and veles uses only a userland signed like
  itself.
- **Order.** Install the userland FIRST, then veles (or, if veles is
  already there, install veles again after the userland): veles asks for
  a permission the userland defines (`…userland.permission.OPEN_SESSION`,
  signature level), and Android grants it only when the userland is
  already installed. After that nothing needs opening: when veles' engine
  starts it binds the userland, which starts it — the first start unpacks
  Alpine (about 9 MB, under a tenth of a second on the SM-S942B).
  Opening Veles userland shows its state and log, with Start and Stop.
- **How the two talk.** Android does not let one app connect to the
  other's socket, so every command's connection comes through Android's
  Binder: veles asks the userland's service for a session, and the
  service hands one end of a fresh socket pair to the userland's broker
  and the other to veles, which passes it to the `ulsh` running the
  command.
- **The switch.** veles uses the userland whenever it is installed,
  signed alike, and a test command runs there when the engine starts;
  the gateway's log then says "the shell is the userland's (ulsh through
  @veles-userland-local, broker uid …; probe … ms)", and when the test
  fails it says what ulsh was refused instead ("the userland package
  does not run commands: …"). To go back to Android's shell, stop the
  userland (its notification's or its screen's Stop) and restart the
  gateway: a Stop holds — veles does not start a stopped userland — until
  Start, or until Veles userland is opened again. (The on-screen switch,
  `Prefs.useUserland`, comes when its translations do.)
- **What the agent sees.** Its commands run as root in Alpine (proot's
  fake root), in `/root`; the userland's `files/shared` is `/shared`.
  Alpine starts small — `apk add bash git` first; a command runs under
  bash once it is installed, under Alpine's sh until then. veles' own
  files are not visible inside, and the userland's are not visible to
  veles. The model is told so: once the userland answers, the prompt's
  platform line names the shell `ulsh (Alpine)`, no longer lists a
  package manager or `/usr/bin/env` as missing, and says that `apk add`
  installs what is missing, that the guest's home is `/root`, and that
  read_file, write_file and search_files work on the app's own files,
  which the guest cannot see. The node's advertised tools (`veles
  nodes`) gain what the rootfs has (`apk`, and `bash` once installed),
  probed through ulsh once at the engine's start.

### Sandboxes in the userland: `[sandbox] backend = "userland"`

Without a podman the phone has no sandbox of its own, so its
sandbox-zone calls (a terminal or file call with `sandbox: "<name>"`, a
below-admin caller's every call, an A2A peer's by default) would go to
`[cluster] sandbox_home` or a peer that has one. With the userland
answering, the phone keeps them, and **this is the default**: on an
Android build `[sandbox] backend` reads `auto` when nobody set it, which
comes to `userland` while the userland answers (the engine's shell is
ulsh: the gateway sets that only after a probe command ran there), and
to `native` while it does not. `veles doctor --sandbox` says which it
came to and why (`backend: auto → userland (default on android, broker
answers)`). `auto` also stays `native` when the section asks for what the
userland cannot keep (`network = false`, `user_env_network = "none"`,
`[[sandbox.mounts]]`, `extra_args`), saying so, rather than refusing every sandboxed call.

To opt out, name the other backend; a named value always wins:

```toml
[sandbox]
backend = "native"
# and, to send the phone's sandboxed calls to a podman node:
# [cluster] sandbox_home = "<laptop id>"
```

`backend = "userland"` spelled out is the same boxes, but refuses to
start (instead of falling back to `native`) where the userland does not
answer. The key is node-local (configsync never replicates
`sandbox.backend`), so the phone's choice is the phone's alone. A
`[cluster] sandbox_home` that is set still sends the calls away: unset
it for the phone to keep them.

- **Each box is a guest of its own.** A box (`sandbox: "box-a"`, or a
  caller's own) is a directory in Veles userland,
  `files/sandboxes/<key>` (`box_a-f3df2148`), made the first time it is
  used from a copy of the userland's rootfs as it is then — what
  `apk add` put there comes along; nothing added later does. Its
  workspace is `/work`, which is also its HOME, as in a podman box.
  What one box writes, no other box sees, and neither does the rootfs
  the terminal runs in. A box runs `apk add` on its own (`apk add tcl`
  for execute_code inside it).
- **Not a fence like podman's.** Every box runs as the userland app's
  uid, under the same proot (a tracer, not a boundary), on the phone's
  network with its loopback, and with no memory cap. So veles never
  counts a userland box as fenced: its commands keep the approval gate,
  and the userland is not a fallback for a podman that is missing on a
  node that is not the phone (it is the phone's default only). Named
  as `backend = "userland"`, `network = false`, `user_env_network =
  "none"`, `[[sandbox.mounts]]` and `extra_args` cannot be kept there
  and are refused, not ignored.
- **The Tcl fence** (`execute_code`'s pure-compute interpreter for
  cron's `.tcl` jobs and the like) runs the userland's own tclsh in its
  rootfs, in a directory of its own per run: `apk add tcl` once from the
  terminal. `veles doctor --sandbox` on the phone says whether it does.
- **Purging a box** empties its `/work`; the box's root stays (veles
  cannot delete the other app's files). To remove one:
  `adb shell run-as chat.janitor.veles.userland rm -rf files/sandboxes/<key>`.
- **Where to look.** The userland's log says how each box was made
  ("sandbox box_a-f3df2148: made from the rootfs, 844 entries, 25889 KiB
  (0 files cloned, 213 copied, 0 hard links kept) in 230 ms"); `veles
  nodes` shows `userland` among the phone's tools while it makes the
  phone's boxes, which is what keeps sandbox-zone calls on the phone.

### Checking the userland on a phone

These checks exercise the userland on a real phone; the T3-R1 labels
name the acceptance gates it was adopted on. From the laptop, in this
checkout:

```sh
# Build both (the install apps build them too).
nix build .#veles-apk .#veles-userland-apk
unzip -l result-1/veles-userland-debug-unsigned.apk | grep -E 'lib/|assets/'
cat result-1/guards.txt

# T3-R1a: install both, same key — the userland FIRST, so veles is
# granted its permission; each prints the install result.
nix run .#android-install-userland
nix run .#android-install
adb shell dumpsys package chat.janitor.veles.userland | grep -E 'targetSdk|versionName'
adb shell dumpsys package chat.janitor.veles | grep OPEN_SESSION      # "granted=true"
# Opening Veles userland (the install opens it), or veles binding it,
# unpacks the rootfs.
adb shell run-as chat.janitor.veles.userland cat files/userland.log   # T3-R1e: "rootfs: extracted …"; "taking sessions on @veles-userland-handoff"
adb shell run-as chat.janitor.veles.userland du -sh files/rootfs      # T3-R1e

# Start the gateway (open veles; after an update the engine is not running), then:
adb shell run-as chat.janitor.veles grep -i -e userland -e ulsh files/gateway.log   # "the shell is the userland's (… probe N ms)"
adb logcat -b all -d | grep 'avc:' | grep -i -e veles -e untrusted_app             # T3-R1b: expect nothing

# The two native library directories (random per install).
GLIB=$(adb shell pm path chat.janitor.veles | sed 's|^package:||; s|/base.apk.*$|/lib/arm64|' | tr -d '\r')
ULIB=$(adb shell pm path chat.janitor.veles.userland | sed 's|^package:||; s|/base.apk.*$|/lib/arm64|' | tr -d '\r')

# T3-R1b/c from adb (run-as runs as the gateway's uid, though in
# run-as's SELinux domain, not the app's: the terminal tool below is the
# authoritative check). ulsh takes its session from the gateway's relay,
# so the engine must be running. Through a link: toybox's env reads the
# library path, which has "==" in it, as an assignment.
adb shell run-as chat.janitor.veles ln -sf "$GLIB/libulsh.so" files/ulsh-probe
adb shell run-as chat.janitor.veles env ULSH_RELAY=veles-userland-local files/ulsh-probe -c "'cat /etc/alpine-release; id -u'"
adb shell run-as chat.janitor.veles rm files/ulsh-probe
```

T3-R1b/c from the gateway itself: in a chat on the phone's own desktop
(an owner session; until the laptop↔phone tokens were rotated on
2026-10-20 at 15:09 a laptop A2A call landed as `vps`, role user, whose
session has no terminal), ask the agent to run, with the terminal tool,

    id; cat /etc/os-release; apk add jq && jq --version

(with VELES_SHELL already on ulsh it runs in Alpine as it is: `uid=0`
and Alpine 3.24.2; to name ulsh explicitly from Android's shell instead —
the engine's environment carries `ULSH_RELAY` —
`"$(dirname "$VELES_TCLSH")/libulsh.so" -c 'cat /etc/os-release; apk add jq && jq --version'`),
then `apk add git && git clone --depth 1 https://github.com/octocat/Hello-World /root/hello-world`.

T3-R1d, proot's overhead — the same busybox `find` over the same tree,
inside proot and outside it (musl's loader run by hand), twice each
(the second run is the warm one):

```sh
adb shell run-as chat.janitor.veles.userland sh -c "'cd files && time env PROOT_TMP_DIR=\$PWD/../cache/proot PROOT_LOADER=$ULIB/libproot_loader.so $ULIB/libproot.so -0 -r rootfs -b /dev -b /proc -b /sys /bin/busybox find /usr >/dev/null'"
adb shell run-as chat.janitor.veles.userland sh -c "'cd files && time rootfs/lib/ld-musl-aarch64.so.1 rootfs/bin/busybox find rootfs/usr >/dev/null'"
# If run-as's domain may not execute the rootfs's files, Android's own
# find over the same tree is the outside number instead:
adb shell run-as chat.janitor.veles.userland sh -c "'cd files && time find rootfs/usr >/dev/null'"
```

T3-R1f, the phantom-process killer, overnight. Unplug the phone (on USB
with "stay awake" it never dozes). At night ask the agent to start
`sleep 36000` as a background shell. The engine stops five idle minutes
later, but the shell does not go with it: its `libulsh.so` is left
running under init (PPID 1) and the guest's `sleep` under the broker,
which is what the morning looks for — the orphan under veles' uid, the
`sleep` under the userland's (`u0_a…`), proot as `libproot.so`:

```sh
adb shell ps -A -o USER,PID,PPID,ETIME,ARGS | grep -E 'libveles_userland|libproot|libulsh|sleep'
adb logcat -b events -d | grep -iE 'phantom|am_kill.*userland'
adb shell settings get global settings_enable_monitor_phantom_procs
```

and, only if the sleep was killed, once:
`adb shell settings put global settings_enable_monitor_phantom_procs false`
(and Veles userland's battery setting to Unrestricted), then repeat.

Measured 2026-10-21: all three alive after nine hours off USB, and the
`sleep 36000` still running eleven hours after its start. The guest's
`sleep` counts awake time only (the phone's suspend stops its clock),
so give it more hours than the night has, and read it before it can
end: a run that ends before the morning proves nothing either way
(pitfall 89).

T3-R1g: with the gateway on ulsh, the phone COVERAGE rows — terminal,
a background shell, a monitor, a `.md` command — and a long command
stopped from the desktop (the guest's process must go:
`adb shell ps -A | grep proot` afterwards). A foreground `sleep` is
refused by the agent's guard (`[agent.guards] foreground_sleep_max_secs`);
`tail -f /dev/null` is the long command that is not one.

### Checking the boxes (T3 design items 4 and 5)

With `backend = "userland"` and no `sandbox_home`, a tool call from the
phone's own desktop needs no model: the desktop API
(`adb forward tcp:18642 tcp:8642`, bearer `DESKTOP_BEARER_TOKEN` from
`files/home/.env`) makes a session (`POST /api/sessions/new`), a command
in it makes its agent (`POST /api/command` with `/sandbox list`), and
the gateway's interpreter calls a tool as that session:

    POST /api/tcl/eval {"code": "veles::tool_call terminal {{\"command\": \"echo a > /work/a.txt; echo $VELES_SANDBOX\", \"sandbox\": \"box-a\"}} <session>"}

Then the same with `"sandbox": "box-b"` and `test -e /work/a.txt`, and
with `"host": true` (the rootfs) — the file is in box-a only — and on
the userland's side
`adb shell run-as chat.janitor.veles.userland ls files/sandboxes` and
`… grep sandbox files/userland.log` (how long each box took to make).
`veles doctor --sandbox` needs the engine's environment to reach ulsh:
run it with `run-as chat.janitor.veles` from a small `sh` script that
exports what Engine.java does (HOME, VELES_HOME, PATH, LD_LIBRARY_PATH,
VELES_SHELL and SHELL = the gateway's `libulsh.so`, `ULSH_RELAY=
veles-userland-local`, `ULSH_BROKER_UID`) and execs `libveles.so doctor
--sandbox` — assignments in the script, since toybox `env` misreads the
library path. The prompt's platform line is the model's to quote: ask
for it verbatim in a turn.

A background shell's completion wakes its session: one started this
way starts a turn when it ends, so check with a session you do not mind
the agent continuing in, or cancel it (`POST /api/sessions/<id>/cancel`).
