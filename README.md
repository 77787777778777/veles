# veles

veles is a personal AI agent. You talk to it from a terminal, a desktop
window, Telegram or IRC; it runs tools, edits files, remembers things
between sessions, schedules work, and can delegate to copies of itself.
The engine is Rust. The extension surface is Tcl: plugins hot-patch into
a running session, and the desktop app is itself a live Tcl/Tk program.

It works with any OpenAI-compatible provider: OpenRouter, OpenAI,
Anthropic, Google Gemini, DeepSeek, a local Ollama, or your own server.

**Documentation:** <https://veles-docs.7778777.online/>

## Install

### Linux (x86_64; arm64 where a build is published)

```sh
curl -fsSL https://raw.githubusercontent.com/77787777778777/veles/master/scripts/install/install.sh | sh
```

The installer downloads the latest release, checks it against the
release's `SHA256SUMS`, verifies the Sigstore signature of that file when
`cosign` is installed (`--require-signature` makes that mandatory), and
installs into your home directory. Nothing needs a compiler, a system Tcl
or root. Running it again upgrades in place and keeps your configuration.
The release also carries an AppImage that runs without installing.

Prefer to look first? Download `install.sh` from the
[latest release](https://github.com/77787777778777/veles/releases/latest),
read it, then run it. Every release asset is listed in `SHA256SUMS`.

### Windows 10/11

In PowerShell:

```powershell
irm https://raw.githubusercontent.com/77787777778777/veles/master/scripts/install/install.ps1 | iex
```

It does the same checks as the Linux installer and puts `veles` on your
user PATH. Or download the Windows zip from the
[latest release](https://github.com/77787777778777/veles/releases/latest),
unpack it anywhere and run `veles.exe`. Native Windows, no WSL needed.

### Android

The release carries a matched pair of APKs, signed with the same key: the
veles app and its userland (a small Alpine Linux the agent's shell
commands run in). Install the userland first, then veles; see
[the Android guide](https://veles-docs.7778777.online/en/android/).

### NixOS / Nix

The flake provides `packages.<system>.default` and a NixOS module
(`services.veles`). See [docs/install.md](docs/install.md).

### Servers and multi-node setups

`veles install profile` sets up a server (system service), a hardened
container host, or emits a NixOS module, with optional components (your
own Bluesky PDS, media publishing, a WireGuard pool). Every privileged
step is shown for review before it runs. See [docs/install.md](docs/install.md).

## First run

```sh
veles setup     # pick a provider, paste its key, choose a model
veles           # chat in the terminal
veles gui       # or the desktop app (its setup wizard opens on first run)
```

Keys go into an encrypted vault, never into `config.toml`; the agent can
use a provider key in commands without ever seeing it. `veles doctor`
checks the whole install, and `veles doctor --sandbox` proves the
command sandbox works on this machine.

## What it can do

- **Tools**: terminal, file read/write/patch/search, web search and
  extraction, browser automation, image and audio generation, vision;
  about fifty, most loaded on demand.
- **Memory and skills**: notes that persist across sessions with every
  revision kept, and markdown skills the agent can write for itself.
- **Scheduling**: cron jobs, channel triggers, and monitors that wake a
  session when a condition holds.
- **Delegation**: subagents in their own sessions, across a pool of
  machines if you have more than one. The `sessions` tool lists and mails
  other live sessions; `nodes` shows compute names, capabilities, load
  and which nodes this one can actually reach before you choose a
  placement. `delegate_task backend='any'` counts the children it has
  already placed on each node until they finish, so a batch or a run of
  background children spreads over the pool instead of piling onto one
  machine (CPU load only: disk IO is not measured).
- **Joining and exporting**: `/join a` finds the live conversation for `a`
  without requiring its session id. Joining needs the `session-join`
  capability: the owner has it, and anyone else only by a grant. A
  grantee can join only another person's session in the channel they are
  typing in (never a DM), and their lines there run with the lower of
  their rights and that session's. `/export [md|json]` (desktop, TUI and
  chat) writes the whole transcript, tool calls and results included, as
  a new file (mode 0600 on unix) under the data directory's `exports/`,
  outside the media directory a guest's attachments may name. On a chat
  connector the file stays on the gateway host.
- **Restart recovery**: a desktop turn cut off by a gateway stop, crash
  or reboot continues from its saved history when the gateway comes back
  (Linux and Android; built for Windows but not yet verified there; off
  on macOS, which lacks the process identity it relies on). A tool action whose result was not
  saved is never repeated; the turn stops for your review instead. If
  the provider is not reachable yet, it retries for a few minutes and
  again on the next start, then asks for review. Connector, A2A,
  session-mail, cron, subagent, TUI and trigger turns are not resumed.
- **Permission review**: the desktop ACL window's **Permission grants…**
  panel lists global grants, the session grants of every live session
  (desktop, Telegram, IRC and other chats, each labelled by surface) and
  the `[approvals] allowlist` entries from config.toml (read-only; edit
  the file to remove one). It revokes an exact selected grant.
- **Multi-hazard approvals**: a call that trips several rules (say a
  recursive delete and `curl | sh`) asks once, with every permission
  class checked; uncheck the ones you do not want remembered. On a typed
  surface (IRC, Telegram text, `veles chat`) a single reply such as `2`
  or `deny` answers the scope and keeps every class checked; `2; <keys>`
  narrows it. A later call is cleared only when ONE source (this
  session's grants, the global grants, or the allowlist) covers every
  class it trips; a call half-covered by each asks again. With several
  classes, an allowlist entry must be the exact `tool:rule` key; a glob
  only clears a call that trips a single rule.
- **Browser tab bridge** (Linux): a small Chrome/Firefox extension lets you
  hand one of your own tabs to the agent. It reads the page and clicks or
  fills by reference, never returns password or one-time-code values, and
  runs no arbitrary JavaScript and reads no cookies or storage. See
  [browser-extension/README.md](browser-extension/README.md).
- **Undo**: every file edit is checkpointed before it lands.
- **Approvals**: risky calls are screened and can be answered from any
  surface: terminal, desktop, Telegram or IRC.
- **Sandboxes**: commands run in podman or systemd-nspawn when
  configured; secrets are stripped from child environments.
- **Extending**: Tcl plugins (hot-reloaded), Rust plugins (in-tree or
  `.so` against a stable ABI), process plugins, and JavaScript, Racket
  or Lisp hosts. Start with [docs/plugins.md](docs/plugins.md).

The interface is translated into 47 languages.

## Building from source

```sh
nix develop                      # the toolchain: Rust, Tcl/Tk, Xvfb, …
cargo build --release
bash scripts/check.sh            # fast incremental checks
nix flake check                  # the full gate: tests, clippy, rustfmt, Tk suites
```

To fully rebuild the development gateway on the laptop, build all workspace
crates together, excluding the separately built Android runtime:

```sh
cargo build --workspace --exclude veles-runtime
```

This keeps `target/debug/veles` and the shared libraries it loads on the same
build. Building only `veles-cli --bin veles` can leave a new binary against
older shared libraries, causing symbol lookup failures on gateway startup.

Without Nix, `scripts/install/install.sh --from-source` installs the
build dependencies for Debian, Ubuntu, Arch, Gentoo or FreeBSD and builds
the checkout.

## License

veles is licensed under the GNU Affero General Public License v3.0 only
([LICENSE](LICENSE)). Vendored code keeps its own license:
`vendor/irc-proto` (MPL-2.0), `vendor/clib` (MIT or Apache-2.0), and GNU
Unifont in `crates/veles-textart/font` (OFL-1.1).
