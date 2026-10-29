# scripts/install/common.sh — shared by every per-OS installer.
#
# Sourced, never executed. POSIX sh: FreeBSD's /bin/sh is not bash, and
# `shell.rs` already made the agent itself fall back from bash to sh for
# exactly that reason.
#
# THE RULE THIS FILE ENFORCES: an installer proves its own install.
# Every script ends by RUNNING `veles doctor` and `veles doctor
# --sandbox` — the second of which executes a real script through the
# configured fence. A script that prints "installed successfully"
# without running the thing it installed is the same defect as a
# sandbox backend whose only test pins the argv it emits.

set -eu

VELES_PREFIX="${VELES_PREFIX:-$HOME/.local}"
VELES_REPO="${VELES_REPO:-$(cd "$(dirname "$0")/../.." && pwd)}"
VELES_SCOPE="${VELES_SCOPE:-user}"   # user | system

say()  { printf '\n\033[1m▸ %s\033[0m\n' "$*"; }
note() { printf '  %s\n' "$*"; }
die()  { printf '\033[31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# sudo only where it is genuinely needed (package installation), which
# is the posture the operator asked for: user-level install, superuser
# through sudo.
SUDO=""
need_root() {
    [ "$(id -u)" = 0 ] && return 0
    command -v sudo >/dev/null 2>&1 || die "need root for package installation and sudo is not installed"
    SUDO="sudo"
}

have() { command -v "$1" >/dev/null 2>&1; }

# ── build ───────────────────────────────────────────────────────────
#
# From source, always. There is no binary distribution: veles links
# libtcl through bindgen, so the build is tied to the Tcl the host
# actually has.
# veles links libtcl through bindgen, and bindgen is clang: without the
# clang RESOURCE directory on the include path it cannot find stddef.h,
# parses tcl.h badly, and emits OPAQUE types rather than failing. The
# symptom is a wall of confusing errors from the `tcl` crate itself:
#
#   error[E0609]: no field `refCount` on type `&Tcl_Obj`
#   = note: available field is: `_address`
#
# which says nothing about clang at all. Arch hit exactly this on
# 2026-09-04 with a perfectly good Tcl 8.6.16 installed.
#
# flake.nix sets these for the nix build (BINDGEN_EXTRA_CLANG_ARGS,
# TCL_LIBRARY, LIBCLANG_PATH); every other OS needs them too. Anything
# the operator has already set is left alone.
setup_build_env() {
    # The clang RESOURCE directory (stddef.h and friends). Ask clang if
    # it is on PATH; otherwise look where distros put it. A Gentoo
    # stage3 has llvm-core/clang installed with no `clang` on PATH at
    # all, so the ask-clang route silently did nothing and bindgen died
    # much later with:
    #
    #   ClangDiagnostic("/usr/include/stdio.h:38:10: fatal error:
    #   'stddef.h' file not found")
    #
    # which is a real header failing to find a real header, and says
    # nothing about resource directories.
    if [ -z "${BINDGEN_EXTRA_CLANG_ARGS:-}" ]; then
        rd=""
        have clang && rd="$(clang -print-resource-dir 2>/dev/null || true)"
        if [ -z "$rd" ] || [ ! -d "$rd/include" ]; then
            rd=""
            for c in /usr/lib/llvm/*/lib/clang/* /usr/lib/clang/* \
                     /usr/lib64/clang/* /usr/local/llvm*/lib/clang/*; do
                [ -d "$c/include" ] && rd="$c"
            done
        fi
        if [ -n "$rd" ] && [ -d "$rd/include" ]; then
            BINDGEN_EXTRA_CLANG_ARGS="-I$rd/include"; export BINDGEN_EXTRA_CLANG_ARGS
            note "BINDGEN_EXTRA_CLANG_ARGS=$BINDGEN_EXTRA_CLANG_ARGS"
        else
            note "WARNING: no clang resource directory found; bindgen may fail on stddef.h"
        fi
    fi
    # bindgen needs libclang as a SHARED LIBRARY, and every distro files
    # it somewhere different — Gentoo under /usr/lib/llvm/<ver>/lib64,
    # Debian in the multiarch dir, Arch and Fedora flat. Missing it is
    # not a graceful degradation: bindgen PANICS, twenty minutes into a
    # cargo build, with a message about shared libraries that says
    # nothing about which package to install.
    #
    #   Unable to find libclang: "couldn't find any valid shared
    #   libraries matching: ['libclang.so', …], set the LIBCLANG_PATH
    #   environment variable…"
    #
    # So search widely, and if it is genuinely absent, fail HERE with a
    # sentence the operator can act on.
    if [ -z "${LIBCLANG_PATH:-}" ]; then
        for d in /usr/lib /usr/lib64 /usr/lib/x86_64-linux-gnu \
                 /usr/lib/llvm/*/lib64 /usr/lib/llvm/*/lib \
                 /usr/lib/llvm-*/lib /usr/local/llvm*/lib /usr/local/lib \
                 /usr/local/lib64; do
            if [ -e "$d/libclang.so" ] || [ -e "$d/libclang.so.1" ]; then
                LIBCLANG_PATH="$d"; export LIBCLANG_PATH
                note "LIBCLANG_PATH=$LIBCLANG_PATH"
                break
            fi
        done
    fi
    if [ -z "${LIBCLANG_PATH:-}" ]; then
        die "no libclang.so found. veles links libtcl through bindgen, which needs
     it as a shared library — the build would panic twenty minutes from now
     with a message that names no package. Install the clang runtime
     (Debian/Ubuntu: libclang-dev · Arch: clang · Gentoo: llvm-core/clang ·
     FreeBSD: llvm), or set LIBCLANG_PATH yourself."
    fi
    if [ -z "${TCL_LIBRARY:-}" ]; then
        for d in /usr/share/tcl8.6 /usr/lib/tcl8.6 /usr/local/lib/tcl8.6 /usr/lib64/tcl8.6; do
            if [ -d "$d" ]; then
                TCL_LIBRARY="$d"; export TCL_LIBRARY
                note "TCL_LIBRARY=$TCL_LIBRARY"
                break
            fi
        done
    fi
}

# Two DIFFERENT ways the `tcl` crate fails to build, which look alike in
# a log and want opposite responses.
#
# 1. bindgen gave up on the Tcl structs. Dozens of:
#
#      error[E0609]: no field `refCount` on type `&Tcl_Obj`
#         = note: available field is: `_address`
#
#    `_address` is bindgen's marker for a type it made OPAQUE. It means
#    the libclang on this host is newer than the bindgen that parsed
#    with it (22+ vs clib 0.2.4's pinned 0.64). Retrying cannot help —
#    it is deterministic. veles carries a patched clib for exactly this
#    (vendor/clib/WHY.md); seeing it here means that patch is not being
#    applied.
#
# 2. inwelling lost the race. `clib`'s build script waits for the build
#    directory to go quiet and then looks for the marker the `tcl`
#    crate's build script leaves behind; when it guesses wrong it
#    writes an EMPTY bindings.rs and every Tcl type vanishes.
#
#    USER (2026-09-04), from ~/szhrmk-flake/slopdrop, which links the
#    same crate: "there are known issues with clib we also had in
#    slopdrop meaning you sometimes have to run cargo build twice (race
#    condition) or cargo clean". A retry works because the marker
#    survives in target/ from the failed attempt, so the second run
#    finds it.
CLIB_OPAQUE_TYPES='available field is: `_address`'
CLIB_TCL_FAILED='could not compile `tcl`'

build_veles() {
    say "building veles from $VELES_REPO"
    have cargo || die "cargo not found — the dependency step should have installed rust"
    setup_build_env
    cd "$VELES_REPO"
    # /tmp is the SSD on some hosts; heavy test I/O belongs on tmpfs.
    # The BUILD does not care, but the suite below does.
    log="${TMPDIR:-/tmp}/veles-build.$$.log"
    rcf="${TMPDIR:-/tmp}/veles-build.$$.rc"
    attempt=1
    while :; do
        # POSIX sh has no PIPESTATUS, and the operator still wants to
        # watch the build: run cargo in a subshell that records its own
        # status, and tee the output.
        ( cargo build --release --bin veles 2>&1; echo $? > "$rcf" ) | tee "$log"
        rc="$(cat "$rcf" 2>/dev/null || echo 1)"
        if [ "$rc" = "0" ]; then
            [ "$attempt" -gt 1 ] && note "the build succeeded on attempt $attempt"
            break
        fi
        if grep -qF "$CLIB_OPAQUE_TYPES" "$log" 2>/dev/null; then
            rm -f "$log" "$rcf"
            die "bindgen made the Tcl structs opaque, so the tcl crate cannot compile.
     This host's libclang ($(clang --version 2>/dev/null | head -1)) is newer
     than the bindgen that clib 0.2.4 pins, and retrying will not change it.
     veles ships a patched clib for this — check that Cargo.toml's
     [patch.crates-io] still has clib = { path = \"vendor/clib\" } and that
     vendor/clib is present. See vendor/clib/WHY.md."
        fi
        if ! grep -qF "$CLIB_TCL_FAILED" "$log" 2>/dev/null; then
            rm -f "$log" "$rcf"
            die "cargo build failed"
        fi
        case "$attempt" in
            1)
                note "the tcl crate failed to build. clib/inwelling lose a race with"
                note "the build directory now and then — retrying as-is, which is"
                note "the documented workaround."
                ;;
            2)
                note "still failing; dropping clib's artifacts so its build script reruns"
                note "against a target/ that now HAS the marker the first attempt wrote"
                cargo clean -p clib 2>/dev/null || true
                ;;
            *)
                rm -f "$log" "$rcf"
                die "cargo build failed 3 times on the tcl crate.
     This is not the transient ordering issue a retry fixes. Check that
     exactly ONE tclConfig.sh is reachable (PKG_CONFIG_PATH, TCL_LIBRARY)
     and that libclang matches the clang headers BINDGEN_EXTRA_CLANG_ARGS
     points at."
                ;;
        esac
        attempt=$((attempt + 1))
    done
    rm -f "$log" "$rcf"
    note "built $VELES_REPO/target/release/veles"
}

install_veles() {
    say "installing to $VELES_PREFIX/bin"
    mkdir -p "$VELES_PREFIX/bin"
    install -m 0755 "$VELES_REPO/target/release/veles" "$VELES_PREFIX/bin/veles"
    note "installed $VELES_PREFIX/bin/veles"
    # The bundled Tcl plugins, at bin/../share/veles/plugins where the
    # loader looks (plugins.rs bundled_roots; flake.nix bundledTclPlugins
    # is the same list). Replaced whole: the tier is read-only to veles.
    for p in tcl-anthropic-driver; do
        rm -rf "$VELES_PREFIX/share/veles/plugins/$p"
        mkdir -p "$VELES_PREFIX/share/veles/plugins"
        cp -R "$VELES_REPO/plugins/examples/$p" "$VELES_PREFIX/share/veles/plugins/$p"
    done
    note "installed $VELES_PREFIX/share/veles/plugins"
    case ":$PATH:" in
        *":$VELES_PREFIX/bin:"*) ;;
        *) note "NOTE: $VELES_PREFIX/bin is not on your PATH — add it." ;;
    esac
}

# ── service ─────────────────────────────────────────────────────────
install_service_systemd() {
    say "installing the systemd unit ($VELES_SCOPE scope)"
    if [ "$VELES_SCOPE" = "system" ]; then
        need_root
        $SUDO install -m 0644 "$VELES_REPO/scripts/install/units/veles.service" \
            /etc/systemd/system/veles.service
        $SUDO systemctl daemon-reload
        note "enable with: sudo systemctl enable --now veles"
    else
        mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
        sed "s|@VELES_BIN@|$VELES_PREFIX/bin/veles|" \
            "$VELES_REPO/scripts/install/units/veles.service" \
            > "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/veles.service"
        systemctl --user daemon-reload 2>/dev/null || true
        note "enable with: systemctl --user enable --now veles"
    fi
}

install_service_rcd() {
    say "installing the rc.d script (FreeBSD)"
    need_root
    $SUDO install -m 0555 "$VELES_REPO/scripts/install/units/veles.rc" \
        /usr/local/etc/rc.d/veles
    note 'enable with: sysrc veles_enable=YES && service veles start'
}

# veles' `tclsh_path()` looks for a binary named exactly `tclsh` on
# PATH. Several systems ship only a VERSIONED one:
#
#   FreeBSD  lang/tcl86     -> /usr/local/bin/tclsh8.6, never `tclsh`
#   Debian   tcl8.6 alone   -> /usr/bin/tclsh8.6 (the `tcl` metapackage
#                              is what provides the unversioned name)
#
# The failure mode is nasty: everything builds, everything installs, and
# every sandboxed run fails at the point of execution. So resolve it at
# INSTALL time and write the answer into the environment the service
# reads, rather than leaving it to be discovered later.
ensure_tclsh() {
    if have tclsh; then
        note "tclsh: $(command -v tclsh)"
        return 0
    fi
    for v in tclsh8.7 tclsh8.6 tclsh86 tclsh8.5; do
        if have "$v"; then
            resolved="$(command -v "$v")"
            note "no unversioned tclsh; using $resolved"
            mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/veles"
            envf="${XDG_CONFIG_HOME:-$HOME/.config}/veles/.env"
            if ! grep -q '^VELES_TCLSH=' "$envf" 2>/dev/null; then
                printf 'VELES_TCLSH=%s\n' "$resolved" >> "$envf"
                chmod 600 "$envf"
                note "recorded VELES_TCLSH in $envf"
            fi
            VELES_TCLSH="$resolved"; export VELES_TCLSH
            return 0
        fi
    done
    die "no tclsh of any version on PATH — the Tcl runtime did not install.
     veles' scripting core, the execute_code tool and every sandbox
     backend need it; there is no degraded mode without it."
}

# ── the part that makes this an installer and not a wish ────────────
#
# `doctor` checks config, the provider route chain, the store and Tcl.
# `doctor --sandbox` resolves the configured fence, reports its binary
# and artifacts, and RUNS `puts [expr {6*7}]` through it. Both exit
# non-zero on failure, so this function is the script's verdict.
verify_install() {
    VELES="$VELES_PREFIX/bin/veles"
    # A source-built program needs the host's interpreter. A signed
    # portable bundle carries its own; probing the host would reject a
    # valid clean-machine install for a toolchain it does not use.
    [ "${1:-source}" = "bundle" ] || ensure_tclsh
    say "verifying the install by running it"
    "$VELES" version || die "the installed binary does not run"

    # doctor needs a config to check a route chain against; on a fresh
    # host there is none, and that is not an install failure. Report it
    # and keep going — the sandbox probe is the part that tests THIS
    # script's work.
    if "$VELES" doctor; then
        note "doctor: OK"
    else
        note "doctor reported problems (expected on a host with no provider configured yet)"
    fi

    if [ "${1:-source}" = "bundle" ]; then
        note "binary and runtime executed; the selected setup profile verifies its sandbox and components"
        return 0
    fi
    say "sandbox fence"
    if "$VELES" doctor --sandbox; then
        note "the fence runs."
    else
        die "the sandbox fence does not work on this host — see the report above.
     This is a real failure: it means untrusted code has nowhere safe to run.
     Fix the reported binary/artifact, or set [sandbox] backend = \"native\"."
    fi
}

summary() {
    say "done"
    note "binary:  $VELES_PREFIX/bin/veles"
    note "config:  ${XDG_CONFIG_HOME:-$HOME/.config}/veles/config.toml"
    note "state:   ${XDG_DATA_HOME:-$HOME/.local/share}/veles"
    note ""
    note "next:    veles setup     # pick a provider and model"
    note "         veles chat      # or: veles gui"
}
