#!/usr/bin/env bash
# scripts/cargo-test-runner-test.sh — what scripts/cargo-test-runner.sh
# promises, checked with a fake test binary (no cargo, a second or two).
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
run="$here/cargo-test-runner.sh"
[ -d /dev/shm ] && [ -w /dev/shm ] || { echo "cargo test runner: no writable /dev/shm here, nothing to check"; exit 0; }
disk=$(mktemp -d "${VELES_RUNNER_TEST_DISK:-/var/tmp}/veles-runner-test.XXXXXX") || exit 1
trap 'rm -rf "$disk"' EXIT
fail=0
ok() { if eval "$2"; then echo "  ok   $1"; else echo "  FAIL $1"; fail=1; fi; }
mkdir -p "$disk/target/debug/deps" "$disk/tmp"
fake="$disk/target/debug/deps/fake-0123456789abcdef"
printf '#!/bin/sh\necho "$TMPDIR" > "%s/seen"\ntouch "$TMPDIR/leak"\nexit ${FAKE_RC:-0}\n' "$disk" > "$fake"; chmod +x "$fake"
plain="$disk/target/debug/fake"
\cp "$fake" "$plain"
on_disk_fs=$(stat -f -c %T "$disk/tmp")
if [ "$on_disk_fs" = tmpfs ]; then echo "cargo test runner: $disk is tmpfs, cannot stage a disk TMPDIR"; exit 0; fi

TMPDIR="$disk/tmp" "$run" "$fake"; rc=$?
seen=$(cat "$disk/seen")
ok "a test binary on a disk TMPDIR runs with one under /dev/shm" '[[ "$seen" == /dev/shm/veles-test.* ]]'
ok "its exit status is the binary's" '[ "$rc" = 0 ]'
ok "the directory is gone afterwards, leak and all" '[ ! -e "$seen" ]'
FAKE_RC=7 TMPDIR="$disk/tmp" "$run" "$fake"; rc=$?
ok "a failing binary's status comes through" '[ "$rc" = 7 ]'
shm=$(mktemp -d /dev/shm/veles-runner-test.XXXXXX)
TMPDIR="$shm" "$run" "$fake"
ok "a TMPDIR that is tmpfs already is left alone" '[ "$(cat "$disk/seen")" = "$shm" ]'
rm -rf "$shm"
TMPDIR="$disk/tmp" "$run" "$plain"
ok "a binary outside deps/ (cargo run) is left alone" '[ "$(cat "$disk/seen")" = "$disk/tmp" ]'
VELES_TEST_TMP_ON_DISK=1 TMPDIR="$disk/tmp" "$run" "$fake"
ok "VELES_TEST_TMP_ON_DISK=1 keeps the disk" '[ "$(cat "$disk/seen")" = "$disk/tmp" ]'
ok "nothing of ours is left in /dev/shm" '! ls -d /dev/shm/veles-test.* >/dev/null 2>&1 || [ -n "${VELES_RUNNER_TEST_BUSY:-}" ]'
[ $fail = 0 ] && echo "cargo test runner: all checks passed" || echo "cargo test runner: FAILED"
exit $fail
