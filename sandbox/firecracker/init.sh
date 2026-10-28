#!/bin/sh
# veles firecracker guest init — PID 1 inside the microVM.
#
# CONTRACT (sandbox.rs FirecrackerBackend):
#   - /dev/vda is the rootfs, mounted READ-ONLY by the kernel. Every
#     writable surface here is tmpfs; nothing this script does can
#     outlive the VM or reach another concurrent run.
#   - /dev/vdb is the per-run work drive: a small ext4 image the host
#     built with `mkfs.ext4 -d`, holding driver.tcl and user.tcl. It is
#     writable because the sandbox driver points HOME and TMPDIR at it.
#   - The serial console is the ONLY channel back to the host (no
#     network by design), so it carries the script's stdout AND the
#     exit status — the latter as the sentinel below.
#
# WHY THE SENTINEL: firecracker's own exit code describes the VMM, not
# the guest. Reporting it as the sandbox's result (which is what shipped
# 2026-09-03) meant `SandboxOutput::success()` said true whenever the VM
# shut down cleanly, however the script had actually fared. This line is
# the truth the host parses — `sandbox::split_console`, and it must be
# the LAST thing written.
set -u

# Everything below is called by NAME. The kernel hands PID 1 an empty
# environment, so without this the very first `mount` fails and the VM
# reports a work-drive error it never actually had. (The nspawn backend
# lost a whole run to the same omission on 2026-09-04 —
# `execv(tclsh) failed` with /usr/bin/tclsh sitting right there.)
PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export PATH

# End the VM so the VMM exits and the host gets its output.
#
# `poweroff` does NOT work here and the guest says so:
#
#   reboot: Power off not available: System halted instead
#
# Powering off needs ACPI, which this guest does not have wired. The VM
# then sat halted forever and firecracker never exited — the host saw a
# TIMEOUT over a run that had already finished and printed its answer.
#
# `reboot -f` pairs with the `reboot=k` boot arg: the keyboard-controller
# reset, which firecracker treats as the VM ending.
#
# REBOOT FIRST. `poweroff -f` does not FAIL on a guest without ACPI — it
# halts the system and never returns, so anything after it is dead code.
# Ordering poweroff first (2026-09-04) left the VM halted forever, the
# VMM running, and the host reporting a TIMEOUT over a run that had
# already printed `VELES_SANDBOX_EXIT:0`.
halt_vm() {
    sync
    reboot -f 2>/dev/null
    poweroff -f 2>/dev/null
    halt -f 2>/dev/null
    # Nothing worked: at least stop burning a vCPU while the host's
    # timeout runs down.
    while :; do sleep 3600; done
}

mount -t proc  proc  /proc  2>/dev/null || true
mount -t sysfs sys   /sys   2>/dev/null || true
mount -t devtmpfs dev /dev  2>/dev/null || true
# Writable surfaces, all volatile.
mount -t tmpfs tmp /tmp     2>/dev/null || true
mount -t tmpfs run /run     2>/dev/null || true

mkdir -p /work
if ! mount -t ext4 /dev/vdb /work 2>/dev/null; then
    echo "veles-init: cannot mount the work drive (/dev/vdb)" >&2
    echo "VELES_SANDBOX_EXIT:2"
    sync; halt_vm
fi

if [ ! -f /work/driver.tcl ]; then
    echo "veles-init: /work/driver.tcl missing — the host staged nothing" >&2
    echo "VELES_SANDBOX_EXIT:2"
    sync; umount /work 2>/dev/null; halt_vm
fi

# The console carries the KERNEL's boot output as well as the script's —
# same serial line, and `quiet` only thins it. Bracket the script so the
# host can return exactly what the script printed and nothing else.
echo "VELES_SANDBOX_BEGIN"
# Same driver bytes as every other backend: the safe-interp wrapper.
# Its own contract is that the script path arrives by env, never argv.
VELES_SANDBOX_SCRIPT=/work/user.tcl HOME=/work TMPDIR=/work \
    tclsh /work/driver.tcl
rc=$?

sync
umount /work 2>/dev/null || true
# LAST line on the console, and the only one the host trusts for status.
echo "VELES_SANDBOX_EXIT:$rc"
halt_vm
