#!/usr/bin/env bash
# R10: execute the actual portable artifact on minimal distro images.
# ON DEMAND ONLY: never in precommit or `nix flake check`.
#
# Required inputs are explicit so a stale latest tag cannot satisfy a run:
#   VELES_SMOKE_SANDBOX_ARCHIVE=/path/veles-sandbox-....oci.tar
#   VELES_SMOKE_SOURCE_SHA=FULL_COMMIT
#   VELES_SMOKE_EXPECTED_REVISION=SHORT_COMMIT[-dirty]
#   VELES_SMOKE_APPIMAGE=/path/veles.AppImage (required for the full proof)
#   VELES_SMOKE_RESULTS=/path/for/retained/logs
#   scripts/release/distro-smoke.sh /path/bundle.tar.zst [debian fedora arch]
#
# The final guest has no source checkout, compiler, distro Tcl, host /nix,
# provider credentials, or network. A private Podman socket and isolated
# storage supply the real sandbox doctor. Both doctor halves must pass.
# The AppImage is checked through an actual FUSE mount and a real GUI launch.
set -euo pipefail
cd "$(dirname "$0")/../.."
repo=$PWD
((EUID != 0)) || { echo "run as an unprivileged user, not root" >&2; exit 2; }
archive=${1:?an explicit archive.tar.zst is required}
shift
targets=("$@")
((${#targets[@]})) || targets=(debian fedora arch)
for t in "${targets[@]}"; do case $t in debian|fedora|arch) ;; *) echo "unknown distro $t" >&2; exit 2 ;; esac; done
for var in VELES_SMOKE_SANDBOX_ARCHIVE VELES_SMOKE_SOURCE_SHA VELES_SMOKE_EXPECTED_REVISION VELES_SMOKE_APPIMAGE; do
    [[ -n ${!var:-} ]] || { echo "$var is required" >&2; exit 2; }
done
sandbox_archive=$VELES_SMOKE_SANDBOX_ARCHIVE
appimage=$VELES_SMOKE_APPIMAGE
[[ $VELES_SMOKE_SOURCE_SHA =~ ^[0-9a-f]{40}$ ]] || { echo "bad sandbox source SHA" >&2; exit 2; }
[[ $VELES_SMOKE_EXPECTED_REVISION =~ ^[0-9a-f]{7,40}(-dirty)?$ ]] || { echo "bad bundle revision" >&2; exit 2; }
for pair in "$archive:tar.zst" "$sandbox_archive:oci.tar" "$appimage:AppImage"; do
    file=${pair%:*}; suffix=${pair##*:}
    [[ $file = /* ]] || file=$repo/$file
    [[ -f $file && $file == *."$suffix" ]] || { echo "expected an existing .$suffix: $file" >&2; exit 2; }
    case ${pair%%:*} in
        "$archive") archive=$file ;;
        "$sandbox_archive") sandbox_archive=$file ;;
        *) appimage=$file ;;
    esac
done
runner=${VELES_SMOKE_RUNNER:-podman}
for tool in "$runner" jq sha256sum tar timeout zstd; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }; done
metadata=$(tar --zstd -xOf "$archive" ./BUNDLE-INFO.json)
jq -e --arg revision "$VELES_SMOKE_EXPECTED_REVISION" \
    '.name == "veles" and .revision == $revision and (.architecture == "x86_64" or .architecture == "aarch64")' \
    <<<"$metadata" >/dev/null || { echo "archive is not the requested source revision" >&2; exit 1; }
unset CONTAINER_HOST CONTAINER_CONNECTION DOCKER_HOST
fixture_root=$(mktemp -d /dev/shm/veles-r10-fixture.XXXXXX)
chmod 755 "$fixture_root"
work=$fixture_root/distro
socket_root=$fixture_root/socket
mkdir -p "$work" "$socket_root"
mkdir -p "$work/artifacts" "$work/results" "$work/smoke" "$work/graph" "$work/run"
chmod 755 "$work" "$work/artifacts" "$work/results" "$work/smoke"
install -m 0755 scripts/release/guest-smoke.sh "$work/smoke/guest-smoke.sh"
install -m 0644 "$archive" "$work/artifacts/bundle.tar.zst"
install -m 0755 "$appimage" "$work/artifacts/veles.AppImage"
sha256sum "$archive" "$appimage" "$sandbox_archive" > "$work/results/input-artifacts.sha256"
# Never depend on or edit the host's v1-only registries.conf. These guests
# use fully qualified, digest-pinned bases; no mutable tag is resolved here.
cat > "$work/registries.conf" <<'REGISTRIES'
unqualified-search-registries = ["docker.io"]
[[registry]]
prefix = "docker.io"
location = "docker.io"
REGISTRIES
cat > "$work/storage.conf" <<STORAGE
[storage]
driver = "overlay"
graphroot = "$work/graph"
runroot = "$work/run"
STORAGE
fuse=$(command -v fuse-overlayfs || true)
if [[ -n $fuse ]]; then
    printf '[storage.options]\nmount_program = "%s"\n' "$fuse" >> "$work/storage.conf"
fi
export CONTAINERS_REGISTRIES_CONF="$work/registries.conf"
declare -A images=(
    [debian]=docker.io/library/debian:sid@sha256:92d7265e51d67d3a2094c1aad9faa88c74cf9d52e6c354200e429cd60237cc55
    [fedora]=docker.io/library/fedora:latest@sha256:43b29f65a41eb9c35e1cd5323e3bdf3b655c2357a9f4f1ff2f9c2798e5045d80
    [arch]=docker.io/library/archlinux:latest@sha256:52dfbd5afb23a220d5ee92dafcb4c788d96c026ed04179890c23adef4c897fa0
)
declare -A setups=(
    [debian]='apt-get update && apt-get install -y --no-install-recommends ca-certificates bash coreutils grep diffutils findutils tar zstd curl xvfb xauth fuse3 fontconfig fonts-dejavu-core && useradd -m -u __SMOKE_UID__ smoke'
    [fedora]='dnf install -y --setopt=install_weak_deps=False ca-certificates bash coreutils grep diffutils findutils tar zstd curl xorg-x11-server-Xvfb xauth fuse3 fontconfig dejavu-sans-fonts shadow-utils && useradd -m -u __SMOKE_UID__ smoke'
    [arch]='pacman -Sy --noconfirm --needed ca-certificates bash coreutils grep diffutils findutils tar zstd curl xorg-server-xvfb xorg-xauth fuse3 fontconfig ttf-dejavu shadow && useradd -m -u __SMOKE_UID__ smoke'
)
if [[ -n ${VELES_SMOKE_RESULTS:-} ]]; then
    mkdir -p "$VELES_SMOKE_RESULTS"
fi
preserve_results() {
    if [[ -n ${VELES_SMOKE_RESULTS:-} ]]; then
        cp -a "$work/results/." "$VELES_SMOKE_RESULTS/" 2>/dev/null || true
    fi
}
pids=()
volumes=()
sock=$socket_root/podman.sock
printf 'socket=%s\nstorage=%s\n' "$sock" "$work/graph" > "$work/results/fixture-paths"
cleanup() {
    local p t
    preserve_results
    for p in "${pids[@]}"; do kill -TERM "$p" 2>/dev/null || true; done
    for t in "${targets[@]}"; do "$runner" rm -f "veles-r10-guest-$t" >/dev/null 2>&1 || true; done
    ((${#volumes[@]})) && "$runner" volume rm -f "${volumes[@]}" >/dev/null 2>&1 || true
}
trap cleanup EXIT
podman_host() {
    CONTAINERS_STORAGE_CONF="$work/storage.conf" \
        "$runner" --url "unix://$sock" "${@:1}"
}
# --root/--runroot as well as the storage.conf: on the GitHub ubuntu-24.04
# runner (podman 4.9) the service ignored CONTAINERS_STORAGE_CONF and served
# the runner's own store ("readiness query reached the host Podman").
CONTAINERS_STORAGE_CONF="$work/storage.conf" \
    "$runner" --root "$work/graph" --runroot "$work/run" system service --time=0 "unix://$sock" >"$work/results/podman-service.log" 2>&1 &
pids+=("$!")
service_pid=$!
graph=""
for _ in $(seq 1 600); do
    if [[ -S $sock ]]; then
        response=$work/results/readiness-response
        timeout --signal=TERM --kill-after=2s 10 env \
            CONTAINERS_STORAGE_CONF="$work/storage.conf" \
            "$runner" --url "unix://$sock" info --format '{{.Store.GraphRoot}}' \
            >"$response" 2>"$work/results/readiness-error" || true
        graph=$(cat "$response")
        [[ $graph == "$work/graph" ]] && break
        [[ $graph == /home/*/.local/share/containers/storage ]] && {
            echo "readiness query reached the host Podman, not $sock" >&2; exit 1
        }
    fi
    if ! kill -0 "$service_pid" 2>/dev/null; then
        service_rc=0
        wait "$service_pid" || service_rc=$?
        echo "private Podman exited during startup with status $service_rc" >&2
        break
    fi
    sleep 0.1
done
[[ $graph == "$work/graph" ]] || {
    ps -fp "$service_pid" >&2 || true
    cat "$work/results/podman-service.log" >&2
    echo "private Podman did not use $work/graph: $graph" >&2
    cat "$work/results/readiness-response" "$work/results/readiness-error" >&2 2>/dev/null || true
    exit 1
}
load_output=$(podman_host load --input "$sandbox_archive")
loaded=$(printf '%s\n' "$load_output" | sed -n 's/^Loaded image: //p')
[[ -n $loaded && $(printf '%s\n' "$loaded" | wc -l) -eq 1 ]] || { printf '%s\n' "$load_output" >&2; echo "could not identify sandbox image" >&2; exit 1; }
image_revision=$(podman_host image inspect "$loaded" --format '{{index .Config.Labels "org.opencontainers.image.revision"}}')
[[ $image_revision == "$VELES_SMOKE_SOURCE_SHA" ]] || { echo "sandbox revision $image_revision is not $VELES_SMOKE_SOURCE_SHA" >&2; exit 1; }
podman_host image inspect "$loaded" --format '{{.Os}}/{{.Architecture}}' | grep -qx 'linux/amd64'
podman_host tag "$loaded" localhost/veles-sandbox:r10-smoke
printf '%s\n' "$image_revision" > "$work/results/sandbox-source.sha"
uid=$(id -u)
for t in "${targets[@]}"; do
    echo "▸ $t: pull the digest-pinned minimal base into the fixture store"
    "$runner" pull "${images[$t]}" >"$work/results/$t.pull.log" 2>&1
    image=localhost/veles-r10-runtime-$t:latest
    setup=${setups[$t]/__SMOKE_UID__/$uid}
    echo "▸ $t: install only the runtime/GUI fixture packages"
    "$runner" build --pull=never --no-cache --tag "$image" -f - >"$work/results/$t.build.log" 2>&1 <<EOF
FROM ${images[$t]}
RUN $setup
USER smoke
WORKDIR /home/smoke
EOF
    volume=veles-r10-$t-$(basename "$work")
    volumes+=("$volume")
    "$runner" volume create "$volume" >/dev/null
    echo "▸ $t: real doctor, archive GUI, AppImage FUSE/GUI, and language hosts"
    # Only the OUTER throwaway distro guest gets FUSE's mount
    # capability and the matching seccomp profile. The inner sandbox
    # and user-env containers still use veles' unchanged drop policy.
    timeout --signal=TERM --kill-after=20s 1200 "$runner" run --rm \
        --name "veles-r10-guest-$t" \
        --userns=keep-id --user smoke --workdir /home/smoke \
        --network=none --stop-timeout=15 --device /dev/fuse \
        --cap-add SYS_ADMIN --security-opt seccomp=unconfined \
        --mount "type=bind,src=$work/artifacts,dst=/artifacts,ro" \
        --mount "type=bind,src=$socket_root,dst=/sockets" \
        --mount "type=bind,src=$work/smoke,dst=/smoke,ro" \
        --mount "type=bind,src=$work/results,dst=/results" \
        --mount "type=volume,src=$volume,dst=/home/smoke" \
        --env HOME=/home/smoke --env VELES_HOME=/home/smoke/archive-home \
        --env CONTAINER_HOST="unix:///sockets/podman.sock" --env TMPDIR=/home/smoke/tmp \
        --env VELES_SMOKE_RESULTS_DIR=/results --env VELES_SMOKE_DISTRO="$t" \
        --env LANG=C.UTF-8 --env LC_ALL=C.UTF-8 \
        --env PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
        --entrypoint /bin/bash "$image" -euo pipefail /smoke/guest-smoke.sh \
        >"$work/results/$t.console.log" 2>&1 || {
            echo "FAILED $t; retained output:" >&2
            cat "$work/results/$t.console.log" >&2
            preserve_results
            exit 1
        }
    grep -F PORTABLE_SMOKE_PASS "$work/results/$t.console.log"
    echo "PASS $t portable archive and AppImage"
    preserve_results
done
printf 'ALL_REQUESTED_DISTROS_PASS\n'
echo "logs: $work/results"
