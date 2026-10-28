# Why `clib` is vendored

`clib` 0.2.4 is vendored here with its `bindgen` build-dependency widened
from `0.64` to `0.72`, plus the deterministic-spec and generated-code lint
patches described below. The functional binding source is otherwise the
published crate.

## The failure

On a host with **libclang 22 or newer**, the published `clib` produces
bindings in which every record containing an anonymous union is
*opaque*. `Tcl_Obj` is one of them, and the `tcl` crate — which reads
`refCount`, `bytes`, `length`, `typePtr`, `internalRep` directly — then
fails to compile with 47 errors of this shape:

```
error[E0609]: no field `refCount` on type `&Tcl_Obj`
   = note: available field is: `_address`
```

`_address` is what bindgen emits for a type it has given up on. The
build script SUCCEEDS; nothing warns; the error surfaces one crate later
as a missing struct field, which reads like a Tcl version mismatch and
is not one.

## What was actually wrong

`bindgen` 0.64 (June 2023) predates libclang 22. Fed that libclang it
mis-parses those records and takes its "make the whole struct opaque
rather than get the layout wrong" fallback (`CompInfo::is_opaque`). It
still emits a 478 KB bindings file, correct except for the handful of
types that matter most.

Measured 2026-09-04, same source tree, same rustup stable 1.98.1:

| host | libclang | published clib | vendored clib |
| --- | --- | --- | --- |
| NixOS (this machine) | 21.1.8 | builds | builds |
| Debian sid | 21.1.8 | builds | builds |
| Ubuntu 24.04 | 18.1.3 | builds | builds |
| Arch | **22.1.8** | **47 errors** | builds |
| Gentoo stage3 | **22.1.8** | **47 errors** | builds |
| FreeBSD 15 | 19 | builds | builds |

The dividing line is the libclang version, nothing else. Debian and
Ubuntu were not passing because they are better supported — they were
passing because they had not reached clang 22 yet, and they will.

## What this is NOT

Two earlier explanations of these 47 errors, both written down in this
repo before the bindings were read, were wrong:

- **"empty clib bindings from `inwelling`"** — `clib`'s `generate_dummy()`
  writes a zero-byte `bindings.rs` when `inwelling::collect_downstream`
  returns nothing. That path was never taken here: the file is 478 KB
  and `inwelling` had found the `tcl` crate. (The race it describes is
  real — `inwelling` waits for the build directory to go quiet and then
  guesses — it just is not this.)

  **That race did land, later.** It is what had been failing CI on every
  push since 2026-09-12 — see *The second patch* below. It was never the
  cause of these 47 errors; it is the cause of a different 186.
- **"Arch's packaged toolchain"** — Arch and Debian both ran rustup
  stable 1.98.1.

## The second patch: the spec clib must not have to race for

Every CI run from 2026-09-12 to 2026-09-16 failed — 15 for 15, every
push — with 186 errors of this shape, all inside the `tcl` crate:

```
error[E0425]: cannot find function `Tcl_SetListObj` in crate `clib`
error[E0425]: cannot find type `Tcl_Obj` in crate `clib`
```

Those are the symptoms of an **empty** `bindings.rs` — the
`generate_dummy()` path the section above rules out for the 47 errors.
It was not taken by that failure. It is taken by this one. (The docs.rs
path is ruled out too: `tcl`'s `for_docs.rs` defines `Tcl_Obj`,
`Tcl_SetListObj` and `Tcl_FindExecutable`, the very symbols CI reports
missing. Nothing was written at all.)

### Why the file was empty

`clib` does not know which C library to bind. It learns that from its
downstream crates through `inwelling`: `tcl`'s build script calls
`inwelling::to("clib")`, which writes `manifest_dir.inwelling` into
tcl's `OUT_DIR`, and clib's build script reads those files back out of
`target/<profile>/build/*/out/`.

Nothing orders those two build scripts. `tcl`'s build script
build-depends on `inwelling`, not on `clib`, so cargo may run it
whenever it likes — including after clib's script has already finished.
inwelling covers that with a guess (0.5.5, `wait_for_other_builds`):

```rust
loop { sleep(5s); walk(build_dir); if nothing_new { break } }
```

Five quiet seconds in the build directory is taken to mean every
downstream has registered. On this machine — 16 cores, warm store, the
whole check in 66 seconds — the directory keeps changing and the guess
holds. On a 4-core GitHub runner it does not: in the CI log `clib`
compiles at 20:33:24 and `tcl` at 20:33:49, and a 25-second gap between
two compiles holds any number of quiet 5-second windows. clib concluded
it had no downstreams at all, wrote a zero-byte `bindings.rs`, and
exited 0.

The build then dies 186 errors later in a crate that is not at fault,
and neither `clib` nor `inwelling` appears anywhere in the output.

### Reproducing it without a slow machine

Losing the race on purpose is awkward. Removing the registrant is not —
`clib` built alone has no downstream by construction, which drives the
identical code path:

```
$ cargo build -p clib --target-dir /dev/shm/t
$ ls -l /dev/shm/t/debug/build/clib-*/out/bindings.rs
-rw-r--r-- 1 wrath users 0 Sep 16 22:48 bindings.rs

$ cargo build -p tcl --target-dir /dev/shm/t
error: could not compile `tcl` (lib) due to 186 previous errors
```

186 — the same count CI reports, with the same symbols.

### The patch

`[package.metadata.clib-fallback]` at the foot of this crate's
`Cargo.toml` carries the same spec the `tcl` crate publishes, and
`build.rs` reads it **before** asking inwelling. Anything inwelling
finds then overrides it, so a downstream that registers in time stays
authoritative and the fallback is only a floor. clib's own manifest is
always readable, so that floor cannot be raced away.

The spec is copied from `tcl`'s `[package.metadata.inwelling.clib]`.
That crate is pinned by rev in `Cargo.lock`, so the copy can only go
stale through a deliberate bump — and when it does, whichever spec
inwelling finds wins over the copy anyway.

`for-docs-rs` is deliberately **not** copied: it names a file that is
resolved relative to the manifest declaring it, and `for_docs.rs` ships
with `tcl`, not here.

### And it no longer fails silently

Both paths that used to call `generate_dummy()` now panic with the
reason, and `generate_dummy` is gone. An empty `bindings.rs` is never a
useful build product here; it only relocates the error to a crate that
cannot explain it. That is what made this failure survive four days of
pushes — the log named `Tcl_ListObjAppendElement` and
`tcl-0.1.9/src/list.rs`, and nothing in it pointed at a build script
that had quietly decided there was nothing to bind.

## The third patch: the first `where` match (2026-10-27)

On Windows the probe falls back to `where tclsh86` / `where tclsh` and
derives the Tcl prefix from the answer. `where` prints every match, one
per line, and the build script used the whole output as one path. The
GitHub Windows image has Git's `ucrt64\bin\tclsh86.exe` on PATH after the
Tcl that `scripts/install/windows.ps1` builds, so the first release run
failed with `lib should be found in "C:\velesbuild\tcl\bin\tclsh86.exe\r\n
C:\Program Files\Git\ucrt64\lib"`. The probe now takes the first line:
the match PATH order chose.

## Upstream

The real fix belongs upstream: `clib` should widen its bindgen range.
Until then this patch is applied through `[patch.crates-io]` in the
workspace `Cargo.toml`. To drop it, remove that entry and this
directory, and check that `Tcl_Obj` still has fields on a clang-22 host
— `scripts/install/test-matrix.sh arch` is exactly that check.
