# greeter: a veles overlay module in a repository of its own. This flake
# builds a whole veles package with the module in it:
#
#   nix build     # result/bin/veles, result/lib/*.so (this module too)
#
# `inputs.veles` is the veles you build against; flake.lock pins its rev.
# `nix flake update veles` moves to a newer veles, and the next build
# rebuilds this module against it with no step of yours. It fails only
# if veles' Rust internals changed under the module.
{
  description = "an overlay module for veles";

  # The veles you build against: veles' repository, or your fork
  # (github:<you>/veles), or a checkout (git+file:///path/to/veles).
  inputs.veles.url = "github:77787777778777/veles";

  outputs =
    { veles, ... }:
    {
      packages.x86_64-linux.default = veles.lib.x86_64-linux.velesWithModules {
        modules = [
          {
            name = "greeter";
            src = ./.;
          }
        ];
      };
    };
}
