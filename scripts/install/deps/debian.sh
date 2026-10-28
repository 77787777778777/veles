#!/bin/sh
# Debian sid / Debian stable dependency step.
#
# Package names verified against Debian's archive naming; anything the
# host's apt cannot find is REPORTED, not skipped silently — a
# dependency that quietly did not install becomes a link error twenty
# minutes into a cargo build.
set -eu
. "$(dirname "$0")/../common.sh"
# Signed bootstrap is a different, deliberately small dependency set.
[ "${VELES_FROM_SOURCE:-0}" = 1 ] || exec "$(dirname "$0")/bootstrap-debian.sh"

say "Debian: installing build and runtime dependencies"
need_root
$SUDO apt-get update

# Split into MUST and NICE. A missing MUST package is fatal; a missing
# NICE one costs a capability that is named when it is lost.
# `tcl` and `tk` (the metapackages) as well as the -dev packages: they
# are what provide the UNVERSIONED /usr/bin/tclsh. veles' tclsh_path()
# looks for exactly `tclsh` on PATH, and tcl8.6 alone installs only
# tclsh8.6 — so without these the build succeeds and every sandboxed run
# fails to find an interpreter.
MUST="build-essential pkg-config git curl ca-certificates
      libssl-dev zlib1g-dev libsqlite3-dev
      tcl tk tcl8.6-dev tk8.6-dev tcllib tclx
      clang libclang-dev"
NICE="openssh-client ripgrep
      tcl-tclreadline libtk-img tcl-tls
      webp librsvg2-bin libheif-examples
      e2fsprogs podman systemd-container xvfb"

missing=""
for p in $MUST; do
    $SUDO apt-get install -y --no-install-recommends "$p" || missing="$missing $p"
done
[ -z "$missing" ] || die "required packages unavailable:$missing"

for p in $NICE; do
    $SUDO apt-get install -y --no-install-recommends "$p" 2>/dev/null \
        || note "optional package unavailable, continuing without it: $p"
done

# tclcurl is the Tcl HTTP client the desktop uses; it is not in every
# suite, and its absence is a named loss rather than a mystery.
$SUDO apt-get install -y --no-install-recommends tclcurl 2>/dev/null \
    || note "tclcurl unavailable — the Tcl desktop's HTTP surface will be degraded"

# Rust: Debian's rustc lags what this tree needs. Prefer the distro
# package when it is new enough, else rustup.
if have cargo && cargo --version | awk '{split($2,v,"."); exit !(v[1]>1 || (v[1]==1 && v[2]>=80))}'; then
    note "using the system cargo: $(cargo --version)"
else
    note "installing rust via rustup (Debian's rustc is too old or absent)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    . "$HOME/.cargo/env"
fi
