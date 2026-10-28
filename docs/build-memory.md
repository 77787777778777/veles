# Build memory — what each build costs, and why it is shaped this way

Written 2026-09-28, after a machine crash during `nix flake check` made the
user look at what building this project costs (USER: "a 30GB peak for a
glorified irc bot"). The crash itself was most likely a GPU hang, not an
OOM: no OOM kill was logged and swap was untouched. The measured peak was
still close enough to the machine's 30 GB that it had to come down.

Every number here comes from `scripts/build-mem.sh`, run alone on this
machine (16 threads, `-j 8`, as nix.conf `cores = 8`), from scratch:
- **per-compile peak:** every rustc runs under GNU time through a
  `RUSTC_WRAPPER`;
- **concurrent sum:** the largest sum of peaks of rustc processes running
  at once, an upper bound;
- **system:** `free`'s `used`, sampled every 2 s, as the rise above
  where it started. This is the figure that matters to the machine.

## Where it was

`nix flake check` built the release package, and buildRustPackage's
checkPhase then ran `cargo test` in the **release** profile with thin LTO.
That compiled ~35 test and example binaries with full optimisation and
cross-crate LTO. `nixos-rebuild` builds the same package
(`services.veles` takes it), so every system rebuild paid for the tests
too.

| stage | wall | system | concurrent sum | largest compiles |
|---|---|---|---|---|
| release build (the package) | 186 s | +4.6 GB | 5.0 GB | veles_core lib 3.3 GB, veles bin 2.8 GB |
| release + LTO test build | 723 s | +8.0 GB | 14.5 GB | veles_core test 4.9 GB, veles_irc test 2.6 GB, 30+ binaries at 0.5-1.8 GB |

A full `nix flake check` with `max-jobs = 1` peaked at 17.2 GB of system
memory (from a ~7.7 GB desktop baseline). With `max-jobs = 2` the release
build and clippy also overlapped.

## What changed (2026-09-28)

1. **The package no longer runs the tests** (`doCheck = false`). A
   rebuild compiles the binary and nothing else.
2. **`checks.tests` runs them in the dev profile**, exactly as
   `scripts/precommit.sh` does: **162 s, +5.0 GB, concurrent sum 9.3 GB**
   (veles_core test 3.5 GB, lib 2.3 GB), against 723 s and +8.0 GB. It
   also tests the build a developer runs, with overflow checks and debug
   assertions on.
3. **The three heavy checks run one at a time.** The release build, clippy
   and the tests each name the previous one's output in their
   environment, so nix orders them `veles-agent → clippy → tests` and
   never overlaps them, whatever `max-jobs` says.
4. **The engine's translations are runtime files**
   (`docs/i18n-runtime.md`). rust-i18n had expanded the 47 catalogs into
   ~11,400 `map.insert` statements in one function, compiled into every
   build of veles-core: +0.34 GB on the release library (3.57 vs 3.23
   GB) and +0.60 GB on its test binary (5.79 vs 5.19 GB), measured with
   each compile running alone.

### The first flake run after the change

`nix flake check -L`: RC=0, 25 test binaries, 2,135 tests, 0 failed.

| | before (max-jobs 1) | after |
|---|---|---|
| wall | 1316 s | 1228 s |
| system peak | 17.2 GB (+10.3 GB over 6.9 GB at start) | 16.0 GB (+7.3 GB over 8.7 GB at start) |

The log has no timestamps. By the order the chain runs in (release
build 4 min 02 s, clippy 59 s, then the tests), the peak falls in the
dev test BUILD, minutes 7–9. From minute 10 on, the test RUN held
~10.5 GB.

Wall time barely moved, and not because of the build. veles-core's
1,842 unit tests take **485 s** in the sandbox in the dev profile, and
took 364 s there in release. Locally, precommit runs them in about
35 s. So the sandbox run dominates, whatever the profile. The dev
profile costs ~2 min of test time and saves ~9 min of test build.

## Tried and rejected

- **Compiling veles-core's test binary in its own step first**
  (`cargo test -p veles-core --lib --no-run`, then the workspace), so the
  3.5 GB compile would not overlap the 2.3 GB library. On its own it took
  42 s instead of 104 s. But `-p veles-core` resolves dependency features
  differently from `--workspace` (chrono `clock`, futures `alloc`, clap,
  irc and ~40 more, because other members enable them), so the second
  step compiled veles-core's test binary **again**: 226 s and +5.9 GB,
  worse on both counts. Making it work would mean mirroring every
  member's dependency features in veles-core, and that drifts.

## The surfaces are their own crate (2026-09-28)

USER: "if we can split out the tests into a separate crate … that would
probably be a huge win too". Moving veles-core's tests out as they are
isn't possible: 131 test modules use `super::*` on private items, so
moving them would mean making the internals public. The block that CAN
move, with its tests, is `frontends/`: 46k of veles-core's 171,768 lines
(desktop_api, commands, tui, gateway, cli, a2a_server, chat and panes).
It is now `crates/veles-frontends`.

Moving it down meant cutting the dozen places where core reached back
into the surfaces:
- **the gateway's message bus** is `veles_core::bus` (the gateway
  re-exports it);
- **new_session_id, the desktop port, json_event and the built-in
  command-name tables** (`slash_names`) are in core;
- **the ask relay's steer dock** takes its store by registration
  (`ask::set_steer_store`);
- **the gateway's wake owner** lives in the gateway.

Test helpers other crates use (`testenv`, `providers::tests`) sit behind
the existing `test-support` feature.

Dev test build, from scratch, `-j 8`, the same command both times:

| | before | after |
|---|---|---|
| wall | 162 s | **98 s** |
| system | +5.0 GB | +4.7 GB |
| veles_core test | 3.54 GB, 104 s | **2.72 GB, 39 s** |
| veles_core lib | 2.33 GB, 31 s | **1.65 GB, 19 s** |
| veles_frontends test / lib | — | 1.45 GB / 1.07 GB |
| sum of concurrent rustc peaks | 9.28 GB | 9.64 GB |

`nix flake check` after the split: RC=0, 27 test binaries, 2,136 tests,
1224 s, system peak **15.0 GB (+6.4 GB over 8.6 GB at start)**. That's
against 16.0 GB (+7.3) after the first change and 17.2 GB (+10.3) at the
start. In the sandbox, veles-core's unit tests now take 290 s (was 485 s)
and veles-frontends' take 271 s: the slow sandbox test run moved with them.

The largest compile is down by a quarter and the build by 40%. The
system peak fell less: the smaller compiles now run side by side, which
is also why the build is faster. An edit to a surface no longer
rebuilds core or its test binary.

## What is left, largest first

- **veles-core is still one ~125k-line crate**, 39% of it inline test
  modules. Its test binary is still the largest compile. Splitting it
  further (store, agent, tools) is a far larger refactor, because those
  modules reach into each other everywhere. Revisit only if these
  numbers become a problem again.
- **The sandbox runs veles-core's unit tests slowly** (485 s, against
  ~35 s locally), whatever the profile. That is time, not memory, and
  unexplained.

## Windows (winbox, 2026-10-13)

The Windows build host: Ryzen 9 3900X (12c/24t),
64 GB, MSVC 14.44, rustc 1.99.0, `C:\veles\src`. Wall-clock from
`C:\veles\check.log`'s START/END stamps, nothing else measured there
(no per-compile peaks: `scripts/build-mem.sh` is Linux-only).

| step | wall | notes |
|---|---|---|
| `cargo check --workspace --all-targets` (dev, minus racket/lisp) | ~9 min cold | the first pass on the host; incremental passes 1–3 min |
| `cargo clippy … -D warnings` (dev) | ~8 min cold, ~2 min warm | |
| `cargo build --profile windev` (gateway + launcher + 5 modules) | ~10 min cold | `windev` = dev at opt-level 2 (Cargo.toml says why); 2 m 59 s for the five modules once the runtime was built |
| `cargo test --no-fail-fast --profile windev` (5 packages) | 9 min 23 s (was 25+ and never finished before every closed-port probe used `127.0.0.1:0` + a connect timeout) | the test binaries at opt-level 2; 32 binaries, 2459 passed, 0 failed, 10 ignored on the fifth run (2026-10-13 22:03 CEST, commit 4fd00218); the first clean run — the 39 unix-assumption failures before it are in PENDING |
| `cargo build --profile winrelease` (the installer: CLI + launcher + 5 modules) | 2 m 32 s warm | release without LTO (Cargo.toml says why); a rebuild after the sqlite link dir moved, i.e. most of the workspace; the cold number is inside the installer's first ~6 min |
| Tcl/Tk 8.6.16 `nmake -f makefile.vc` (release + install, both) | ~3 min | the installer and the dev host alike |
| `cargo build --release` (the installer) | see PENDING 2026-10-13 | thin LTO |

Sizes (windev): `veles.exe` 18.9 MB, `veles_runtime.dll` 63 MB
(18,362 exports), modules 0.3–11.9 MB. A plain dev build of the
runtime DLL cannot link at all: its export table exceeds the PE import
library's 65,535 (`LNK1189`).

