#!/usr/bin/env bash
# scripts/check.sh — the same gate as `nix flake check`, run incrementally.
#
# `nix flake check` is authoritative but rebuilds from scratch in a
# sandbox, which is slow for a dev loop. This runs the identical four
# checks against the local cargo target dir. Keep the two in sync: if you
# add a check here, add it to flake.nix `checks` too, or CI won't enforce
# it.
#
# Usage: scripts/check.sh            (from inside the dev shell)
#        direnv exec . scripts/check.sh
set -uo pipefail
cd "$(dirname "$0")/.."

# Run tests against a tmpfs, not the SSD.
#
# `/tmp` is on the root filesystem here, not a tmpfs, and the test suite
# is write-heavy: several fixtures build a scratch VELES_HOME with its
# own git repo, and memory::tests::consecutive_same_size_writes_each_commit
# alone performs 80 commits per run. Repeated stress runs put a lot of
# needless wear on the disk. tempfile and everything downstream of it
# honour TMPDIR.
#
# And in a directory of this run's own, removed on exit: whatever a test
# or a killed child leaves behind goes with it instead of piling up at
# the top of /dev/shm (see scripts/precommit.sh).
shm="${TMPDIR:-}"
if [ -z "$shm" ] && [ -d /dev/shm ] && [ -w /dev/shm ]; then
    shm=/dev/shm
fi
RUN_TMP=$(mktemp -d "${shm:-/tmp}/veles-check.XXXXXX") || exit 1
trap 'rm -rf "$RUN_TMP"' EXIT
export TMPDIR="$RUN_TMP"

fail=0
run() {
    local name="$1"; shift
    echo "── $name ─────────────────────────────────────────────"
    if "$@"; then
        echo "   ✓ $name"
    else
        echo "   ✗ $name" >&2
        fail=1
    fi
}

run "cargo fmt"   cargo fmt --all --check
run "cargo clippy" cargo clippy --workspace --all-targets --all-features -- -D warnings
run "cargo test"  cargo test --workspace --all-features
run "tcl desktop" bash scripts/tcl_tests.sh

echo
if [ "$fail" -eq 0 ]; then
    echo "all checks passed"
else
    echo "CHECKS FAILED" >&2
fi
exit "$fail"
