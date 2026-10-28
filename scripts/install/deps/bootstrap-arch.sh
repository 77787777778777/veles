#!/bin/sh
set -eu
. "$(dirname "$0")/../common.sh"
need_root
$SUDO pacman -Sy --needed --noconfirm curl ca-certificates cosign jq zstd \
    || die "could not install the signed-bootstrap dependencies"
