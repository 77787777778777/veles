---
name: windows-node
description: Working on a native Windows node (not WSL) — the shell is PowerShell, long jobs go through the Task Scheduler, there is no systemctl/apt/xdg-open; what to use instead.
platforms: [windows]
---

# Working on a Windows node

This node runs veles natively on Windows (the desktop, not WSL). Read this
before reaching for a Linux habit: most of them fail here with an error
that looks like a permissions problem and is not.

## The shell is PowerShell

The `terminal` tool runs **pwsh** (PowerShell 7; Windows PowerShell 5.1
or `cmd` when pwsh is absent — the tool card names which). So:

- `ls`, `cat`, `rm`, `cp`, `mv`, `pwd`, `echo` work as PowerShell
  aliases, but their *flags* are PowerShell's: `ls -la` is an error.
  Use `Get-ChildItem -Force`, `Get-Content -Tail 20`, `Remove-Item -Recurse`.
- No `grep`/`sed`/`awk`/`which` unless installed: use `Select-String`
  (`rg` is usually installed and is simpler), `-replace`, `Get-Command`.
- Pipelines carry OBJECTS, not text. `| Select-Object -First 5`, not `| head`.
- `$env:NAME` reads an environment variable; `$env:NAME = "x"` sets it
  for this process only. `setx` persists one (new processes only).
- Quoting: single quotes are literal, double quotes expand `$vars`.
  `&` runs a command named by a string: `& "C:\Program Files\x\x.exe"`.
- `cmd /c "<command>"` for something that is really a cmd.exe idiom
  (`dir`, `start`, batch files). `start "" <url-or-file>` opens it with
  the default app — or use the `device` tool's `open`.
- An exit code: `$LASTEXITCODE` after a native command; `$?` is a bool.
- Paths use `\`, but `/` works in nearly every API. Drive letters:
  `C:\`. The user's home is `$env:USERPROFILE`, temp is `$env:TEMP`.

## Where veles keeps things

| what | where |
| --- | --- |
| config, `.env`, `node.toml`, vault | `$env:APPDATA\veles` |
| state (`state.db`, modules, `gateway.log`, `run\`) | `$env:LOCALAPPDATA\veles` |
| cache | `$env:LOCALAPPDATA\veles\cache` |
| the binaries | `veles.exe`, `veles-gw.exe`, `veles_runtime.dll`, `sqlite3.dll`, Tcl beside them |

`veles gateway status | stop | restart` work; the gateway is a **logon
task** (Task Scheduler, `veles-gateway`), not a service — `schtasks
/Query /TN veles-gateway`. A stop is graceful (over the local pipe),
there is no SIGTERM.

## What is NOT here (and what to use)

| Linux habit | on this node |
| --- | --- |
| `systemctl`, `journalctl` | `Get-Service`, `Get-EventLog`/`Get-WinEvent`; veles' own log is `gateway.log` |
| `apt`, `pip` as root | `winget install <id>` (user scope, no admin needed for most packages); `pip --user` |
| `sudo` | there is none; an elevated action needs a UAC prompt the user must click |
| `xdg-open`, `notify-send` | `start "" <x>` / the `device` tool (`open`, `notify` = a toast) |
| `nohup … &`, `setsid` | the Task Scheduler (below); a plain background job dies with the shell |
| `ps aux`, `kill -9` | `Get-Process`, `Stop-Process -Id` / `taskkill /F /PID` |
| `/proc`, `lsof` | `Get-Process`, `Get-NetTCPConnection -State Listen`, `handle.exe` (Sysinternals) |
| podman boxes, the Xvfb browser | none: the Tcl fence is native; the browser draws on the desktop |
| symlinks | need Developer Mode or admin; prefer a copy or a junction (`New-Item -ItemType Junction`) |
| `chmod`/`chown` | `icacls`; a file made by the gateway is private to the user by ACL already |

## Long-running work: the Task Scheduler

A process started from a terminal call is killed with the call. For
anything longer than one call (a build, a download, a server), register
a task and poll its log:

```powershell
schtasks /Create /TN veles-job /SC ONCE /ST 23:59 /IT /F /TR 'pwsh -NoProfile -File C:\path\job.ps1'
schtasks /Run /TN veles-job
Get-Content C:\path\job.log -Tail 20          # poll
schtasks /End /TN veles-job; schtasks /Delete /TN veles-job /F
```

`/IT` runs it in the interactive session (the desktop, drives and the
user's PATH). Have the script write its own log with `-Encoding utf8`
(`Out-File` defaults to UTF-16, which `rg`/`findstr` do not read).

## Batch-file traps

- A `.bat` needs **CRLF** line endings; quotes inside `cmd /c "…"`
  passed over ssh get mangled — write a file and run the file.
- `if exist X (…) & …`: everything after an `if` is its body. One
  statement per line.
- `%ERRORLEVEL%` is read when the LINE is parsed: `cmd & echo %ERRORLEVEL%`
  prints the previous value. Use a second line.

## Encoding and line endings

- Console output may be the OEM code page: set `[Console]::OutputEncoding = [Text.Encoding]::UTF8`
  when non-ASCII matters, or `chcp 65001` in cmd.
- Git on Windows may check out CRLF; veles' repo pins `eol=lf` in
  `.gitattributes`. A file hashed for a build key must not drift.
