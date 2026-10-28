#!/bin/sh
# veles installer for Linux: download a prebuilt release, verify it, install it.
#
#   curl -fsSL https://raw.githubusercontent.com/77787777778777/veles/master/scripts/install/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --version v0.1.0 --require-signature
#   sh scripts/install/install.sh [--version vX.Y.Z]          # from a checkout
#   sh scripts/install/install.sh --from-source [debian|ubuntu|arch|gentoo|freebsd]
#
# Verification: the release's SHA256SUMS is always checked against the
# downloaded archive. When `cosign` is on PATH, SHA256SUMS' keyless
# Sigstore signature is verified against this repository's release
# workflow first; without cosign the exact verify command is printed and
# the install continues on the checksum alone, unless --require-signature.
#
# Self-contained POSIX sh on purpose: piped through `sh` there is no
# checkout beside it. Only --from-source sources the checkout's common.sh.
set -eu

# The one place the repository slug lives (scripts/install/install.ps1
# has its twin). VELES_REPOSITORY overrides it.
DEFAULT_REPOSITORY=77787777778777/veles

say()  { printf '\n\033[1m> %s\033[0m\n' "$*"; }
note() { printf '  %s\n' "$*"; }
warn() { printf '  \033[33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31mx %s\033[0m\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

usage() {
    cat <<'EOF'
Usage: install.sh [--version vX.Y.Z] [--require-signature] [--prefix DIR]
       install.sh --from-source [debian|ubuntu|arch|gentoo|freebsd]

Installs the prebuilt veles release for this machine (Linux x86_64 or
aarch64) into ~/.local/share/veles/install and a `veles` launcher into
~/.local/bin. Re-running upgrades in place; ~/.config/veles is never
touched. No compiler, Tcl or root is needed (missing curl/tar/zstd can be
installed through the package manager with sudo).

  --version V           install release V (default: the latest release)
  --require-signature   refuse to install unless cosign verifies SHA256SUMS
  --prefix DIR          put the launcher in DIR/bin (default: ~/.local)
  --from-source         build this checkout instead (development install)

Environment: VELES_VERSION, VELES_REQUIRE_SIGNATURE=1, VELES_PREFIX,
VELES_REPOSITORY (default 77787777778777/veles), VELES_HOME,
VELES_SKIP_DEPS=1 (never touch the package manager),
VELES_RELEASE_BASE_URL (a mirror of https://github.com/OWNER/REPO/releases).
EOF
}

from_source=0
target=""
version=${VELES_VERSION:-}
require_sig=${VELES_REQUIRE_SIGNATURE:-0}
while [ "$#" -gt 0 ]; do
    case "$1" in
        --from-source) from_source=1 ;;
        --version) [ "$#" -ge 2 ] || die "--version needs a value"; version=$2; shift ;;
        --version=*) version=${1#--version=} ;;
        --require-signature) require_sig=1 ;;
        --prefix) [ "$#" -ge 2 ] || die "--prefix needs a value"; VELES_PREFIX=$2; shift ;;
        --prefix=*) VELES_PREFIX=${1#--prefix=} ;;
        --help|-h) usage; exit 0 ;;
        debian|ubuntu|arch|gentoo|freebsd)
            [ -z "$target" ] || die "only one target OS may be named"
            target=$1 ;;
        *) usage >&2; die "unknown argument: $1" ;;
    esac
    shift
done
case "$require_sig" in 0|1) ;; *) die "VELES_REQUIRE_SIGNATURE must be 0 or 1" ;; esac
VELES_PREFIX=${VELES_PREFIX:-$HOME/.local}
export VELES_PREFIX

# ── development install from a checkout ─────────────────────────────
if [ "$from_source" -eq 1 ]; then
    HERE=$(cd "$(dirname "$0")" && pwd)
    { [ -f "$HERE/common.sh" ] && [ -d "$HERE/deps" ]; } || \
        die "--from-source needs a checkout: git clone the repository and run scripts/install/install.sh --from-source"
    # shellcheck disable=SC1091
    . "$HERE/common.sh"
    detect_os() {
        if [ "$(uname -s)" = "FreeBSD" ]; then echo freebsd; return; fi
        [ -r /etc/os-release ] || die "cannot read /etc/os-release and this is not FreeBSD"
        # shellcheck disable=SC1091
        . /etc/os-release
        case "${ID:-}${ID_LIKE:+ $ID_LIKE}" in
            ubuntu*) echo ubuntu ;;
            debian*|*" debian"*) echo debian ;;
            arch*|*" arch"*) echo arch ;;
            gentoo*) echo gentoo ;;
            nixos*) die "NixOS: use the flake's services.veles module or the reviewed NixOS profile; do not mutate /etc/nixos with this script" ;;
            *) die "unsupported distribution for a source build: ${ID:-unknown}. Supported: debian, ubuntu, arch, gentoo, freebsd." ;;
        esac
    }
    OS=${target:-$(detect_os)}
    [ -f "$HERE/deps/$OS.sh" ] || die "no dependency script for '$OS'"
    say "veles source build - target: $OS (a development installation, not a release)"
    if [ "${VELES_SKIP_DEPS:-0}" = 1 ]; then
        note "VELES_SKIP_DEPS=1 - not touching the package manager"
    else
        sh "$HERE/deps/$OS.sh"
    fi
    # shellcheck disable=SC1091
    if [ -f "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi
    build_veles
    install_veles
    case "$OS" in
        freebsd) install_service_rcd ;;
        *) install_service_systemd ;;
    esac
    verify_install source
    summary
    exit 0
fi

# ── prebuilt release ─────────────────────────────────────────────────
[ -z "$target" ] || note "ignoring '$target': the prebuilt release is the same on every distribution"
case "$(uname -s)" in
    Linux) ;;
    FreeBSD) die "there is no prebuilt FreeBSD release; from a checkout run: scripts/install/install.sh --from-source freebsd" ;;
    *) die "this installer is for Linux; on Windows use scripts/install/install.ps1" ;;
esac
case "$(uname -m)" in
    x86_64|amd64) cpu=x86_64 ;;
    aarch64|arm64) cpu=aarch64 ;;
    *) die "no prebuilt release for $(uname -m); build from a checkout with --from-source" ;;
esac

repo=${VELES_REPOSITORY:-$DEFAULT_REPOSITORY}
case "$repo" in
    */*/*|/*|*/|*[!A-Za-z0-9_./-]*|*..*) die "invalid VELES_REPOSITORY '$repo'; expected OWNER/NAME" ;;
    */*) ;;
    *) die "invalid VELES_REPOSITORY '$repo'; expected OWNER/NAME" ;;
esac
base=${VELES_RELEASE_BASE_URL:-https://github.com/$repo/releases}
base=${base%/}
case "$base" in
    https://*) proto='=https' ;;
    http://127.0.0.1[:/]*|http://localhost[:/]*)
        proto='=http,https'
        warn "plain-HTTP loopback release source $base (a test fixture, never a real release)" ;;
    *) die "VELES_RELEASE_BASE_URL must be https:// (or loopback http:// for tests)" ;;
esac
[ -z "${VELES_RELEASE_BASE_URL:-}" ] || note "release source: $base"

# Tools. A minimal image may lack zstd (Debian) or curl; offer the one
# package-manager command that adds them, never anything else.
missing=""
for tool in curl tar zstd sha256sum awk sed grep mktemp readlink; do
    have "$tool" || missing="$missing $tool"
done
if [ -n "$missing" ]; then
    # shellcheck disable=SC2086
    pkgs=$(printf '%s\n' $missing | sed 's/^sha256sum$/coreutils/;s/^mktemp$/coreutils/;s/^readlink$/coreutils/;s/^awk$/gawk/' | sort -u | tr '\n' ' ')
    pm="" pre=""
    if have apt-get; then pm="apt-get install -y --no-install-recommends"; pre="apt-get update"
    elif have dnf; then pm="dnf install -y"
    elif have pacman; then pm="pacman -S --needed --noconfirm"; pre="pacman -Sy"
    elif have zypper; then pm="zypper --non-interactive install"
    elif have apk; then pm="apk add"
    fi
    [ -n "$pm" ] || die "missing:$missing - install them with your package manager and run this again"
    sudo=""
    [ "$(id -u)" = 0 ] || sudo=sudo
    if [ "${VELES_SKIP_DEPS:-0}" = 1 ] || { [ -n "$sudo" ] && ! have sudo; }; then
        die "missing:$missing - install them with: $sudo $pm $pkgs"
    fi
    say "installing missing tools:$missing"
    note "$sudo $pm $pkgs"
    if [ -n "$pre" ]; then
        # shellcheck disable=SC2086
        $sudo $pre >/dev/null || die "could not refresh the package index ($sudo $pre)"
    fi
    # shellcheck disable=SC2086
    $sudo $pm $pkgs || die "could not install:$missing"
fi

fetch() { # URL OUT
    curl --fail --silent --show-error --location --proto "$proto" --proto-redir "$proto" \
        --connect-timeout 30 --retry 2 --output "$2" "$1"
}

# The release to install.
if [ -n "$version" ]; then
    tag=v${version#v}
else
    latest=$(curl --fail --silent --show-error --location --proto "$proto" --proto-redir "$proto" \
        --connect-timeout 30 --output /dev/null --write-out '%{url_effective}' "$base/latest") || \
        die "could not find the latest release at $base/latest (pass --version vX.Y.Z)"
    case "$latest" in
        */tag/v*) tag=${latest##*/tag/} ;;
        *) die "$base/latest did not lead to a release tag (got $latest); pass --version vX.Y.Z" ;;
    esac
fi
printf '%s\n' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$' || \
    die "'$tag' is not a release version such as v0.1.0"
rel=$base/download/$tag
say "veles $tag for linux-$cpu from $repo"

# Private staging on the same filesystem as the installation (no noexec
# /tmp surprises, and the install is a same-disk copy).
data=${VELES_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/veles}
if [ -n "${VELES_PROFILE:-}" ] && [ "$VELES_PROFILE" != default ]; then
    case "$VELES_PROFILE" in
        *[!A-Za-z0-9_.-]*|.*|*..*) die "invalid VELES_PROFILE" ;;
    esac
    data=$data/profiles/$VELES_PROFILE
fi
stage_parent=$data/install-sources/.bootstrap-staging
for p in "$data" "$data/install-sources" "$stage_parent"; do
    [ ! -L "$p" ] || die "refusing a symlinked staging ancestor: $p"
done
mkdir -p "$stage_parent" || die "cannot create $stage_parent"
chmod 700 "$data/install-sources" "$stage_parent"
stage=$(mktemp -d "$stage_parent/run.XXXXXX") || die "cannot create private staging"
trap 'rm -rf "$stage"' EXIT
trap 'exit 130' HUP INT TERM

# The launcher must be ours (or absent) before anything is downloaded.
launcher=$VELES_PREFIX/bin/veles
marker='# managed by the veles installer'
if [ -L "$launcher" ]; then
    case "$(readlink "$launcher")" in
        */install/current/bin/veles) ;;
        *) die "refusing to replace $launcher -> $(readlink "$launcher") (not installed by this script)" ;;
    esac
elif [ -e "$launcher" ] && ! grep -qF "$marker" "$launcher" 2>/dev/null; then
    die "refusing to replace the existing $launcher (not installed by this script); remove it or use --prefix"
fi

# 1. SHA256SUMS, and its signature when it can be checked.
fetch "$rel/SHA256SUMS" "$stage/SHA256SUMS" || die "could not download $rel/SHA256SUMS - is $tag a published release of $repo?"
identity="https://github.com/$repo/.github/workflows/release.yml@refs/tags/$tag"
issuer=https://token.actions.githubusercontent.com
if have cosign; then
    fetch "$rel/SHA256SUMS.sigstore.json" "$stage/SHA256SUMS.sigstore.json" || \
        die "the release has no SHA256SUMS.sigstore.json, so cosign cannot verify it; refusing"
    cosign verify-blob --bundle "$stage/SHA256SUMS.sigstore.json" \
        --certificate-identity "$identity" --certificate-oidc-issuer "$issuer" \
        "$stage/SHA256SUMS" >"$stage/cosign.log" 2>&1 || {
            cat "$stage/cosign.log" >&2
            die "cosign rejected SHA256SUMS for $repo $tag; nothing was installed"
        }
    note "signature: SHA256SUMS is signed by $repo's release workflow at $tag (cosign)"
elif [ "$require_sig" = 1 ]; then
    die "--require-signature: cosign is not on PATH (https://docs.sigstore.dev/cosign/system_config/installation/); nothing was installed"
else
    note "signature: not checked (cosign is not installed). To check it yourself:"
    note "  curl -fsSLO $rel/SHA256SUMS -fsSLO $rel/SHA256SUMS.sigstore.json"
    note "  cosign verify-blob --bundle SHA256SUMS.sigstore.json --certificate-identity $identity --certificate-oidc-issuer $issuer SHA256SUMS"
fi

# 2. The archive SHA256SUMS names for this machine: exactly one.
pkgver=${tag#v}; pkgver=${pkgver%%-*}
pattern="^veles-$(printf '%s' "$pkgver" | sed 's/\./[.]/g')-[0-9a-z]+-linux-${cpu}[.]tar[.]zst\$"
lines=$(awk -v re="$pattern" '$1 ~ /^[0-9a-f]+$/ && length($1) == 64 { n = $2; sub(/^\*/, "", n); if (n ~ re) print $1, n }' "$stage/SHA256SUMS")
count=$(printf '%s' "$lines" | grep -c . || true)
[ "$count" -eq 1 ] || die "SHA256SUMS of $tag lists $count linux-$cpu archives for $pkgver (expected exactly one)"
sum=${lines%% *}
name=${lines#* }

# 3. Download and check.
note "downloading $name"
fetch "$rel/$name" "$stage/$name" || die "could not download $rel/$name"
actual=$(sha256sum "$stage/$name" | awk '{print $1}')
[ "$actual" = "$sum" ] || die "checksum mismatch for $name: SHA256SUMS says $sum, the download is $actual; refusing to install it"
note "checksum: $name matches SHA256SUMS"

mkdir "$stage/bundle"
tar --zstd -xf "$stage/$name" -C "$stage/bundle" --no-same-owner || die "could not unpack $name"
info=$(tr -d ' \n' < "$stage/bundle/BUNDLE-INFO.json" 2>/dev/null) || die "$name has no BUNDLE-INFO.json"
case "$info" in *"\"version\":\"$pkgver\""*) ;; *) die "$name does not contain veles $pkgver" ;; esac
case "$info" in *"\"architecture\":\"$cpu\""*) ;; *) die "$name is not a $cpu bundle" ;; esac
[ -x "$stage/bundle/bin/veles" ] || die "$name has no bin/veles"

# 4. Install: the bundle's own installer copies it into
#    $data/install/releases/<version>-<revision> and switches `current`
#    atomically, keeping the previous release. Config is not touched.
say "installing"
previous=$(readlink "$data/install/current" 2>/dev/null || true)
"$stage/bundle/bin/veles" install bundle "$stage/bundle" --yes >"$stage/install.log" 2>&1 || {
    cat "$stage/install.log" >&2
    die "veles install bundle failed; the previous installation (if any) is unchanged"
}
current=$data/install/current/bin/veles
[ -x "$current" ] || die "the install finished without $current"
mkdir -p "$VELES_PREFIX/bin"
cat >"$launcher.new.$$" <<EOF
#!/bin/sh
$marker (scripts/install/install.sh); re-run it to upgrade.
exec "$current" "\$@"
EOF
chmod 755 "$launcher.new.$$"
mv -f "$launcher.new.$$" "$launcher"
now=$(readlink "$data/install/current" 2>/dev/null || true)
if [ -n "$previous" ] && [ "$previous" != "$now" ]; then
    note "upgraded: ${previous##*/} -> ${now##*/} (the previous release is kept for rollback)"
else
    note "installed: ${now##*/}"
fi
note "launcher: $launcher"

# 5. Prove it runs.
say "checking the installation"
"$launcher" --version || die "the installed veles does not run"
limit=""
have timeout && limit="timeout 120"
# shellcheck disable=SC2086
if $limit "$launcher" doctor >"$stage/doctor.log" 2>&1; then
    note "veles doctor: OK"
else
    sed 's/^/    /' "$stage/doctor.log"
    note "veles doctor reported problems above; on a new machine that is usually just"
    note "the missing provider, which 'veles setup' configures."
fi

say "done"
case ":$PATH:" in
    *":$VELES_PREFIX/bin:"*) run=veles ;;
    *) run=$launcher
       note "$VELES_PREFIX/bin is not on your PATH; add it (e.g. in ~/.profile):"
       note "  export PATH=\"$VELES_PREFIX/bin:\$PATH\""
       note "" ;;
esac
note "next:  $run setup     # pick a provider and model (terminal)"
note "       $run gui       # or the desktop app, which walks you through it"
