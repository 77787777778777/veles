# Sandbox artifacts the container/VM backends need, built with nix.
#
# WHY THIS FILE EXISTS
#
# Three of veles' five sandbox backends need something on disk before
# they can run at all: nspawn wants a root filesystem TREE with tclsh
# inside it, firecracker wants a kernel AND a rootfs IMAGE. Both were
# recorded in PENDING.md as "the backend + config contract exist; the
# artifacts don't". A fence nobody can start is not a fence — and it is
# why the five backends shipped 2026-09-03 with argv-pinning tests and
# no runtime evidence at all.
#
# Building them here, rather than documenting a debootstrap recipe,
# means `veles doctor --sandbox` can be pointed at a store path and
# actually execute a script through the fence.
{ pkgs, lib, nixpkgs }:

let
  # tclsh plus the shell the guest init needs. Deliberately small: the
  # sandboxed surface is pure-compute Tcl (the safe interp has no exec,
  # no open, no sockets), so a full userland would be weight with no
  # capability behind it. The DEV sandbox — where a model installs
  # packages — is the podman image (sandbox/Dockerfile, ubuntu LTS,
  # what models were RL'd on); these two are the hardened fences.
  guestPkgs = [ pkgs.tcl-8_6 pkgs.busybox ];

  # /bin and /sbin entries pointing into the image's own /nix/store.
  # `readlink -f` resolves the wrapper chains; the sed rewrites the
  # HOST store prefix to the guest's, which are the same path today but
  # need not be.
  # A usr-merged layout, NOT a bare /bin.
  #
  # `systemd-nspawn --directory` refuses a tree outright — *"doesn't
  # look like it has an OS tree (/usr/ directory is missing). Refusing."*
  # — so the binaries live in /usr/bin with /bin and /sbin as symlinks,
  # and os-release is written to /usr/lib/os-release (the location
  # nspawn actually probes) as well as /etc.
  linkFarmCommands = target: ''
    mkdir -p ${target}/{usr/bin,usr/lib,etc,proc,sys,dev,tmp,run,work,sbin}
    ln -sfn usr/bin ${target}/bin
    # /sbin is a REAL directory, not a third name for /usr/bin: busybox
    # ships an `init` applet, so the link farm below puts an `init`
    # symlink in /usr/bin, and writing OUR /sbin/init through a
    # /sbin → /usr/bin symlink followed that link into the read-only
    # store ("cp: cannot create regular file './files/sbin/init':
    # Permission denied"). Keeping them separate also means the guest
    # kernel's /sbin/init is unambiguously ours.
    for b in ${lib.concatStringsSep " " (map (p: "${p}/bin") guestPkgs)}; do
      for f in "$b"/*; do
        [ -e "$f" ] || continue
        ln -sf "$(readlink -f "$f")" "${target}/usr/bin/$(basename "$f")" || true
      done
    done
    # busybox ships mount/umount/poweroff/halt/sync/sh as applets; the
    # guest init calls them by name.
    for a in mount umount poweroff halt sync sh; do
      ln -sf ${pkgs.busybox}/bin/busybox ${target}/usr/bin/$a
    done
    cat > ${target}/usr/lib/os-release <<EOF
    NAME="veles-sandbox"
    ID=veles-sandbox
    VERSION_ID="1"
    PRETTY_NAME="veles sandbox rootfs"
    EOF
    ln -sf ../usr/lib/os-release ${target}/etc/os-release
    echo "root:x:0:0:root:/work:/bin/sh" > ${target}/etc/passwd
    echo "root:x:0:" > ${target}/etc/group
  '';

  # ── nspawn root filesystem ──────────────────────────────────────────
  #
  # `systemd-nspawn --directory` wants a tree, not an image, and it does
  # NOT share /nix/store with the host — so the closure is copied in,
  # not symlinked out.
  closure = pkgs.closureInfo { rootPaths = guestPkgs; };

  nspawnRootfs = pkgs.runCommand "veles-nspawn-rootfs"
    {
      meta.description = "Minimal tclsh rootfs for the veles nspawn sandbox backend";
    } ''
    mkdir -p $out/nix/store
    xargs -I % cp -a --reflink=auto % -t $out/nix/store/ < ${closure}/store-paths
    ${linkFarmCommands "$out"}
  '';

  # ── firecracker guest kernel ────────────────────────────────────────
  #
  # This is a GUEST kernel, not the host's — firecracker loads its own
  # vmlinux and the running kernel is untouched. It is nonetheless built
  # from whatever kernel the caller passes, and the NixOS module
  # defaults that to `config.boot.kernelPackages.kernel`: the same
  # source this machine already runs, so nix has usually fetched it
  # already and only the CONFIG differs.
  #
  # That delta is the whole point. The host kernel has
  #     CONFIG_VIRTIO_MMIO=m   CONFIG_VIRTIO_BLK=m   CONFIG_EXT4_FS=m
  # as MODULES, so a firecracker guest booting it without an initrd
  # cannot find its own root device. These force them in, plus the
  # 8250 serial console — the only channel out of a network-less
  # microVM, and therefore where the exit sentinel travels.
  firecrackerKernelFor = base:
    let
      k = base.override {
        structuredExtraConfig = with lib.kernel; {
          VIRTIO = yes;
          VIRTIO_PCI = yes;
          VIRTIO_MMIO = yes;
          VIRTIO_MMIO_CMDLINE_DEVICES = yes;
          VIRTIO_BLK = yes;
          EXT4_FS = yes;
          SERIAL_8250 = yes;
          SERIAL_8250_CONSOLE = yes;
          # A guest with no ACPI shuts down through the keyboard
          # controller; `reboot=k` in the backend's boot_args pairs
          # with these.
          SERIO_I8042 = yes;
          KEYBOARD_ATKBD = yes;
          # `MODULES = no` BELONGS HERE AND DOES NOT WORK. Tried
          # 2026-09-04, because this derivation builds a full distro
          # kernel — every driver for hardware this VM does not have,
          # a 21 GB build directory, the better part of an hour — to
          # extract one vmlinux for a guest whose entire world is two
          # virtio devices and a serial port.
          #
          # It fails in CONFIG GENERATION, before any of that:
          #
          #   linux-config-7.2.2.drv:  Error in reading or end of file.
          #
          # nixpkgs' generate-config.pl feeds canned answers to kconfig,
          # and turning module support off changes which prompts appear,
          # so the script runs out of input. `ignoreConfigErrors` does
          # not help — it governs the config CHECK, not its generation.
          #
          # The real route to a small guest kernel is `linuxManualConfig`
          # with an explicit `.config` checked into sandbox/firecracker/,
          # which is a different piece of work (and a file that has to be
          # maintained against kernel versions). Left as-is deliberately:
          # a big kernel that boots beats a small one that does not.
        };
        ignoreConfigErrors = true;
      };
    in
    pkgs.runCommand "veles-firecracker-vmlinux"
      {
        meta.description = "Firecracker guest kernel: virtio + ext4 built in";
        passthru.kernelVersion = k.version;
        passthru.baseKernel = k;
      } ''
      mkdir -p $out

      # PROVE THE OVERRIDE TOOK.
      #
      # `base.override { structuredExtraConfig = …; }` works for a
      # `buildLinux` kernel and SILENTLY DOES NOTHING for expressions
      # that take `...` and never forward it — which is what
      # nixpkgs' xanmod expression does. Found 2026-09-04 by booting the
      # result: the module's guest kernel had
      #     CONFIG_VIRTIO_MMIO=m  CONFIG_VIRTIO_BLK=m  CONFIG_EXT4_FS=m
      # exactly as the host kernel does, `strings vmlinux | grep -c
      # virtio_mmio` was 0, and the microVM panicked with
      #     VFS: Unable to mount root fs on "/dev/vda"
      #
      # An override that quietly does nothing is the same defect class
      # this project keeps finding in config files. Check the CONFIG
      # THAT WAS USED, and fail the build rather than shipping a kernel
      # that cannot boot as a guest.
      for opt in VIRTIO_MMIO VIRTIO_BLK EXT4_FS SERIAL_8250_CONSOLE; do
        if ! grep -q "^CONFIG_$opt=y$" ${k.configfile}; then
          echo "veles-firecracker-vmlinux: CONFIG_$opt is not =y in the kernel" >&2
          echo "  that was actually built (${k.configfile})." >&2
          echo "" >&2
          echo "  structuredExtraConfig did not reach this kernel expression." >&2
          echo "  A firecracker guest boots with no initrd, so a driver built as" >&2
          echo "  a module might as well not exist: the VM panics on" >&2
          echo "  'Unable to mount root fs on /dev/vda'." >&2
          echo "" >&2
          echo "  Pass a base kernel built with buildLinux (pkgs.linux_latest and" >&2
          echo "  friends honour .override; nixpkgs' xanmod expression does not)." >&2
          exit 1
        fi
      done
      # nixpkgs installs the unstripped ELF into the DEV output
      # (pkgs/os-specific/linux/kernel/build.nix: `cp vmlinux $dev/`).
      # Firecracker needs that ELF; a bzImage will not boot.
      if [ -f ${k.dev}/vmlinux ]; then
        cp ${k.dev}/vmlinux $out/vmlinux
      elif [ -f ${k}/vmlinux ]; then
        cp ${k}/vmlinux $out/vmlinux
      else
        echo "no vmlinux in either output of ${k.name} — firecracker needs" >&2
        echo "the ELF image, not bzImage" >&2
        exit 1
      fi
      chmod +w $out/vmlinux
      # The dev output's vmlinux is unstripped: 405 MB of DWARF measured
      # 2026-09-04. Firecracker loads the program headers and never
      # looks at debug info, so every byte of that is copied into the
      # store and read off disk at each VM boot for nothing.
      ${pkgs.stdenv.cc.bintools.bintools}/bin/strip --strip-debug $out/vmlinux
      echo "guest kernel: $(stat -c%s $out/vmlinux) bytes after --strip-debug"
    '';

  # ── firecracker guest rootfs ────────────────────────────────────────
  #
  # An ext4 image whose /sbin/init is sandbox/firecracker/init.sh: it
  # mounts the per-run work drive, runs the SAME safe-interp driver
  # every other backend runs, and prints the `VELES_SANDBOX_EXIT:<n>`
  # sentinel the host parses (`sandbox::split_console`).
  #
  # Read-only at run time — the backend declares the root drive RO and
  # the init takes its writable surfaces from tmpfs — so one image
  # serves any number of concurrent runs without them seeing each other.
  firecrackerRootfs = pkgs.callPackage "${nixpkgs}/nixos/lib/make-ext4-fs.nix" {
    storePaths = guestPkgs;
    volumeLabel = "veles-fc-root";
    populateImageCommands = ''
      ${linkFarmCommands "./files"}
      install -m 0755 ${../sandbox/firecracker/init.sh} ./files/sbin/init
    '';
  };
in
{
  inherit nspawnRootfs firecrackerKernelFor firecrackerRootfs;
  # Default base kernel when nothing passes one (the flake package). The
  # NixOS module overrides it with the host's own.
  firecrackerKernel = firecrackerKernelFor pkgs.linuxPackages_latest.kernel;
}
