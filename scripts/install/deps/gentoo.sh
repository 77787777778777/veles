#!/bin/sh
# Gentoo dependency step.
#
# `--getbinpkg` by default: a from-source toolchain build is hours, and
# Gentoo's own binhost is what most users actually run. Set
# VELES_GENTOO_SOURCE=1 to build everything from source instead.
set -eu
. "$(dirname "$0")/../common.sh"
[ "${VELES_FROM_SOURCE:-0}" = 1 ] || exec "$(dirname "$0")/bootstrap-gentoo.sh"

say "Gentoo: installing build and runtime dependencies"
need_root

# A stage3 container ships /etc/portage/make.profile UNSET, and portage
# refuses every merge until it points at a profile:
#
#   !!! Your current profile is invalid.
#   !!! Allowed actions are limited to --help, --info, --search, --sync,
#       and --version.
#
# On a real Gentoo install this is done once at setup and never thought
# about again, which is exactly why an installer that only ever ran on
# real Gentoo installs would never have noticed. Found 2026-09-04 by
# running it in a stage3.
if [ ! -L /etc/portage/make.profile ]; then
    note "no profile selected (stage3 default) — selecting one"
    $SUDO eselect profile list >/dev/null 2>&1 || die "eselect not available"
    prof=$($SUDO eselect profile list 2>/dev/null \
           | grep -oE 'default/linux/amd64/[0-9.]+ ?\(stable\)?' \
           | head -1 | awk '{print $1}')
    [ -n "$prof" ] || prof=$($SUDO eselect profile list 2>/dev/null \
           | grep -oE 'default/linux/amd64/[0-9.]+' | head -1)
    [ -n "$prof" ] || die "no default/linux/amd64 profile offered by eselect"
    note "eselect profile set $prof"
    $SUDO eselect profile set "$prof" || die "could not select profile $prof"
fi

# A stage3 has no portage tree at all until it is synced.
if [ ! -d /var/db/repos/gentoo/profiles ]; then
    note "no portage tree — syncing (this is slow)"
    $SUDO emerge-webrsync || $SUDO emerge --sync || die "could not fetch a portage tree"
fi

EMERGE_OPTS="--noreplace --quiet-build=y"
if [ "${VELES_GENTOO_SOURCE:-0}" = "1" ]; then
    note "building from source (VELES_GENTOO_SOURCE=1) — this takes hours"
else
    EMERGE_OPTS="$EMERGE_OPTS --getbinpkg --binpkg-respect-use=y"
    note "using the binhost; set VELES_GENTOO_SOURCE=1 to build from source"
fi

# shellcheck disable=SC2086
# ATOM NAMES MATTER AND THEY MOVE. `sys-devel/clang` has not existed
# for some time — Gentoo relocated the LLVM stack to the `llvm-core/`
# category, and emerge says so helpfully but fatally:
#
#   emerge: there are no ebuilds to satisfy "sys-devel/clang".
#   emerge: Maybe you meant any of these: sys-devel/bc, sys-devel/gcc
#
# rust-bin rather than rust: the source build is a multi-hour toolchain
# compile, and a binary toolchain is what the binhost posture is for.
$SUDO emerge $EMERGE_OPTS \
    dev-lang/tcl dev-lang/tk dev-tcltk/tcllib dev-tcltk/tclx \
    dev-libs/openssl sys-libs/zlib dev-db/sqlite \
    llvm-core/clang dev-lang/rust-bin dev-vcs/git \
    net-misc/openssh sys-apps/ripgrep media-libs/libwebp media-libs/libheif \
    sys-fs/e2fsprogs virtual/pkgconfig \
    || die "emerge could not install the required set"

for p in app-containers/podman dev-tcltk/tkimg dev-tcltk/tclcurl; do
    # shellcheck disable=SC2086
    $SUDO emerge $EMERGE_OPTS "$p" 2>/dev/null \
        || note "optional package unavailable: $p"
done
