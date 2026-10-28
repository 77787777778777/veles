#!/bin/sh
# Runtime verification only: no compiler, Tcl development headers or rustup.
set -eu
. "$(dirname "$0")/../common.sh"
need_root
$SUDO apt-get update
$SUDO apt-get install -y --no-install-recommends \
    curl ca-certificates cosign jq zstd \
    || die "could not install the signed-bootstrap dependencies"
