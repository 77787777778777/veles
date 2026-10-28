# services.veles — the NixOS module.
#
# WHY IT EXISTS
#
# Until 2026-09-04 veles was a thing you ran out of a dev tree. There
# was no unit, no declarative config, no way for the sandbox backends to
# be pointed at artifacts that had been built. This module is the
# install story on NixOS, and it is also what makes the fences testable:
# choosing `sandbox.backend = "firecracker"` WIRES the kernel and rootfs
# derivations into `[sandbox]`, so a config cannot name artifacts that
# were never built.
#
# USER SCOPE (2026-09-04): *"at user level, any superuser functionality
# through sudo as is now"*. `scope = "user"` is therefore the default —
# a `systemd.user` unit running as the logged-in user against their own
# XDG directories. `scope = "system"` is the same unit as a system
# service with a dedicated user, for servers.
{ self }:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.veles;
  inherit (lib) mkOption mkEnableOption mkIf types literalExpression;

  artifacts = import ./sandbox-artifacts.nix {
    inherit pkgs lib;
    nixpkgs = self.inputs.nixpkgs.outPath or pkgs.path;
  };

  # The guest kernel is built from the HOST's kernel source by default —
  # same tarball nix already fetched, differing only in the virtio/ext4
  # config the guest needs built IN rather than as modules.
  # The guest kernel's BASE.
  #
  # This was `config.boot.kernelPackages.kernel` — build the guest from
  # the source the host already runs, which is what the operator asked
  # for and is the right instinct. It does not work, and the failure is
  # silent: nixpkgs' xanmod expression takes `...` and never forwards
  # `structuredExtraConfig`, so `.override` was accepted and ignored.
  # The result was a plain host kernel with VIRTIO_MMIO=m, VIRTIO_BLK=m
  # and EXT4_FS=m, which a firecracker guest — booting with no initrd,
  # and so unable to load a module — cannot mount a root filesystem
  # with. Measured 2026-09-04 by booting it:
  #     VFS: Unable to mount root fs on "/dev/vda"
  #
  # `pkgs.linux_latest` is a buildLinux kernel and DOES honour
  # `.override`. `sandbox.kernelBase` puts the choice back in the
  # operator's hands, and the derivation now checks the config it
  # actually built with rather than trusting the override.
  kernelBase =
    if cfg.sandbox.kernelBase != null then cfg.sandbox.kernelBase else pkgs.linux_latest;

  firecrackerKernel =
    if cfg.sandbox.kernel != null then
      cfg.sandbox.kernel
    else
      "${artifacts.firecrackerKernelFor kernelBase}/vmlinux";

  # Resolve the artifact each backend needs, so a config can never point
  # at something that does not exist.
  sandboxImage =
    if cfg.sandbox.image != null then cfg.sandbox.image
    else if cfg.sandbox.backend == "nspawn" then "${artifacts.nspawnRootfs}"
    else if cfg.sandbox.backend == "firecracker" then "${artifacts.firecrackerRootfs}"
    else "veles-sandbox:latest";

  sandboxSettings = lib.optionalAttrs (cfg.sandbox.backend != null) {
    sandbox = {
      inherit (cfg.sandbox) backend network capabilities memory_mb mounts extra_args;
      image = sandboxImage;
    } // lib.optionalAttrs (cfg.sandbox.backend == "firecracker") {
      kernel_path = firecrackerKernel;
    };
  };

  settings = lib.recursiveUpdate cfg.settings sandboxSettings;
  configFile = (pkgs.formats.toml { }).generate "veles-config.toml" settings;

  # State and config roots. Under `scope = "user"` these are the user's
  # own XDG directories, which is exactly what the binary picks by
  # default — so the unit sets nothing and inherits them.
  systemStateDir = "/var/lib/veles";

  commonEnv = {
    # Tcl is the scripting core; the wrapper in the package sets these
    # for interactive runs, and a unit does not inherit a login shell.
    LANG = "C.UTF-8";
    LC_ALL = "C.UTF-8";
  } // lib.optionalAttrs cfg.desktopApi.enable {
    VELES_DESKTOP_HOST = cfg.desktopApi.host;
    VELES_DESKTOP_PORT = toString cfg.desktopApi.port;
  } // lib.optionalAttrs cfg.a2a.enable {
    A2A_PORT = toString cfg.a2a.port;
  } // lib.optionalAttrs (cfg.vault.identityDir != null) {
    VELES_VAULT_DIR = cfg.vault.identityDir;
  };

  # PRODUCTION AND DEV, SIDE BY SIDE.
  #
  # USER, 2026-09-04: *"the flake should have a production and dev
  # option, dev runs like it does now, production runs the one built by
  # nix"*, then *"it's easiest if it's just two versions of the
  # user-level systemd unit, the current one becoming dev and a new one
  # installed for prod"*, then *"the toggle doesn't even need to be in
  # the module"* / *"just let the user symlink it from the system
  # store"*.
  #
  # So: both units are INSTALLED, there is no mode option, and NEITHER
  # is wanted by anything. `systemctl --user enable` makes exactly that
  # symlink, and it knows things the module does not — such as which
  # binary this particular login should start.
  #
  #   veles-gateway      ${package}/bin/veles — nix-built and pinned
  #   veles-gateway-dev  ${devTree}/target/<profile>/veles
  #
  # The dev unit runs a binary nix did not build, cannot pin and does
  # not own: every `cargo build` in that tree replaces the running
  # daemon's executable underneath it. That is what dev mode is FOR, and
  # it is why the two have different names — `systemctl --user status`
  # has to be able to tell you which one is live.
  #
  # This is not a hypothetical tidy-up. On 2026-09-04 this machine had a
  # hand-written `~/.config/systemd/user/veles-gateway.service` from
  # 2026-08-22 running `target/debug/veles`, while the module — added
  # the same day with `gateway.enable = false` — generated no unit at
  # all. Two mechanisms doing one job, and nothing naming which.
  prodUnit = "veles-gateway";
  devUnit = "veles-gateway-dev";

  # `throw` rather than letting a null interpolate into the middle of a
  # systemd unit as "cannot coerce null to a string". The assertion
  # below says the same thing the NixOS way; whichever fires first names
  # the option.
  # `%h`, systemd's specifier for the running user's home, so no
  # username appears in the unit — the same reason the tmpfiles rules
  # below use it, and USER 2026-09-04: *"ideally the username wouldn't
  # be hardcoded anywhere"*. A relative devTree is taken from $HOME; an
  # absolute one (a tree in /srv, say) is used as given.
  #
  # Verified rather than assumed: systemd expands specifiers in
  # ExecStart before resolving the executable, so `%h/…/veles` starts.
  # (`systemd-analyze --user verify` accepts it, and a probe unit ran
  # and printed the version — 2026-09-04.)
  devTreePath =
    if cfg.gateway.devTree == null then
      throw "services.veles.gateway.devTree must be set to run the dev unit"
    else if lib.hasPrefix "/" cfg.gateway.devTree then
      cfg.gateway.devTree
    else
      "%h/${cfg.gateway.devTree}";

  devBin = "${devTreePath}/target/${cfg.gateway.devProfile}/veles";

  # systemd's answer to a missing ExecStart is `status=203/EXEC`, which
  # names no path and suggests nothing. A dev tree that has not been
  # built yet is the ordinary case, so say which path and what to do.
  preflight = pkgs.writeShellScript "veles-dev-preflight" ''
    bin="$1"
    if [ ! -x "$bin" ]; then
      tree=$(dirname "$(dirname "$(dirname "$bin")")")
      echo "veles-gateway-dev: $bin is missing or not executable." >&2
      echo "Build it first:  cd $tree && cargo build" >&2
      exit 1
    fi
  '';

  # A dev unit is a USER-scope idea: the system unit runs as a dedicated
  # user with ProtectHome = "tmpfs", which cannot see a working tree in
  # /home at all.
  hasDevUnit = cfg.scope == "user" && cfg.gateway.devTree != null;

  # `veles gateway run` is the long-lived surface: it owns the platform
  # adapters, the cron scheduler (the ONLY thing that fires jobs — see
  # cron.rs) and the desktop API.
  serviceBase = {
    description = "Veles Agent gateway";
    after = [ "network-online.target" ];
    wants = [ "network-online.target" ];
    environment = commonEnv;
    serviceConfig = {
      ExecStart = "${cfg.package}/bin/veles gateway run";
      Restart = "on-failure";
      RestartSec = 5;
      # veles' own lifecycle code is the init story (pid file,
      # crash-loop guard, graceful SIGTERM with platform
      # announcements), so systemd only has to restart it.
      KillSignal = "SIGTERM";
      TimeoutStopSec = 30;
    } // lib.optionalAttrs (cfg.environmentFile != null) {
      EnvironmentFile = cfg.environmentFile;
    };
  };
in
{
  options.services.veles = {
    enable = mkEnableOption "the Veles Agent gateway";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = literalExpression "veles-agent.packages.\${system}.default";
      description = "The veles package to run and to install.";
    };

    scope = mkOption {
      type = types.enum [ "user" "system" ];
      default = "user";
      description = ''
        `user` runs veles as a `systemd.user` service for
        {option}`services.veles.user`, against that user's own XDG
        directories (`~/.config/veles`, `~/.local/share/veles`) — the
        posture for a workstation, where anything needing root goes
        through sudo.

        `system` runs it as a system service with a dedicated
        unprivileged user and `${systemStateDir}` — the posture for a
        server.
      '';
    };

    user = mkOption {
      type = types.str;
      default = "veles";
      description = ''
        Under `scope = "system"`, the dedicated service user (created).
        Under `scope = "user"`, the existing login user whose systemd
        user session runs the unit — no account is created.
      '';
    };

    settings = mkOption {
      type = (pkgs.formats.toml { }).type;
      default = { };
      example = literalExpression ''
        {
          model.provider = "custom:my-provider";
          model.default = "some-model";
        }
      '';
      description = ''
        `config.toml` contents. Rendered into the store, so it must not
        carry secrets — put API keys in
        {option}`services.veles.environmentFile`.
      '';
    };

    environmentFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      example = "/var/lib/secrets/veles.env";
      description = ''
        `EnvironmentFile` for the unit: API keys and bot tokens. Read at
        start time from OUTSIDE the nix store, so secrets are never
        world-readable in `/nix/store`.

        A value exported here OUTRANKS the vault (an operator's explicit
        override). The vault is the better home for keys: it is
        encrypted, shared with the pool's other nodes, and gives each key
        a policy for what the model may do with it. `veles vault import`
        copies what a `.env` holds into it.
      '';
    };

    vault = {
      identityDir = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "/persist/veles/vault";
        description = ''
          Where this node's vault lives: its age identity, its signing
          key (neither ever leaves the machine) and the replicated
          `vault.json`. Unset, veles uses `<config dir>/vault`
          (`~/.config/veles/vault` under user scope,
          `/var/lib/veles/vault` under system scope). Set it to keep the
          node's keys somewhere that survives a wiped home, such as an
          impermanence `/persist`. Passed to veles as `VELES_VAULT_DIR`;
          the directory is created 0700.
        '';
      };
      ageCli = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Install the `age` command-line tool. Every value in the vault
          is an armored age file, so `age -d -i <identityDir>/identity.txt`
          opens one by hand — the way out when veles itself cannot.
        '';
      };
    };

    installConfig = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Link the generated `config.toml` into the config directory.

        Off by default under `scope = "user"`, and deliberately: a
        workstation user edits `~/.config/veles/config.toml` by hand and
        through `veles::config_set`, and a store symlink would make both
        fail on a read-only file.
      '';
    };

    gateway.enable = mkOption {
      type = types.bool;
      default = true;
      description = ''
        INSTALL the gateway units. Neither is wanted by anything, so
        pick one yourself:

        ```sh
        systemctl --user start veles-gateway          # this session
        ln -s /etc/systemd/user/veles-gateway.service \
              ~/.config/systemd/user/default.target.wants/   # every login
        systemctl --user disable veles-gateway        # undoes that symlink
        ```

        NOT `systemctl --user enable`: like most NixOS units these are
        `static` (no `[Install]` section, because that is generated from
        `wantedBy` — which would also plant a root-owned symlink under
        /etc that the user could not remove). `enable` says so and only
        symlinks the unit into the user's directory, which shadows
        rather than enables. `disable` does undo a hand-made
        `default.target.wants` symlink.

        Turn this off to install the package, sandbox artifacts and
        config with no units at all.
      '';
    };

    gateway.devTree = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "veles-agent";
      description = ''
        Working tree for the `veles-gateway-dev` unit. Null (the
        default) means no dev unit is installed at all.

        RELATIVE paths are taken from the running user's home, through
        systemd's `%h` — so the unit carries no username and is correct
        for whoever starts it. An absolute path (a tree in `/srv`, say)
        is used as given.

        A STRING, not a path: a nix path literal would copy the tree
        into the store on every evaluation, and this one carries a
        multi-gigabyte `target/`. The unit reads it at start time, from
        outside the store, which is the whole point.
      '';
    };

    gateway.devProfile = mkOption {
      type = types.enum [ "debug" "release" ];
      default = "debug";
      description = ''
        Which cargo profile the dev unit runs: `target/debug/veles` or
        `target/release/veles`.
      '';
    };

    desktopApi = {
      enable = mkEnableOption "the desktop/HTTP API";
      host = mkOption {
        type = types.str;
        default = "127.0.0.1";
        description = ''
          Bind address. Loopback by default, and widening it needs a
          credential: the gateway refuses a non-loopback bind with no
          token configured (`authn::resolve_bind_host`), because this
          route table holds config writes, two live interpreters, ACL
          edits and sandbox binds.

          Auth is a bearer token, and it is on by default: with nothing
          configured the gateway MINTS one into `<config_dir>/.env`
          (0600) at startup and logs the path. Set
          `DESKTOP_BEARER_TOKEN` (and/or `DESKTOP_PEER_TOKENS` for named
          remote desktops) in the unit's environment or in that `.env` to
          choose your own; `VELES_DESKTOP_OPEN=1` declines the token and
          keeps the historical open-on-loopback posture. Reach a widened
          bind over SSH or WireGuard either way.
        '';
      };
      port = mkOption {
        type = types.port;
        default = 8642;
        description = "Desktop API port.";
      };
    };

    a2a = {
      enable = mkEnableOption "the A2A JSON-RPC server";
      port = mkOption {
        type = types.port;
        default = 9900;
        description = "A2A port.";
      };
    };

    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Open the A2A port. Never opens the desktop API port: that one is
        a single-user local surface, and the way to reach it from
        elsewhere is a tunnel, not a hole in the firewall — a bearer
        token (which it now has by default) authenticates a client, it
        does not make an exposed operator API a good idea.
      '';
    };

    sandbox = {
      backend = mkOption {
        type = types.nullOr (types.enum [ "native" "host" "podman" "nspawn" "firecracker" "jail" ]);
        default = null;
        description = ''
          `[sandbox] backend`. Leaving it null writes no `[sandbox]`
          section, so the binary's own default (`native`) applies.

          Choosing `nspawn` or `firecracker` also BUILDS and WIRES the
          artifact that backend needs — see
          {option}`services.veles.sandbox.image` and
          {option}`services.veles.sandbox.kernel`. That coupling is the
          point: a `[sandbox]` section naming a rootfs nobody built is
          the failure this module exists to prevent.
        '';
      };
      image = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          Override the resolved artifact: a podman image NAME, an nspawn
          rootfs DIRECTORY, a firecracker rootfs IMAGE, or a FreeBSD
          jail root. Null resolves to the nix-built artifact for the
          chosen backend (`veles-sandbox:latest` for podman, which
          `scripts/build-sandbox-image.sh` produces — dockerTools cannot
          honestly build an ubuntu base with a working apt).
        '';
      };
      kernel = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          firecracker only: path to a prebuilt guest `vmlinux`. Null
          builds one from {option}`services.veles.sandbox.kernelBase`,
          forcing virtio-mmio, virtio-blk and ext4 to be built IN — a
          stock kernel has all three as modules, and a guest booting
          with no initrd cannot load a module, so it panics with
          `Unable to mount root fs on "/dev/vda"`.
        '';
      };
      kernelBase = mkOption {
        type = types.nullOr types.package;
        default = null;
        defaultText = literalExpression "pkgs.linux_latest";
        description = ''
          The kernel the guest kernel is DERIVED from. Must be a
          `buildLinux` kernel: those honour
          `.override { structuredExtraConfig = …; }`.

          It is deliberately NOT `config.boot.kernelPackages.kernel`,
          however appealing "build the guest from the source the host
          already runs" is. nixpkgs' xanmod expression takes `...` and
          never forwards `structuredExtraConfig`, so the override is
          accepted and silently dropped, and the result is a host kernel
          wearing a guest kernel's name. The artifact derivation checks
          the config it actually built with and fails loudly if a base
          does this, so trying another one is safe.
        '';
      };
      network = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Network inside container backends. True is the DEV posture
          (models install packages); false is the benchmark/untrusted
          posture. firecracker rejects true — it has no tap devices.
        '';
      };
      capabilities = mkOption {
        type = types.enum [ "drop" "default" ];
        default = "drop";
        description = ''
          `drop` is the hardened pure-compute fence (`--cap-drop ALL`).
          `default` is needed for package installation INSIDE the image
          (dpkg/pip want CAP_CHOWN within the userns). Host safety comes
          from rootless isolation, never from this knob.
        '';
      };
      memory_mb = mkOption {
        type = types.ints.positive;
        default = 256;
        description = "Memory cap for a sandboxed run.";
      };
      mounts = mkOption {
        type = types.listOf (types.submodule {
          options = {
            host = mkOption { type = types.str; description = "Absolute host path."; };
            container = mkOption { type = types.str; description = "Absolute path inside the sandbox."; };
            mode = mkOption { type = types.enum [ "ro" "rw" ]; description = "Access mode."; };
          };
        });
        default = [ ];
        description = ''
          Explicit host access, default DENY. Ignored by native (which
          is the host already) and rejected by firecracker (no virtio-fs).
        '';
      };
      extra_args = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "Extra argv appended to the container/VM command.";
      };
    };
  };

  config = mkIf cfg.enable (lib.mkMerge [
    {
      # Options that are SET but cannot take effect, said out loud. The
      # desktop-API and A2A knobs reach veles only as environment on the
      # gateway unit, so with no unit they are inert — which is the
      # "a config line that does nothing is a lie" shape this project
      # keeps finding elsewhere, and there is no reason to ship it here.
      warnings =
        lib.optional (!cfg.gateway.enable && (cfg.desktopApi.enable || cfg.a2a.enable)) ''
          services.veles: desktopApi/a2a are passed to veles as environment on
          the gateway unit, and gateway.enable = false means there is no unit —
          so they currently do nothing. Set gateway.enable = true, or put the
          ports in config.toml / the environment of however you launch veles.
        ''
        ++ lib.optional (cfg.gateway.devTree != null && cfg.scope == "system") ''
          services.veles.gateway: devTree is set but scope = "system", so no
          veles-gateway-dev unit is installed — the system unit's
          ProtectHome = "tmpfs" would hide the tree anyway. Use scope = "user"
          for a dev unit.
        ''
        ++ lib.optional (cfg.installConfig && cfg.scope == "user") ''
          services.veles: installConfig writes /etc/veles/config.toml, which a
          user-scope veles does not read — it reads ~/.config/veles/config.toml.
          The sandbox artifacts ARE wired for user scope, through the stable
          paths under ~/.local/share/veles/sandbox/ (see the module source).
        '';

      assertions = [
        {
          assertion = cfg.sandbox.backend != "firecracker" || !cfg.sandbox.network;
          message = ''
            services.veles.sandbox: firecracker has no networking (it
            needs tap devices). Set sandbox.network = false, or choose
            podman/nspawn.
          '';
        }
        {
          assertion = cfg.sandbox.backend != "firecracker" || cfg.sandbox.mounts == [ ];
          message = ''
            services.veles.sandbox: firecracker cannot mount host paths
            (no virtio-fs). Use podman or nspawn for explicit mounts.
          '';
        }
        {
          assertion = cfg.scope != "system" || cfg.environmentFile != null || cfg.settings != { };
          message = ''
            services.veles: a system-scope gateway with neither settings
            nor an environmentFile has no provider credentials and will
            fail its first turn. Set one of them.
          '';
        }
      ];

      environment.systemPackages =
        [ cfg.package ]
        # The runtimes the chosen fence needs, so `veles doctor
        # --sandbox` can actually run rather than reporting a missing
        # binary. podman is NOT added here: it needs
        # virtualisation.podman.enable for subuid/subgid, which is the
        # operator's call, not a package drop.
        ++ lib.optional (cfg.sandbox.backend == "firecracker") pkgs.firecracker
        ++ lib.optional (cfg.sandbox.backend == "firecracker") pkgs.e2fsprogs
        ++ lib.optional (cfg.sandbox.backend == "nspawn") pkgs.systemd
        ++ lib.optional cfg.vault.ageCli pkgs.age;

      networking.firewall.allowedTCPPorts =
        lib.optional (cfg.openFirewall && cfg.a2a.enable) cfg.a2a.port;
    }

    (mkIf (cfg.scope == "system") {
      users.users.${cfg.user} = {
        isSystemUser = true;
        group = cfg.user;
        home = systemStateDir;
        createHome = true;
      };
      users.groups.${cfg.user} = { };

      # Renamed from `veles` on 2026-09-04 so the production and dev
      # units read as a pair (`veles-gateway` / `veles-gateway-dev`)
      # rather than as two unrelated things.
      # A vault dir the operator moved: created 0700 for the service
      # user before the unit starts (veles creates it too, but a missing
      # parent under /persist is the operator's to see at boot).
      systemd.tmpfiles.rules = lib.optional (cfg.vault.identityDir != null)
        "d ${cfg.vault.identityDir} 0700 ${cfg.user} ${cfg.user} -";

      systemd.services.${prodUnit} = lib.mkIf cfg.gateway.enable (lib.recursiveUpdate serviceBase {
        wantedBy = [ "multi-user.target" ];
        environment = commonEnv // { VELES_HOME = systemStateDir; };
        serviceConfig = {
          User = cfg.user;
          Group = cfg.user;
          StateDirectory = "veles";
          WorkingDirectory = systemStateDir;
          # Hardening. NOT ProtectSystem = "strict": the terminal tool
          # and execute_code exist to touch the filesystem, and a fence
          # that silently removes a documented capability is worse than
          # no fence. The sandbox backends are where untrusted code is
          # bounded.
          ProtectSystem = "full";
          ProtectHome = "tmpfs";
          BindPaths = [ systemStateDir ]
            ++ lib.optional (cfg.vault.identityDir != null) cfg.vault.identityDir;
          NoNewPrivileges = true;
          PrivateTmp = true;
          ProtectKernelTunables = true;
          ProtectControlGroups = true;
          RestrictSUIDSGID = true;
        };
      });
    })

    (mkIf (cfg.scope == "user") {
      # A systemd USER unit: veles runs as the logged-in user, against
      # that user's own XDG directories, and reaches root only through
      # sudo — the posture the operator asked for.
      # BOTH units, always — `mode` decides which one is wanted at
      # login, so switching between the build you trust and the tree you
      # are editing is `systemctl --user`, not a rebuild.
      # NEITHER unit is wanted by anything, and that is the design:
      # `gateway.enable` INSTALLS them, `systemctl --user enable` picks
      # one. USER, 2026-09-04: *"just let the user symlink it from the
      # system store"*.
      #
      # It is also the only arrangement that works. A wantedBy here
      # lands in /etc/systemd/user/default.target.wants/, which is
      # root-owned and nix-managed — so `systemctl --user disable` could
      # not turn it off, and a login would start the production gateway
      # on top of whichever one you were actually using. `enable` writes
      # into ~/.config/systemd/user/default.target.wants/ instead, which
      # is the user's to change.
      systemd.user.services.${prodUnit} = lib.mkIf cfg.gateway.enable (
        lib.recursiveUpdate serviceBase {
          wantedBy = [ ];
        }
      );

      systemd.user.services.${devUnit} = lib.mkIf (cfg.gateway.enable && hasDevUnit) (
        lib.recursiveUpdate serviceBase {
          description = "Veles Agent gateway (dev tree)";
          wantedBy = [ ];
          serviceConfig = {
            ExecStart = "${devBin} gateway run";
            # systemd's answer to a missing ExecStart is
            # `status=203/EXEC`, which names no path and suggests
            # nothing. A dev tree that has not been built yet is the
            # ordinary case, so say so instead.
            # The path arrives as $1 so systemd expands `%h` on the unit
            # line; a specifier inside the script body would be a
            # literal two characters.
            ExecStartPre = "${preflight} ${devBin}";
          };
        }
      );

      # STABLE PATHS for the sandbox artifacts.
      #
      # Under user scope the module does NOT own config.toml — the user
      # edits it by hand and through `veles::config_set`, and a store
      # symlink would make both fail on a read-only file. But that left
      # `services.veles.sandbox.*` configuring precisely nothing, which
      # is the "a config line that does nothing is a lie" defect this
      # project keeps calling out, committed here in the module itself.
      #
      # The fix is a stable name. tmpfiles links each built artifact
      # into ~/.local/share/veles/sandbox/, so config.toml names a path
      # that does not change when the store path does:
      #
      #   [sandbox]
      #   backend     = "firecracker"
      #   kernel_path = "~/.local/share/veles/sandbox/firecracker-vmlinux"
      #   image       = "~/.local/share/veles/sandbox/firecracker-rootfs.img"
      #   network     = false
      #
      # nspawn is a COPY, not a link: systemd-nspawn takes a lock next
      # to the rootfs and a /nix/store path fails with "Failed to lock
      # …: Read-only file system".
      systemd.user.tmpfiles.rules =
        # `%h/.local/share`, NOT `%S`. systemd's %S is $XDG_STATE_HOME
        # (~/.local/state); veles' data_dir() is $XDG_DATA_HOME
        # (~/.local/share). Using %S put the artifacts one directory
        # away from where everything that reads them looks — the links
        # existed, and nothing found them.
        #
        # This does not honour a non-default $XDG_DATA_HOME, because
        # tmpfiles has no specifier for it. A user who moves that will
        # need to point [sandbox] at the store paths directly.
        let d = "%h/.local/share/veles/sandbox"; in
        [ "d ${d} 0755 - - -" ]
        ++ lib.optional (cfg.vault.identityDir != null)
          "d ${cfg.vault.identityDir} 0700 - - -"
        ++ lib.optionals (cfg.sandbox.backend == "firecracker") [
          "L+ ${d}/firecracker-vmlinux - - - - ${firecrackerKernel}"
          "L+ ${d}/firecracker-rootfs.img - - - - ${sandboxImage}"
        ]
        ++ lib.optionals (cfg.sandbox.backend == "nspawn") [
          "C+ ${d}/nspawn-root 0755 - - ${sandboxImage}"
        ];
    })

    (mkIf cfg.installConfig {
      # Only meaningful for the system scope; a user-scope install edits
      # its own config.toml (see the option's description).
      environment.etc."veles/config.toml".source = configFile;
    })
  ]);
}
