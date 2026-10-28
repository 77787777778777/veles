# greeter

an overlay module for veles.

A veles overlay module: a Rust module that lives in a repository of its
own and is built inside veles' own build, so it loads into the gateway
that build makes. veles' docs/extending.md, "Rust overlay modules", has
the whole story.

- `Cargo.toml`: a `dylib` crate. It names `veles-runtime = { workspace = true }`
  and only crates veles' `Cargo.lock` already has.
- `src/lib.rs`: the module, with one tool, `greeter_hello`.
- `flake.nix`: builds a whole veles package with this module in it,
  against veles' repository. Point `inputs.veles.url` at your fork or
  checkout if you run one.

## Build and install

    nix build

Everything in `result/` comes from one build, and only that build's
gateway loads this module. Run `result/bin/veles`, and install every
module you use from `result/lib/`, the stock ones too: the gateway
refuses a module from any other build ("core changed").

    dir=${XDG_DATA_HOME:-$HOME/.local/share}/veles/modules/greeter
    so=result/lib/libveles_overlay_greeter.so
    sha=$(sha256sum $so | cut -d' ' -f1)
    install -d -m 0700 $dir
    install -m 0600 $so $dir/$sha.so
    ln -sfn $sha.so $dir/current
    printf 'name = "greeter"\nkind = "module"\nentry = "module.so"\n' > $dir/plugin.toml
    result/bin/veles plugins rescan

## Updating veles

    nix flake update veles
    nix build

Install the modules again and restart the gateway on the new
`result/bin/veles`: the new gateway refuses the old build's modules.

## Not stable

This tier is not part of veles' stable extension API. It calls veles'
Rust internals, so a veles update can break its build. When an override
point, a script, a `.so` or a process plugin can do the job, prefer it.
