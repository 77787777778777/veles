# Apps — small programs the agent writes for you

Ask for one, and the agent writes it: "make me an app that counts the
eggs", "a timer for the bread", "a list of what to buy that I can tick
off". It runs in the veles window, on the phone or the laptop, beside the
chat.

## What an app is

A folder in the data directory, `apps/<name>/` (on a phone,
`files/home/apps/`):

```text
apps/eggs/app.toml   title, description, entry, icon, capabilities
apps/eggs/main.tcl   the script (Tk)
apps/eggs/icon.png   optional
```

```toml
title = "Eggs"
description = "How many eggs are left"
capabilities = ["store", "notify"]
```

The agent makes one with its `app` tool (`new`). It writes the code with
the same tool (`write` for a whole file, `edit` to change one place),
runs it on a window (`run`), reads its errors (`logs`), looks at it
(`screenshot`, on a phone), and changes it until it works.

The `app` tool writes only inside the app's own folder, one folder deep
at most, text files only, never through a symlink, and checks an
`app.toml` before it lands. It is the owner's tool. The file tools would
ask the owner at every write, because the data directory is veles' own
(on a phone it is the agent's whole home).

## Where it runs, and what it can reach

An app runs in a **safe interpreter with Tk**. It has its own widgets and
nothing else of the window's: no `exec`, no `open`, no `socket`, no
`load`, no files, and no `wm`: the app draws in the space the window
gives it (on a phone, safe Tk has no `wm` at all). A loop that never
returns to the event loop for two seconds is stopped, and the window
keeps running.

What it can do beyond its widgets goes through `veles::` commands, and
each is checked against what its `app.toml` **declared** and the person
**allowed**:

| capability | the command |
|---|---|
| `notify` | `veles::notify ?title? text` |
| `toast` | `veles::toast text` |
| `vibrate` | `veles::vibrate ms` |
| `speak` | `veles::speak text` |
| `open` | `veles::open uri` (https, geo, tel, mailto, sms, market) |
| `share` | `veles::share text` |
| `clipboard` | `veles::clipboard ?text?` |
| `location` | `veles::location` |
| `store` | `veles::store get\|set\|delete\|keys key ?value?` — the app's own data, kept when its code changes |
| `ask` | `veles::ask text callback` — a turn with the agent; the reply comes to `callback reply` |
| `http:<host>` | `veles::http url callback` — only from that host |

`veles::log text` (its log, which the agent reads) and `veles::app` (its
name and title) need nothing.

The first time an app runs, the person is asked: "Let Eggs: post
notifications, keep its own data". Their yes covers that declaration. A
changed one, such as an app that now wants the location, asks again.
Changed code under the same declaration does not. An app that asks the
agent does so as itself, `app:<name>` in its own session, with a User's
rights unless the owner names it higher (`[acl]`). It never gets the
owner's whole hand. The gateway refuses a store or an ask to an app that
did not declare it, whatever the window allowed.

## The launcher

⋮ → Agent → **Apps** (F10 on a keyboard) shows them as a grid. A tap
runs one. Touch and hold for Run, Stop, *Ask the agent to change it*
(puts the request in the composer), and on a phone *Pin to home
screen*: Android asks, and the icon it makes opens that app directly.
