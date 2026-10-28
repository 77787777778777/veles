#!/usr/bin/env bash
# scripts/install/test-matrix.sh — prove the installers, don't claim them.
#
# WHY CONTAINERS FOR FOUR OF THEM AND A VM FOR THE FIFTH
#
# The question each rig has to answer is "does the dependency step get
# this distro to a working `veles doctor`?". For Debian sid, Ubuntu,
# Arch and Gentoo that is entirely answerable in a container: apt/
# pacman/emerge run, cargo builds, the binary runs, the native
# safe-interp fence executes a script. Nothing in that chain needs PID 1
# or KVM.
#
# What a container CANNOT answer: systemd units, the nspawn fence, the
# firecracker fence. Those go to VMs —
#   * `nix build .#nixos-module-test` (systemd + backends)
#   * scripts/sandbox_vm_check.sh freebsd  (the jail fence)
# FreeBSD has no container path at all, so it is a VM by necessity;
# $VELES_VM_DIR/ci-freebsd.qcow2 is the image (scripts/sandbox_vm_check.sh).
#
# Gentoo uses the binhost (`--getbinpkg`). A from-source toolchain build
# is hours, and the binhost is what Gentoo users actually run; the
# ebuild's dependency resolution is still exercised for real.
#
# ══ THIS IS AN ON-DEMAND RIG. NOTHING RUNS IT FOR YOU. ══
#
# It is not in scripts/precommit.sh, not in `nix flake check`, and not
# in any hook. A full four-target run is roughly an hour and writes
# several GB of container overlay, because each arm does a complete
# `cargo build --release` of the workspace from a clean tree.
#
# USER (2026-09-04): "can we make sure those tests only ever run
# on-demand explicitly, because this takes hours and uses up all my disk
# space for very little benefit beyond testing it worked once. I guess
# maybe it won't be so bad once the test images are built though."
#
# So: the dependency step — the genuinely slow, network-bound half — is
# cached as a per-distro image after its first success, and the cargo
# REGISTRY is shared across runs and distros in one volume. A second run
# of the same target skips apt/pacman/emerge entirely and reuses every
# downloaded crate. What is deliberately NOT cached by default is the
# target/ directory: that is 4-6 GB per distro, which is the disk this
# rig was eating. Ask for it explicitly if you are iterating.
#
# Usage:
#   scripts/install/test-matrix.sh              # every container target
#   scripts/install/test-matrix.sh debian arch  # a subset
#   VELES_MATRIX_FRESH=1 ...                    # ignore the cached deps images
#   VELES_MATRIX_CACHE_BUILD=1 ...              # also keep target/ per distro (GBs)
#   VELES_MATRIX_KEEP=1 ...                     # leave containers behind
#   scripts/install/test-matrix.sh --clean      # delete every cached image/volume
set -uo pipefail
cd "$(dirname "$0")/../.."
REPO="$PWD"

RUNNER="${VELES_MATRIX_RUNNER:-podman}"
command -v "$RUNNER" >/dev/null 2>&1 || {
    echo "no $RUNNER on this host." >&2
    echo "On NixOS: virtualisation.podman.enable = true (see /etc/nixos/README.md)." >&2
    exit 1
}

declare -A IMAGES=(
    [debian]="docker.io/library/debian:sid"
    [ubuntu]="docker.io/library/ubuntu:24.04"
    [arch]="docker.io/library/archlinux:latest"
    [gentoo]="docker.io/gentoo/stage3:latest"
)

CACHE_PREFIX="veles-matrix"
REGISTRY_VOL="${CACHE_PREFIX}-cargo-registry"

if [ "${1:-}" = "--clean" ]; then
    echo "removing cached matrix images and volumes"
    # `podman rmi -f` and `volume rm -f` both exit 0 for something that
    # was never there, so ASK first — a cleanup that reports removing
    # four images it did not have is the same kind of lie as a test that
    # cannot fail.
    removed=0
    for t in debian ubuntu arch gentoo; do
        if "$RUNNER" image exists "${CACHE_PREFIX}-${t}:deps" 2>/dev/null; then
            "$RUNNER" rmi -f "${CACHE_PREFIX}-${t}:deps" >/dev/null 2>&1 \
                && { echo "  image ${CACHE_PREFIX}-${t}:deps"; removed=$((removed + 1)); }
        fi
        if "$RUNNER" volume exists "${CACHE_PREFIX}-${t}-target" 2>/dev/null; then
            "$RUNNER" volume rm -f "${CACHE_PREFIX}-${t}-target" >/dev/null 2>&1 \
                && { echo "  volume ${CACHE_PREFIX}-${t}-target"; removed=$((removed + 1)); }
        fi
    done
    if "$RUNNER" volume exists "$REGISTRY_VOL" 2>/dev/null; then
        "$RUNNER" volume rm -f "$REGISTRY_VOL" >/dev/null 2>&1 \
            && { echo "  volume $REGISTRY_VOL"; removed=$((removed + 1)); }
    fi
    [ "$removed" -eq 0 ] && echo "  nothing was cached"
    exit 0
fi

TARGETS=("$@")
[ ${#TARGETS[@]} -eq 0 ] && TARGETS=(debian ubuntu arch gentoo)

# A full run needs room. Say so BEFORE spending an hour discovering it:
# the failure mode of a full disk here is a cargo error deep inside a
# container log, not a message about disk.
free_gb=$(df -BG --output=avail "$HOME" 2>/dev/null | tail -1 | tr -dc '0-9')
if [ -n "${free_gb:-}" ] && [ "$free_gb" -lt 15 ]; then
    echo "WARNING: only ${free_gb}G free where the container store lives." >&2
    echo "         Each arm writes a few GB. \`$0 --clean\` frees the caches," >&2
    echo "         and \`$RUNNER system prune -a\` frees everything else." >&2
fi

declare -A RESULT
declare -A DETAIL

for t in "${TARGETS[@]}"; do
    img="${IMAGES[$t]:-}"
    if [ -z "$img" ]; then
        RESULT[$t]="SKIP"; DETAIL[$t]="no container image for '$t'"
        continue
    fi
    echo
    echo "════════════════════════════════════════════════════════════"
    echo "  $t  ($img)"
    echo "════════════════════════════════════════════════════════════"
    log="$(mktemp -p "${TMPDIR:-/dev/shm}" veles-matrix-"$t"-XXXX.log)"

    # ── the dependency step, cached ──────────────────────────────
    #
    # apt/pacman/emerge is the slow, network-bound half and it changes
    # only when deps/<t>.sh does. Run it ONCE into an image and reuse
    # that image afterwards, so a re-run of this rig goes straight to
    # the cargo build. `VELES_MATRIX_FRESH=1` forces it again, which is
    # what you want after editing a dependency script.
    deps_img="${CACHE_PREFIX}-${t}:deps"
    run_img="$img"
    skip_deps=0
    if [ -z "${VELES_MATRIX_FRESH:-}" ] && "$RUNNER" image exists "$deps_img" 2>/dev/null; then
        echo "  using cached dependency image $deps_img (VELES_MATRIX_FRESH=1 to rebuild)"
        run_img="$deps_img"
        skip_deps=1
    else
        echo "  building the dependency image $deps_img (once per distro)"
        cid="${CACHE_PREFIX}-${t}-deps-$$"
        if "$RUNNER" run --name "$cid" -v "$REPO:/src:ro" "$img" \
                sh -c "VELES_FROM_SOURCE=1 sh /src/scripts/install/deps/${t}.sh" >>"$log" 2>&1; then
            "$RUNNER" commit -q "$cid" "$deps_img" >>"$log" 2>&1 || true
            "$RUNNER" rm -f "$cid" >/dev/null 2>&1 || true
            run_img="$deps_img"
            skip_deps=1
        else
            # Keep going against the base image: the dependency step
            # failing is itself a result this rig should report, and the
            # install run below will report it in context.
            "$RUNNER" rm -f "$cid" >/dev/null 2>&1 || true
            echo "  the dependency step failed; running uncached so the log shows why"
        fi
    fi

    # The cargo registry is architecture-independent and the same for
    # every distro, so ONE volume serves them all — that is the download
    # half of the build, cached, for about a gigabyte total.
    cache_args=(-v "$REGISTRY_VOL:/root/.cargo/registry")
    # target/ is NOT cached by default: 4-6 GB per distro is the disk
    # this rig was accused of eating, and the whole point of the run is a
    # build from clean. Opt in when iterating on a build failure.
    if [ -n "${VELES_MATRIX_CACHE_BUILD:-}" ]; then
        cache_args+=(-v "${CACHE_PREFIX}-${t}-target:/build/target")
        echo "  VELES_MATRIX_CACHE_BUILD=1 — keeping target/ in a volume (GBs)"
    fi

    # The repo is mounted READ-ONLY and copied in: an installer that
    # writes to the source tree would corrupt the host's checkout, and
    # this rig should not be the thing that discovers that.
    #
    # `--network` stays ON: the whole point is that the dependency step
    # can reach the distro's archive.
    if "$RUNNER" run --rm \
        -v "$REPO:/src:ro" \
        "${cache_args[@]}" \
        -e VELES_PREFIX=/usr/local \
        -e VELES_SKIP_DEPS="$skip_deps" \
        -e VELES_FROM_SOURCE=1 \
        -e CARGO_TERM_COLOR=never \
        "$run_img" \
        sh -c '
            set -e
            # Copy out of the read-only mount so cargo can write target/.
            #
            # EXCLUDE target/ AND the release tarball. `cp -a /src /build`
            # followed by `rm -rf /build/target` — which is what this did
            # until 2026-09-04 — copies 36 GB of build artifacts into the
            # container overlay and then deletes them. The first run of
            # this matrix spent 25 minutes on that and was killed before
            # apt had even started, with an empty log that looked like a
            # hang.
            echo "=== staging the source tree ==="
            mkdir -p /build
            tar -C /src -cf - \
                --exclude=./target --exclude=./.git \
                --exclude=./*.tar.gz --exclude=./.direnv \
                . | tar -C /build -xf -
            cd /build
            echo "=== staged $(du -sh /build | cut -f1) ==="
            export VELES_REPO=/build HOME=/root
            sh scripts/install/install.sh --from-source '"$t"'
        ' >"$log" 2>&1
    then
        RESULT[$t]="PASS"
        DETAIL[$t]="$(grep -E 'result: OK|the fence runs' "$log" | tail -1)"
    else
        RESULT[$t]="FAIL"
        DETAIL[$t]="$(grep -E '^✗|die|error|E: |Permission denied' "$log" | tail -1)"
        echo "--- last 25 lines ---"
        tail -25 "$log"
    fi
    echo "  log: $log"
done

echo
echo "════════════════════════════════════════════════════════════"
printf '  %-10s %-6s %s\n' "TARGET" "RESULT" "DETAIL"
fail=0
for t in "${TARGETS[@]}"; do
    printf '  %-10s %-6s %s\n' "$t" "${RESULT[$t]}" "${DETAIL[$t]:0:70}"
    [ "${RESULT[$t]}" = "FAIL" ] && fail=1
done
echo "════════════════════════════════════════════════════════════"
echo
echo "NOT covered here — these need a real machine, by nature:"
echo "  systemd units, nspawn, firecracker →  nix build .#nixos-module-test"
echo "  FreeBSD + the jail fence           →  scripts/sandbox_vm_check.sh freebsd"
exit $fail
