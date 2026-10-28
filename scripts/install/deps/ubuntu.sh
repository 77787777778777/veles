#!/bin/sh
# Ubuntu LTS dependency step.
#
# Ubuntu is Debian's archive with different defaults, so the package set
# is the same list. Kept as its own file rather than a symlink because
# the two DO diverge (universe, snap-only packages, an older tcl), and a
# shared file would make the first divergence someone else's problem.
set -eu
. "$(dirname "$0")/../common.sh"

say "Ubuntu: enabling universe, then installing dependencies"
need_root
have add-apt-repository && $SUDO add-apt-repository -y universe 2>/dev/null || true
[ "${VELES_FROM_SOURCE:-0}" = 1 ] || exec "$(dirname "$0")/bootstrap-ubuntu.sh"

# Delegate: the package names are identical today. If they stop being,
# this is where the fork goes.
exec "$(dirname "$0")/debian.sh"
