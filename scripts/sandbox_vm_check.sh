#!/usr/bin/env bash
# sandbox_vm_check.sh — RUNTIME verification of the sandbox backends.
#
# WHAT CHANGED 2026-09-04, AND WHY IT MATTERED
#
# The first version of this script hand-rolled an equivalent
# `systemd-nspawn` / `podman` / `jail` invocation and checked that IT
# worked. That proves the runtime is installed. It cannot fail if
# `NspawnBackend` or `JailBackend` is wrong — which they were: running
# the real backend for the first time turned up three defects in one
# sitting (`--drop-capability=ALL` is a hard parse error for nspawn, the
# container got no PATH so `execv(tclsh)` failed, and the firecracker
# work drive was mounted read-only under a driver that writes to it).
#
# So every arm now drives the BINARY:
#
#     veles doctor --sandbox
#
# which resolves `[sandbox]` through `backend_from`, reports the binary
# and artifacts, and executes `puts [expr {6*7}]` through the fence. The
# hand-rolled probes survive as a PRE-FLIGHT — they say why the real
# check could not run — but they are no longer the evidence.
#
# Usage:
#   scripts/sandbox_vm_check.sh local        # native + nspawn + firecracker, here
#   scripts/sandbox_vm_check.sh firecracker  # just the microVM, here
#   scripts/sandbox_vm_check.sh nspawn       # just the namespace jail, here
#   scripts/sandbox_vm_check.sh podman       # just the container, here
#   scripts/sandbox_vm_check.sh freebsd      # the jail fence, in a VM
#   scripts/sandbox_vm_check.sh linux        # nspawn+podman in the ci-linux VM
set -uo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"

VM_DIR="${VELES_VM_DIR:-$HOME/.local/share/veles-ci}"
FREEBSD_IMG="$VM_DIR/ci-freebsd.qcow2"
LINUX_IMG="$VM_DIR/ci-linux.qcow2"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/ci-vm}"
SSH_PORT="${SSH_PORT:-2222}"
VM_MEM="${VM_MEM:-4096}"

# The CI VMs are libvirt DOMAINS on this machine, not loose qcow2 files:
# `ci-freebsd` at 192.168.122.60 and `ci-linux` at 192.168.122.50 on the
# `default` network, provisioned by cloud-init with the `ci` account and
# ~/.ssh/ci-vm, and reachable by those names through ~/.ssh/config.
#
# The first version of this script booted the qcow2 by hand with
# `qemu-system-x86_64 -nic user,hostfwd=…` instead. That looks
# equivalent and is not: the guest comes up on 10.0.2.x with a DIFFERENT
# resolver and no cloud-init seed, so the accounts the operator actually
# provisioned appear not to exist. The FreeBSD arm was written off as
# "image provisioning does not match ~/.ssh/ci-vm" on that evidence. It
# matched fine — the arm was starting a different machine.
#
# So: use the domain when libvirt has one, and keep the hand-rolled boot
# as the fallback for a machine that does not.
FREEBSD_DOMAIN="${FREEBSD_DOMAIN:-ci-freebsd}"
FREEBSD_HOST="${FREEBSD_HOST:-192.168.122.60}"
FREEBSD_USER="${FREEBSD_USER:-ci}"
LINUX_DOMAIN="${LINUX_DOMAIN:-ci-linux}"
LINUX_HOST="${LINUX_HOST:-192.168.122.50}"
LINUX_USER="${LINUX_USER:-ci}"
VIRSH="${VIRSH:-virsh -c qemu:///system}"

SSH_BASE="-i $SSH_KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o BatchMode=yes"
SSH_OPTS="$SSH_BASE -p $SSH_PORT"

VELES="${VELES_BIN:-$REPO/target/release/veles}"
[ -x "$VELES" ] || VELES="$REPO/target/debug/veles"

stage() { printf '\n▸ %s\n' "$*"; }
pass()  { printf '  \033[32m✓ %s\033[0m\n' "$*"; }
warn()  { printf '  \033[33m! %s\033[0m\n' "$*"; }
die()   { printf '\033[31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# ── the real check ──────────────────────────────────────────────────
#
# Write a [sandbox] section, then make veles run a script through it.
# VELES_HOME forces the single-directory layout, which is exactly what a
# throwaway probe home wants.
probe() { # $1 = label, $2 = toml body, $3... = command prefix (e.g. sudo)
    local label="$1" toml="$2"; shift 2
    local home
    home="$(mktemp -d -p "${TMPDIR:-/dev/shm}" veles-sbprobe-XXXX)"
    printf '%s\n' "$toml" > "$home/config.toml"
    stage "$label — veles doctor --sandbox"
    # PIPESTATUS, not $?: the pipe's status is grep's, and grep succeeds
    # whenever it printed a line — which it always does. A verdict taken
    # from the wrong end of a pipe is how a check reports green over a
    # failure.
    "$@" env VELES_HOME="$home" PATH="$PATH" "$VELES" doctor --sandbox 2>&1 \
        | grep -v 'INFO\|future version of systemd-nspawn'
    if [ "${PIPESTATUS[0]}" -eq 0 ]; then
        pass "$label fence runs"
        rm -rf "$home"; return 0
    fi
    warn "$label fence did NOT run — see the report above"
    rm -rf "$home"; return 1
}

rc=0

check_native() {
    probe "native" '[sandbox]
backend = "native"' || rc=1
}

check_nspawn() {
    command -v systemd-nspawn >/dev/null || { warn "systemd-nspawn absent — skipping"; return; }
    stage "nspawn: staging a writable rootfs"
    # nspawn takes a per-container lock NEXT TO the rootfs, so a
    # /nix/store path fails with "Failed to lock ...: Read-only file
    # system". The artifact is built read-only by design; the operator
    # stages a writable copy once.
    local stage_dir="${VELES_NSPAWN_ROOT:-$HOME/.local/share/veles/sandbox/nspawn-root}"
    if [ ! -d "$stage_dir" ]; then
        local built
        built="$(nix build "$REPO#nspawnRootfs" --no-link --print-out-paths 2>/dev/null)" \
            || { warn "cannot build .#nspawnRootfs — skipping nspawn"; return; }
        mkdir -p "$(dirname "$stage_dir")"
        cp -a "$built" "$stage_dir" && chmod -R u+w "$stage_dir"
    fi
    pass "rootfs at $stage_dir"
    # Root-gated by the backend itself; sudo is the operator posture.
    probe "nspawn" "[sandbox]
backend = \"nspawn\"
image = \"$stage_dir\"
network = false
capabilities = \"drop\"" sudo || rc=1
}

check_firecracker() {
    command -v firecracker >/dev/null || { warn "firecracker absent — skipping (add pkgs.firecracker)"; return; }
    [ -r /dev/kvm ] || { warn "/dev/kvm not readable — skipping"; return; }
    stage "firecracker: locating the guest kernel and rootfs"
    local kern root stable="$HOME/.local/share/veles/sandbox"
    # PREFER what the NixOS module already built. It derives the guest
    # kernel from the HOST's kernel source and links it to a stable
    # name; `.#firecrackerKernel` derives one from linuxPackages_latest
    # instead. Both are ~an hour of compilation, and building a second
    # kernel just to test the first is pure waste — so use the module's
    # when it is there.
    if [ -e "$stable/firecracker-vmlinux" ]; then
        kern="$(dirname "$(readlink -f "$stable/firecracker-vmlinux")")"
        pass "kernel from the NixOS module: $stable/firecracker-vmlinux"
    else
        kern="$(nix build "$REPO#firecrackerKernel" --no-link --print-out-paths 2>/dev/null)" \
            || { warn "cannot build .#firecrackerKernel — skipping"; return; }
        pass "kernel $kern/vmlinux"
    fi
    if [ -e "$stable/firecracker-rootfs.img" ]; then
        root="$(readlink -f "$stable/firecracker-rootfs.img")"
        pass "rootfs from the NixOS module: $stable/firecracker-rootfs.img"
    else
        root="$(nix build "$REPO#firecrackerRootfs" --no-link --print-out-paths 2>/dev/null)" \
            || { warn "cannot build .#firecrackerRootfs — skipping"; return; }
        pass "rootfs $root"
    fi
    # network MUST be false: firecracker has no tap devices here, and
    # backend_from refuses the combination loudly rather than silently
    # dropping the expectation.
    probe "firecracker" "[sandbox]
backend = \"firecracker\"
kernel_path = \"$kern/vmlinux\"
image = \"$root\"
network = false
memory_mb = 256" || rc=1
}

check_podman() {
    command -v podman >/dev/null || { warn "podman absent — skipping"; return; }
    if ! podman image exists veles-sandbox:latest 2>/dev/null; then
        warn "veles-sandbox:latest not built — run scripts/build-sandbox-image.sh"
        return
    fi
    # Both postures, because they are different fences: the dev posture
    # (network on, runtime caps) is what a model installs packages in;
    # the benchmark posture is the hardened pure-compute one.
    probe "podman (hardened)" '[sandbox]
backend = "podman"
image = "veles-sandbox:latest"
network = false
capabilities = "drop"' || rc=1
    probe "podman (dev)" '[sandbox]
backend = "podman"
image = "veles-sandbox:latest"
network = true
capabilities = "default"' || rc=1
}

# ── VM arms ─────────────────────────────────────────────────────────

# Bring a libvirt domain up and wait for SSH. Returns non-zero (without
# killing the run) when there is no such domain, so the caller can fall
# back to booting an image by hand.
domain_up() { # $1 = domain, $2 = host, $3 = user
    local dom="$1" host="$2" user="$3" state
    command -v virsh >/dev/null 2>&1 || return 1
    state="$($VIRSH domstate "$dom" 2>/dev/null)" || return 1
    [ -n "$state" ] || return 1
    if [ "$state" != "running" ]; then
        stage "starting libvirt domain $dom"
        $VIRSH start "$dom" >/dev/null 2>&1 || return 1
    else
        stage "libvirt domain $dom is already running"
    fi
    local i
    for i in $(seq 1 60); do
        if ssh $SSH_BASE "$user@$host" true 2>/dev/null; then
            pass "ssh up as $user@$host"
            VM_SSH="$SSH_BASE $user@$host"
            return 0
        fi
        sleep 3
    done
    # Up but unreachable is a real failure, not "no domain": say so
    # rather than silently booting a second copy of the same disk.
    die "domain $dom is running but SSH never answered at $user@$host.
     If it is stuck before the network, look at the console:
       $VIRSH console $dom
     A guest whose /etc/resolv.conf points at a nameserver it cannot
     reach ACCEPTS the TCP connection and then never sends an SSH
     banner — sshd blocks in the reverse lookup libwrap does. That is
     what an earlier ad-hoc qemu boot left behind on ci-freebsd."
}

boot() { # $1 = image, $2 = optional cloud-init seed image
    local seed=""
    # The ci-freebsd image is cloud-init provisioned, and cloud-init
    # reads its config from a SEED drive. Booting the qcow2 alone gives a
    # VM whose sshd is up and whose authorized_keys is empty — which
    # presents as "maximum authentication attempts exceeded" and looks
    # like a wrong key rather than a missing drive. Attach it when it is
    # there.
    [ -n "${2:-}" ] && [ -f "$2" ] && seed="-drive file=$2,format=raw,if=virtio"
    # shellcheck disable=SC2086
    qemu-system-x86_64 \
        -m "$VM_MEM" \
        -drive "file=$1,format=qcow2,if=virtio" \
        $seed \
        -nic "user,hostfwd=tcp:127.0.0.1:$SSH_PORT-:22" \
        -nographic -serial mon:stdio &
    VM_PID=$!
    trap 'kill $VM_PID 2>/dev/null || true' EXIT
    echo "  qemu pid $VM_PID${seed:+ (with cloud-init seed)}"
}

# Which account the image provisioned is not knowable from here, so try
# the ones these images actually use. SSH_USER pins it when you know.
ssh_wait() {
    local users="${SSH_USER:-${*:-root}}"
    stage "waiting for SSH on 127.0.0.1:$SSH_PORT (users: $users)"
    for _ in $(seq 1 60); do
        for u in $users; do
            if ssh $SSH_OPTS "$u@127.0.0.1" true 2>/dev/null; then
                pass "ssh up as $u"
                VM_USER="$u"
                return 0
            fi
        done
        sleep 2
    done
    return 1
}

check_freebsd() {
    if ! domain_up "$FREEBSD_DOMAIN" "$FREEBSD_HOST" "$FREEBSD_USER"; then
        warn "no libvirt domain $FREEBSD_DOMAIN — falling back to booting the image by hand"
        [ -f "$FREEBSD_IMG" ] || die "no $FREEBSD_IMG (the ci-freebsd image) and no $FREEBSD_DOMAIN domain"
        stage "booting $FREEBSD_IMG"
        boot "$FREEBSD_IMG" "$VM_DIR/freebsd-seed.img"
        ssh_wait root freebsd ci ubuntu || die "SSH never came up as root, freebsd, ci or ubuntu.
     Note this fallback boots with qemu USER networking, so the guest
     lands on 10.0.2.x with no cloud-init seed and no libvirt DHCP —
     accounts that exist on the provisioned machine can look absent.
     Prefer the libvirt domain: $VIRSH start $FREEBSD_DOMAIN"
        VM_SSH="$SSH_OPTS ${VM_USER:-root}@127.0.0.1"
    fi

    # PRE-FLIGHT only — this says whether the real check CAN run.
    stage "jail preconditions"
    # shellcheck disable=SC2086
    ssh $VM_SSH '
        echo "jail:   $(command -v jail || echo MISSING)"
        echo "tclsh:  $(command -v tclsh8.6 || command -v tclsh || echo MISSING)"
        echo "nullfs: $(kldstat -m nullfs 2>/dev/null | tail -1 || echo not-loaded)"
    ' || true

    stage "building veles IN the VM and running its own sandbox probe"
    # THE check: the VM builds veles from this tree and runs
    # `veles doctor --sandbox` against the jail backend. Nothing
    # hand-rolled, so a wrong JailBackend fails here.
    # shellcheck disable=SC2086
    tar -cf - --exclude=target --exclude=.git --exclude=.direnv -C "$REPO" . \
        | ssh $VM_SSH 'rm -rf ~/veles && mkdir -p ~/veles && tar -xf - -C ~/veles' \
        || die "could not copy the tree into the VM"
    # shellcheck disable=SC2086
    ssh $VM_SSH '
        set -e
        cd ~/veles
        sh scripts/install/install.sh --from-source freebsd
        JROOT=/usr/local/veles-jail
        if [ ! -x "$JROOT/usr/local/bin/tclsh8.6" ]; then
            sudo mkdir -p "$JROOT"
            sudo pkg -r "$JROOT" install -y tcl86 || {
                echo "  could not build a jail root; the jail fence stays unverified"; exit 1; }
        fi
        H=$(mktemp -d)
        printf "[sandbox]\nbackend = \"jail\"\nimage = \"%s\"\nnetwork = false\n" "$JROOT" > "$H/config.toml"
        # jail(8) is root-only, exactly like nspawn on Linux.
        sudo env VELES_HOME="$H" "$HOME/.local/bin/veles" doctor --sandbox
    ' || { warn "the FreeBSD arm did not reach a passing fence — see above"; rc=1; }
}

check_linux_vm() {
    if ! domain_up "$LINUX_DOMAIN" "$LINUX_HOST" "$LINUX_USER"; then
        warn "no libvirt domain $LINUX_DOMAIN — falling back to booting the image by hand"
        [ -f "$LINUX_IMG" ] || die "no $LINUX_IMG (the ci-linux image) and no $LINUX_DOMAIN domain"
        stage "booting $LINUX_IMG"
        boot "$LINUX_IMG"
        ssh_wait root || ssh_wait ci || die "SSH never came up"
        VM_SSH="$SSH_OPTS ${VM_USER:-root}@127.0.0.1"
    fi
    stage "backend binaries in the VM (pre-flight)"
    # shellcheck disable=SC2086
    ssh $VM_SSH '
        echo "nspawn: $(command -v systemd-nspawn || echo MISSING)"
        echo "podman: $(command -v podman || echo MISSING)"
        echo "tclsh:  $(command -v tclsh || echo MISSING)"
    ' || true
    warn "the ci-linux VM arm is a pre-flight only: nspawn and podman are"
    warn "verified for real on the HOST (scripts/sandbox_vm_check.sh local),"
    warn "which is where the artifacts and the built binary already are."
}

case "${1:-}" in
    local)       check_native; check_nspawn; check_firecracker; check_podman ;;
    native)      check_native ;;
    nspawn)      check_nspawn ;;
    firecracker) check_firecracker ;;
    podman)      check_podman ;;
    freebsd)     check_freebsd ;;
    linux)       check_linux_vm ;;
    *)
        cat <<USAGE
usage: $0 local|native|nspawn|firecracker|podman|freebsd|linux

  local        every backend this host can run: native, nspawn (sudo),
               firecracker (KVM), podman
  freebsd      the jail fence, in $VELES_VM_DIR/ci-freebsd.qcow2 — builds
               veles in the VM and runs its own doctor --sandbox
  linux        pre-flight in the ci-linux VM

Every arm runs \`veles doctor --sandbox\`, which EXECUTES a script
through the fence. Before 2026-09-04 this script hand-rolled equivalent
commands instead, and so could not fail on a wrong backend — it did not
notice three real defects that the first genuine run found immediately.
USAGE
        exit 1
        ;;
esac

exit $rc
