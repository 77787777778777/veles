#!/bin/sh
# FreeBSD dependency step.
#
# FreeBSD is the ONLY host for the `jail` sandbox backend, and it is
# where `shell.rs`'s bash → sh fallback earns its place: /bin/sh here is
# not bash, and veles shells out with `-c` from six call sites.
set -eu
. "$(dirname "$0")/../common.sh"

say "FreeBSD: installing build and runtime dependencies"
need_root
$SUDO pkg install -y \
    pkgconf git curl \
    openssl sqlite3 \
    tcl86 tk86 tcllib tclX \
    llvm rust \
    openssh-portable ripgrep webp libheif \
    || die "pkg could not install the required set"

for p in tclcurl tkimg; do
    $SUDO pkg install -y "$p" 2>/dev/null || note "optional package unavailable: $p"
done

# FreeBSD's lang/tcl86 installs /usr/local/bin/tclsh8.6 and NEVER an
# unversioned `tclsh`, which is the name veles looks for. common.sh's
# ensure_tclsh records VELES_TCLSH for the agent; this symlink makes the
# ordinary `tclsh` on PATH work too, for anything else that expects it.
if ! have tclsh && have tclsh8.6; then
    note "linking /usr/local/bin/tclsh -> tclsh8.6 (FreeBSD ships only the versioned name)"
    $SUDO ln -sf "$(command -v tclsh8.6)" /usr/local/bin/tclsh || \
        note "could not create the symlink; ensure_tclsh will record VELES_TCLSH instead"
fi

# The jail fence's root tree. Written out in full because every step of
# it was a separate wrong turn on 2026-09-04:
#
#   * `pkg -r $JROOT` resolves the trust keys and repo config RELATIVE to
#     the root, so an empty directory gives
#       pkg: Error opening the trusted directory /usr/share/keys/pkg/trusted
#       pkg: No packages available to install matching 'tcl86'
#     — which reads like a broken mirror. Copy the host's keys and
#     /etc/pkg in first.
#   * tcl86 alone is not enough: a jail needs a userland. FreeBSD-runtime
#     from the pkgbase repo supplies /libexec/ld-elf.so.1, /usr/bin/env
#     and libc, without which the jail execs nothing at all.
#   * FreeBSD's tcl86 installs `tclsh8.6` and never `tclsh`. The backend
#     now FINDS the interpreter inside the root (JailBackend::
#     resolve_tclsh), so no symlink is needed here — but if you build a
#     root by hand for something else, that is the name it will have.
#
# Verified end to end on FreeBSD 15.0-RELEASE: `veles doctor --sandbox`
# against this root prints `result: OK (42)`.
note "the jail sandbox backend needs a jail ROOT TREE — build one with:"
note "    JROOT=/usr/local/veles-jail"
note "    sudo mkdir -p \$JROOT/usr/share/keys \$JROOT/etc"
note "    sudo cp -a /usr/share/keys/pkg /usr/share/keys/pkgbase-* \$JROOT/usr/share/keys/"
note "    sudo cp -a /etc/pkg \$JROOT/etc/"
note "    sudo pkg -r \$JROOT install -y FreeBSD-runtime tcl86"
note "    # then in config.toml:  [sandbox] backend = \"jail\"  image = \"\$JROOT\""
note "    # and prove it:         sudo veles doctor --sandbox"
