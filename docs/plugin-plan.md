# Plan: Real plugins (Tcl + Rust) + sandbox backends & portability

*Status: SUPERSEDED IN PART - EXECUTED, then the remainders too.
Parts 1-4 shipped and gated; a follow-up round closed every OPEN
remainder (Rust Phase B `.so` loading, the nspawn/firecracker/jail
backends, the dev sandbox image, `config_set`). The live plugin
author's guide is **docs/plugins.md**; the executed state lives in the
tree (`crates/veles-plugin-abi`, `rustplugins.rs`). The original
"Verified current state" inventory and the part tables described the
tree BEFORE this plan ran - they are retired to git history.*

## The ABI history

Phase B shipped an ABI that was ONE-WAY: three function pointers, all
registration, so an out-of-tree `.so` could be called by veles and
could never call veles. The plan never asked for the other direction,
and the round that executed it checked whether the gate ran rather than
whether a plugin could be written. What closed it: ABI 2 (`host_call`,
`register_command`), ABI 3 (`register_provider`), a language-neutral op
table both surfaces adapt over, and a provider driver written twice to
prove the two languages are one capability. `VELES_PLUGIN_ABI_VERSION`
was 3 at that point; it is 4 now (platforms, timers, sockets, async tools,
in-process unload — docs/plugins.md), and an ABI-3 library is refused by
name before any of its code runs.

## The one deviation to remember

The sandbox image is `sandbox/Dockerfile` (ubuntu LTS, full dev
environment, package-install capable) built by
`scripts/build-sandbox-image.sh`, NOT a nix `sandboxImage` flake
output - USER direction was a debian/ubuntu base with a working apt
(what models were RL'd on), and dockerTools cannot honestly produce
that. Runtime verification of the root/BSD/artifact-bound backends is
the opt-in `scripts/sandbox_vm_check.sh`; the cargo gate stays
daemon-less and pins argv/config construction instead.

Executor gotchas that outlived this plan: the Tcl interp is `!Send`,
one thread - every bridge command hops; NEVER panic across FFI; never
`blocking_recv` inside a tokio runtime (use `spawn_blocking` for Tcl
hops from async). Detail (parts, phases, original session findings) is
in git history.