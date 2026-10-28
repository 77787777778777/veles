# Devices — what the phone and the desktop can do for the agent

A veles window — the desktop on a laptop, or veles open on a phone — tells
the gateway it is attached to what it can do: post a notification, speak
aloud, open a link, share text, say where the phone is. The agent reaches
those through one tool, `device` (deferred, so it finds it through
`tool_search`):

- `action=list` shows every connected device and its actions, with their
  arguments.
- `action=<name>`, `args={…}` runs one. `device=<name>` picks the device
  when more than one can do it (a notification on the laptop is not one on
  the phone).

From a shell, the same tool:

```sh
veles tool --name device --args '{"action":"list"}'
veles tool --name device --args '{"action":"speak","device":"phone","args":{"text":"hello"}}'
```

What the phone itself allows veles (notifications, running in the
background, location) is on the Phone page: see [phone.md](phone.md).

## What each device offers

**A phone** (veles open on it, through AndroWish's `borg`):

| action | what it does |
|---|---|
| `notify` | posts a notification (`title`, `text`) |
| `toast` | shows a short message on the screen |
| `vibrate` | vibrates for `ms` milliseconds (1–5000) |
| `speak` | reads `text` aloud, in `lang` when given |
| `open` | opens an `https:`, `geo:` (map), `tel:` (the dialer, not a call), `mailto:`, `sms:` or `market:` link |
| `share` | opens the share sheet with `text` |
| `info` | the phone's model, Android version, network and screen |
| `location` | where the phone is, waiting up to `wait` seconds for a fix |

The phone's gateway service (Java, alive while the engine runs, the window
open or not) adds two, and the agent sees them under the same `phone`:

| action | what it does |
|---|---|
| `notifications` | what other apps notified lately, newest first (`limit`, 1–50, default 10): app, title, text, time; only when the person turned on Reading notifications on the Phone page |
| `battery` | the level in percent, whether it is charging, and how it is plugged in |

**A desktop window** (a laptop): `notify` (through `notify-send`), `open`
(http, https and mailto links), `clipboard` (read it, or set it with
`text`), `info`.

Actions marked *foreground* need the window on the screen: a phone freezes
an app in the background, and a call to it then answers that the phone did
not reply. The rest answer while veles is open.

## Rules

- **Owner only.** The tool is classified `All`: a phone's location, its
  notifications and a laptop's clipboard are the owner's. `[tool_caps]
  device = "<cap>"` can widen it; nothing does by default.
- **From another node**, re-home the tool to the node the device is on
  (`[tools.home] device = "phone"`): the call then runs there, under the
  pool's ticket rules — a peer is a User, and `device` is owner-only, so
  it needs the owner's ticket. No test drives this path yet. Session mail
  from the laptop does not get around it: a mailed turn on the phone runs
  at what the laptop holds there (a User, unless the owner names
  `a2a:laptop` higher in the phone's ACL), and a User cannot use `device`.
- **A window that goes away** is listed as *not connected now*, and a call
  to it is refused at once rather than after a timeout. A window
  reconnects by itself when its gateway comes back (a phone's engine stops
  after five minutes without use and starts when veles is opened again);
  keeping that connection open is not *use*, so it does not keep the phone
  awake.

## How it works

The window opens `GET /api/device/events?client_id=…` on its gateway's
desktop API and, when the stream says it is live, posts its catalog to
`/api/device/hello`: each action a name, a description and a JSON schema
of its arguments. A call (`POST /api/device/call`, from the tool in
whichever process its session runs) is pushed down that one window's
stream as a `request`; the window runs the provider and posts the answer
to `/api/device/result`. A provider is one Tcl proc registered with
`::vdevice::provide name description schema proc ?-foreground 1?
?-timeout secs?` (veles-desktop/lib/device.tcl); the phone's are in
device_android.tcl, the laptop's in device_desktop.tcl.

The phone's background capabilities — reading incoming notifications,
alarms that wake the engine, SMS — need the gateway's own service process
on the phone, in Java, and come in a later round.
