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
  machines if you have more than one.
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

Without Nix, `scripts/install/install.sh --from-source` installs the
build dependencies for Debian, Ubuntu, Arch, Gentoo or FreeBSD and builds
the checkout.

## License

veles is licensed under the GNU Affero General Public License v3.0 only
([LICENSE](LICENSE)). Vendored code keeps its own license:
`vendor/irc-proto` (MPL-2.0), `vendor/clib` (MIT or Apache-2.0), and GNU
Unifont in `crates/veles-textart/font` (OFL-1.1).
