# A portable Linux runtime, shared by the AppImage and tar.zst.
#
# Keep the closure's absolute /nix/store names. Racket's compiled-file-roots
# and config.rktd contain absolute source paths; rewriting them without
# recompiling the tree produces a slow or broken first boot. A STATIC
# nix-user-chroot bind-mounts the private closure, so those paths work on a
# host with no Nix installation. This does not give model commands a fence:
# user envs still use the configured podman backend.
{
  pkgs,
  system,
  velesPkg,
  racketRuntime,
  eclRuntime,
  echoPlugin,
  atprotoPlugin,
  podmanRemote,
  nixUserChroot,
  revision,
}:
let
  arch = pkgs.stdenv.hostPlatform.parsed.cpu.name;
  appimageArch =
    if arch == "x86_64" || arch == "aarch64" then
      arch
    else
      throw "the portable bundle supports x86_64 and aarch64 Linux, not ${system}";
  appimageToolSource = pkgs.fetchurl {
    url = "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${appimageArch}.AppImage";
    sha256 =
      if arch == "x86_64" then
        "sha256-lcvnzOlxf86QxITjQFLufH8ddjWzPBJSW0d2gmp9KbY="
      else
        "sha256-pZXqNM1hNsf1lencuxbz6XJddhDvueQ7OMPG6GyvwnA=";
  };
  runtimeSource = pkgs.fetchurl {
    url = "https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-${appimageArch}";
    sha256 =
      if arch == "x86_64" then
        "sha256-YXqncxTVfIuOT66cPws1r3sw1VKctp6Y9k+SUQ7ZikY="
      else
        "sha256-mnKnRNIhV2yTR+wHQkOsxcPHVoiWsScU4Ogunwv0eQA=";
  };
  # The official tool is itself a static AppImage. Extraction needs no
  # FUSE or Nix-specific loader patching; use its unpacked /usr tree.
  appimageTool = pkgs.runCommand "veles-appimagetool-${arch}" { } ''
    mkdir work
    cd work
    cp ${appimageToolSource} appimagetool.AppImage
    chmod +x appimagetool.AppImage
    ./appimagetool.AppImage --appimage-extract >/dev/null
    mkdir -p "$out"
    cp -a squashfs-root/usr/. "$out/"
    "$out/bin/appimagetool" --version
  '';
  bundleName = "veles-${velesPkg.version}-${revision}-linux-${arch}";
  # The licence texts travel at the bundle root (AppImage and archive alike).
  # Appended to an existing line, so a tree without them builds the same
  # derivation as before they existed.
  licenseFiles = builtins.filter builtins.pathExists [
    ../LICENSE
    ../THIRD_PARTY_LICENSES.md
  ];
  copyLicenses = pkgs.lib.concatMapStrings (f: " && cp '${f}' \"$out/${baseNameOf (toString f)}\"") licenseFiles;
  info = pkgs.writeText "veles-bundle-info.json" (
    builtins.toJSON {
      name = "veles";
      version = velesPkg.version;
      revision = revision;
      architecture = arch;
      runtime = "self-contained closure with a static user-namespace launcher";
      modules = [
        "irc"
        "slopdrop"
        "telegram"
        "atproto-cmd"
        "lang-js"
        "lang-racket"
        "lang-lisp"
        "echo-module"
      ];
      processPlugins = [
        "atproto"
        "echo"
      ];
      # The read-only bundled Tcl tier, in velesPkg's share/veles/plugins
      # (flake.nix bundledTclPlugins); asserted present in `tree` below.
      tclPlugins = [
        "tcl-anthropic-driver"
      ];
      sourcePackage = velesPkg;
    }
  );
  closure = pkgs.closureInfo {
    rootPaths = [
      velesPkg
      racketRuntime
      eclRuntime
      nixUserChroot
      echoPlugin
      atprotoPlugin
      podmanRemote
    ];
  };
  tree =
    pkgs.runCommand bundleName
      {
        nativeBuildInputs = [
          pkgs.binutils
          pkgs.file
        ];
      }
      ''
            set -eu
            mkdir -p "$out/nix/store" "$out/bin"
            while IFS= read -r p; do
              cp -a "$p" "$out/nix/store/"
            done < ${closure}/store-paths
            chmod -R u+w "$out"

            pkg="$out/nix/store/${baseNameOf velesPkg}"
            # Phone-only broker/client programs do not belong in this desktop and
            # server artifact. The process plugins are replaced with the checked
            # static-musl lane below; their ordinary dynamically linked copies
            # would otherwise drag a second TLS/runtime dependency into the tree.
            rm -f "$pkg/bin/ulsh" "$pkg/bin/veles-userland-broker" \
              "$pkg/bin/veles-gw" "$pkg/bin/veles-plugin-echo" "$pkg/bin/veles-plugin-atproto"
            test -x "$pkg/bin/veles"
            # The bundled Tcl plugins ride in the package, at the
            # <exe>/../share/veles/plugins plugins::bundled_roots reads.
            for p in tcl-anthropic-driver; do
              test -f "$pkg/share/veles/plugins/$p/plugin.toml" \
                || { echo "the bundle lost bundled Tcl plugin $p" >&2; exit 1; }
            done

            # Modules MUST come from the SAME velesPkg invocation as the core.
            # The normal discovery layout is <prefix>/lib/veles/modules/<name>/.
            for pair in "irc:irc" "slopdrop:slopdrop" "telegram:telegram" "atproto-cmd:atproto_cmd" "lang-js:lang_js" "lang-racket:lang_racket" "lang-lisp:lang_lisp" "echo-module:echo"; do
              m=''${pair%%:*}
              so="libveles_module_''${pair#*:}.so"
              mkdir -p "$pkg/lib/veles/modules/$m"
              ln -s "../../../$so" "$pkg/lib/veles/modules/$m/module.so"
              printf 'name = "%s"\nkind = "module"\nentry = "module.so"\ndescription = "In-process module %s"\n' "$m" "$m" \
                > "$pkg/lib/veles/modules/$m/plugin.toml"
            done
            mkdir -p "$pkg/lib/veles/plugins"
            cp -a ${echoPlugin}/lib/veles/plugins/echo "$pkg/lib/veles/plugins/"
            cp -a ${atprotoPlugin}/lib/veles/plugins/atproto "$pkg/lib/veles/plugins/"
            chmod -R u+w "$pkg/lib/veles/plugins"
            for p in echo atproto; do
              ln -s "../lib/veles/plugins/$p/veles-plugin-$p" "$pkg/bin/veles-plugin-$p"
            done

            # Strip OUR release objects, not the third-party runtime closure.
            # .veles_module_key is an ABI marker, not debug information.
            for f in "$pkg"/bin/* "$pkg"/lib/*.so "$pkg"/lib/veles/plugins/*/*; do
              [ -f "$f" ] || continue
              case "$(file -b "$f")" in
                *ELF*) strip --strip-unneeded "$f" ;;
              esac
            done
            for f in "$pkg"/lib/libveles_module_*.so; do
              key=$(objcopy -O binary --only-section=.veles_module_key "$f" /dev/stdout 2>/dev/null | tr -dc '0-9a-f' | head -c 64)
              [ "''${#key}" -eq 64 ] || { echo "stripping lost the module key: $f" >&2; exit 1; }
            done

            cp ${info} "$out/BUNDLE-INFO.json"${copyLicenses}
            cp ${../packaging/veles.svg} "$out/veles.svg"
            ln -s veles.svg "$out/.DirIcon"
            cat > "$out/veles.desktop" <<'DESKTOP'
        [Desktop Entry]
        Type=Application
        Name=Veles
        Comment=Rust core and Tcl scripting personal agent
        Exec=veles gui
        Icon=veles
        Terminal=false
        Categories=Utility;
        DESKTOP
            cat > "$out/bin/veles" <<'LAUNCH'
        #!/bin/sh
        set -eu
        if [ -n "''${VELES_BUNDLE_ROOT:-}" ]; then
            root=$VELES_BUNDLE_ROOT
        else
            here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
            root=$(CDPATH= cd -- "$here/.." && pwd -P)
        fi
        chroot="$root/nix/store/${baseNameOf nixUserChroot}/bin/nix-user-chroot"
        [ -x "$chroot" ] || { echo "veles bundle: missing launcher $chroot" >&2; exit 1; }
        # No local podman on a clean host: use the remote-only client. It still
        # requires the operator's CONTAINER_HOST; no service is started for them.
        if [ -z "''${VELES_PODMAN_BIN:-}" ] && ! command -v podman >/dev/null 2>&1; then
            VELES_PODMAN_BIN="$root/nix/store/${baseNameOf podmanRemote}/bin/podman"
            export VELES_PODMAN_BIN
        fi
        veles="$root/nix/store/${baseNameOf velesPkg}/bin/veles"
        # Root: nix-user-chroot's user namespace maps only uid 0, so a root run
        # could neither read a bundle under another account's home nor chown to a
        # service account. When root may make a private mount namespace, mount the
        # closure on /nix there, for this process tree only. Slave propagation:
        # mounts the host makes later (a user runtime dir from linger) still
        # reach it. No chroot: systemctl refuses to talk to its managers from one. A host without /nix gets an
        # empty /nix directory as the mount point. Otherwise (an unprivileged
        # container) keep the user-namespace launcher.
        if [ "$(id -u)" = 0 ] && [ -z "''${VELES_BUNDLE_ROOTNS:-}" ] \
            && command -v unshare >/dev/null 2>&1 \
            && unshare --mount --propagation private true 2>/dev/null; then
            [ -d /nix ] || mkdir -m 0755 /nix
            VELES_BUNDLE_ROOTNS=1 VELES_BUNDLE_ROOT=$root exec unshare --mount --propagation slave -- "$root/bin/veles" "$@"
        fi
        if [ "''${VELES_BUNDLE_ROOTNS:-}" = 1 ]; then
            VELES_BUNDLE_ROOTNS=2
            export VELES_BUNDLE_ROOTNS
            mount --bind "$root/nix" /nix
            exec "$veles" "$@"
        fi
        # Already inside that namespace: /nix is the closure.
        if [ "''${VELES_BUNDLE_ROOTNS:-}" = 2 ]; then
            exec "$veles" "$@"
        fi
        # nix-user-chroot restores the working directory and aborts when it
        # cannot (`sudo -u veles …` from another account's home): start in /.
        [ -x . ] || cd /
        exec "$chroot" "$root/nix" "$veles" "$@"
        LAUNCH
            chmod +x "$out/bin/veles"
            cat > "$out/AppRun" <<'APPRUN'
        #!/bin/sh
        set -eu
        root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
        VELES_BUNDLE_ROOT=$root
        export VELES_BUNDLE_ROOT
        exec "$root/bin/veles" "$@"
        APPRUN
            chmod +x "$out/AppRun"
            test -x "$out/nix/store/${baseNameOf velesPkg}/bin/veles"
            test -x "$out/nix/store/${baseNameOf nixUserChroot}/bin/nix-user-chroot"
            ! find "$out" -type f \( -name rustc -o -name cargo \) | grep .
            ! find "$out" -type d -path '*/lib/rustlib/src' | grep .
      '';
  archive =
    pkgs.runCommand "${bundleName}.tar.zst"
      {
        nativeBuildInputs = [
          pkgs.zstd
          pkgs.gnutar
        ];
      }
      ''
        mkdir -p "$out"
        tar --sort=name --format=gnu --mtime='@1' --owner=0 --group=0 \
          --numeric-owner --hard-dereference -cf - -C ${tree} . \
          | zstd -q -10 -T1 -o "$out/${bundleName}.tar.zst"
        cp ${info} "$out/${bundleName}.json"
        test -s "$out/${bundleName}.tar.zst"
      '';
  appimage =
    pkgs.runCommand "${bundleName}.AppImage"
      {
        nativeBuildInputs = [
          pkgs.squashfsTools
          pkgs.desktop-file-utils
        ];
        SOURCE_DATE_EPOCH = "1";
      }
      ''
        mkdir -p "$out"
        ${appimageTool}/bin/appimagetool --no-appstream --runtime-file ${runtimeSource} \
          ${tree} "$out/${bundleName}.AppImage"
        chmod +x "$out/${bundleName}.AppImage"
        cp ${info} "$out/${bundleName}.json"
        test -s "$out/${bundleName}.AppImage"
      '';
in
{
  inherit
    tree
    archive
    appimage
    info
    ;
}
