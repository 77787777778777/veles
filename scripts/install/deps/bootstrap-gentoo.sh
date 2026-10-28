#!/bin/sh
set -eu
. "$(dirname "$0")/../common.sh"
need_root
# Keep the source-build profile selection and binhost policy intact.
if [ ! -L /etc/portage/make.profile ]; then
    $SUDO eselect profile list >/dev/null 2>&1 || die "eselect not available"
    prof=$($SUDO eselect profile list 2>/dev/null | grep -oE 'default/linux/(amd64|arm64)/[0-9.]+ ?\(stable\)?' | head -1 | awk '{print $1}')
    [ -n "$prof" ] || die "select a Gentoo profile before installing bootstrap dependencies"
    $SUDO eselect profile set "$prof"
fi
[ -d /var/db/repos/gentoo/profiles ] || { $SUDO emerge-webrsync || $SUDO emerge --sync; }
opts="--noreplace --quiet-build=y"
[ "${VELES_GENTOO_SOURCE:-0}" = 1 ] || opts="$opts --getbinpkg --binpkg-respect-use=y"
# shellcheck disable=SC2086
$SUDO emerge $opts net-misc/curl app-crypt/ca-certificates app-containers/cosign app-misc/jq app-arch/zstd \
    || die "could not install the signed-bootstrap dependencies"
