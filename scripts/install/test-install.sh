#!/usr/bin/env bash
# shellcheck disable=SC2015 # `a && b || fail` is meant: fail if either check fails
# scripts/install/test-install.sh — the release installer against a fixture
# release served over loopback HTTP (scripts/install/fixture-release.sh).
#
#   nix shell nixpkgs#socat -c bash scripts/install/test-install.sh
#
# Real install.sh, real curl/tar/zstd/sha256sum, a GitHub-shaped server
# (latest redirect, CDN redirect). The bundle is a stub whose bin/veles
# records what it is asked to do and performs `install bundle` the way
# the real one lays it out (releases/<id>, `current`). cosign is a stub
# that checks the exact identity/issuer arguments: real keyless bundles
# need GitHub's OIDC, so the real signature path is proven in CI.
# The real artifact on real distributions: scripts/release/install-smoke.sh.
set -euo pipefail
cd "$(dirname "$0")/../.."
repo_root=$PWD
for tool in socat curl tar zstd sha256sum; do command -v "$tool" >/dev/null || { echo "$tool required" >&2; exit 2; }; done
t=$(mktemp -d "${TMPDIR:-/tmp}/veles-install-test.XXXXXX")
server=""
cleanup() { [[ -z $server ]] || kill "$server" 2>/dev/null || true; rm -rf "$t"; }
trap cleanup EXIT
cpu=$(uname -m); [[ $cpu == amd64 ]] && cpu=x86_64; [[ $cpu == arm64 ]] && cpu=aarch64
repo=77787777778777/veles

# Tool directory: exactly what the installer needs, so a host cosign never
# leaks into the "no cosign" cases.
mkdir -p "$t/tools" "$t/stub"
for tool in sh bash env curl tar zstd sha256sum awk sed grep mktemp readlink tr cat chmod mkdir rm mv cp ln \
            dirname basename uname id sort head tail wc timeout printf stat date touch cut; do
    p=$(command -v "$tool") && ln -s "$p" "$t/tools/$tool"
done

make_release() { # VERSION REVISION
    local v=$1 r=$2 tree=$t/tree-$1 dir=$t/srv/download/v$1
    mkdir -p "$tree/bin" "$dir"
    cat >"$tree/bin/veles" <<'SH'
#!/bin/sh
printf 'payload %s\n' "$*" >>"$VELES_TEST_LOG"
case "$1" in
  install)
    [ "$2" = bundle ] && [ "$4" = --yes ] || exit 9
    id=$(sed -n 's/.*"version":"\([^"]*\)".*"revision":"\([^"]*\)".*/\1-\2/p' "$3/BUNDLE-INFO.json")
    root=$VELES_HOME/install
    mkdir -p "$root/releases"
    [ -e "$root/releases/$id" ] || cp -a "$3" "$root/releases/$id"
    ln -sfn "$root/releases/$id" "$root/current.tmp" && mv -Tf "$root/current.tmp" "$root/current"
    echo '{"ok":true}' ;;
  --version) here=$(dirname "$0"); sed -n 's/.*"version":"\([^"]*\)".*/veles \1/p' "$here/../BUNDLE-INFO.json" ;;
  doctor) echo 'doctor fixture' ;;
  *) echo "unexpected $*" >&2; exit 9 ;;
esac
SH
    chmod 755 "$tree/bin/veles"
    printf '{"name":"veles","version":"%s","revision":"%s","architecture":"%s"}\n' "$v" "$r" "$cpu" >"$tree/BUNDLE-INFO.json"
    local name=veles-$v-$r-linux-$cpu.tar.zst
    tar --zstd -cf "$dir/$name" -C "$tree" .
    printf 'x' >"$dir/veles-plugin-echo-$v-linux-$cpu.tar.zst"
    (cd "$dir" && sha256sum "$name" "veles-plugin-echo-$v-linux-$cpu.tar.zst" >SHA256SUMS)
    printf '{"fixture":"bundle"}\n' >"$dir/SHA256SUMS.sigstore.json"
}
make_release 1.0.0 aaaaaaa
make_release 1.0.1 bbbbbbb
echo v1.0.1 >"$t/srv/LATEST"

cat >"$t/stub/cosign" <<'SH'
#!/bin/sh
printf 'cosign %s\n' "$*" >>"$VELES_TEST_LOG"
[ "$1" = verify-blob ] || exit 91
case " $* " in *" --certificate-identity https://github.com/77787777778777/veles/.github/workflows/release.yml@refs/tags/$VELES_TEST_TAG "*) ;; *) exit 92 ;; esac
case " $* " in *" --certificate-oidc-issuer https://token.actions.githubusercontent.com "*) ;; *) exit 93 ;; esac
[ "${VELES_TEST_COSIGN_REJECT:-0}" = 1 ] && { echo 'fixture: signature rejected' >&2; exit 94; }
exit 0
SH
chmod 755 "$t/stub/cosign"

port=$((20000 + RANDOM % 20000))
bash scripts/install/fixture-release.sh serve "$t/srv" "$port" 2>"$t/server.log" &
server=$!
for _ in $(seq 50); do curl -fsS -o /dev/null "http://127.0.0.1:$port/releases/tag/v0" 2>/dev/null && break; sleep 0.1; done

export VELES_TEST_LOG=$t/log
run() { # NAME [ENV=VALUE...] [-- ARGS]: run install.sh from OUTSIDE the checkout through `sh`
    local name=$1; shift
    local envs=() args=()
    while (($#)) && [[ $1 != -- ]]; do envs+=("$1"); shift; done
    (($#)) && shift && args=("$@")
    : >"$VELES_TEST_LOG"
    (cd "$t" && env -i HOME="$t/home" XDG_CONFIG_HOME="$t/home/.config" VELES_HOME="$t/data" \
        VELES_PREFIX="$t/prefix" PATH="$t/tools" VELES_TEST_LOG="$VELES_TEST_LOG" VELES_TEST_TAG=v1.0.1 \
        VELES_RELEASE_BASE_URL="http://127.0.0.1:$port/releases" "${envs[@]}" \
        sh -s -- "${args[@]}" <"$repo_root/scripts/install/install.sh" >"$t/$name.out" 2>&1)
}
pass() { printf '  ok  %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1" >&2; sed 's/^/    | /' "$t/$2.out" >&2; exit 1; }
current() { readlink "$t/data/install/current" 2>/dev/null || true; }

# 1. Happy path without cosign: the latest release, checksum only, the
#    exact verify command printed, the next step named.
mkdir -p "$t/home"
run happy || fail "happy path without cosign" happy
[[ $(current) == "$t/data/install/releases/1.0.1-bbbbbbb" ]] || fail "latest release installed" happy
[[ $("$t/prefix/bin/veles" --version) == 'veles 1.0.1' ]] || fail "launcher runs the install" happy
grep -q 'signature: not checked' "$t/happy.out" && grep -q "cosign verify-blob --bundle SHA256SUMS.sigstore.json --certificate-identity https://github.com/$repo/.github/workflows/release.yml@refs/tags/v1.0.1" "$t/happy.out" \
    || fail "prints the cosign command" happy
grep -q 'checksum: veles-1.0.1-bbbbbbb' "$t/happy.out" || fail "reports the checksum" happy
grep -q 'veles setup' "$t/happy.out" && grep -q 'veles gui' "$t/happy.out" || fail "names the next step" happy
[[ -z $(find "$t/data/install-sources/.bootstrap-staging" -mindepth 1 -print -quit) ]] || fail "staging cleaned" happy
pass "latest release, SHA256SUMS only, cosign command printed, next step named"

# 2. cosign present and accepting the exact identity.
run cosign-ok PATH="$t/stub:$t/tools" || fail "cosign present + valid" cosign-ok
grep -q 'signed by 77787777778777/veles.s release workflow at v1.0.1' "$t/cosign-ok.out" || fail "reports the verified signature" cosign-ok
grep -q '^cosign verify-blob --bundle .*SHA256SUMS.sigstore.json' "$VELES_TEST_LOG" || fail "cosign was asked" cosign-ok
pass "cosign present: SHA256SUMS signature verified against the release workflow identity"

# 3. cosign present and rejecting: refused, nothing executed or switched.
before=$(current)
if run cosign-bad PATH="$t/stub:$t/tools" VELES_TEST_COSIGN_REJECT=1; then fail "a rejected signature was accepted" cosign-bad; fi
grep -q 'cosign rejected SHA256SUMS' "$t/cosign-bad.out" || fail "explains the rejection" cosign-bad
! grep -q '^payload' "$VELES_TEST_LOG" || fail "payload ran after a rejected signature" cosign-bad
[[ $(current) == "$before" ]] || fail "current unchanged" cosign-bad
pass "cosign rejection refuses before anything runs"

# 4. --require-signature without cosign: refused before the archive.
if run nosig -- --require-signature; then fail "--require-signature without cosign was accepted" nosig; fi
grep -q 'require-signature: cosign is not on PATH' "$t/nosig.out" || fail "explains the missing cosign" nosig
! grep -q '^payload' "$VELES_TEST_LOG" || fail "payload ran" nosig
if run nosig-env VELES_REQUIRE_SIGNATURE=1; then fail "VELES_REQUIRE_SIGNATURE=1 without cosign was accepted" nosig-env; fi
pass "--require-signature / VELES_REQUIRE_SIGNATURE=1 without cosign is refused"

# 5. Checksum mismatch: the archive changed after SHA256SUMS. Refused,
#    nothing unpacked, executed or switched.
arch=$t/srv/download/v1.0.1/veles-1.0.1-bbbbbbb-linux-$cpu.tar.zst
\cp "$arch" "$t/good.tar.zst"
printf 'x' >>"$arch"
if run tamper; then fail "a tampered archive was accepted" tamper; fi
grep -q 'checksum mismatch for veles-1.0.1-bbbbbbb' "$t/tamper.out" || fail "explains the mismatch" tamper
! grep -q '^payload' "$VELES_TEST_LOG" || fail "tampered payload ran" tamper
[[ $(current) == "$before" ]] || fail "current unchanged after tamper" tamper
\mv -f "$t/good.tar.zst" "$arch"
pass "checksum mismatch is refused; the previous install is untouched"

# 6. Upgrade over an existing install, config untouched: start from 1.0.0
#    (an explicit --version), then the latest.
rm -rf "$t/data" "$t/prefix"
run old -- --version v1.0.0 || fail "explicit --version" old
[[ $(current) == "$t/data/install/releases/1.0.0-aaaaaaa" ]] || fail "v1.0.0 installed" old
mkdir -p "$t/home/.config/veles"
printf '[model]\nprovider = "fixture"\n' >"$t/home/.config/veles/config.toml"
printf 'SECRET=kept\n' >"$t/home/.config/veles/.env"
config_sum=$(cd "$t/home/.config/veles" && sha256sum config.toml .env)
run upgrade || fail "upgrade" upgrade
[[ $(current) == "$t/data/install/releases/1.0.1-bbbbbbb" ]] || fail "current switched to 1.0.1" upgrade
[[ -d $t/data/install/releases/1.0.0-aaaaaaa ]] || fail "previous release kept" upgrade
grep -q 'upgraded: 1.0.0-aaaaaaa -> 1.0.1-bbbbbbb' "$t/upgrade.out" || fail "reports the upgrade" upgrade
[[ $(cd "$t/home/.config/veles" && sha256sum config.toml .env) == "$config_sum" ]] || fail "config untouched" upgrade
[[ $("$t/prefix/bin/veles" --version) == 'veles 1.0.1' ]] || fail "launcher runs the upgrade" upgrade
run again || fail "re-running the same version" again
grep -q 'installed: 1.0.1-bbbbbbb' "$t/again.out" || fail "same-version rerun is quiet" again
pass "upgrade 1.0.0 -> 1.0.1 keeps the previous release and ~/.config/veles byte-for-byte"

# 7. A launcher this script did not write is never replaced.
rm -f "$t/prefix/bin/veles"; printf '#!/bin/sh\necho foreign\n' >"$t/prefix/bin/veles"; chmod 755 "$t/prefix/bin/veles"
if run foreign; then fail "a foreign launcher was replaced" foreign; fi
[[ $("$t/prefix/bin/veles") == foreign ]] || fail "foreign launcher intact" foreign
rm -f "$t/prefix/bin/veles"
pass "a foreign ~/.local/bin/veles is refused, not replaced"

# 8. Inputs that never reach the network.
if run badrepo VELES_REPOSITORY='../x'; then fail "a bad repository slug was accepted" badrepo; fi
if run badver -- --version 'v1;rm'; then fail "a bad version was accepted" badver; fi
if run badurl VELES_RELEASE_BASE_URL=http://example.com/releases; then fail "plain HTTP to a non-loopback host was accepted" badurl; fi
if run missing -- --version v9.9.9; then fail "a missing release was accepted" missing; fi
grep -q 'could not download .*/v9.9.9/SHA256SUMS' "$t/missing.out" || fail "names the missing release" missing
pass "bad slug / version / non-HTTPS source / missing release are refused"

# 9. From a checkout too (not only piped through sh).
rm -rf "$t/data" "$t/prefix"
: >"$VELES_TEST_LOG"
env -i HOME="$t/home" VELES_HOME="$t/data" VELES_PREFIX="$t/prefix" PATH="$t/tools" VELES_TEST_LOG="$VELES_TEST_LOG" \
    VELES_RELEASE_BASE_URL="http://127.0.0.1:$port/releases" VELES_VERSION=1.0.0 \
    sh scripts/install/install.sh >"$t/checkout.out" 2>&1 || fail "run from a checkout" checkout
[[ $(current) == "$t/data/install/releases/1.0.0-aaaaaaa" ]] || fail "VELES_VERSION honoured" checkout
pass "run from a checkout with VELES_VERSION"
echo 'install.sh fixtures: PASS'
