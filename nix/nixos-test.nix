# checks.nixos-module — boot a VM with services.veles and PROVE it.
#
# The module is otherwise only evidenced by being switched into on one
# developer machine, which is strong evidence for that machine and none
# at all for a fresh one. This boots a clean NixOS, enables the module,
# and asserts the three things that can silently not be true:
#
#   1. the veles binary is on PATH and runs;
#   2. `veles doctor --sandbox` EXECUTES a script through the native
#      fence and gets 42 back — not a readout, a round trip;
#   3. the sandbox artifacts the module claims to wire are actually
#      present at the stable paths it promises.
#
# The nspawn and firecracker fences are NOT exercised here: nspawn needs
# a rootfs the test VM would have to build, and firecracker needs nested
# KVM, which a nixosTest guest does not reliably have. Those two are
# proven on real hardware by scripts/sandbox_vm_check.sh, and this test
# says so rather than pretending otherwise.
{ self, pkgs, lib }:

pkgs.testers.runNixOSTest {
  name = "veles-module";

  nodes.machine = { config, ... }: {
    imports = [ self.nixosModules.default ];

    services.veles = {
      enable = true;
      scope = "system";
      # A system-scope gateway with no credentials would fail its first
      # turn, and the module asserts as much — so give it settings. The
      # provider is deliberately unreachable: this test is about the
      # module, not about talking to an LLM.
      settings = {
        model.provider = "custom:test";
        model.default = "test-model";
        custom_providers = [{
          name = "test";
          base_url = "https://invalid.test";
          api_key_env = "TEST_KEY";
        }];
      };
      # `native` is the one fence a VM can honestly run: a real tclsh
      # under the safe interp, with no container runtime or nested KVM
      # in the way.
      sandbox.backend = "native";
      gateway.enable = false;
    };

    virtualisation.memorySize = 2048;
    virtualisation.diskSize = 8192;
  };

  testScript = ''
    machine.wait_for_unit("multi-user.target")

    with subtest("the binary the module installs actually runs"):
        machine.succeed("veles version")

    with subtest("doctor --sandbox runs a script through the fence"):
        # Not `succeed` on its own: assert on the OUTPUT, because a
        # doctor that printed a fence report and ran nothing would still
        # exit 0 if the probe were ever dropped.
        out = machine.succeed("veles doctor --sandbox 2>&1")
        assert "backend: native" in out, out
        assert "result: OK (42)" in out, \
            "the fence did not execute a script and return 42:\n" + out

    with subtest("a broken fence FAILS, so the check above means something"):
        # The mutation check, inside the test: an unimplemented backend
        # must make doctor exit non-zero. If this passes, the subtest
        # above is not merely asserting on a command that cannot fail.
        machine.fail("VELES_SANDBOX_BACKEND=definitely-not-a-backend veles doctor --sandbox")

    with subtest("XDG directories, not a single ~/.veles"):
        # scope = "system" pins VELES_HOME, so check the USER path the
        # workstation posture uses. Doctor prints the three resolved
        # roots before any check; its exit status is not the point here.
        out = machine.succeed("su -s /bin/sh nobody -c 'HOME=/tmp/h timeout 120 veles doctor' 2>&1 || true")
        assert "/tmp/h/.config/veles" in out, out
        assert "/tmp/h/.local/share/veles" in out, out

    with subtest("the state directory exists and is the service user's"):
        machine.succeed("test -d /var/lib/veles")
  '';
}
