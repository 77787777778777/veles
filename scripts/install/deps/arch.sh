#!/bin/sh
# Arch Linux dependency step.
#
# ══ THE 47-ERROR BUILD FAILURE, AND WHAT IT REALLY WAS ══
#
# Until 2026-09-04 this arm failed, every time, with 47 errors of the
# form:
#
#   error[E0609]: no field `refCount` on type `&Tcl_Obj`
#   = note: available field is: `_address`
#
# The cause is Arch's clang, and only that: bindgen 0.64 — the version
# `clib` 0.2.4 pins — cannot parse correctly with libclang 22, and falls
# back to marking every record containing an anonymous union OPAQUE.
# `Tcl_Obj` is one. Arch had 22.1.8; Debian sid and NixOS had 21.1.8 and
# built fine. veles now vendors clib with a bindgen that handles it
# (vendor/clib/WHY.md), and this arm passes.
#
# Two explanations written down here before the generated bindings were
# actually read, both wrong, both recorded so they are not re-derived:
#
#   * "inwelling returned no packages, so clib's generate_dummy() wrote
#     an EMPTY bindings.rs". The file was 478 KB and inwelling HAD found
#     the tcl crate. generate_dummy() was never reached. (The race it
#     describes is real, just not this failure — see common.sh.)
#   * "Arch's packaged rust toolchain". Arch and Debian both ran rustup
#     stable 1.98.1.
#
# Also eliminated, and kept because they are right anyway: the clang
# resource dir (setup_build_env still sets it — Gentoo needs it) and
# clib's include-dir layout (/usr/include/tcl8.6, created below,
# because clib genuinely looks there).
set -eu
. "$(dirname "$0")/../common.sh"
[ "${VELES_FROM_SOURCE:-0}" = 1 ] || exec "$(dirname "$0")/bootstrap-arch.sh"

say "Arch: installing build and runtime dependencies"
need_root
# NOTE tcllib is NOT here. It is not in Arch's official repositories —
# `error: target not found: tcllib`, which failed the whole transaction
# on 2026-09-04 and took the required set down with it. It lives in the
# AUR, and veles' engine does not need it: the Tcl DESKTOP does (its
# json/http helpers), the CLI and every sandbox backend do not.
# NOTE `rust` is NOT in this list; rustup is installed below instead.
# Not because Arch's packaged toolchain is broken — it is not, and an
# earlier version of this comment said so wrongly — but because rustup
# is what the other arms use, so the matrix compares like with like.
$SUDO pacman -Sy --needed --noconfirm \
    base-devel pkgconf git curl \
    openssl zlib sqlite \
    tcl tk \
    clang llvm \
    openssh ripgrep libwebp libheif e2fsprogs \
    || die "pacman could not install the required set"

# The `clib` crate (a build dependency of `tcl`) locates Tcl headers by
# looking for an `include/tcl8.6/` or `include/tcl/` SUBDIRECTORY:
#
#   [package.metadata.inwelling.clib.spec.tcl86]
#   headers    = ["tcl.h"]
#   includedir = ["tcl8.6", "tcl"]
#
# Debian puts them in /usr/include/tcl8.6/ and matches. Arch puts
# tcl.h directly in /usr/include and does NOT — so clib falls back to
# the crate's docs.rs STUB, where Tcl_Obj is an opaque `_address`, and
# the build dies with 47 errors about missing struct fields that say
# nothing about a missing header:
#
#   error[E0609]: no field `refCount` on type `&Tcl_Obj`
#   = note: available field is: `_address`
#
# Give it the layout it looks for. Symlinks, so there is one copy.
if [ -f /usr/include/tcl.h ] && [ ! -e /usr/include/tcl8.6/tcl.h ]; then
    note "creating /usr/include/tcl8.6 (clib looks for headers in a versioned subdir)"
    $SUDO mkdir -p /usr/include/tcl8.6
    for h in /usr/include/tcl*.h; do
        $SUDO ln -sf "$h" "/usr/include/tcl8.6/$(basename "$h")"
    done
fi

# rustup rather than Arch's packaged rust, matching what the Debian arm
# does. NOTE: this is consistency, not a fix — the tcl crate fails to
# build on Arch either way (see the KNOWN FAILURE note at the top of
# this file). An earlier version of this comment claimed the packaged
# toolchain was the cause; testing rustup stable 1.98.1, the exact
# version the passing Debian arm uses, disproved that.
if [ ! -x "$HOME/.cargo/bin/cargo" ]; then
    note "installing rust via rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
        || die "rustup install failed"
fi
. "$HOME/.cargo/env"

# Optional, and named when absent — installed ONE AT A TIME, because
# pacman fails the whole transaction on a single unknown target and a
# missing nice-to-have must not take the install with it.
for p in podman xorg-server-xvfb; do
    $SUDO pacman -S --needed --noconfirm "$p" 2>/dev/null \
        || note "optional package unavailable: $p"
done
note "AUR (optional, for the full Tcl desktop): tcllib tclx tkimg tclcurl resvg"
note "without them the CLI and the sandbox fences work; the desktop is degraded"
