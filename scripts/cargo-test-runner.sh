#!/bin/sh
# scripts/cargo-test-runner.sh — cargo's runner on Linux (.cargo/config.toml):
# a test binary keeps its scratch files in RAM.
#
# WHY. Most tests make a scratch home with a SQLite store in the temp
# directory. `nix develop` sets TMPDIR to a directory under /tmp, which is
# on disk here, so a bare `cargo test -p veles-core --lib` syncs thousands
# of little databases to disk: 68 minutes on 2026-11-11 with the disk busy,
# against 157 seconds for the same 2,411 tests in RAM (default threads). scripts/precommit.sh and
# the flake's test derivation have always pointed TMPDIR at /dev/shm; a
# plain `cargo test` did not, and that is what people and agents type.
#
# WHAT IT DOES. For a cargo TEST binary (target/<profile>/deps/<name>-<hash>)
# whose TMPDIR is not already on tmpfs, it makes one directory under
# /dev/shm, runs the binary with TMPDIR there, and removes the directory
# when the binary exits, so what a test leaks does not pile up in RAM
# (2.3 GB had by 2026-09-28, the reason precommit.sh uses a per-run
# directory too). Everything else runs untouched: `cargo run`, a TMPDIR
# that is already tmpfs (the gate's), a machine without a writable
# /dev/shm, and VELES_TEST_TMP_ON_DISK=1 for a test that must see a real
# disk.
case "$1" in
    */deps/*) ;;
    *) exec "$@" ;;
esac
[ -z "${VELES_TEST_TMP_ON_DISK:-}" ] || exec "$@"
[ -d /dev/shm ] && [ -w /dev/shm ] || exec "$@"
fs=$(stat -f -c %T "${TMPDIR:-/tmp}" 2>/dev/null) || fs=
[ "$fs" != tmpfs ] || exec "$@"
d=$(mktemp -d /dev/shm/veles-test.XXXXXX) || exec "$@"
trap 'rm -rf "$d"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP
TMPDIR="$d" "$@"
exit $?
