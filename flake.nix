{
  description = "Veles Agent — Rust core + Tcl scripting AI agent framework";

  # Flake skeleton copied from ~/szhrmk-flake/flake.nix (nixpkgs master +
  # flake-utils + fenix). From ~/szhrmk-flake/slopdrop/flake.nix we take ONLY
  # the Tcl-related deps and the TCL_LIBRARY/TCLLIBPATH/LIBCLANG env-var
  # dance. The slopdrop flake's own nixos-25.05 nixpkgs pin + rust-overlay
  # are deliberately NOT used (defunct server environment).
  #
  # PURITY RULE (user, 2026-08-09): everything comes from this flake, the
  # system, or the wrath profile. No hand-picked /nix/store paths. The C/C++
  # tooling is llvmPackages stdenv (clang/lld) as sanctioned by the user.
  inputs = {
#    nixpkgs.url = "github:NixOS/nixpkgs?rev=d2346664acd94bacb4e69eee02e238ea40c726ef";
    nixpkgs.url = "github:NixOS/nixpkgs?rev=3a3470d69417f777720d1c2dabc0e87b4eab2f59";
    utils.url = "github:numtide/flake-utils?rev=11707dc2f618dd54ca8739b309ec4fc024de578b";
    fenix = {
      url = "github:nix-community/fenix?rev=6bd8c2ef0033a19933ce0d0a4152a7d204e430d3";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      fenix,
      nixpkgs,
      utils,
    }@inputs:
    # nixosModules is system-INDEPENDENT, so it lives outside
    # eachDefaultSystem; everything else stays inside it.
    {
      # `services.veles` — see nix/module.nix. `self` is passed in so
      # the module can default `package` to this flake's own build and
      # reach nixpkgs for the ext4 builder.
      nixosModules.default = import ./nix/module.nix { self = self // { inherit inputs; }; };
      nixosModules.veles = self.nixosModules.default;
    }
    // utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
          overlays = [ fenix.overlays.default ];
        };

        # Explicit source filter for anything built in the nix sandbox.
        #
        # `src = ./.` carries the whole working tree into /nix/store.
        # `.gitignore` already keeps `target/` (36 GB here) and the OCI
        # tarballs (524 MB) out of flake evaluation, but that is opt-OUT:
        # one new un-ignored build artifact silently starts writing
        # gigabytes into the store on every build. This list is opt-IN,
        # so a new artifact has to be named here to get copied at all.
        rustSrc = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            # clippy's own config (await-holding-invalid-types): without
            # it the sandbox's clippy would pass what the local one denies.
            ./clippy.toml
            ./crates
            # The WHOLE plugins tree (64K), not just veles_api.tcl:
            #   - tclrt.rs `include_str!`s veles_api.tcl at compile time
            #     (sync_api_stub) — a bare `cargo build` finds it in the
            #     tree, the sandbox only gets what is named here, and the
            #     failure is a cryptic "couldn't read .../veles_api.tcl".
            #   - `the_sample_plugins_load_and_work` (tclrt.rs) loads
            #     plugins/examples/* through the REAL boot path. Naming
            #     only veles_api.tcl made `nix flake check` FAIL on a
            #     directory that plainly exists in the tree — the exact
            #     hazard this opt-in list's own comment predicts, found
            #     2026-09-04.
            #   - guards.rs `engine_sources()` walks plugins/**.tcl. That
            #     walk returns empty on a missing dir by design, so its
            #     absence made `no_unwired_guard_is_secretly_in_use` pass
            #     VACUOUSLY for any guard reached only from Tcl.
            ./plugins
            # Same vacuous-pass hazard: guards.rs walks veles-desktop
            # for .tcl, and tclCheck already copies this tree separately.
            # 1.8M of text.
            ./veles-desktop
            # sandbox/firecracker/init.sh is baked into the guest rootfs
            # derivation, and sandbox/Dockerfile is what
            # build-sandbox-image.sh feeds podman.
            ./sandbox
            # The [patch.crates-io] copy of irc-proto. Opt-in is the
            # right shape here — it is what keeps target/ out of the
            # store — but it does mean a new PATH DEPENDENCY is invisible
            # to nix while cargo is perfectly happy, and the build then
            # fails with "failed to read Cargo.toml" about a file that
            # plainly exists. Naming it here is the whole fix.
            ./vendor
            # The extension API's published spec, its changelog and the author
            # doc: crates/veles-cli/tests/ext_api.rs holds the three to the code
            # and to each other (PLAN-extension-api.md D7). Named file by file:
            # the rest of docs/ is prose no build reads.
            ./docs/extension-api.json
            ./docs/extension-api-changes.md
            ./docs/extending.md
            # veles-install `include_str!`s the reviewed WireGuard peer helper:
            # the pool-hub host component installs the same bytes the VPS
            # runbook and scripts/test-wg-peers.sh pin.
            ./deploy/vps/wg/veles-wg-peers.sh
            # examples/plugins/failover-backoff: the module-host case
            # `failover-example` copies it into a scratch plugins dir and
            # loads it through the real JS host (PLAN-extension-api.md E2).
            ./examples
          ];
        };

        # What compiling the engine reads, and nothing the tests read:
        # rustSrc without the desktop, the sandbox, the example plugins
        # and the engine's runtime catalogs (only en.yml is compiled in,
        # i18n.rs). A derivation's hash covers its whole source, so on
        # rustSrc every edit to a desktop .tcl file cross-built the
        # Android engine again (219 s for arm64 here, 2026-10-01) before
        # the APK could be repacked with the one file that changed; the
        # repack alone is 15 s.
        # scripts/android-rebuild-scope.sh (precommit) edits the desktop,
        # android/desktop.tcl, a catalog and the README in a copy of the
        # tree and fails if anything but the APK's assembly changes, and
        # fails if the engine `include_str!`s a file not named here.
        engineSrc = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            (pkgs.lib.fileset.difference ./crates ./crates/veles-core/locales)
            ./crates/veles-core/locales/en.yml
            # tclrt.rs `include_str!`s it (sync_api_stub).
            ./plugins/veles_api.tcl
            # veles-install `include_str!`s it (pool-hub host component).
            ./deploy/vps/wg/veles-wg-peers.sh
            ./vendor
          ];
        };

        # Rust toolchain from fenix — STABLE, deliberately.
        # (nightly cargo >=1.99 switched the build-dir layout to
        # build/<pkg>/<hash>/out, which breaks `inwelling` 0.5.5's
        # locate_manifest_paths (skip(2) assumes the flat layout) and
        # leaves the tcl crate's clib bindings empty. stable keeps the
        # flat layout that clib/inwelling were written for.)
        rustToolchain = fenix.packages.${system}.stable.withComponents [
          "rustc"
          "cargo"
          "rust-std"
          "rust-src"
          "rust-analyzer"
          "clippy"
          "rustfmt"
        ];

        # ── Android (PLAN-android.md, phase A) ─────────────────────────────
        # USER 2026-09-30: "This is a NixOS system, you're in a nix flake,
        # add the aarch64-android target [...] fetch [the NDK]".
        #
        #   nix develop .#android
        #   cargo build --target aarch64-linux-android -p veles-cli
        #
        # - Its own nixpkgs import, cross to aarch64 Android: the NDK's
        #   licence has to be accepted (android_sdk.accept_license), and
        #   the default shell and the checks never evaluate it.
        # - API 24 (Android 7), Termux's floor. nixpkgs' own
        #   aarch64-android-prebuilt example says 35, and a binary built
        #   for 35 runs on Android 15 and nothing older.
        # - The NDK comes from nixpkgs, its binaries patched for NixOS.
        #   Android Studio's NDK does not run here without nix-ld, and the
        #   one in ~/Android/Sdk is r21, older than Rust's r25 floor.
        # - The linker is nixpkgs' wrapper around the NDK's clang, so the
        #   sysroot and the API level come with it.
        # - Tcl is cross-built by nixpkgs; the tcl crate finds it through
        #   the TARGET's pkg-config path, so build scripts and proc macros
        #   keep linking the host's Tcl. nixpkgs' recipe also cross-builds
        #   bash (for shebangs) and tzdata (a path in clock.tcl), and
        #   neither builds for Android below API 35 (bash: no mblen before
        #   26; tzdata: bionic's own localtime_rz) — measured 2026-09-30.
        #   Neither is needed to link against libtcl, so both are dropped;
        #   clock.tcl looks where Termux keeps its zoneinfo.
        #
        # The APK (USER 2026-09-30: "an apk contains the necessary
        # dependencies and installs a system service [...] push through
        # adb and ideally at some future point release this in a store"):
        #
        #   nix build .#veles-apk          → result/veles-{debug,release}-unsigned.apk
        #   scripts/android-install.sh     signs the debug one, adb install
        #
        # One cross set per ABI the APK can carry: arm64-v8a is the phone,
        # x86_64 the emulator the smoke test boots
        # (scripts/android-emulator-smoke.sh).
        androidApi = "24";
        androidAbis = {
          arm64-v8a = {
            config = "aarch64-unknown-linux-android";
            rustTarget = "aarch64-linux-android";
          };
          x86_64 = {
            config = "x86_64-unknown-linux-android";
            rustTarget = "x86_64-linux-android";
          };
        };
        androidFor =
          abi:
          let
            a = androidAbis.${abi};
            cross = import nixpkgs {
              inherit system;
              config = {
                allowUnfree = true;
                android_sdk.accept_license = true;
              };
              crossSystem = {
                inherit (a) config;
                androidSdkVersion = androidApi;
                androidNdkVersion = "27";
                useAndroidPrebuilt = true;
                rust.rustcTarget = a.rustTarget;
              };
            };
            # nixpkgs' wrapper passes -mtls-dialect=gnu2 on x86 when clang
            # is 19.1 or newer, but compares the NDK's version (27) where
            # clang's belongs: the NDK's clang is 18 and rejects the flag
            # for x86_64 Android ("unsupported argument 'gnu2'", measured
            # 2026-09-30). Taken out of the x86_64 wrapper only, so the
            # arm64 build is the one nixpkgs gives.
            isx86 = cross.stdenv.hostPlatform.isx86;
            cc =
              if isx86 then
                cross.stdenv.cc.overrideAttrs (old: {
                  postFixup = (old.postFixup or "") + ''
                    substituteInPlace $out/nix-support/add-local-cc-cflags-before.sh \
                      --replace-fail " '-mtls-dialect=gnu2'" ""
                  '';
                })
              else
                cross.stdenv.cc;
            crossTcl =
              if isx86 then
                cross.tcl-8_6.override { stdenv = cross.overrideCC cross.stdenv cc; }
              else
                cross.tcl-8_6;
            # cargo, cc-rs and pkg-config spell the target in their
            # variable names: CC_aarch64_linux_android,
            # CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER.
            var = builtins.replaceStrings [ "-" ] [ "_" ] a.rustTarget;
            VAR = pkgs.lib.toUpper var;
            # Google Play refuses native code that cannot load on a 16 KB
            # page kernel (Android 15 devices); NDK r27 still links for
            # 4 KB unless asked.
            pages16k = "-Wl,-z,max-page-size=16384";
            # SQLite for the APK. rusqlite links the shared libsqlite3 on
            # every target (crates/veles-core's Cargo.toml says why), and
            # the APK carries it beside libtcl8.6.so. The source and the
            # flags are nixpkgs' sqlite's, so the phone runs the SQLite the
            # laptop does: the host's tools make the amalgamation, the NDK
            # compiles it into one shared library. Not nixpkgs' cross
            # recipe: it builds sqlite's Tcl bindings, which cross-builds
            # Tcl and bash, and bash does not build below API 26.
            sqlite = pkgs.stdenv.mkDerivation {
              pname = "sqlite-${a.rustTarget}";
              inherit (pkgs.sqlite) version src;
              nativeBuildInputs = [
                pkgs.unzip
                pkgs.tcl-8_6
              ];
              configurePhase = "./configure --enable-update-limit --disable-readline --disable-tcl";
              # HAVE_*: what bionic has, which the amalgamation cannot probe
              # for. HAVE_USLEEP matters most: without it the busy handler
              # sleeps in whole seconds.
              buildPhase = ''
                make sqlite3.c
                ${cc}/bin/${cc.targetPrefix}cc -shared -fPIC -O2 \
                  ${pkgs.sqlite.env.NIX_CFLAGS_COMPILE} -DSQLITE_ENABLE_UPDATE_DELETE_LIMIT \
                  -DSQLITE_THREADSAFE=1 -DHAVE_USLEEP=1 -DHAVE_LOCALTIME_R=1 \
                  -DHAVE_GMTIME_R=1 -DHAVE_ISNAN=1 -D_POSIX_THREAD_SAFE_FUNCTIONS \
                  sqlite3.c -o libsqlite3.so -lm -ldl \
                  -Wl,-soname,libsqlite3.so ${pages16k}
              '';
              installPhase = ''
                install -Dm755 libsqlite3.so $out/lib/libsqlite3.so
                install -Dm644 sqlite3.h sqlite3ext.h -t $out/include
                mkdir -p $out/lib/pkgconfig
                cat > $out/lib/pkgconfig/sqlite3.pc <<EOF
                Name: SQLite
                Description: SQL database engine (veles, ${a.rustTarget})
                Version: ${pkgs.sqlite.version}
                Libs: -L$out/lib -lsqlite3
                Cflags: -I$out/include
                EOF
              '';
              dontFixup = true;
            };
            tcl = crossTcl.overrideAttrs (old: {
              buildInputs = [ ];
              postPatch = ''
                substituteInPlace library/clock.tcl \
                  --replace-fail "/usr/share/zoneinfo" "/data/data/com.termux/files/usr/share/zoneinfo" \
                  --replace-fail "/usr/share/lib/zoneinfo" "" \
                  --replace-fail "/usr/lib/zoneinfo" "" \
                  --replace-fail "/usr/local/etc/zoneinfo" ""
              '';
              preConfigure = (old.preConfigure or "") + ''
                export LDFLAGS="$LDFLAGS ${pages16k}"
              '';
            });
          in
          {
            inherit abi cc tcl sqlite;
            inherit (a) rustTarget;
            env = {
              "CARGO_TARGET_${VAR}_LINKER" = "${cc}/bin/${cc.targetPrefix}cc";
              "CC_${var}" = "${cc}/bin/${cc.targetPrefix}cc";
              "CXX_${var}" = "${cc}/bin/${cc.targetPrefix}c++";
              "AR_${var}" = "${cc}/bin/${cc.targetPrefix}ar";
              # The tcl crate (vendor/clib) and libsqlite3-sys ask pkg-config
              # for the TARGET.
              "PKG_CONFIG_PATH_${var}" = "${tcl}/lib/pkgconfig:${sqlite}/lib/pkgconfig";
              "PKG_CONFIG_ALLOW_CROSS_${var}" = "1";
              # bindgen runs the host's libclang for the target: it needs
              # the NDK's sysroot for tcl.h's <stdio.h>.
              "BINDGEN_EXTRA_CLANG_ARGS_${var}" = "--sysroot=${cc.cc}/toolchain/sysroot -I${clangResourceInclude}";
              # bindgen's include-path detection (rquickjs-sys turns it on)
              # runs the `clang` on PATH with `-E -v` and promotes every
              # dir it reports to `-isystem`, AHEAD of the sysroot above.
              # On PATH that is nixpkgs' WRAPPED host clang, which adds the
              # host glibc: the R9 APK build died in rquickjs-sys with
              # "gnu/stubs-32.h not found" from glibc's features.h
              # (2026-10-10). The unwrapped clang reports only its resource
              # dir and the NDK sysroot for this target, so clang-sys is
              # pointed at it; compiling still goes through the wrapper.
              CLANG_PATH = "${pkgs.llvmPackages.clang-unwrapped}/bin/clang";
              # libtcl8.6.so is looked for beside the binary first: in the
              # APK both are in the app's native library directory. Then
              # where Termux keeps it (`pkg install tcl`), so the same
              # binary runs there too.
              #
              # --sysroot: the toolchain without Android's libstd-*.so
              # (rustAndroidSysroot), so std is linked into
              # libveles_runtime.so.
              "CARGO_TARGET_${VAR}_RUSTFLAGS" = "--sysroot=${rustAndroidSysroot} -C link-arg=-Wl,-rpath,$ORIGIN:/data/data/com.termux/files/usr/lib -C link-arg=${pages16k}";
            };
          };
        androidArm64 = androidFor "arm64-v8a";

        # The Android toolchain without libstd's .so (R1 check 10). Rust's
        # prebuilt libstd-*.so for Android has 4 KB-aligned segments, which
        # a 16 KB page kernel will not load and the engine's check refuses.
        # With only the rlib to link, std goes into libveles_runtime.so,
        # and the engine and every module take it from there (cargo still
        # passes the runtime `-C prefer-dynamic`; there is no dylib to
        # prefer). A tree of symlinks, so nothing is copied.
        rustAndroidSysroot = pkgs.runCommand "rust-android-sysroot" { } ''
          cd ${rustToolchainAndroid}
          find -L . -type d -exec mkdir -p "$out/{}" \;
          find -L . ! -type d -exec ln -s "${rustToolchainAndroid}/{}" "$out/{}" \;
          rm -f $out/lib/rustlib/*-linux-android/lib/libstd-*.so
          test -n "$(ls $out/lib/rustlib/aarch64-linux-android/lib/libstd-*.rlib)"
        '';
        rustToolchainAndroid = fenix.packages.${system}.combine (
          [ rustToolchain ]
          ++ map (a: fenix.packages.${system}.targets.${a.rustTarget}.stable.rust-std) (
            builtins.attrValues androidAbis
          )
        );

        # The desktop's Tk: nixpkgs' 8.6 with Chinese and Japanese line
        # breaks (nix/tk-kinsoku.patch, which AndroWish's sdl2tk takes too).
        # Only what runs wish uses it. The Rust builds link and rpath the
        # stock libtk, which nothing there draws with, so the patch never
        # rebuilds them.
        tkDesktop = pkgs.tk-8_6.overrideAttrs (o: {
          patches = (o.patches or [ ]) ++ [ ./nix/tk-kinsoku.patch ];
        });

        # The dependencies veles actually needs — the szhrmk-flake devShell
        # minus everything that belonged to other projects (no CUDA, Vulkan,
        # COSMIC, onnxruntime, ffmpeg, pipewire, ...), plus the Tcl runtime
        # stack slopdrop proved out.
        buildInputs = with pkgs; [
          openssl # TLS for providers + tclcurl https
          zlib
          # R9's Racket host links libracketcs.a (racketRuntime) and these
          # beneath it; ncurses and GNU libiconv are Racket's, not ours.
          lz4
          libiconvReal
          ncurses
          sqlite # session/memory store (rusqlite)
          tcl-8_6 # the scripting core
          tk-8_6 # veles-desktop GUI toolkit
          xvfb # offscreen X for the Tcl tests (scripts/tcl_tests.sh)
                    # — they must never map a window on the real desktop
          tclPackages.tcllib
          tclPackages.tclx
          tclPackages.tclcurl
          # tkimg: JPEG/TIFF/BMP/ICO/PCX/TGA/XPM decoders for Tk photos.
          # Core Tk 8.6 reads PNG/GIF/PPM and nothing else, so without
          # this an attached .jpg — the commonest artifact there is —
          # could only ever be a filename in the transcript.
          tclPackages.tkimg
          # dwebp / resvg / heif-dec (see imageDecoders below) — the dev
          # shell has to decode what the installed app decodes, or the
          # suite passes against a capability the user does not have.
          libwebp
          resvg
          libheif.bin
          git # git-backed plugin state (slopdrop pattern), checkpoints
          cacert
        ];

        nativeBuildInputs = with pkgs; [
          pkg-config
          rustToolchain
          # The memory store shells out to `git` (git-backed memory.d/),
          # so the TEST phase needs it on PATH — buildInputs is not on
          # PATH at build time. Without this, five memory:: tests fail in
          # the nix sandbox with "git not available" while passing on a
          # developer box that happens to have git installed.
          git
          # `search_files` shells out to `rg` for the same reason, and
          # the same trap: ripgrep is already in the installed binary's
          # wrapper PATH (a declared RUNTIME dep), but that does not put
          # it on PATH during the test phase. Without this the partial-
          # results test fails in the sandbox with "is ripgrep
          # installed?" while passing on any developer box — which is
          # exactly how `nix flake check` caught it and precommit did not.
          ripgrep
          # The podman background-runner cancellation regression exercises
          # the image's actual `setsid` launch shape. The development shell
          # has util-linux already; the Nix build sandbox does not unless
          # it is declared here (the container image asserts it too).
          util-linux
          # wrapProgram for the installed binary (TCL_LIBRARY etc.)
          makeBinaryWrapper
          # C/C++ tooling for build scripts + linking: llvmPackages stdenv
          # (clang), plus libclang for bindgen (the tcl crate's clib probe).
          llvmPackages.stdenv
          llvmPackages.llvm
          llvmPackages.libclang
          llvmPackages.libclang.lib
        ];

        # clang's resource dir holds stddef.h & friends bindgen needs.
        # Its version tracks llvm's major version, resolved at eval time
        # so clang bumps don't rot the reference.
        clangMajor = nixpkgs.lib.versions.major pkgs.llvmPackages.llvm.version;
        clangResourceInclude =
          "${pkgs.llvmPackages.libclang.lib}/lib/clang/${clangMajor}/include";

        # R9's Racket host (crates/veles-lang-racket): Racket CS 9.3 embedded
        # through libracketcs.a. What the module needs at build time (the
        # archive, the headers) and at run time (the three boot files, the
        # collects, their compiled tree, etc/racket) — and nothing else of
        # racket-minimal's 186 MB (no launchers, no gracket). The install's
        # config names its own store path nine times (compiled-file roots
        # among them); rewritten to this derivation so the closure is this
        # derivation, not the whole package.
        racketRuntime =
          pkgs.runCommand "veles-racket-runtime-${pkgs.racket-minimal.version}" { } ''
            r=${pkgs.racket-minimal}
            mkdir -p $out/lib/racket $out/share/racket $out/etc
            cp $r/lib/libracketcs.a $out/lib/
            cp -r $r/include $out/include
            cp $r/lib/racket/petite.boot $r/lib/racket/scheme.boot $r/lib/racket/racket.boot $out/lib/racket/
            cp -r $r/lib/racket/compiled $out/lib/racket/compiled
            # The compiled tree is keyed by the SOURCES' absolute path (the
            # config's compiled-file-roots): the .zo of collects/racket/base.rkt
            # lives at compiled/nix/store/<THIS derivation>/share/racket/collects/
            # racket/compiled/base_rkt.zo, so the copied key (racket-minimal's
            # own name) is renamed to this one. Without it Racket finds no .zo
            # and compiles racket/base from source at EVERY boot, 10 s (found
            # 2026-10-10 by an strace with zero .zo opens).
            chmod u+w $out/lib/racket/compiled/nix/store
            mv $out/lib/racket/compiled/nix/store/$(basename $r) $out/lib/racket/compiled/nix/store/$(basename $out)
            test -e $out/lib/racket/compiled/nix/store/$(basename $out)/share/racket/collects/racket/compiled/base_rkt.zo
            cp -r $r/share/racket/collects $out/share/racket/collects
            cp -r $r/etc/racket $out/etc/racket
            chmod -R u+w $out/etc/racket
            sed -i "s|$r|$out|g" $out/etc/racket/config.rktd
          '';
        # The shared libraries beneath libracketcs.a (build.rs links them;
        # buildEnv's RUSTFLAGS bakes their rpaths, like Tcl's).
        racketLibs = [
          pkgs.lz4.lib
          pkgs.libiconvReal
          pkgs.zlib
          pkgs.ncurses
        ];
        racketLibDirs = nixpkgs.lib.concatMapStringsSep ":" (l: "${l}/lib") racketLibs;
        racketRpathFlags =
          nixpkgs.lib.concatMapStringsSep " " (l: "-C link-arg=-Wl,-rpath,${l}/lib") racketLibs;

        # R9's Common Lisp host (crates/veles-lang-lisp): ECL embedded
        # through libecl.so. What it needs at run time: the library (its
        # Boehm GC is inside), the encodings, and the contrib modules a
        # script may `require`. It leaves out the C compiler (cmp.fas),
        # which is why nixpkgs' ecl keeps gcc and binutils in its 407 MB
        # closure; scripts load as bytecode. Two references would pull that
        # closure back in. libecl names its own store path as the default
        # ECLDIR: it is rewritten away, and the engine sets ECLDIR. Each
        # contrib .fas is a shared object whose RUNPATH names the package:
        # it is pointed at this derivation.
        eclRuntime =
          pkgs.runCommand "veles-ecl-runtime-${pkgs.ecl.version}"
            { nativeBuildInputs = [ pkgs.patchelf pkgs.removeReferencesTo ]; }
            ''
              e=${pkgs.ecl}
              v=${pkgs.ecl.version}
              mkdir -p $out/lib/ecl-$v
              cp -P $e/lib/libecl.so* $out/lib/
              cp -r $e/include $out/include
              cp -r $e/lib/ecl-$v/encodings $out/lib/ecl-$v/
              cp $e/lib/ecl-$v/help.doc $out/lib/ecl-$v/
              for f in $e/lib/ecl-$v/*.fas; do
                b=$(basename $f .fas)
                case $b in cmp|ecl-quicklisp|ecl-curl) continue ;; esac
                cp $f $out/lib/ecl-$v/
                if [ -e $e/lib/ecl-$v/$b.asd ]; then cp $e/lib/ecl-$v/$b.asd $out/lib/ecl-$v/; fi
              done
              chmod -R u+w $out
              for f in $out/lib/ecl-$v/*.fas; do
                patchelf --set-rpath $out/lib:${pkgs.glibc}/lib $f
              done
              remove-references-to -t $e $out/lib/libecl.so.$v
              find $out/include -type f -exec remove-references-to -t $e {} +
              if grep -rl $e $out; then echo "still references $e" >&2; exit 1; fi
            '';

        # Env vars shared by the package build and the dev shell.
        # TCLLIBPATH uses a glob at shell time so it survives tcllib version
        # bumps on nixpkgs master (slopdrop hardcoded tcllib1.21).
        buildEnv = {
          # Real cosign for rejection tests; not a local signing identity.
          VELES_TEST_COSIGN = "${pkgs.cosign}/bin/cosign";
          PKG_CONFIG_PATH =
            "${pkgs.tcl-8_6}/lib/pkgconfig"
            + ":${pkgs.zlib.dev}/lib/pkgconfig"
            + ":${pkgs.openssl.dev}/lib/pkgconfig";
          TCL_LIBRARY = "${pkgs.tcl-8_6}/lib/tcl8.6";
          TK_LIBRARY = "${pkgs.tk-8_6}/lib/tk8.6";
          OPENSSL_DIR = "${pkgs.openssl.dev}";
          OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
          OPENSSL_INCLUDE_DIR = "${pkgs.openssl.dev}/include";
          # bindgen (clib, via the tcl crate) — libclang comes from the
          # flake's llvmPackages, never from hand-picked store paths.
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          BINDGEN_EXTRA_CLANG_ARGS = "-I${clangResourceInclude}";
          CPATH = "${pkgs.glibc.dev}/include:${pkgs.zlib.dev}/include";
          # R9's Racket host: the install build.rs links and the engine boots
          # from, and the library dirs build.rs searches.
          VELES_RACKET_HOME = "${racketRuntime}";
          VELES_RACKET_LIBS = racketLibDirs;
          # R9's Common Lisp host: the trimmed ECL build.rs links and the
          # engine boots from (its RUNPATH goes into RUSTFLAGS below).
          VELES_ECL_HOME = "${eclRuntime}";
          # Bake rpaths so binaries find libtcl8.6.so / libtk8.6.so outside
          # the shell too (the flake package wrapProgram covers installed
          # builds; this covers dev-shell runs). libsqlite3.so joins them:
          # rusqlite links the shared sqlite (crates/veles-core's Cargo.toml
          # says why), so every binary and test binary needs its lib dir.
          # The dev shell and the package each add where libveles_runtime.so
          # and libstd live (devShellRustflags, velesPkg).
          RUSTFLAGS = racketRpathFlags + " -C link-arg=-Wl,-rpath,${eclRuntime}/lib -C link-arg=-Wl,-rpath,${pkgs.tcl-8_6}/lib -C link-arg=-Wl,-rpath,${pkgs.tk-8_6}/lib -C link-arg=-Wl,-rpath,${pkgs.sqlite.out}/lib";
        };

        # Formats with no Tk image handler at all — tkimg 2.1 has no webp,
        # heic, avif or svg reader and no Tcl extension provides one. Each
        # has a small command-line decoder that writes a PNG, which Tk does
        # read; lib/media.tcl shells out to whichever of these is on PATH
        # and falls back to an artifact card when none is.
        # USER: "couldnt we add a webp decoder? surely there is a library,
        # even if it's not core."
        imageDecoders = [
          pkgs.libwebp # dwebp:    webp
          pkgs.resvg # resvg:    svg, svgz
          pkgs.libheif.bin # heif-dec: heic, heif, avif
        ];

        tcllibPathExpr =
          "$(echo ${pkgs.tclPackages.tclcurl}/lib ${pkgs.tclPackages.tkimg}/lib ${pkgs.tclPackages.tcllib}/lib/tcllib*)";

        # The veles binary (wrapped with TCL_LIBRARY etc.) — a let-binding so
        # both packages.default and the OCI image below can reference it.
        velesPkg = pkgs.rustPlatform.buildRustPackage rec {
          pname = "veles-agent";
          version = "0.1.0";
          src = rustSrc;
          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              # oooutlk/tcltk @ slopdrop's pinned rev (HEAD doesn't compile)
              "tcl-0.1.9" = "sha256-eDkfHA9eQyOYPQr7JyKT4im3V6WXWl8yscIhHvhuJYw=";
            };
          };
          inherit buildInputs nativeBuildInputs;
          # Everything but veles-runtime, which must build as a dependency
          # (scripts/precommit.sh's cargo test step says why).
          cargoBuildFlags = [
            "--workspace"
            "--exclude"
            "veles-runtime"
          ];
          env = buildEnv // {
            # Real verifier for the installer rejection fixture. Tests do
            # not sign locally or inherit an ambient cosign binary.
            VELES_TEST_COSIGN = buildEnv.VELES_TEST_COSIGN;
            # The gateway and the modules NEED libveles_runtime.so and
            # libstd-*.so; both are installed in $out/lib (postInstall).
            RUSTFLAGS = buildEnv.RUSTFLAGS + " -C link-arg=-Wl,-rpath,$ORIGIN/../lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-I${clangResourceInclude}";
            # tests/plugin_so.rs loads the REAL out-of-tree cdylib. With
            # this unset the test SKIPS when it cannot find the artifact,
            # which is how the Phase B ABI went untested in CI while the
            # test file claimed "the daily gate always runs this for
            # real". Set here, the skip becomes a failure.
            VELES_REQUIRE_PLUGIN_SO = "1";
          };

          # The package is the release binary and nothing else; the test
          # suite is `checks.tests` (testsCheck below). Until 2026-09-28
          # this ran `cargo test` in the RELEASE profile with thin LTO, so
          # every build of the package — including each `nixos-rebuild`,
          # whose `services.veles` takes this package — compiled ~35 test
          # and example binaries with full optimisation and cross-crate
          # LTO: 723 s and +8.0 GB of RAM on top of the 186 s, +4.6 GB
          # the binary itself costs (measured by a RUSTC_WRAPPER under GNU
          # time, `scripts/build-mem.sh`). USER, 2026-09-28: "a 30GB peak
          # for a glorified irc bot".
          doCheck = false;

          # The engine's translations are runtime files (i18n.rs
          # `locales_dir`): installed beside the binary, and the wrapper
          # names them too.
          #
          # libveles_runtime.so (and the module and plugin .so files) are in
          # $out/lib already: cargo's install hook copies every .so of the
          # build. libstd-*.so comes from the toolchain that built them
          # (rustToolchain, which nativeBuildInputs puts first on PATH).
          # The IRC MODULE (R4, crates/veles-module-irc) is one of those .so
          # files — assert it so a build that lost it fails here, not at
          # the VPS's first `veles plugins rescan`. And the gateway must
          # LINK no Rust dylib but the runtime: a crate with a dylib form
          # gets linked dynamically, and a module the gateway maps at
          # startup is the copy every later load of that module binds its
          # symbols to (the dual-form veles-irc of R4).
          postInstall = ''
            cp ${rustToolchain}/lib/rustlib/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/lib/libstd-*.so $out/lib/
            test -e $out/lib/libveles_runtime.so
            test -e $out/lib/libveles_module_irc.so
            test -e $out/lib/libveles_module_slopdrop.so
            test -e $out/lib/libveles_module_atproto_cmd.so
            test -e $out/lib/libveles_module_telegram.so
            test -e $out/lib/libveles_module_lang_js.so
            test -e $out/lib/libveles_module_lang_racket.so
            test -e $out/lib/libveles_module_lang_lisp.so
            # Binaries and shared libraries embed rust-src paths (panic
            # locations), which dragged the complete toolchain into every
            # OCI image. Auxiliary executables need the same treatment as
            # `veles` and the module `.so`s. The strings are informational;
            # rewriting the hash drops the build-only reference.
            find $out/bin $out/lib -type f \
              -exec ${pkgs.removeReferencesTo}/bin/remove-references-to -t ${rustToolchain} {} +
            if $READELF -d $out/bin/veles | grep NEEDED | grep -v 'libveles_runtime\.so' | grep -q 'libveles_'; then
              echo "the gateway links a module:" >&2
              $READELF -d $out/bin/veles | grep NEEDED >&2
              exit 1
            fi
            mkdir -p $out/share/veles
            cp -r ${./crates/veles-core/locales} $out/share/veles/locales
            # The bundled skills, one up from bin/ where
            # config::beside_binary("skills") looks (PLAN-windows.md §6).
            cp -r ${./skills} $out/skills
            wrapProgram $out/bin/veles \
              --set TCL_LIBRARY "${pkgs.tcl-8_6}/lib/tcl8.6" \
              --set TK_LIBRARY "${tkDesktop}/lib/tk8.6" \
              --set TCLLIBPATH "${tcllibPathExpr}" \
              --set WISH "${tkDesktop}/bin/wish" \
              --set VELES_DESKTOP_DIR "${./veles-desktop}" \
              --set VELES_LOCALES_DIR "$out/share/veles/locales" \
              --prefix PATH : ${pkgs.lib.makeBinPath ([ pkgs.git pkgs.openssh pkgs.cacert pkgs.tcl-8_6 pkgs.ripgrep pkgs.cosign ] ++ imageDecoders)}
          '';

          # The installed binary finds its translations. They are runtime
          # files now, so a wrong path here would pass every unit test and
          # ship an English-only binary; this runs the wrapped `veles`
          # the way a user does and reads German out of it.
          doInstallCheck = true;
          installCheckPhase = ''
            runHook preInstallCheck
            test -f $out/share/veles/locales/de.yml
            out_de=$(HOME=$TMPDIR/ic VELES_LOCALE=de $out/bin/veles memory doctor)
            echo "$out_de" | grep -q 'Speicher-Diagnose' \
              || { echo "installed veles rendered no German: $out_de" >&2; exit 1; }
            runHook postInstallCheck
          '';

          meta = with pkgs.lib; {
            description = "Veles Agent — Rust core + Tcl scripting AI agent framework";
            license = licenses.agpl3Only;
            maintainers = [ ];
            platforms = platforms.unix;
            mainProgram = "veles";
          };
        };

        # Rust OVERLAY modules (PLAN-extension-api.md E5): module crates
        # that live outside this repository, built INSIDE velesPkg's own
        # cargo invocation, so they carry its build key and load into the
        # gateway this produces (a module built in any other invocation is
        # refused: crates/veles-core/src/module.rs). The result is a whole
        # veles package with its own key: every module comes from it, the
        # stock ones included.
        #
        #   veles.lib.${system}.velesWithModules {
        #     modules = [ { name = "greeter"; src = ./my-module; } ];
        #   }
        #
        # Each `src` is a crate directory (Cargo.toml + src/) whose
        # `veles-runtime = { workspace = true }` resolves here. It may use
        # only veles-runtime's re-exports and crates already in this
        # Cargo.lock: the build is offline, and the lock only gains the
        # overlays' own path entries. Not part of the stable extension API
        # (docs/extending.md): an update can break an overlay's build.
        velesWithModules =
          { modules }:
          velesPkg.overrideAttrs (old: {
            pname = "veles-agent-with-overlays";
            postPatch =
              (old.postPatch or "")
              + pkgs.lib.concatMapStrings (
                m:
                # The name is a directory and a sed pattern below, and the
                # scaffold's module and crate name (`veles plugins new
                # <name> --kind overlay`).
                assert pkgs.lib.assertMsg (builtins.match "[a-z][a-z0-9-]*" m.name != null)
                  "velesWithModules: the module name `${m.name}` must be lowercase letters, digits and `-`, starting with a letter";
                ''
                cp -r ${m.src} crates/overlay-${m.name}
                chmod -R u+w crates/overlay-${m.name}
                sed -i 's|^members = \[$|members = [\n    "crates/overlay-${m.name}",|' Cargo.toml
                grep -q '"crates/overlay-${m.name}"' Cargo.toml
              ''
              ) modules;
            preBuild = (old.preBuild or "") + ''
              # The overlays' own lock entries: path crates only, offline,
              # against the vendored set of this lock.
              if ! cargo update --workspace --offline; then
                echo "veles overlay: a module depends on a crate this Cargo.lock does not have." >&2
                echo "An overlay may use only veles-runtime's re-exports and crates the lock already has." >&2
                exit 1
              fi
            '';
          });

        # ── Process plugins, static (PLAN-plugins-installer.md D2) ──────────
        # A process plugin links veles-types and the SDK — never veles-core,
        # never libtcl — so it builds as ONE static musl executable: no
        # interpreter, no RUNPATH, nothing from the store at run time. The
        # same file runs in the gateway image, on any Linux, and in the
        # release bundle (Phase 7), and ships without rebuilding either.
        #
        # Linked by rust-lld against the musl the target's rust-std carries
        # (self-contained), so no musl C toolchain is built. Build scripts
        # still run on the host, with the stdenv's cc.
        muslTarget = "${pkgs.stdenv.hostPlatform.parsed.cpu.name}-unknown-linux-musl";
        rustToolchainMusl = fenix.packages.${system}.combine [
          rustToolchain
          fenix.packages.${system}.targets.${muslTarget}.stable.rust-std
        ];
        staticPlugin =
          {
            crate,
            name,
            # The binary the crate builds, when it differs from the package
            # name (veles-atproto's binary is veles-plugin-atproto).
            bin ? crate,
          }:
          pkgs.stdenv.mkDerivation {
            pname = crate;
            inherit (velesPkg) version cargoDeps;
            src = engineSrc;
            nativeBuildInputs = [
              pkgs.rustPlatform.cargoSetupHook
              rustToolchainMusl
              pkgs.binutils
              pkgs.jq
            ];
            env."CARGO_TARGET_${pkgs.lib.toUpper (builtins.replaceStrings [ "-" ] [ "_" ] muslTarget)}_LINKER" =
              "${rustToolchainMusl}/lib/rustlib/${pkgs.stdenv.hostPlatform.config}/bin/rust-lld";
            buildPhase = ''
              runHook preBuild
              cargo build --release --offline --target ${muslTarget} -p ${crate} -j $NIX_BUILD_CORES
              runHook postBuild
            '';
            # Installed the way a bundle carries it: <prefix>/lib/veles/plugins/<name>/,
            # the gateway's last discovery root (pluginhost::default_roots).
            installPhase = ''
              runHook preInstall
              d=$out/lib/veles/plugins/${name}
              install -Dm755 target/${muslTarget}/release/${bin} $d/${bin}
              # The static executable is the entire runtime. Fenix's
              # combined toolchain otherwise remains in this derivation's
              # package references and drags rustc/cargo/rust-src into a
              # portable release, although the plugin cannot execute any
              # of them. Nothing here needs a build-time toolchain.
              find $out -type f -exec ${pkgs.removeReferencesTo}/bin/remove-references-to -t ${rustToolchainMusl} {} +
              printf 'name = "%s"\nkind = "process"\nentry = "%s"\n' ${name} ${bin} > $d/plugin.toml
              mkdir -p $out/bin
              ln -s ../lib/veles/plugins/${name}/${bin} $out/bin/${bin}
              runHook postInstall
            '';
            dontFixup = true;
            # Static means: no program interpreter and no shared library
            # wanted — and it runs, and knows its own name.
            doInstallCheck = true;
            installCheckPhase = ''
              f=$out/lib/veles/plugins/${name}/${bin}
              if readelf -lW $f | grep -q INTERP; then
                echo "$f has a program interpreter: not static" >&2; exit 1
              fi
              if readelf -dW $f | grep -q NEEDED; then
                echo "$f needs shared libraries: not static" >&2; exit 1
              fi
              # Read the complete manifest before inspecting it. grep -q
              # can close the pipe as soon as "name" appears, making the
              # plugin fail with EPIPE while printing later Knobs.
              if ! description=$($f --describe); then
                echo "$f --describe failed" >&2; exit 1
              fi
              if ! jq -e --arg expected "${name}" \
                '.name == $expected and .protocol == 1' \
                <<<"$description" >/dev/null; then
                echo "$f --describe does not name ${name} with protocol 1" >&2
                exit 1
              fi
            '';
          };

        # The rest of the runtime closure (git, openssh, tcl, openssl...)
        # lands in the image automatically: dockerTools copies the FULL
        # closure of every copyToRoot path, and the wrapper script baked
        # into `veles` references git/openssh in its PATH prefix.
        # ── checks (`nix flake check`) ─────────────────────────────────
        #
        # The project had NO automated verification of any kind before
        # this: 251 Rust tests and a 16-assertion Tcl suite existed, but
        # nothing ever ran them. The Tcl suite was sitting failing on a
        # real Tk bug, the tree had never been rustfmt-clean, and clippy
        # had 107 warnings hiding genuine dead code. These checks are the
        # gate that stops that recurring.

        # Tcl/Tk desktop suite. Tk `bbox` returns nothing on an unmapped
        # window, so the tests need a real X server — a throwaway Xvfb,
        # never the developer's session.
        tclCheck = pkgs.runCommand "veles-tcl-tests"
          {
            nativeBuildInputs = [
              tkDesktop
              pkgs.tcl-8_6
              pkgs.xorg.xvfb
              pkgs.tclPackages.tcllib
              pkgs.tclPackages.tclcurl
              pkgs.tclPackages.tkimg
            ] ++ imageDecoders;
          }
          ''
            cp -r ${./veles-desktop} desktop
            # uitest.tcl sources scripts/i18n-keys.tcl (the l10n extractor
            # the generator also uses) through ../../scripts — the suites
            # expect desktop/ and scripts/ as siblings, like the repo.
            cp -r ${./scripts} scripts
            chmod -R u+w desktop scripts
            # uitest.tcl reads the engine's slash-command table straight
            # out of the Rust tree to pin the desktop docs against it, AND
            # the i18n key set, which walks the whole `crates/` tree: the
            # catalog check extracts Rust `i18n::lookup(...)` keys so an
            # engine-only key is not mistaken for an orphan. Copying only
            # commands.rs made every engine key look orphaned HERE while
            # the same suite passed on the host — a sandbox-only failure
            # with no local signal, which is the worst kind.
            #
            # walk_rs skips any path matching *target*, and the flake's
            # source is git-tracked files only, so the src tree arrives
            # without build artifacts.
            mkdir -p crates/veles-core crates/veles-frontends
            cp -r ${./crates/veles-core/src} crates/veles-core/src
            # The surfaces (commands.rs among them) are their own crate
            # since 2026-09-28; the key walk and the table pin need both.
            cp -r ${./crates/veles-frontends/src} crates/veles-frontends/src
            cp -r ${./crates/veles-core/locales} crates/veles-core/locales
            # The sandbox has no /etc/fonts: every suite used to log
            # "Fontconfig error" and run on whatever fallback metrics
            # libfontconfig improvises — geometry assertions like
            # panetest's scroll-anchor pin were never written against
            # those, and fail under them. Noto, the family the host
            # they were written on resolves for sans (DejaVu measured
            # differently and flips the anchor pin), makes the sandbox
            # render like a machine that has fonts.
            export FONTCONFIG_FILE="$PWD/fonts.conf"
            cat > fonts.conf <<FONTCONFIG
            <?xml version="1.0"?>
            <!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
            <fontconfig>
              <dir>${pkgs.noto-fonts}/share/fonts</dir>
              <cachedir>$PWD/fccache</cachedir>
            </fontconfig>
            FONTCONFIG
            export HOME="$PWD"
            export TCL_LIBRARY="${pkgs.tcl-8_6}/lib/tcl8.6"
            export TK_LIBRARY="${tkDesktop}/lib/tk8.6"
            export TCLLIBPATH="${tcllibPathExpr}"
            # UTF-8 or Tcl decodes the catalogs as iso8859-1 (mojibake).
            export LANG=C.UTF-8 LC_ALL=C.UTF-8

            Xvfb :99 -screen 0 1024x768x24 &
            xvfb_pid=$!
            trap 'kill $xvfb_pid 2>/dev/null || true' EXIT
            # Wait for the server rather than sleeping a fixed amount.
            for _ in $(seq 1 50); do
              [ -e /tmp/.X11-unix/X99 ] && break
              sleep 0.1
            done

            export DISPLAY=:99 XVFB_RUNNING=1
            # `wish` exits 0 even when the script fails, and the output is
            # piped into tee — so assert on each suite's reported tally,
            # not on $?.
            for suite in i18ntest i18nkeysetest pathtest jointest mdtest mdspectest transcriptspectest uitest listtest emacstest titletest sessiontest texttest rendertest lsptest fgtest devtest mediatest chrometest statetest coachtest whichkeytest comptest tabtest irctest apittest insttest installtest sectest acltest setuptest cmdstest sandboxtest lspstarttest tiptest tipzonetest statustest statusbuildtest quotatest taskstest asktest panetest sidebartest sessionswintest overlaytest responsivitytest memfilestest streamtest androidtest cmdtreetest devicetest phonetest settingsformtest appstest; do
              wish desktop/lib/$suite.tcl | tee $suite.log
              grep -qE "^$suite: [0-9]+ passed, 0 failed" $suite.log \
                || { echo "FAIL: $suite did not report a clean tally" >&2; exit 1; }
            done
            mkdir -p $out && cp ./*.log $out/
          '';

        # cargo clippy with warnings denied. Shares velesPkg's vendored
        # deps so the lockfile/outputHashes stay in one place.
        #
        # ONE HEAVY BUILD AT A TIME. The release build, clippy and the
        # test build each hold several GB of rustc at their peak, and nix
        # runs `max-jobs` derivations at once, so `nix flake check` used
        # to stack them. Each one names the previous one's output in its
        # environment — a real input, so nix builds them in the order
        # velesPkg → clippy → tests and never two at once. The cost is
        # wall time; the Tcl suite and the eval-only checks still run
        # alongside.
        clippyCheck = velesPkg.overrideAttrs (old: {
          pname = "veles-agent-clippy";
          env = old.env // { VELES_CHECK_AFTER = "${velesPkg}"; };
          buildPhase = ''
            runHook preBuild
            cargo clippy --workspace --all-targets --all-features -- -D warnings
            runHook postBuild
          '';
          doCheck = false;
          installPhase = "touch $out";
          postInstall = "";
          dontFixup = true;
          doInstallCheck = false;
        });

        # The test suite, exactly as scripts/precommit.sh runs it: the dev
        # profile, no LTO. The release package no longer runs it (see
        # velesPkg's doCheck), and this is cheaper than the release run it
        # replaces — 162 s and +5.0 GB to build, against 723 s and +8.0 GB
        # (2026-09-28, from scratch, `-j 8`) — and it tests the build the
        # developer runs, with overflow checks and debug assertions on.
        testsCheck = velesPkg.overrideAttrs (old: {
          pname = "veles-agent-tests";
          env = old.env // { VELES_CHECK_AFTER = "${clippyCheck}"; };
          buildPhase = ''
            runHook preBuild
            # cargo gives no build-order guarantee for a sibling cdylib
            # with no dependency edge, so build the ones tests/plugin_so.rs
            # loads first. scripts/precommit.sh does the same.
            cargo build -p veles-plugin-sample-so -p veles-plugin-badabi-so \
              -p veles-plugin-anthropic-so -p veles-plugin-echo \
              --offline -j $NIX_BUILD_CORES
            # The device-bridge test runs the desktop's own api.tcl, which
            # needs tcllib's json: the same path the desktop wrapper sets.
            export TCLLIBPATH="${tcllibPathExpr}"
            # The tests' scratch homes and stores go where the local gate
            # puts them: tmpfs (the sandbox's /dev/shm), not $TMPDIR=/build
            # on the disk. On 2026-10-13 two of four sandbox runs timed out
            # in tests/device_trigger.rs because a concurrent image build
            # was writing to the same SSD: the gateway's synchronous=FULL
            # store made its boot take 21 s and a 60 s wait ran out. The
            # machine notes say the same for every write-heavy suite.
            if [ -d /dev/shm ] && [ -w /dev/shm ]; then
              export TMPDIR=$(mktemp -d /dev/shm/veles-tests.XXXXXX)
            fi
            # --exclude veles-runtime: scripts/precommit.sh says why.
            cargo test --workspace --exclude veles-runtime --all-features --offline \
              -j $NIX_BUILD_CORES -- --test-threads=$NIX_BUILD_CORES
            runHook postBuild
          '';
          doCheck = false;
          installPhase = "touch $out";
          postInstall = "";
          dontFixup = true;
          doInstallCheck = false;
        });

        sandboxArtifacts = import ./nix/sandbox-artifacts.nix {
          inherit pkgs;
          inherit (pkgs) lib;
          nixpkgs = nixpkgs.outPath;
        };

        fmtCheck = pkgs.runCommand "veles-rustfmt" { nativeBuildInputs = [ rustToolchain ]; } ''
          cp -r ${rustSrc} src && chmod -R u+w src && cd src
          cargo fmt --all --check
          touch $out
        '';

        # ── The Android APK ────────────────────────────────────────────
        #
        # Gradle-free: aapt2, javac, d8 and zipalign, the way the SDK's
        # own build-tools do it, so the whole APK is one nix build with no
        # dependency resolver reaching the network. What the app does is in
        # android/README.md.
        #
        # The build is UNSIGNED on purpose: a signing key does not belong
        # in the nix store (world-readable), and an unsigned build is what
        # a store's reproducibility check compares.
        androidHost = import nixpkgs {
          inherit system;
          config = {
            allowUnfree = true;
            android_sdk.accept_license = true;
          };
        };
        androidTargetSdk = "36";
        androidBuildTools = "36.1.0";
        # The app itself needs API 26 (notification channels, foreground
        # service starts); the engine binary runs from 24 (Termux).
        androidMinSdk = "26";
        # Raised by hand for each release a store sees; a sideloaded
        # reinstall of the same code keeps it.
        androidVersionCode = "1";
        androidSdk =
          (androidHost.androidenv.composeAndroidPackages {
            platformVersions = [ androidTargetSdk ];
            buildToolsVersions = [ androidBuildTools ];
            includeCmake = false;
          }).androidsdk;
        androidBuildToolsDir = "${androidSdk}/libexec/android-sdk/build-tools/${androidBuildTools}";
        # The smoke test's emulator: x86_64 under KVM. An arm64 image would
        # run the phone's binaries, but emulated instruction by instruction.
        # ps16k: a 16 KB page kernel, where 4 KB-aligned native code does
        # not load — what Google Play checks for.
        androidEmulatorImage = "google_apis_ps16k";
        androidEmulatorSdk =
          (androidHost.androidenv.composeAndroidPackages {
            platformVersions = [ androidTargetSdk ];
            buildToolsVersions = [ androidBuildTools ];
            includeEmulator = true;
            includeSystemImages = true;
            systemImageTypes = [ androidEmulatorImage ];
            abiVersions = [ "x86_64" ];
            includeCmake = false;
          }).androidsdk;

        # The engine for one ABI: the release `veles`, stripped, with the
        # Tcl it links. Built with velesPkg's vendored crates, offline.
        velesAndroid =
          abi:
          let
            t = androidFor abi;
          in
          pkgs.llvmPackages.stdenv.mkDerivation {
            pname = "veles-android-${abi}";
            inherit (velesPkg) version cargoDeps;
            src = engineSrc;
            nativeBuildInputs = [
              pkgs.rustPlatform.cargoSetupHook
              rustToolchainAndroid
              pkgs.pkg-config
              pkgs.tcl-8_6
              pkgs.llvmPackages.llvm
            ];
            env = {
              inherit (buildEnv) PKG_CONFIG_PATH TCL_LIBRARY LIBCLANG_PATH;
            } // t.env;
            buildPhase = ''
              runHook preBuild
              # The gateway AND the modules it loads, in ONE cargo invocation
              # (the build key and the runtime's symbols only match that way;
              # crates/veles-core/src/module.rs).
              cargo build --release --offline --target ${t.rustTarget} \
                -p veles-cli -p veles-module-irc -p veles-module-slopdrop -p veles-module-telegram \
                -p veles-module-lang-js \
                -j $NIX_BUILD_CORES
              runHook postBuild
            '';
            # The three things that pass every build and fail on the phone:
            # a library neither the APK nor Android carries (the ones
            # allowed are the NDK's stable system libraries, plus libtcl),
            # a runpath that does not find libtcl beside the binary, and
            # 4 KB segments.
            installPhase = ''
              runHook preInstall
              install -Dm755 target/${t.rustTarget}/release/veles $out/bin/veles
              llvm-strip --strip-all $out/bin/veles
              # The shared core (std is inside it: rustAndroidSysroot) and
              # the SQLite it links, beside the binary.
              install -Dm755 target/${t.rustTarget}/release/libveles_runtime.so $out/lib/libveles_runtime.so
              llvm-strip --strip-unneeded $out/lib/libveles_runtime.so
              # The modules (R6): the APK ships them beside the runtime; the
              # app writes a plugin.toml per module for the module host
              # (android/java/chat/janitor/veles/Engine.java).
              # R9 adds the JS host (lang-js; the crate's dylib spells it lang_js).
              for m in irc slopdrop telegram lang_js; do
                install -Dm755 target/${t.rustTarget}/release/libveles_module_$m.so $out/lib/libveles_module_$m.so
                llvm-strip --strip-unneeded $out/lib/libveles_module_$m.so
              done
              install -Dm755 ${t.sqlite}/lib/libsqlite3.so $out/lib/libsqlite3.so
              install -Dm755 ${t.tcl}/lib/libtcl8.6.so $out/lib/libtcl8.6.so
              install -Dm755 ${t.tcl}/bin/tclsh8.6 $out/bin/tclsh
              for f in $out/bin/veles $out/bin/tclsh $out/lib/libtcl8.6.so $out/lib/libveles_runtime.so $out/lib/libsqlite3.so $out/lib/libveles_module_*.so; do
                for lib in $(llvm-readelf -d $f | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
                  case $lib in
                    libc.so|libm.so|libdl.so|liblog.so|libz.so|libtcl8.6.so|libveles_runtime.so|libsqlite3.so) ;;
                    *) echo "$f needs $lib, which the APK does not carry" >&2; exit 1 ;;
                  esac
                done
                if llvm-readelf -lW $f | awk '$1 == "LOAD" && $NF != "0x4000" { bad = 1 } END { exit !bad }'; then
                  echo "$f has LOAD segments not aligned to 16 KB" >&2; exit 1
                fi
              done
              llvm-readelf -d $out/bin/veles | grep -q 'RUNPATH.*\$ORIGIN' \
                || { echo "veles's runpath does not start beside the binary" >&2; exit 1; }
              mkdir -p $out/share
              cp -r ${t.tcl}/lib/tcl8.6 $out/share/tcl8.6
              runHook postInstall
            '';
            dontFixup = true;
          };

        # ripgrep for one ABI, shipped as librg.so (the plan's Step 4.1:
        # "do both ripgrep on the phone and an .aab bundle now"). The
        # engine finds it through VELES_RG (Engine.java), the way it
        # finds tclsh through VELES_TCLSH; the terminal tool gets `rg`
        # on PATH through a symlink beside the engine's home.
        #
        # Source and vendored crates are nixpkgs' ripgrep (15.2.0), and
        # the cross environment is androidFor's — the same NDK, the same
        # guards. The output is NAMED lib*.so because Android extracts
        # only lib-prefixed files from the APK's lib/ directory.
        velesRgAndroid =
          abi:
          let
            t = androidFor abi;
          in
          pkgs.llvmPackages.stdenv.mkDerivation {
            pname = "veles-rg-android-${abi}";
            version = pkgs.ripgrep.version;
            src = pkgs.ripgrep.src;
            cargoDeps = pkgs.ripgrep.cargoDeps;
            nativeBuildInputs = [
              pkgs.rustPlatform.cargoSetupHook
              rustToolchainAndroid
              pkgs.llvmPackages.llvm
              t.cc
            ];
            env = t.env;
            buildPhase = ''
              runHook preBuild
              cargo build --release --offline --target ${t.rustTarget} -j $NIX_BUILD_CORES
              runHook postBuild
            '';
            # The same two guards the engine's build runs: only system
            # libraries an APK may expect, and 16 KB-aligned segments
            # (a ps16k kernel refuses to load anything else).
            installPhase = ''
              runHook preInstall
              install -Dm755 target/${t.rustTarget}/release/rg $out/lib/librg.so
              llvm-strip --strip-all $out/lib/librg.so
              for lib in $(llvm-readelf -d $out/lib/librg.so | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
                case $lib in
                  libc.so|libm.so|libdl.so|liblog.so|libz.so) ;;
                  *) echo "librg.so needs $lib, which Android does not carry" >&2; exit 1 ;;
                esac
              done
              if llvm-readelf -lW $out/lib/librg.so | awk '$1 == "LOAD" && $NF != "0x4000" { bad = 1 } END { exit !bad }'; then
                echo "librg.so has LOAD segments not aligned to 16 KB" >&2; exit 1
              fi
              runHook postInstall
            '';
            dontFixup = true;
          };

        # busybox and git (below) ship for arm64-v8a only, the phones this
        # is for: under x86_64 Android's filter for app processes, static
        # musl binaries die (SIGSYS listing busybox's applets, SIGSEGV
        # running busybox or git; the emulator, 2026-10-12), while on the
        # real phone (arm64, Android 16) both run inside the engine. The
        # rest of the static toolbox (bash, jq, curl, sqlite3, ssh — below
        # git) is arm64-only for the same reason; the emulator keeps mksh.
        #
        # The static package set every phone tool comes from.
        androidStaticPkgs =
          abi:
          if abi == "arm64-v8a" then
            pkgs.pkgsCross.aarch64-multiplatform-musl.pkgsStatic
          else
            pkgs.pkgsStatic;

        # A package relinked with 16 KB pages. The cache's static builds
        # are 64 KB-aligned on aarch64 and 4 KB on x86_64; the guard below
        # wants exactly 16 KB (a ps16k kernel, and Play's rule). Some
        # expressions set NIX_LDFLAGS under `env` (openssh does, static),
        # where a top-level one would clash, so it goes where it already is.
        pages16k =
          drv:
          drv.overrideAttrs (
            o:
            let
              flags = " -z max-page-size=16384 -z common-page-size=16384";
            in
            if (o.env or { }) ? NIX_LDFLAGS then
              { env = o.env // { NIX_LDFLAGS = o.env.NIX_LDFLAGS + flags; }; }
            else
              { NIX_LDFLAGS = (o.NIX_LDFLAGS or "") + flags; }
          );

        # The two guards every static tool in the APK passes, as a shell
        # snippet over the files named: no NEEDED entry at all (a static
        # binary needs nothing Android does not carry), and every LOAD
        # segment at 0x4000 (a 16 KB-page kernel refuses anything else;
        # the default aarch64 link says 0x10000). Needs llvm-readelf.
        checkAndroidStatic = files: ''
          for f in ${files}; do
            if llvm-readelf -d $f | grep -q NEEDED; then
              echo "$f is not static" >&2; exit 1
            fi
            if llvm-readelf -lW $f | awk '$1 == "LOAD" && $NF != "0x4000" { bad = 1 } END { exit !bad }'; then
              echo "$f has LOAD segments not aligned to 16 KB" >&2; exit 1
            fi
          done
        '';

        # One static tool for the APK: `files` maps each lib*.so name the
        # payload ships to the binary it is, stripped and guarded.
        # Android extracts only lib-prefixed .so files from lib/(abi), so
        # every program is named like a library; Engine.java's linkTools
        # gives each its real name in files/bin.
        androidStaticTool =
          name: files:
          pkgs.runCommand "veles-${name}"
            {
              nativeBuildInputs = [ pkgs.llvmPackages.llvm ];
            }
            ''
              mkdir -p $out/lib
              ${pkgs.lib.concatStrings (
                pkgs.lib.mapAttrsToList (so: bin: ''
                  install -Dm755 ${bin} $out/lib/${so}
                  llvm-strip --strip-all $out/lib/${so}
                '') files
              )}
              ${checkAndroidStatic "$out/lib/*.so"}
            '';

        #
        # busybox for one ABI, shipped as libbusybox.so beside librg.so
        # (PLAN-sessions-and-phone.md A3, "a better shell"): awk, vi, less,
        # wget and the rest of what Android's toybox lacks. Engine.java
        # links each applet into files/bin, which is FIRST on PATH: a
        # toybox twin of an applet (ls, ps, sed, grep …) takes other flags,
        # and the agent writes for the GNU/busybox ones (HANDOFF pitfall 77).
        # Toybox-only commands (getprop, am, pm, logcat) still resolve.
        #
        # nixpkgs' static musl busybox, relinked with 16 KB pages: the
        # cache's is 4 KB-aligned on x86_64, which a ps16k kernel (the
        # smoke's emulator, and Play's rule) will not load. Static, so it
        # needs nothing Android does not carry.
        velesBusyboxAndroid =
          abi:
          let
            static = androidStaticPkgs abi;
            # No setuid support: busybox drops privileges before an applet
            # runs (setgid, setuid), and Android's seccomp filter for app
            # processes kills that with SIGSYS — `busybox --list` ran in
            # the app, `busybox true` died (the real phone, 2026-10-12).
            bb = pages16k (
              static.busybox.override {
                extraConfig = "CONFIG_FEATURE_SUID n";
              }
            );
          in
          androidStaticTool "busybox-android-${abi}" {
            "libbusybox.so" = "${bb}/bin/busybox";
          };

        # git for one ABI, shipped as libgit.so and libgit_remote_http.so
        # (PLAN-sessions-and-phone.md A3: plugin rollback, git installs and
        # the Review pane need it on the phone). Engine.java links `git`
        # into files/bin and the remote helpers into GIT_EXEC_PATH.
        #
        # Static against musl, built from nixpkgs' git source with no Perl,
        # Python, Tcl/Tk, gettext, expat or Rust parts, and HTTPS through
        # nixpkgs' static curlMinimal and openssl (the certificates are the
        # engine's SSL_CERT_DIR). 16 KB pages, like busybox. Stripped: 4.5 MB
        # for git, 11 MB for the HTTPS helper (x86_64, 2026-10-12).
        velesGitAndroid =
          abi:
          let
            static = androidStaticPkgs abi;
            arch =if abi == "arm64-v8a" then "aarch64" else "x86_64";
            git = static.stdenv.mkDerivation {
              pname = "veles-git-static";
              inherit (pkgs.git) version src;
              buildInputs = [
                static.zlib
                static.curlMinimal
                static.openssl
                static.nghttp2
                static.libssh2
              ];
              enableParallelBuilding = true;
              dontConfigure = true;
              makeFlags = [
                "prefix=/"
                "NO_PERL=YesPlease"
                "NO_PYTHON=YesPlease"
                "NO_TCLTK=YesPlease"
                "NO_GETTEXT=YesPlease"
                "NO_EXPAT=YesPlease"
                "NO_REGEX=YesPlease"
                "NO_RUST=YesPlease"
                "NO_INSTALL_HARDLINKS=YesPlease"
                "INSTALL_SYMLINKS=YesPlease"
                "uname_S=Linux"
                "uname_M=${arch}"
                "CC=${static.stdenv.cc.targetPrefix}cc"
                "AR=${static.stdenv.cc.targetPrefix}ar"
                "CURL_CONFIG=${static.curlMinimal.dev}/bin/curl-config"
              ];
              # Words with spaces: a makeFlags list is split on them.
              preBuild = ''
                makeFlagsArray+=(
                  "CURL_LDFLAGS=-lcurl -lnghttp2 -lssh2 -lssl -lcrypto -lz"
                  "LDFLAGS=-static -Wl,-z,max-page-size=16384 -Wl,-z,common-page-size=16384"
                )
              '';
              installFlags = [ "DESTDIR=$(out)" ];
              dontFixup = true;
              doCheck = false;
            };
          in
          androidStaticTool "git-android-${abi}" {
            "libgit.so" = "${git}/bin/git";
            "libgit_remote_http.so" = "${git}/libexec/git-core/git-remote-http";
          };

        # ── The rest of the phone's static toolbox (PLAN-phone-userland.md
        # T1) ── what a model expects of a Linux shell and Android has not
        # got: bash (the shell the terminal tool runs; the phone's was
        # mksh), jq, a curl CLI, the sqlite3 CLI, and the OpenSSH client.
        # Each is nixpkgs' static musl package relinked with 16 KB pages
        # and guarded like busybox and git; Engine.java's linkTools names
        # each in files/bin. No package manager can follow them: at
        # targetSdk 36 nothing in the app's own storage may execute, so
        # what the APK ships is what there is (Tier 3, a separate package,
        # is the way past that).

        # bash, as libbash.so: VELES_SHELL and SHELL point at its files/bin
        # name, so the terminal, background shells, monitors and the
        # sandbox's memory wrapper all run bash, as on a laptop.
        velesBashAndroid =
          abi:
          let
            bash = pages16k (androidStaticPkgs abi).bash;
          in
          androidStaticTool "bash-android-${abi}" {
            "libbash.so" = "${bash}/bin/bash";
          };

        velesJqAndroid =
          abi:
          let
            jq = pages16k (androidStaticPkgs abi).jq;
          in
          androidStaticTool "jq-android-${abi}" {
            "libjq.so" = "${jq.bin}/bin/jq";
          };

        # The curl CLI: the same static curlMinimal git's HTTPS helper
        # links (velesGitAndroid), relinked with 16 KB pages for its
        # program. Static musl cannot resolve names on Android, so
        # Engine.java writes it a curlrc that goes out through the engine's
        # CONNECT proxy, as git does, with the CA bundle git uses.
        velesCurlCliAndroid =
          abi:
          let
            curl = pages16k (androidStaticPkgs abi).curlMinimal;
          in
          androidStaticTool "curl-cli-android-${abi}" {
            "libcurl_cli.so" = "${curl.bin}/bin/curl";
          };

        # The sqlite3 CLI as libsqlite3_cli.so: libsqlite3.so is already
        # the engine's sqlite LIBRARY (velesAndroid), and the two must not
        # share a name in lib/(abi).
        velesSqliteCliAndroid =
          abi:
          let
            sqlite = pages16k (androidStaticPkgs abi).sqlite;
          in
          androidStaticTool "sqlite3-cli-android-${abi}" {
            "libsqlite3_cli.so" = "${sqlite.bin}/bin/sqlite3";
          };

        # The OpenSSH client: ssh and scp. Two changes to nixpkgs' static
        # build, both for a phone:
        #   - scp runs ssh by NAME (SSH_PROGRAM=ssh; it execvp()s it), not
        #     the store path the build bakes in, which no phone has.
        #   - Android has no /etc/passwd, so musl's getpwuid() finds no
        #     user and ssh refuses to start ("No user exists for uid …";
        #     scp: "unknown user"). A header forced into every compile
        #     answers from the environment when libc cannot: HOME for
        #     ~/.ssh, USER, SHELL.
        # Static musl cannot resolve names here either: ssh reaches hosts
        # by address (the pool's tailnet IPs), PLAN-phone-userland.md T1.
        velesSshAndroid =
          abi:
          let
            passwdShim = pkgs.writeText "veles-android-passwd.h" ''
              #ifndef VELES_ANDROID_PASSWD_H
              #define VELES_ANDROID_PASSWD_H
              /* pwd.h only: openssh's config.h renames calloc and friends
                 (rpl_calloc) AFTER this header, so a stdlib.h pulled in
                 here would leave the renamed ones undeclared. */
              #include <pwd.h>
              extern char *getenv(const char *);
              extern gid_t getgid(void);
              static inline struct passwd *veles_getpwuid(uid_t uid) {
                static struct passwd pw;
                struct passwd *real = getpwuid(uid);
                const char *v;
                if (real)
                  return real;
                pw.pw_name = (char *)((v = getenv("USER")) && *v ? v : "veles");
                pw.pw_passwd = (char *)"*";
                pw.pw_uid = uid;
                pw.pw_gid = getgid();
                pw.pw_gecos = (char *)"";
                pw.pw_dir = (char *)((v = getenv("HOME")) && *v ? v : "/");
                pw.pw_shell = (char *)((v = getenv("SHELL")) && *v ? v : "/system/bin/sh");
                return &pw;
              }
              #define getpwuid veles_getpwuid
              #endif
            '';
            ssh = (pages16k (androidStaticPkgs abi).openssh).overrideAttrs (o: {
              makeFlags = (o.makeFlags or [ ]) ++ [ "SSH_PROGRAM=ssh" ];
              # At build time only: configure's own test programs must see
              # the real getpwuid.
              preBuild = (o.preBuild or "") + ''
                export NIX_CFLAGS_COMPILE="$NIX_CFLAGS_COMPILE -include ${passwdShim}"
              '';
            });
          in
          androidStaticTool "ssh-android-${abi}" {
            "libssh.so" = "${ssh}/bin/ssh";
            "libscp.so" = "${ssh}/bin/scp";
          };

        # ── The desktop frontend's Tk (PLAN-android.md, phase C) ──────────
        # USER 2026-09-26: "Android gateway AND android frontend. (maybe
        # androidwish/tk on android to keep single codebase)".
        #
        # AndroWish (https://androwish.org, Christian Werner's Tcl/Tk for
        # Android) draws Tk through SDL2 and mounts the APK itself at
        # /assets, where it runs /assets/app/main.tcl. So veles-desktop
        # rides in the APK as assets, unchanged (android/desktop.tcl).
        #
        # Built from its 2024 release source, not taken from its APKs:
        # those are built by NDK r16 for API 14, and a store checks what
        # it gets against a source build. Only what the desktop needs:
        # Tcl, Tk, SDL2, FreeType, and libmain (the JNI entry and `borg`,
        # its Android bridge). Its Java builds with the APK's
        # (velesApk), on the platform API rather than the support library
        # (android/androwish/platform-apis.patch); its Tk resolves font
        # families the way fontconfig does (font-fallback.patch), leaves
        # the device's clipboard alone when a selection goes away
        # (clipboard.patch), takes a finger held still for a long press,
        # as the rest of the phone does (touch.patch), titles a dialog in
        # the phone's script (title-font.patch), and breaks Chinese and
        # Japanese lines where those languages do (nix/tk-kinsoku.patch,
        # the desktop's Tk patch, applied to sdl2tk's Tk). Backgrounded,
        # it stops polling hundreds of times a second: SDL's event wait
        # and sdl2tk's X queue look 4 times a second, and SDL's timed
        # semaphore waits sleep instead of polling every millisecond
        # (sdl-background-poll.patch).
        androwishVersion = "8e9d986cd1";
        androwish =
          pkgs.runCommand "androwish-src-${androwishVersion}"
            {
              src = pkgs.fetchurl {
                url = "https://androwish.org/download/androwish-${androwishVersion}.tar.gz";
                hash = "sha256-qPCHBWJxuadtRTdECiL1Sgs3Jq4GYBVQGSuAY8E3pNw=";
              };
              nativeBuildInputs = [ pkgs.patch ];
            }
            ''
              mkdir -p $out
              tar xzf $src -C $out --strip-components=1 \
                androwish-${androwishVersion}/{license.terms,src,shortcut26/src} \
                androwish-${androwishVersion}/jni/{Android.mk,tcl,SDL2,freetype,sdl2tk,libsonivox,src}
              patch -d $out -p1 < ${./android/androwish/platform-apis.patch}
              patch -d $out -p1 < ${./android/androwish/font-fallback.patch}
              patch -d $out -p1 < ${./android/androwish/clipboard.patch}
              patch -d $out -p1 < ${./android/androwish/touch.patch}
              patch -d $out -p1 < ${./android/androwish/title-font.patch}
              patch -d $out -p1 < ${./android/androwish/sdl-background-poll.patch}
              patch -d $out -p1 < ${./android/androwish/notification-channel.patch}
              patch -d $out -p1 < ${./android/androwish/pendingintent-flags.patch}
              patch -d $out -p1 < ${./android/androwish/sdl-textinput-split.patch}
              patch -d $out/jni/sdl2tk -p1 < ${./nix/tk-kinsoku.patch}
            '';
        androidNdk = "${
          (androidHost.androidenv.composeAndroidPackages {
            includeNDK = true;
            ndkVersions = [ "27.0.12077973" ];
            includeCmake = false;
          }).ndk-bundle
        }/libexec/android-sdk/ndk-bundle";
        # One ABI's libraries. ndk-build reads jni/Android.mk, which
        # includes every subdirectory: the six extracted above.
        velesTkAndroid =
          abi:
          pkgs.stdenvNoCC.mkDerivation {
            pname = "veles-tk-android-${abi}";
            version = androwishVersion;
            src = androwish;
            nativeBuildInputs = [ pkgs.llvmPackages.llvm ];
            # API 26 is the app's; flexible page sizes links for 16 KB.
            buildPhase = ''
              runHook preBuild
              HOME=$TMPDIR ${androidNdk}/ndk-build -j$NIX_BUILD_CORES \
                NDK_PROJECT_PATH=$PWD APP_BUILD_SCRIPT=$PWD/jni/Android.mk \
                APP_ABI=${abi} APP_PLATFORM=android-${androidMinSdk} APP_MODULES=main \
                APP_SUPPORT_FLEXIBLE_PAGE_SIZES=true
              runHook postBuild
            '';
            # The same guards as the engine's: only libraries Android
            # itself has, and 16 KB segments.
            installPhase = ''
              runHook preInstall
              mkdir -p $out/lib
              for f in libs/${abi}/*.so; do
                for lib in $(llvm-readelf -d $f | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
                  case $lib in
                    libc.so|libm.so|libdl.so|liblog.so|libz.so|libandroid.so|libGLESv1_CM.so|libGLESv2.so|libstdc++.so) ;;
                    libtcl.so|libtk.so|libSDL2.so|libfreetype.so) ;;
                    *) echo "$f needs $lib, which the APK does not carry" >&2; exit 1 ;;
                  esac
                done
                if llvm-readelf -lW $f | awk '$1 == "LOAD" && $NF != "0x4000" { bad = 1 } END { exit !bad }'; then
                  echo "$f has LOAD segments not aligned to 16 KB" >&2; exit 1
                fi
                install -m755 $f $out/lib/
              done
              runHook postInstall
            '';
            dontFixup = true;
          };

        # The app's resources and Java, compiled: the same for every ABI,
        # and its source is only what aapt2, javac and d8 read, so an
        # edit to android/desktop.tcl or the README compiles nothing.
        androidCode =
          let
            bt = androidBuildToolsDir;
            jar = "${androidSdk}/libexec/android-sdk/platforms/android-${androidTargetSdk}/android.jar";
          in
          pkgs.stdenvNoCC.mkDerivation {
            pname = "veles-android-code";
            inherit (velesPkg) version;
            src = pkgs.lib.fileset.toSource {
              root = ./android;
              fileset = pkgs.lib.fileset.unions [
                ./android/AndroidManifest.xml
                ./android/java
                ./android/res
                ./android/strings.tsv
              ];
            };
            nativeBuildInputs = [
              pkgs.jdk17_headless
              pkgs.tcl-8_6
            ];
            buildPhase = ''
              runHook preBuild
              tclsh ${./scripts/android-strings.tcl} strings.tsv res
              ${bt}/aapt2 compile --dir res -o res.zip
              # Binary-format link, for R.java only: the APK's own link
              # is the variant-specific one (debuggable or not), and
              # the bundle's is proto-format. R.java's content does not
              # depend on either flag, so one link feeds javac.
              ${bt}/aapt2 link -o stub.apk -I ${jar} --manifest AndroidManifest.xml \
                --min-sdk-version ${androidMinSdk} --target-sdk-version ${androidTargetSdk} \
                --version-code ${androidVersionCode} --version-name ${velesPkg.version} \
                --java gen --extra-packages tk.tcl.wish -R res.zip --auto-add-overlay
              # AndroWish's Java first, on its own: it is someone else's
              # code, so its deprecation warnings do not fail the build.
              # UsbSerial needs a library the desktop does not use, and the
              # launcher and script slots are AndroWish's own app's.
              javac -source 8 -target 8 -bootclasspath ${jar}:${bt}/core-lambda-stubs.jar -classpath ${jar} \
                -encoding UTF-8 -Xlint:-options -nowarn -d classes \
                ${androwish}/src/org/libsdl/app/SDLActivity.java \
                $(find ${androwish}/src/tk ${androwish}/shortcut26/src -name '*.java' \
                    ! -name UsbSerial.java ! -name 'AndroWishLauncher.java' ! -name 'AndroWishScript*.java' | sort) \
                gen/tk/tcl/wish/R.java
              # core-lambda-stubs: android.jar has no
              # LambdaMetafactory.metafactory, and javac needs one to
              # compile a lambda (d8 then rewrites it away).
              javac -source 8 -target 8 -bootclasspath ${jar}:${bt}/core-lambda-stubs.jar -classpath ${jar}:classes \
                -encoding UTF-8 -Xlint:-options -Werror -d classes \
                $(find java gen/chat -name '*.java' | sort)
              mkdir dex
              ${bt}/d8 --release --min-api ${androidMinSdk} --lib ${jar} --output dex \
                $(find classes -name '*.class' | sort)
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              install -Dm644 -t $out dex/classes.dex res.zip
              runHook postInstall
            '';
          };

        # Everything BOTH artifacts carry (the APK and the store's
        # .aab): the compiled resources, the dex, and the per-ABI
        # libraries and assets, 16 KB-padded. One derivation so the two
        # cannot drift. It compiles nothing, so a desktop edit costs
        # this copy and the APK's zip (scripts/android-rebuild-scope.sh).
        # `stamp`: zip stores mtimes, and the store sets them all to
        # 1970, which zip cannot represent, so every entry gets one date.
        androidPayload =
          abis:
          let
            engines = map (abi: {
              inherit abi;
              drv = velesAndroid abi;
              tk = velesTkAndroid abi;
              rg = velesRgAndroid abi;
              busybox = velesBusyboxAndroid abi;
              git = velesGitAndroid abi;
              # arm64 only, like busybox and git (static musl dies on the
              # x86_64 emulator); each a lib*.so set linkTools names.
              tools = map (f: f abi) [
                velesBashAndroid
                velesJqAndroid
                velesCurlCliAndroid
                velesSqliteCliAndroid
                velesSshAndroid
              ];
              # veles-userland: ulsh (PLAN-phone-userland.md T3)
              userland = velesUserlandBins abi;
            }) abis;
          in
          pkgs.stdenvNoCC.mkDerivation {
            pname = "veles-android-payload";
            inherit (velesPkg) version;
            dontUnpack = true;
            nativeBuildInputs = [ pkgs.zip ];
            buildPhase = ''
              runHook preBuild
              stamp=198001010000
              mkdir -p payload/assets runtime
              cp ${androidCode}/classes.dex payload/
              cp ${androidCode}/res.zip res.zip
              ${pkgs.lib.concatMapStrings (e: ''
                mkdir -p payload/lib/${e.abi}
                cp ${e.drv}/bin/veles payload/lib/${e.abi}/libveles.so
                cp ${e.drv}/bin/tclsh payload/lib/${e.abi}/libtclsh.so
                cp ${e.drv}/lib/libtcl8.6.so payload/lib/${e.abi}/libtcl8.6.so
                cp ${e.drv}/lib/libveles_runtime.so payload/lib/${e.abi}/libveles_runtime.so
                cp ${e.drv}/lib/libsqlite3.so payload/lib/${e.abi}/libsqlite3.so
                cp ${e.drv}/lib/libveles_module_*.so payload/lib/${e.abi}/
                cp ${e.rg}/lib/librg.so payload/lib/${e.abi}/librg.so
                ${pkgs.lib.optionalString (e.abi == "arm64-v8a") ''
                  cp ${e.busybox}/lib/libbusybox.so payload/lib/${e.abi}/libbusybox.so
                  cp ${e.git}/lib/libgit.so ${e.git}/lib/libgit_remote_http.so payload/lib/${e.abi}/
                  ${pkgs.lib.concatMapStrings (t: ''
                    cp ${t}/lib/*.so payload/lib/${e.abi}/
                  '') e.tools}
                  # veles-userland: the shell that runs in the userland package
                  cp ${e.userland}/lib/libulsh.so payload/lib/${e.abi}/libulsh.so
                ''}
                cp ${e.tk}/lib/*.so payload/lib/${e.abi}/
              '') engines}
              # The desktop: AndroWish's Tcl reads its script libraries and
              # packages from /assets, the APK itself, and runs
              # app/main.tcl. json is tcllib's, as on a laptop.
              cp -r ${androwish}/jni/tcl/library payload/assets/tcl8.6
              cp -r ${androwish}/jni/sdl2tk/library payload/assets/sdl2tk8.6
              cp -r ${pkgs.tclPackages.tcllib}/lib/tcllib*/json payload/assets/json
              cp -r ${./veles-desktop} payload/assets/veles-desktop
              mkdir -p payload/assets/app
              cp ${./android/desktop.tcl} payload/assets/app/main.tcl
              chmod -R u+w payload/assets
              rm -rf payload/assets/sdl2tk8.6/demos
              find payload/assets/veles-desktop -name '*test.tcl' -delete
              # The last page of every segment must lie in the file. The
              # linker zero-fills a writable segment from its end in the
              # file to the end of its page, and where a kernel emulates
              # 16 KB pages (the x86_64 emulator) a write into a 4 KB page
              # past the end of the file is a SIGBUS. ndk-build's stripped
              # libraries end right after that segment: libSDL2 did, and
              # the desktop died loading it (emulator, 2026-09-30). The
              # zeros compress to nothing in the APK.
              chmod u+w payload/lib/*/*.so
              truncate -s %16384 payload/lib/*/*.so
              # Tcl's script library is the same for every ABI.
              cp -r ${(builtins.head engines).drv}/share/tcl8.6 runtime/tcl8.6
              cp -r ${./crates/veles-core/locales} runtime/locales
              chmod -R u+w payload runtime
              find payload runtime -exec touch -t $stamp {} +
              (cd runtime && find . -type f | sort | zip -X -D -q ../payload/assets/runtime.zip -@)
              touch -t $stamp payload/assets/runtime.zip
              touch -t $stamp res.zip
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              mkdir -p $out
              cp -r payload res.zip $out/
              find $out -exec touch -t 198001010000 {} +
              runHook postInstall
            '';
          };

        # The APK for a set of ABIs. Two variants from one build: `debug`
        # is debuggable (`adb shell run-as` reaches the app's files, which
        # is how a config gets onto the phone today), `release` is what a
        # store gets.
        velesApk =
          abis:
          let
            bt = androidBuildToolsDir;
            jar = "${androidSdk}/libexec/android-sdk/platforms/android-${androidTargetSdk}/android.jar";
            payload = androidPayload abis;
          in
          pkgs.stdenvNoCC.mkDerivation {
            pname = "veles-apk";
            inherit (velesPkg) version;
            dontUnpack = true;
            nativeBuildInputs = [
              pkgs.jdk17_headless
              pkgs.zip
            ];
            buildPhase = ''
              runHook preBuild
              for v in debug release; do
                ${bt}/aapt2 link -o $v.apk -I ${jar} --manifest ${./android/AndroidManifest.xml} \
                  --min-sdk-version ${androidMinSdk} --target-sdk-version ${androidTargetSdk} \
                  --version-code ${androidVersionCode} --version-name ${velesPkg.version} \
                  -R ${payload}/res.zip --auto-add-overlay \
                  $([ $v = debug ] && echo --debug-mode)
              done
              for v in debug release; do
                (cd ${payload}/payload && find . -type f | sed 's|^\./||' | sort | zip -X -D -q $TMPDIR/$v.apk -@)
                ${bt}/zipalign -P 16 -f 4 $v.apk veles-$v-unsigned.apk
              done
              runHook postBuild
            '';
            installPhase = ''
              mkdir -p $out
              cp veles-debug-unsigned.apk veles-release-unsigned.apk $out/
            '';
          };

        # The store's bundle (the plan's Step 4.2): same payload, same
        # version numbers, proto-format resources. bundletool takes a
        # module zip (manifest/AndroidManifest.xml + resources.pb +
        # res/, all proto; dex/, lib/, assets/ as they are). The
        # BundleConfig turns OFF bundletool's uncompressed native
        # libraries: left on, the split APKs it makes for a device say
        # extractNativeLibs=false, the libraries stay inside the APK,
        # and the engine, an executable run from the extracted library
        # directory, does not exist on disk (measured 2026-10-01: a
        # store-style install's gateway never started). build-bundle
        # validates the bundle itself; the output is unsigned, like
        # the APK (a store signs on upload; the throwaway-key install
        # is the smoke test's job).
        velesAab =
          abis:
          let
            bt = androidBuildToolsDir;
            jar = "${androidSdk}/libexec/android-sdk/platforms/android-${androidTargetSdk}/android.jar";
            payload = androidPayload abis;
          in
          pkgs.stdenvNoCC.mkDerivation {
            pname = "veles-aab";
            inherit (velesPkg) version;
            dontUnpack = true;
            nativeBuildInputs = [
              pkgs.jdk17_headless
              pkgs.zip
              pkgs.unzip
            ];
            buildPhase = ''
              runHook preBuild
              stamp=198001010000
              # The store's manifest has no sideload-only blocks: the
              # accessibility driver (USER decision D4, PLAN-sessions-and-
              # phone.md A3) and the launcher query it needs. A manifest
              # with no such block, or a driver still declared after the
              # cut, fails the build rather than ship it to the store.
              grep -q 'sideload-only:begin' ${./android/AndroidManifest.xml} \
                || { echo "velesAab: android/AndroidManifest.xml has no sideload-only block" >&2; exit 1; }
              sed '/sideload-only:begin/,/sideload-only:end/d' ${./android/AndroidManifest.xml} > store-manifest.xml
              if grep -q -E 'AccessibilityDriver|BIND_ACCESSIBILITY_SERVICE|android.accessibilityservice' store-manifest.xml; then
                echo "velesAab: the store bundle still declares the accessibility driver" >&2
                exit 1
              fi
              # Two variants, like the APK: `release` is what a store
              # takes; `debug` is debuggable, which is what the
              # emulator smoke needs (`adb shell run-as` reaches the
              # app's files only when the package is debuggable).
              for v in debug release; do
                ${bt}/aapt2 link -o base-proto-$v.apk --proto-format -I ${jar} \
                  --manifest store-manifest.xml \
                  --min-sdk-version ${androidMinSdk} --target-sdk-version ${androidTargetSdk} \
                  --version-code ${androidVersionCode} --version-name ${velesPkg.version} \
                  -R ${payload}/res.zip --auto-add-overlay \
                  $([ $v = debug ] && echo --debug-mode)
                rm -rf module && mkdir module
                (cd module && unzip -q ../base-proto-$v.apk)
                mkdir module/manifest && mv module/AndroidManifest.xml module/manifest/
                mkdir module/dex && cp ${payload}/payload/classes.dex module/dex/
                (cd module && cp -r ${payload}/payload/lib ${payload}/payload/assets .)
                # One BundleConfig, checked in (android/BundleConfig.json):
                # native libraries extracted at install, as the APK has them.
                chmod -R u+w module
                find module -exec touch -t $stamp {} +
                (cd module && find . -type f | sed 's|^\./||' | sort | zip -X -D -q ../base-$v.zip -@)
                touch -t $stamp base-$v.zip
                ${pkgs.bundletool}/bin/bundletool build-bundle \
                  --modules=base-$v.zip --output=veles-$v-unsigned.aab \
                  --config=${./android/BundleConfig.json}
              done
              runHook postBuild
            '';
            installPhase = ''
              mkdir -p $out
              cp veles-debug-unsigned.aab veles-release-unsigned.aab $out/
            '';
          };
        apkArm64 = velesApk [ "arm64-v8a" ];
        apkX86_64 = velesApk [ "x86_64" ];
        aabArm64 = velesAab [ "arm64-v8a" ];
        aabX86_64 = velesAab [ "x86_64" ];

        # ── veles-userland: our own distro as a second APK ──────────────
        # (PLAN-phone-userland.md T3; docs/phone.md "The userland package")
        #
        # USER 2026-10-20: the gateway APK stays at targetSdk 36, and "our
        # own distro" ships as a SEPARATE package. From targetSdk 29 on an
        # app's SELinux domain may not execute (exec or mmap PROT_EXEC) its
        # own data files, so a package manager can only live in an app that
        # targets 28: chat.janitor.veles.userland, carrying Termux's proot
        # and an Alpine aarch64 rootfs, and running the exec broker
        # (crates/veles-userland) that the gateway's `ulsh` talks to.
        # arm64-v8a only, like busybox and git (the phones this is for).
        #
        # Everything executable is lib/arm64-v8a/lib*.so (extractNativeLibs),
        # in this APK too: proot and its loader run from the native library
        # directory; only the guest's own binaries run from files/rootfs.
        userlandTargetSdk = "28";

        # Termux's proot fork (Android fixes upstream lacks: ashmem/memfd,
        # sysvipc, link2symlink, the arm64 loader placement), static against
        # musl with 16 KB pages and no NEEDED, like velesBusyboxAndroid. The
        # loader is UNBUNDLED (a file of its own, libproot_loader.so, named
        # through PROOT_LOADER by the broker): bundled, proot writes it to
        # PROOT_TMP_DIR and execs it from there, which works at targetSdk 28
        # but is one more exec of app data than needed. No 32-bit loader:
        # the guest is aarch64 Alpine.
        #
        # talloc is compiled from its one C file with a stub replace.h: the
        # waf build cross-compiles CPython for aarch64 musl to get there.
        velesProotAndroid =
          let
            static = pkgs.pkgsCross.aarch64-multiplatform-musl.pkgsStatic;
            tv = pkgs.lib.splitVersion pkgs.talloc.version;
            talloc = static.stdenv.mkDerivation {
              pname = "veles-talloc-static";
              inherit (pkgs.talloc) version src;
              dontConfigure = true;
              buildPhase = ''
                runHook preBuild
                mkdir stub
                cat > stub/replace.h <<'EOF'
                #ifndef VELES_TALLOC_REPLACE_H
                #define VELES_TALLOC_REPLACE_H
                #include <errno.h>
                #include <limits.h>
                #include <stdarg.h>
                #include <stdbool.h>
                #include <stddef.h>
                #include <stdint.h>
                #include <stdio.h>
                #include <stdlib.h>
                #include <string.h>
                #include <sys/types.h>
                #include <unistd.h>
                #define HAVE_SYS_AUXV_H 1
                #define HAVE_INTPTR_T 1
                #define HAVE_VA_COPY 1
                #define HAVE_CONSTRUCTOR_ATTRIBUTE 1
                #define HAVE_DESTRUCTOR_ATTRIBUTE 1
                #define _PUBLIC_ __attribute__((visibility("default")))
                #ifndef MIN
                #define MIN(a, b) ((a) < (b) ? (a) : (b))
                #endif
                #ifndef MAX
                #define MAX(a, b) ((a) > (b) ? (a) : (b))
                #endif
                /* C23's, which musl lacks; lib/replace would supply it. */
                static inline void *veles_memset_explicit(void *s, int c, size_t n)
                {
                    volatile unsigned char *p = s;
                    while (n--)
                        *p++ = (unsigned char)c;
                    return s;
                }
                #define memset_explicit veles_memset_explicit
                #endif
                EOF
                $CC -c -O2 -Wall \
                  -DTALLOC_BUILD_VERSION_MAJOR=${builtins.elemAt tv 0} \
                  -DTALLOC_BUILD_VERSION_MINOR=${builtins.elemAt tv 1} \
                  -DTALLOC_BUILD_VERSION_RELEASE=${builtins.elemAt tv 2} \
                  -Istub -I. talloc.c -o talloc.o
                $AR rcs libtalloc.a talloc.o
                runHook postBuild
              '';
              installPhase = ''
                install -Dm644 libtalloc.a $out/lib/libtalloc.a
                install -Dm644 talloc.h $out/include/talloc.h
              '';
              dontFixup = true;
            };
            proot = static.stdenv.mkDerivation {
              pname = "veles-proot-termux-static";
              version = "termux-e2d843ee";
              src = pkgs.fetchFromGitHub {
                owner = "termux";
                repo = "proot";
                rev = "e2d843ee6dab076ea0ed1ffd5d0ea12232185499";
                sha256 = "17jqs9mcgkvgdxqa60536894ldpzjhbyxfi14lccnax65xnz11rq";
              };
              # The host's readelf: loader-info.c is computed from the
              # loader's symbols (src/loader/loader-info.awk).
              nativeBuildInputs = [ pkgs.binutils-unwrapped ];
              # The loader is freestanding (-nostdlib): no stack protector
              # or fortify calls for it to resolve.
              hardeningDisable = [ "all" ];
              # The cross stdenv's CC/AR/… are the right ones (nixpkgs'
              # own proot recipe does the same).
              # Termux builds it against bionic; what bionic (and glibc)
              # have that musl does not, forced into every file.
              postPatch = ''
                sed -i /CROSS_COMPILE/d src/GNUmakefile
                cat > src/veles-musl-compat.h <<'EOF'
                #ifndef __ASSEMBLER__
                #include <errno.h>
                #ifndef MSG_COPY
                #define MSG_COPY 040000
                #endif
                #ifndef TEMP_FAILURE_RETRY
                #define TEMP_FAILURE_RETRY(e) ({ long __r; do __r = (long)(e); while (__r == -1L && errno == EINTR); __r; })
                #endif
                #endif
                EOF
              '';
              dontConfigure = true;
              enableParallelBuilding = true;
              # Through the environment, not make's command line: the
              # makefile appends (+=) to these.
              preBuild = ''
                # musl keeps the *64 names (rlimit64) behind this since 1.2.4.
                export CPPFLAGS="-I${talloc}/include -D_LARGEFILE64_SOURCE -include $PWD/src/veles-musl-compat.h"
                export LDFLAGS="-static -L${talloc}/lib -Wl,-z,max-page-size=16384 -Wl,-z,common-page-size=16384"
                export LOADER_LDFLAGS="-Wl,-z,max-page-size=16384 -Wl,-z,common-page-size=16384"
              '';
              makeFlags = [
                "-C"
                "src"
                "V=1"
                # Never used: the broker always sets PROOT_LOADER.
                "PROOT_UNBUNDLE_LOADER=/veles-userland-sets-PROOT_LOADER"
                "HAS_LOADER_32BIT="
              ];
              installPhase = ''
                install -Dm755 src/proot $out/bin/proot
                install -Dm755 src/loader/loader $out/libexec/proot/loader
              '';
              dontFixup = true;
            };
          in
          pkgs.runCommand "veles-proot-android-arm64-v8a"
            {
              nativeBuildInputs = [ pkgs.llvmPackages.llvm ];
            }
            ''
              install -Dm755 ${proot}/bin/proot $out/lib/libproot.so
              install -Dm755 ${proot}/libexec/proot/loader $out/lib/libproot_loader.so
              llvm-strip --strip-all $out/lib/libproot.so $out/lib/libproot_loader.so
              for f in $out/lib/*.so; do
                if llvm-readelf -d $f | grep -q NEEDED; then
                  echo "$f is not static" >&2; exit 1
                fi
                if llvm-readelf -lW $f | awk '$1 == "LOAD" && $NF != "0x4000" { bad = 1 } END { exit !bad }'; then
                  echo "$f has LOAD segments not aligned to 16 KB" >&2; exit 1
                fi
              done
            '';

        # The guest: Alpine's aarch64 minirootfs, re-packed as a plain
        # ustar (regular files, directories, symlinks, hard links — what
        # UserlandService's extractor reads), owners 0, one mtime, sorted.
        # The APK's zip compresses it. Extracted without setuid bits (the
        # nix sandbox refuses them; proot's -0 makes nothing need them).
        # Baking a package set in (bash git ripgrep jq curl …, fetched
        # .apk by URL + hash and unpacked into this tar) is the follow-up:
        # PLAN-phone-userland.md T3.
        alpineVersion = "3.24.2";
        alpineMinirootfs = pkgs.fetchurl {
          url = "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/aarch64/alpine-minirootfs-${alpineVersion}-aarch64.tar.gz";
          sha256 = "9bf70a7f18ea44094cbb5f70c58f9af129c8214745743db0e68e5502cc2ce773";
        };
        userlandRootfs =
          pkgs.runCommand "veles-userland-rootfs-${alpineVersion}"
            {
              nativeBuildInputs = [ pkgs.gnutar ];
            }
            ''
              mkdir root
              tar -xzf ${alpineMinirootfs} -C root --no-same-owner --no-same-permissions
              # The broker binds the userland app's files/shared here.
              mkdir -p root/shared
              mkdir -p $out
              tar --format=ustar --sort=name --owner=0 --group=0 --numeric-owner \
                --mtime=@315532800 -cf $out/rootfs.tar -C root .
              tar -tvf $out/rootfs.tar > $out/contents.txt
              echo "rootfs.tar: $(wc -l < $out/contents.txt) entries, $(du -k $out/rootfs.tar | cut -f1) KiB"
            '';

        # The broker (libveles_userland.so, this APK) and ulsh (libulsh.so,
        # the gateway's) for one ABI: androidFor's NDK, the same guards as
        # the engine's (only Android's own libraries, 16 KB segments).
        velesUserlandBins =
          abi:
          let
            t = androidFor abi;
          in
          pkgs.llvmPackages.stdenv.mkDerivation {
            pname = "veles-userland-bins-${abi}";
            inherit (velesPkg) version cargoDeps;
            src = engineSrc;
            nativeBuildInputs = [
              pkgs.rustPlatform.cargoSetupHook
              rustToolchainAndroid
              pkgs.llvmPackages.llvm
              t.cc
            ];
            env = t.env;
            buildPhase = ''
              runHook preBuild
              cargo build --release --offline --target ${t.rustTarget} -p veles-userland -j $NIX_BUILD_CORES
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              install -Dm755 target/${t.rustTarget}/release/veles-userland-broker $out/lib/libveles_userland.so
              install -Dm755 target/${t.rustTarget}/release/ulsh $out/lib/libulsh.so
              llvm-strip --strip-all $out/lib/libveles_userland.so $out/lib/libulsh.so
              for f in $out/lib/*.so; do
                for lib in $(llvm-readelf -d $f | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
                  case $lib in
                    libc.so|libm.so|libdl.so) ;;
                    *) echo "$f needs $lib, which Android does not carry" >&2; exit 1 ;;
                  esac
                done
                if llvm-readelf -lW $f | awk '$1 == "LOAD" && $NF != "0x4000" { bad = 1 } END { exit !bad }'; then
                  echo "$f has LOAD segments not aligned to 16 KB" >&2; exit 1
                fi
              done
              runHook postInstall
            '';
            dontFixup = true;
          };

        # The userland app's resources and Java (android/userland), the
        # gateway's androidCode recipe at targetSdk 28.
        userlandCode =
          let
            bt = androidBuildToolsDir;
            jar = "${androidSdk}/libexec/android-sdk/platforms/android-${androidTargetSdk}/android.jar";
          in
          pkgs.stdenvNoCC.mkDerivation {
            pname = "veles-userland-code";
            inherit (velesPkg) version;
            src = ./android/userland;
            nativeBuildInputs = [ pkgs.jdk17_headless ];
            buildPhase = ''
              runHook preBuild
              ${bt}/aapt2 compile --dir res -o res.zip
              ${bt}/aapt2 link -o stub.apk -I ${jar} --manifest AndroidManifest.xml \
                --min-sdk-version ${androidMinSdk} --target-sdk-version ${userlandTargetSdk} \
                --version-code ${androidVersionCode} --version-name ${velesPkg.version} \
                --java gen -R res.zip --auto-add-overlay
              javac -source 8 -target 8 -bootclasspath ${jar}:${bt}/core-lambda-stubs.jar -classpath ${jar} \
                -encoding UTF-8 -Xlint:-options -Werror -d classes \
                $(find java gen -name '*.java' | sort)
              mkdir dex
              ${bt}/d8 --release --min-api ${androidMinSdk} --lib ${jar} --output dex \
                $(find classes -name '*.class' | sort)
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              install -Dm644 -t $out dex/classes.dex res.zip
              runHook postInstall
            '';
          };

        # The userland APK, unsigned like the gateway's (debug and release
        # variants; `nix run .#android-install-userland` signs the debug
        # one with the same sideload key — the broker refuses to serve a
        # gateway signed by another). $out/guards.txt is the native code's
        # check, for reading next to `unzip -l`.
        userlandApk =
          let
            bt = androidBuildToolsDir;
            jar = "${androidSdk}/libexec/android-sdk/platforms/android-${androidTargetSdk}/android.jar";
            bins = velesUserlandBins "arm64-v8a";
          in
          pkgs.stdenvNoCC.mkDerivation {
            pname = "veles-userland-apk";
            inherit (velesPkg) version;
            dontUnpack = true;
            nativeBuildInputs = [
              pkgs.jdk17_headless
              pkgs.zip
              pkgs.llvmPackages.llvm
            ];
            buildPhase = ''
              runHook preBuild
              stamp=198001010000
              mkdir -p payload/lib/arm64-v8a payload/assets
              cp ${userlandCode}/classes.dex payload/
              cp ${velesProotAndroid}/lib/libproot.so ${velesProotAndroid}/lib/libproot_loader.so payload/lib/arm64-v8a/
              cp ${bins}/lib/libveles_userland.so payload/lib/arm64-v8a/
              cp ${userlandRootfs}/rootfs.tar payload/assets/rootfs.tar
              chmod -R u+w payload
              # The gateway payload's rule: the last page of every segment
              # lies in the file (androidPayload says why).
              truncate -s %16384 payload/lib/*/*.so
              : > guards.txt
              for f in payload/lib/arm64-v8a/*.so; do
                needed=$(llvm-readelf -d $f | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | tr '\n' ' ')
                aligns=$(llvm-readelf -lW $f | awk '$1 == "LOAD" { print $NF }' | sort -u | tr '\n' ' ')
                echo "$(basename $f): NEEDED [''${needed% }] LOAD align [''${aligns% }]" >> guards.txt
                if [ "''${aligns% }" != 0x4000 ]; then
                  echo "$f has LOAD segments not aligned to 16 KB" >&2; exit 1
                fi
              done
              cat guards.txt
              find payload -exec touch -t $stamp {} +
              for v in debug release; do
                ${bt}/aapt2 link -o $v.apk -I ${jar} --manifest ${./android/userland/AndroidManifest.xml} \
                  --min-sdk-version ${androidMinSdk} --target-sdk-version ${userlandTargetSdk} \
                  --version-code ${androidVersionCode} --version-name ${velesPkg.version} \
                  -R ${userlandCode}/res.zip --auto-add-overlay \
                  $([ $v = debug ] && echo --debug-mode)
                (cd payload && find . -type f | sed 's|^\./||' | sort | zip -X -D -q "$OLDPWD/$v.apk" -@)
                ${bt}/zipalign -P 16 -f 4 $v.apk veles-userland-$v-unsigned.apk
              done
              runHook postBuild
            '';
            installPhase = ''
              mkdir -p $out
              cp veles-userland-debug-unsigned.apk veles-userland-release-unsigned.apk guards.txt $out/
            '';
          };
        # ── end veles-userland ─────────────────────────────────────────

        # The podman CLIENT and nothing else: on the VPS the gateway's
        # container drives the sandbox user's podman through its socket
        # (CONTAINER_HOST, deploy/vps), so it needs neither a runtime nor
        # conmon nor the network helpers nixpkgs' wrapper puts on PATH.
        # Upstream's own target, `make podman-remote`.
        podmanRemote = pkgs.podman.overrideAttrs (o: {
          pname = "podman-remote";
          outputs = [ "out" ];
          # nixpkgs' hardcode-paths patch writes the helper binaries' store
          # path into podman, and with it runc, netavark, python… — 413 MB
          # of closure for a client that never runs a helper.
          patches = builtins.filter (
            p: !(pkgs.lib.hasInfix "hardcode-paths" (toString p))
          ) (o.patches or [ ]);
          buildPhase = ''
            runHook preBuild
            patchShebangs .
            make podman-remote
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            install -Dm755 bin/podman-remote $out/bin/podman
            runHook postInstall
          '';
          postFixup = "";
          doInstallCheck = false;
        });

        # R10: one relocatable Linux runtime supplies both distribution
        # formats. Keep the static launcher and Racket's compiled paths;
        # see nix/bundle.nix for the path-preservation contract.
        linuxBundle = import ./nix/bundle.nix {
          inherit
            pkgs
            system
            velesPkg
            racketRuntime
            eclRuntime
            podmanRemote
            ;
          revision = self.shortRev or self.dirtyShortRev or "dirty";
          nixUserChroot = pkgs.pkgsStatic.nix-user-chroot;
          echoPlugin = staticPlugin { crate = "veles-plugin-echo"; name = "echo"; };
          atprotoPlugin = staticPlugin {
            crate = "veles-atproto";
            bin = "veles-plugin-atproto";
            name = "atproto";
          };
        };

        # The image's tag names the commit it was built from, so the VPS
        # can say which one it runs.
        imageTag = "0.1.0-${self.shortRev or self.dirtyShortRev or "dirty"}";
        imageRevision = self.rev or self.dirtyRev or "dirty";
        # The release builder overrides only this label. Published images
        # must be rejected if their source does not name the authenticated
        # repository that produced them.
        imageSource = "veles-agent (local build, not pushed)";

        # /etc/passwd and /etc/group with uid 1000 in them: git and ssh
        # look the running user up, and refuse a uid with no entry.
        imageNss = pkgs.dockerTools.fakeNss.override {
          extraPasswdLines = [ "veles:x:1000:1000:veles:/opt/data:/bin/sh" ];
          extraGroupLines = [ "veles:x:1000:" ];
        };

      in
      {
        packages.default = velesPkg;
        packages.bundleTree = linuxBundle.tree;
        packages.bundleArchive = linuxBundle.archive;
        packages.bundleAppImage = linuxBundle.appimage;
        # Rust overlay modules (PLAN-extension-api.md E5): `velesWithModules`,
        # and the example overlay built with it (examples/overlay-module).
        lib.velesWithModules = velesWithModules;
        packages.veles-overlay-example = velesWithModules {
          modules = [
            {
              name = "greeter";
              src = ./examples/overlay-module;
            }
          ];
        };

        # Process plugins, one static executable each (staticPlugin).
        # `echo` is the example the host's tests run, and the template.
        packages.veles-plugin-echo = staticPlugin {
          crate = "veles-plugin-echo";
          name = "echo";
        };
        # `atproto` is the first real one: the public-feed Bluesky platform.
        packages.veles-plugin-atproto = staticPlugin {
          crate = "veles-atproto";
          bin = "veles-plugin-atproto";
          name = "atproto";
        };

        # The Android app, unsigned (see velesApk). `veles-apk` is the
        # phone's; `veles-apk-x86_64` is the emulator's, for
        # scripts/android-emulator-smoke.sh.
        packages.veles-apk = apkArm64;
        packages.veles-apk-x86_64 = apkX86_64;
        # The store's app bundle, unsigned (see velesAab). `veles-aab`
        # is the phone's (arm64, what Play takes); `veles-aab-x86_64`
        # is for the emulator smoke, which installs the split APKs
        # bundletool makes for the device and runs the same checks as
        # the plain APK.
        packages.veles-aab = aabArm64;
        packages.veles-aab-x86_64 = aabX86_64;
        # ── veles-userland (PLAN-phone-userland.md T3) ──
        # The second APK, chat.janitor.veles.userland (targetSdk 28), and
        # its pieces on their own for checking.
        packages.veles-userland-apk = userlandApk;
        packages.veles-proot-android-arm64-v8a = velesProotAndroid;
        packages.veles-userland-rootfs = userlandRootfs;
        packages.veles-userland-bins-arm64-v8a = velesUserlandBins "arm64-v8a";
        # ── end veles-userland ──
        # The engine alone (bin/veles, bin/tclsh, lib/libtcl8.6.so and
        # Tcl's script library), e.g. for Termux.
        packages.veles-android-arm64-v8a = velesAndroid "arm64-v8a";
        packages.veles-android-x86_64 = velesAndroid "x86_64";
        # ripgrep alone, for the same uses (a Termux install, a check).
        packages.veles-rg-android-arm64-v8a = velesRgAndroid "arm64-v8a";
        packages.veles-rg-android-x86_64 = velesRgAndroid "x86_64";
        # The phone's static toolbox, one attribute each, for building
        # and guarding a tool without the whole APK.
        packages.veles-busybox-android-arm64-v8a = velesBusyboxAndroid "arm64-v8a";
        packages.veles-git-android-arm64-v8a = velesGitAndroid "arm64-v8a";
        packages.veles-bash-android-arm64-v8a = velesBashAndroid "arm64-v8a";
        packages.veles-jq-android-arm64-v8a = velesJqAndroid "arm64-v8a";
        packages.veles-curl-cli-android-arm64-v8a = velesCurlCliAndroid "arm64-v8a";
        packages.veles-sqlite3-cli-android-arm64-v8a = velesSqliteCliAndroid "arm64-v8a";
        packages.veles-ssh-android-arm64-v8a = velesSshAndroid "arm64-v8a";
        packages.android-emulator-sdk = androidEmulatorSdk;
        packages.android-sdk = androidSdk;

        # `nix run .#android-install` signs the phone's APK with this
        # machine's sideload key and adb-installs it;
        # `nix run .#android-emulator-smoke` proves the x86_64 one on a
        # headless emulator. The scripts' headers say what each does.
        apps.android-install = {
          type = "app";
          program = pkgs.lib.getExe (
            pkgs.writeShellApplication {
              name = "veles-android-install";
              runtimeInputs = [
                pkgs.jdk17_headless
                androidHost.android-tools
              ];
              runtimeEnv = {
                VELES_APK_DIR = apkArm64;
                VELES_BUILD_TOOLS = androidBuildToolsDir;
              };
              text = builtins.readFile ./scripts/android-install.sh;
            }
          );
        };
        # ── veles-userland: the same install, for the second APK, signed
        # with the same sideload key (the broker serves only a gateway
        # whose signature matches its own).
        apps.android-install-userland = {
          type = "app";
          program = pkgs.lib.getExe (
            pkgs.writeShellApplication {
              name = "veles-android-install-userland";
              runtimeInputs = [
                pkgs.jdk17_headless
                androidHost.android-tools
              ];
              runtimeEnv = {
                VELES_APK_DIR = userlandApk;
                VELES_BUILD_TOOLS = androidBuildToolsDir;
                VELES_APK_PACKAGE = "chat.janitor.veles.userland";
                VELES_APK_STEM = "veles-userland";
              };
              text = builtins.readFile ./scripts/android-install.sh;
            }
          );
        };
        # ── end veles-userland ──
        apps.android-emulator-smoke = {
          type = "app";
          program = pkgs.lib.getExe (
            pkgs.writeShellApplication {
              name = "veles-android-emulator-smoke";
              runtimeInputs = [
                pkgs.jdk17_headless
                pkgs.curl
                pkgs.python3
                androidEmulatorSdk
              ];
              runtimeEnv = {
                VELES_APK_DIR = apkX86_64;
                VELES_ANDROID_SDK = "${androidEmulatorSdk}/libexec/android-sdk";
                VELES_BUILD_TOOLS = androidBuildToolsDir;
                VELES_SYSTEM_IMAGE = "system-images;android-${androidTargetSdk};${androidEmulatorImage};x86_64";
                # The turns of the notification checks.
                VELES_MOCK_PROVIDER = ./scripts/devharness/mock_provider.py;
              };
              text = builtins.readFile ./scripts/android-emulator-smoke.sh;
            }
          );
        };
        # Part F of PLAN-pool-round.md, rehearsed on the emulator: the
        # two-node pool with the PHONE node on the emulator and the
        # laptop node (plus the arbiter) as a scratch gateway here.
        # scripts/f-partf-emulator.sh says what it checks.
        apps.android-pool-rehearsal = {
          type = "app";
          program = pkgs.lib.getExe (
            pkgs.writeShellApplication {
              name = "veles-android-pool-rehearsal";
              runtimeInputs = [
                pkgs.jdk17_headless
                pkgs.curl
                pkgs.jq
                pkgs.sqlite
                pkgs.python3
                androidEmulatorSdk
              ];
              runtimeEnv = {
                VELES_APK_DIR = apkX86_64;
                VELES_ANDROID_SDK = "${androidEmulatorSdk}/libexec/android-sdk";
                VELES_BUILD_TOOLS = androidBuildToolsDir;
                VELES_SYSTEM_IMAGE = "system-images;android-${androidTargetSdk};${androidEmulatorImage};x86_64";
                VELES_BIN = "${velesPkg}/bin/veles";
                VELES_MOCK_PROVIDER = ./scripts/devharness/mock_provider.py;
              };
              text = builtins.readFile ./scripts/f-partf-emulator.sh;
            }
          );
        };
        # The three-node pool (the VPS as the hub), rehearsed on this
        # machine: scratch gateways for vps, laptop and phone, the real
        # Telegram adapter against a stub Bot API, a real signer, podman
        # user envs (the vps's through a socket). scripts/g-three-nodes.sh
        # says what it checks. It uses the host's podman (rootless needs
        # the system's newuidmap) and its veles-sandbox:latest image.
        apps.pool-three-rehearsal = {
          type = "app";
          program = pkgs.lib.getExe (
            pkgs.writeShellApplication {
              name = "veles-pool-three-rehearsal";
              runtimeInputs = [
                pkgs.curl
                pkgs.jq
                pkgs.sqlite
                pkgs.python3
                pkgs.tcl-8_6
              ];
              runtimeEnv = {
                VELES_BIN = "${velesPkg}/bin/veles";
                VELES_MOCK_PROVIDER = ./scripts/devharness/mock_provider.py;
                VELES_TG_BOT_STUB = ./scripts/devharness/tg_bot_stub.tcl;
                VELES_TG_STUB = ./scripts/devharness/tg_stub.tcl;
                TCLLIBPATH = "${pkgs.tclPackages.tcllib}/lib";
              };
              text = builtins.readFile ./scripts/g-three-nodes.sh;
            }
          );
        };
        # The same smoke test against the store's bundle instead of the
        # plain APK: the script sees VELES_AAB_DIR and, before it
        # installs anything, has bundletool make the x86_64 .aab into
        # the split APKs it would serve this device, signed with a
        # throwaway key in /dev/shm (what Play installs, unlike a
        # universal APK), and runs every check unchanged against THEM.
        apps.android-aab-smoke = {
          type = "app";
          program = pkgs.lib.getExe (
            pkgs.writeShellApplication {
              name = "veles-android-aab-smoke";
              runtimeInputs = [
                pkgs.jdk17_headless
                pkgs.curl
                pkgs.unzip
                pkgs.bundletool
                pkgs.python3
                androidEmulatorSdk
              ];
              runtimeEnv = {
                VELES_AAB_DIR = aabX86_64;
                VELES_ANDROID_SDK = "${androidEmulatorSdk}/libexec/android-sdk";
                VELES_BUILD_TOOLS = androidBuildToolsDir;
                VELES_SYSTEM_IMAGE = "system-images;android-${androidTargetSdk};${androidEmulatorImage};x86_64";
                VELES_MOCK_PROVIDER = ./scripts/devharness/mock_provider.py;
              };
              text = builtins.readFile ./scripts/android-emulator-smoke.sh;
            }
          );
        };

        # ── sandbox artifacts (nix/sandbox-artifacts.nix) ────────────
        #
        # PENDING.md carried these as open: "the backend + config
        # contract exist; the artifacts don't". Building them is what
        # turns nspawn and firecracker from argv-pinned into runnable —
        # point `[sandbox]` at a store path and `veles doctor --sandbox`
        # executes a real script through the fence.
        packages.nspawnRootfs = sandboxArtifacts.nspawnRootfs;
        packages.firecrackerRootfs = sandboxArtifacts.firecrackerRootfs;
        packages.firecrackerKernel = sandboxArtifacts.firecrackerKernel;

        checks = {
          # The release package builds and installs (wrapper, catalogs);
          # the test suite is `tests`.
          veles-agent = velesPkg;
          tests = testsCheck;

          # A process plugin still builds as one static executable: a
          # dependency on veles-core or libtcl would end that, and the
          # derivation's install check names it.
          plugin-echo-static = staticPlugin {
            crate = "veles-plugin-echo";
            name = "echo";
          };

          # The first real platform plugin builds static too (the atproto
          # crate links reqwest/ring for the PDS + Jetstream — the static
          # build proves the no-C-toolchain rust-lld path still closes).
          plugin-atproto-static = staticPlugin {
            crate = "veles-atproto";
            bin = "veles-plugin-atproto";
            name = "atproto";
          };

          # The gateway's two units, EVALUATED rather than booted.
          #
          # `veles-gateway` and `veles-gateway-dev` differ in the two
          # things that stay invisible until one is live: which binary
          # each runs, and which one a login starts. Both are silent
          # when wrong — a unit that starts a stale binary, or two
          # mechanisms sharing one name, which is the state this machine
          # was actually in (a hand-written veles-gateway.service
          # running a debug build, while the module generated nothing).
          #
          # It belongs in `checks` rather than with the VM test because
          # it builds NOTHING: two module evaluations and some string
          # comparisons, about three seconds. USER (2026-09-04) asked
          # for the heavy rigs to stay on-demand; this is neither.
          module-units =
            let
              evalWith =
                velesCfg:
                (nixpkgs.lib.nixosSystem {
                  inherit system;
                  modules = [
                    self.nixosModules.default
                    {
                      # The minimum that makes a NixOS config evaluate.
                      fileSystems."/" = {
                        device = "none";
                        fsType = "tmpfs";
                      };
                      boot.loader.grub.enable = false;
                      system.stateVersion = "25.05";
                      # recursiveUpdate, not //: a shallow merge lets
                      # `{ gateway.x = …; }` replace the whole gateway
                      # attrset and silently drop what is set here.
                      services.veles = lib.recursiveUpdate {
                        enable = true;
                        sandbox.backend = "native";
                      } velesCfg;
                    }
                  ];
                }).config;

              withTree = (evalWith { gateway.devTree = "veles-agent"; }).systemd.user.services;
              noTree = (evalWith { }).systemd.user.services;
              release =
                (evalWith {
                  gateway.devTree = "veles-agent";
                  gateway.devProfile = "release";
                }).systemd.user.services;
              absolute = (evalWith { gateway.devTree = "/srv/veles"; }).systemd.user.services;
              vaultMoved = evalWith {
                gateway.devTree = "veles-agent";
                vault.identityDir = "/persist/veles/vault";
              };
              vaultDefault = evalWith { gateway.devTree = "veles-agent"; };
              inherit (pkgs) lib;
            in
            # Each unit runs its own binary — the one thing a rename or
            # a refactor can quietly swap.
            assert lib.assertMsg
              (lib.hasPrefix builtins.storeDir withTree."veles-gateway".serviceConfig.ExecStart)
              "veles-gateway must run the nix-built binary, got: ${withTree."veles-gateway".serviceConfig.ExecStart}";
            assert lib.assertMsg
              (
                withTree."veles-gateway-dev".serviceConfig.ExecStart
                == "%h/veles-agent/target/debug/veles gateway run"
              )
              "veles-gateway-dev must run the working tree's binary, got: ${withTree."veles-gateway-dev".serviceConfig.ExecStart}";
            assert lib.assertMsg
              (
                release."veles-gateway-dev".serviceConfig.ExecStart
                == "%h/veles-agent/target/release/veles gateway run"
              )
              "devProfile must choose the target/ subdirectory, got: ${release."veles-gateway-dev".serviceConfig.ExecStart}";
            # NEITHER unit is wanted: a wantedBy from the module lands
            # in root-owned /etc/systemd/user/default.target.wants/,
            # which `systemctl --user disable` cannot remove — so a
            # login would start the production gateway on top of
            # whichever one you were actually using.
            assert lib.assertMsg (withTree."veles-gateway".wantedBy == [ ])
              "veles-gateway must not be wanted by anything";
            assert lib.assertMsg (withTree."veles-gateway-dev".wantedBy == [ ])
              "veles-gateway-dev must not be wanted by anything";
            # An absolute devTree is used as given — %h is for the
            # relative case, not a prefix glued onto everything.
            assert lib.assertMsg
              (
                absolute."veles-gateway-dev".serviceConfig.ExecStart
                == "/srv/veles/target/debug/veles gateway run"
              )
              "an absolute devTree must not be rewritten, got: ${absolute."veles-gateway-dev".serviceConfig.ExecStart}";
            # No devTree, no dev unit — rather than one that cannot start.
            assert lib.assertMsg (noTree ? "veles-gateway" && !(noTree ? "veles-gateway-dev"))
              "without devTree there must be no dev unit";
            # A moved vault reaches BOTH units as VELES_VAULT_DIR, and its
            # directory is made 0700; unset, veles picks <config>/vault and
            # the unit says nothing — an empty VELES_VAULT_DIR would be read.
            assert lib.assertMsg
              (
                vaultMoved.systemd.user.services."veles-gateway".environment.VELES_VAULT_DIR
                  == "/persist/veles/vault"
                && vaultMoved.systemd.user.services."veles-gateway-dev".environment.VELES_VAULT_DIR
                  == "/persist/veles/vault"
              )
              "vault.identityDir must reach both units as VELES_VAULT_DIR";
            assert lib.assertMsg
              (builtins.elem "d /persist/veles/vault 0700 - - -" vaultMoved.systemd.user.tmpfiles.rules)
              "a moved vault dir must be created 0700";
            assert lib.assertMsg
              (!(vaultDefault.systemd.user.services."veles-gateway".environment ? VELES_VAULT_DIR))
              "with no vault.identityDir the unit must not set VELES_VAULT_DIR";
            # The age CLI is the way into a record when veles cannot be.
            assert lib.assertMsg
              (builtins.any (p: (p.pname or "") == "age") vaultDefault.environment.systemPackages)
              "vault.ageCli (default on) must install age";
            pkgs.runCommand "veles-module-units" { } "touch $out";

          clippy = clippyCheck;
          rustfmt = fmtCheck;
          tcl-desktop = tclCheck;

          # AndroWish's SDL 2.0.6 posted soft-keyboard text as ONE 32-byte
          # SDL_TEXTINPUT event and dropped the rest: a paste on the phone
          # arrived as its first few words (pitfall 79;
          # android/androwish/sdl-textinput-split.patch). The real
          # SDL_SendKeyboardText from the patched tree, driven on the host
          # with the event queue stubbed: 5 failures on the unpatched tree,
          # none with the patch. The log is the artifact.
          androwish-sdl-textinput = pkgs.runCommandCC "androwish-sdl-textinput" { } ''
            R=${androwish}/jni/SDL2
            cc -w -O0 -I$R/include -I$R/src \
              -o harness ${./android/androwish/sdl-textinput-harness.c} \
              $R/src/events/SDL_keyboard.c $R/src/stdlib/SDL_string.c \
              -Wl,--unresolved-symbols=ignore-all
            ./harness | tee $out
          '';
          # NOTE the NixOS module VM test is deliberately NOT here — it
          # is `packages.nixos-module-test`, below. `nix flake check`
          # builds every attribute in `checks`, and booting a VM under
          # qemu is minutes of CPU and gigabytes of disk for something
          # that only changes when nix/module.nix does. USER
          # (2026-09-04): "can we make sure those tests only ever run
          # on-demand explicitly, because this takes hours and uses up
          # all my disk space".
        };

        # The NixOS module, proven by BOOTING it — on demand:
        #
        #     nix build .#nixos-module-test
        #
        # A package rather than a check, so `nix flake check` stays a
        # fast gate. See nix/nixos-test.nix for what this deliberately
        # does NOT claim to cover.
        packages.nixos-module-test = import ./nix/nixos-test.nix {
          inherit self pkgs;
          inherit (pkgs) lib;
        };

        # veles-desktop: Tcl/Tk app (hermes-desktop replacement).
        packages.veles-desktop = pkgs.writeShellScriptBin "veles-desktop" ''
          export TCL_LIBRARY="${pkgs.tcl-8_6}/lib/tcl8.6"
          export TK_LIBRARY="${tkDesktop}/lib/tk8.6"
          export TCLLIBPATH="${tcllibPathExpr}"
          export VELES_DESKTOP_DIR="${./veles-desktop}"
          # UTF-8 locale so Tcl's system encoding is Unicode (not the
          # iso8859-1 fallback that causes mojibake).
          export LANG=C.UTF-8 LC_ALL=C.UTF-8
          # dwebp / resvg / heif-dec — see imageDecoders above.
          export PATH="${pkgs.lib.makeBinPath imageDecoders}:$PATH"
          exec ${tkDesktop}/bin/wish ${./veles-desktop}/main.tcl "$@"
        '';

        # ── Container image (VPS deployment path, replaces Demon one day) ──
        #
        # Daemon-free OCI archive built with nixpkgs dockerTools — works on
        # this laptop (no docker/podman installed here) and loads on the VPS
        # with `podman load`. The image is a nix-style /nix/store tree, so it
        # runs on Linux/x86_64 hosts only (a Linux/x86_64 VPS qualifies).
        #
        # Design decisions (2026-08-14):
        #   - VELES_HOME=/opt/data, a bind-mounted persistent volume —
        #     mirrors Demon's HERMES_HOME layout (/root/.hermes == /opt/data).
        #     The mountpoint is created by the OCI runtime; no runAsRoot VM
        #     step needed. Host dir gets chown 1000:1000 at deploy time.
        #   - Unprivileged fixed identity 1000:1000 ("veles"), matching the
        #     bind-mount owner on the host.
        #   - Entrypoint is the wrapped `veles` binary; Cmd defaults to
        #     `gateway run` but any subcommand works:
        #     `podman run --rm veles-agent:0.1.0 status` etc.
        #   - No s6 / entrypoint.sh — veles' own lifecycle code (pid file,
        #     crash-loop guard, graceful SIGTERM, shutdown log) IS the init
        #     story; PID 1 is `veles` itself. `--restart unless-stopped` on
        #     the podman side covers crash restarts.
        #   - copyToRoot links /bin of: veles (+ git/openssh/tclsh/rg via
        #     the wrapper's PATH prefix), curl/ip/less for debugging,
        #     busybox for /bin/sh + core utils (the terminal tool shells
        #     out), tclsh for the execute_code tool (Tcl harness; python
        #     is user-banned, 2026-08-21).
        packages.podmanRemote = podmanRemote;

        packages.ociImage = pkgs.dockerTools.buildImage {
          name = "veles-agent";
          tag = imageTag;
          copyToRoot = pkgs.buildEnv {
            name = "veles-image-root";
            paths = [
              velesPkg
              pkgs.cacert
              pkgs.curl
              pkgs.iproute2
              pkgs.less
              pkgs.busybox
              pkgs.tcl-8_6
              podmanRemote
              imageNss
            ];
            pathsToLink = [ "/bin" "/etc" ];
          };
          # A writable /tmp: the image has none, and veles' scratch dirs,
          # git and curl all reach for it.
          extraCommands = ''
            mkdir -p tmp
            chmod 1777 tmp
          '';
          config = {
            User = "1000:1000";
            WorkingDir = "/opt/data";
            Env = [
              # State resolves to the persistent volume, never $HOME.
              "VELES_HOME=/opt/data"
              "HOME=/opt/data"
              # UTF-8 everywhere (Tcl system encoding — the hermes image
              # sets the same).
              "LANG=C.UTF-8"
              "LC_ALL=C.UTF-8"
              # Desktop API (binds loopback INSIDE the container; probed via
              # `podman exec <c> curl http://127.0.0.1:8642/api/health` —
              # see deploy/README.md). /api/health is the only route that
              # needs no credential: with nothing configured the gateway
              # MINTS a bearer token into /opt/data/.env on first boot,
              # which is the volume, so it survives a container rebuild.
              # Read it there before curling any other route, or set
              # DESKTOP_BEARER_TOKEN here to choose your own.
              # + A2A port (binds wider only when a token + a host are set —
              # A2A_HOST or platforms.a2a.extra.host — by design).
              "VELES_DESKTOP_PORT=8642"
              "A2A_PORT=9900"
              "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              "SSL_CERT_DIR=${pkgs.cacert}/etc/ssl/certs"
            ];
            Entrypoint = [ "${velesPkg}/bin/veles" ];
            Cmd = [ "gateway" "run" ];
            Labels = {
              # NOTE: dockerTools does NOT interpret dots as namespaces —
              # flat dotted keys are the literal OCI label names.
              "org.opencontainers.image.title" = "Veles Agent";
              "org.opencontainers.image.description" = "Rust core + Tcl scripting AI agent framework";
              "org.opencontainers.image.version" = imageTag;
              "org.opencontainers.image.revision" = imageRevision;
              "org.opencontainers.image.source" = imageSource;
            };
            ExposedPorts = {
              # A2A. The desktop API is published on the host's loopback
              # only (deploy/vps/user/veles/veles-gateway.container).
              "9900/tcp" = { };
            };
          };
        };

        # The headless ticket signer, its own image for its own container
        # and user on the VPS (deploy/vps): the gateway reaches its API on
        # 9911 and nothing else of it. The same binary (`veles signer run`)
        # without the gateway's tools around it.
        packages.ociSignerImage = pkgs.dockerTools.buildImage {
          name = "veles-signer";
          tag = imageTag;
          copyToRoot = pkgs.buildEnv {
            name = "veles-signer-root";
            paths = [
              pkgs.cacert
              imageNss
            ];
            pathsToLink = [ "/etc" ];
          };
          extraCommands = ''
            mkdir -p tmp
            chmod 1777 tmp
          '';
          config = {
            User = "1000:1000";
            WorkingDir = "/opt/signer";
            Env = [
              "HOME=/opt/signer"
              # Nothing of the gateway's: a home of its own, never read for
              # anything but this process's own scratch.
              "VELES_HOME=/opt/signer/home"
              "LANG=C.UTF-8"
              "LC_ALL=C.UTF-8"
              "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              "SSL_CERT_DIR=${pkgs.cacert}/etc/ssl/certs"
            ];
            # signer.toml, signer.env and the key live in /opt/signer (the
            # volume); SIGNER_PASSPHRASE_FILE names the mounted secret.
            Entrypoint = [ "${velesPkg}/bin/veles" ];
            Cmd = [ "signer" "run" "--dir" "/opt/signer" ];
            Labels = {
              "org.opencontainers.image.title" = "Veles ticket signer";
              "org.opencontainers.image.version" = imageTag;
              "org.opencontainers.image.revision" = imageRevision;
              "org.opencontainers.image.source" = imageSource;
            };
            ExposedPorts = {
              "9911/tcp" = { };
            };
          };
        };

        devShells.default = pkgs.mkShell {
          stdenv = pkgs.llvmPackages.stdenv;
          inherit buildInputs nativeBuildInputs;
          # tkDesktop's wish ahead of buildInputs' stock one: the Tcl
          # suites and the desktop run on what ships.
          packages = with pkgs; [
            tkDesktop
            # Real cosign rejection tests; live signing still belongs to
            # the protected GitHub tag workflow, not this dev shell.
            cosign
            cargo-watch
            rust-analyzer-nightly
          ];
          TK_LIBRARY = "${tkDesktop}/lib/tk8.6";
          VELES_TEST_COSIGN = buildEnv.VELES_TEST_COSIGN;

          inherit (buildEnv)
            PKG_CONFIG_PATH
            TCL_LIBRARY
            OPENSSL_DIR
            OPENSSL_LIB_DIR
            OPENSSL_INCLUDE_DIR
            LIBCLANG_PATH
            CPATH
            VELES_RACKET_HOME
            VELES_RACKET_LIBS
            VELES_ECL_HOME
            ;
          # The gateway and every module link libveles_runtime.so, and with
          # it libstd-*.so (PLAN-plugins-installer.md M1). `$ORIGIN` finds
          # the runtime beside target/debug/veles (cargo puts a copy there)
          # and beside a test binary in deps/; the toolchain dir has libstd.
          # So target/debug/veles starts outside cargo, which is how
          # veles-gateway-dev runs it.
          RUSTFLAGS =
            buildEnv.RUSTFLAGS
            + " -C link-arg=-Wl,-rpath,$ORIGIN"
            + " -C link-arg=-Wl,-rpath,${rustToolchain}/lib/rustlib/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/lib";

          shellHook = ''
            export TCLLIBPATH="${tcllibPathExpr}"
            # clang resource dir (stddef.h etc.)
            export BINDGEN_EXTRA_CLANG_ARGS="-I${clangResourceInclude}"
            echo "veles-agent development shell (pure)"
            echo "Rust: $(rustc --version)"
            echo "C:    $(cc --version | head -1)"
            echo "Tcl:  $(echo 'puts [info patchlevel]' | tclsh)"
            echo "Tk:   ${tkDesktop}/bin/wish"
          '';
        };

        # `nix develop .#android` — the Android block beside rustToolchain
        # says what is in it and why.
        devShells.android = pkgs.mkShell (
          {
            stdenv = pkgs.llvmPackages.stdenv;
            inherit buildInputs;
            nativeBuildInputs = [
              rustToolchainAndroid
              # adb, for scripts/android-install.sh and the smoke test.
              androidHost.android-tools
            ] ++ builtins.filter (p: p != rustToolchain) nativeBuildInputs;

            # Not CPATH and not RUSTFLAGS: clang reads CPATH on EVERY
            # compile, the NDK's too, and glibc's headers would land in
            # ring's and SQLite's Android builds; RUSTFLAGS carries the
            # host Tcl's rpath.
            inherit (buildEnv) PKG_CONFIG_PATH TCL_LIBRARY LIBCLANG_PATH;

            # Its own target dir: a cross build puts its build scripts and
            # proc macros in the host's target/debug, built with other
            # flags, and the two shells would rebuild each other's (and
            # swap target/debug/veles under the running dev gateway's
            # feet).
            #
            # `nix develop` keeps the calling environment, and a terminal
            # in this repo has the DEFAULT shell's loaded (direnv): its
            # CPATH put glibc's headers into ring's Android build
            # ("gnu/stubs-32.h not found", measured 2026-09-30), and its
            # RUSTFLAGS carry the host Tcl's rpath. Not inheriting them is
            # not enough; clear them.
            shellHook = ''
              unset CPATH RUSTFLAGS
              export CARGO_TARGET_DIR="$(git rev-parse --show-toplevel 2>/dev/null || pwd)/target/android"
              echo "veles-agent Android shell: API ${androidApi}, ${pkgs.lib.concatStringsSep " " (map (a: a.rustTarget) (builtins.attrValues androidAbis))}"
              echo "Rust: $(rustc --version)"
              echo "  cargo build --target aarch64-linux-android -p veles-cli"
              echo "  → $CARGO_TARGET_DIR/aarch64-linux-android/debug/veles"
              echo "The APK: nix build .#veles-apk, then scripts/android-install.sh"
            '';
          }
          // androidArm64.env
          // (androidFor "x86_64").env
        );
      }
    );
}
