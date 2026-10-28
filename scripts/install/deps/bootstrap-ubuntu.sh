#!/bin/sh
set -eu
. "$(dirname "$0")/../common.sh"
need_root
have add-apt-repository && $SUDO add-apt-repository -y universe 2>/dev/null || true
exec "$(dirname "$0")/bootstrap-debian.sh"
