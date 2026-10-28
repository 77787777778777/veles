#!/usr/bin/env bash
# Build, never publish, the R10 artifact inventory.
#
# Release: build-artifacts.sh OUT 'git+file:///repo?rev=FULL_SHA'
# CI:      VELES_RELEASE_EXPECTED_SHA=FULL_SHA build-artifacts.sh OUT .
# Draft:   VELES_RELEASE_DRAFT=1 build-artifacts.sh OUT .
# CI uses the authenticated actions/checkout tree, not an unauthenticated
# git+https fetch of a private repository. The expected commit is mandatory.
# Draft output is deliberately unfit for manifest/signing/publication.
set -euo pipefail
cd "$(dirname "$0")/../.."
repo=$PWD
out=${1:?usage: build-artifacts.sh OUT FLAKE_REF}
flake=${2:?a rev-pinned flake ref, or . for an explicit draft, is required}
[[ $out = /* ]] || out=$repo/$out
case $out in "$repo"|"$repo/"*|/nix/store/*) echo "refusing source/store output: $out" >&2; exit 2 ;; esac
mkdir -p "$out/payload" "$out/unsigned"
out=$(realpath "$out")
work=$(mktemp -d "${TMPDIR:-/tmp}/veles-r10-build.XXXXXX")
trap 'rm -rf "$work"' EXIT

if [[ $flake == git+* ]]; then
    [[ $flake =~ [\?\&]rev=([0-9a-f]{40})(\&|$) ]] || {
        echo "release flake ref must contain rev=<40 lowercase hex>" >&2; exit 2
    }
    sha=${BASH_REMATCH[1]}
    git -C "$repo" cat-file -e "$sha^{commit}"
elif [[ $flake == . && ${VELES_RELEASE_DRAFT:-0} == 1 ]]; then
    sha=$(git -C "$repo" rev-parse HEAD)
    echo "DRAFT: mutable checkout; these are not release artifacts" >&2
elif [[ $flake == . && -n ${VELES_RELEASE_EXPECTED_SHA:-} ]]; then
    sha=$(git -C "$repo" rev-parse HEAD)
    [[ $sha == "$VELES_RELEASE_EXPECTED_SHA" && $sha =~ ^[0-9a-f]{40}$ ]] || {
        echo "checkout $sha is not the expected release commit $VELES_RELEASE_EXPECTED_SHA" >&2; exit 2
    }
    test -z "$(git -C "$repo" status --porcelain)" || {
        echo "release checkout is dirty" >&2; exit 2
    }
else
    echo "use a rev-pinned git flake ref, or VELES_RELEASE_DRAFT=1 with ." >&2; exit 2
fi
for tool in nix git jq zstd tar readelf; do
    command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }
done
system=$(nix eval --impure --raw --expr 'builtins.currentSystem')
case $system in
    x86_64-linux) cpu=x86_64 ;;
    aarch64-linux) cpu=aarch64 ;;
    *) echo "unsupported release host $system" >&2; exit 2 ;;
esac
version=$(nix eval --raw "$flake#packages.$system.default.version")
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "invalid package version $version" >&2; exit 1; }
draft=${VELES_RELEASE_DRAFT:-0}
[[ $draft == 0 || $draft == 1 ]] || { echo "VELES_RELEASE_DRAFT must be 0 or 1" >&2; exit 2; }
[[ ! ($draft == 1 && -n ${VELES_RELEASE_EXPECTED_SHA:-}) ]] || {
    echo "draft mode and an expected release commit are mutually exclusive" >&2; exit 2
}
# Repeated invocations add lanes to one output. Never silently combine
# different source revisions, versions, architectures, or draft states.
if [[ -f $out/SOURCE_COMMIT ]]; then
    for pair in "SOURCE_COMMIT:$sha" "SOURCE_VERSION:$version" "SOURCE_SYSTEM:$system" "DRAFT:$draft"; do
        key=${pair%%:*}; expected=${pair#*:}
        [[ -f $out/$key && $(cat "$out/$key") == "$expected" ]] || {
            echo "$out/$key does not match this $key=$expected lane" >&2; exit 1
        }
    done
elif find "$out/payload" "$out/unsigned" -type f -print -quit | grep -q .; then
    echo "refusing to add provenance to an existing unlabelled inventory" >&2; exit 1
fi
lanes=" ${VELES_RELEASE_LANES:-linux plugins android oci-nix images} "
has() { [[ $lanes == *" $1 "* ]]; }

if has linux; then
    for attr in bundleArchive bundleAppImage; do
        path=$(nix build "$flake#$attr" --no-link --print-out-paths)
        find "$path" -maxdepth 1 -type f \( -name '*.tar.zst' -o -name '*.AppImage' -o -name '*.json' \) \
            -exec install -m 0644 {} "$out/payload/" \;
    done
fi
if has plugins; then
    for name in echo atproto; do
        path=$(nix build "$flake#veles-plugin-$name" --no-link --print-out-paths)
        bin="$path/lib/veles/plugins/$name/veles-plugin-$name"
        test -x "$bin"
        ! readelf -d "$bin" | grep -q NEEDED || { echo "$name is not static" >&2; exit 1; }
        "$bin" --describe | jq -e '.protocol == 1' >/dev/null
        file="veles-plugin-$name-$version-linux-$cpu.tar.zst"
        tar --sort=name --format=gnu --mtime='@1' --owner=0 --group=0 --numeric-owner \
            -cf - -C "$path/lib/veles/plugins/$name" . | zstd -q -10 -T1 -o "$out/payload/$file"
    done
fi
if has android; then
    for attr in veles-apk veles-userland-apk; do
        path=$(nix build "$flake#$attr" --no-link --print-out-paths)
        for file in "$path"/*-release-unsigned.apk; do
            test -f "$file"
            base=$(basename "$file" -release-unsigned.apk)
            install -m 0644 "$file" "$out/unsigned/$base-$version-android-arm64-v8a-release-unsigned.apk"
        done
    done
    path=$(nix build "$flake#veles-aab" --no-link --print-out-paths)
    mapfile -t aabs < <(find "$path" -maxdepth 1 -type f -name 'veles-release-unsigned.aab' -print)
    ((${#aabs[@]} == 1)) || { echo "expected one unsigned gateway AAB" >&2; exit 1; }
    install -m 0644 "${aabs[0]}" "$out/unsigned/veles-$version-android-arm64-v8a-release-unsigned.aab"
fi
if has oci-nix; then
    if [[ $cpu != x86_64 ]]; then
        echo "the four R10 OCI images are amd64-only" >&2; exit 2
    fi
    # A source URL is required for a publishable lane. The local development
    # default in flake.nix must never leak into a release-signed image.
    [[ $draft == 1 || ${VELES_RELEASE_REPOSITORY:-} =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || {
        echo "VELES_RELEASE_REPOSITORY=OWNER/REPO is required for image provenance" >&2; exit 2
    }
    source_label='veles-agent (local build, not pushed)'
    [[ -z ${VELES_RELEASE_REPOSITORY:-} ]] || source_label="https://github.com/$VELES_RELEASE_REPOSITORY"
    for pair in gateway:ociImage signer:ociSignerImage; do
        kind=${pair%%:*}; attr=${pair#*:}
        path=$(nix build "$flake#$attr" --no-link --print-out-paths)
        install -m 0644 "$path" "$work/$kind.docker-archive.tar.gz"
        mkdir -p "$work/$kind-image"
        tar -xzf "$work/$kind.docker-archive.tar.gz" -C "$work/$kind-image"
        # Nix outputs are read-only. Editing an extracted config or cleaning
        # this temporary tree requires restoring owner write permission.
        chmod -R u+w "$work/$kind-image"
        mapfile -t configs < <(find "$work/$kind-image" -maxdepth 1 -type f -name '*.json' ! -name manifest.json)
        ((${#configs[@]} == 1)) || { echo "expected one $kind OCI config" >&2; exit 1; }
        jq -c --arg sha "$sha" --arg source "$source_label" --arg title "veles-$kind" \
            '.config.Labels["org.opencontainers.image.revision"] = $sha |
             .config.Labels["org.opencontainers.image.source"] = $source |
             .config.Labels["org.opencontainers.image.title"] = $title' \
            "${configs[0]}" > "$work/$kind.config"
        # The config filename is its content digest. Updating labels in
        # place would leave a corrupt archive whose manifest still names
        # the old config. Rename the object and update that reference.
        config_digest=$(sha256sum "$work/$kind.config" | cut -d' ' -f1)
        config_dir=$(dirname "${configs[0]}")
        install -m 0644 "$work/$kind.config" "$config_dir/$config_digest.json"
        [[ ${configs[0]} == "$config_dir/$config_digest.json" ]] || rm "${configs[0]}"
        jq -c --arg config "$config_digest.json" '.[0].Config = $config' \
            "$work/$kind-image/manifest.json" > "$work/$kind.manifest"
        install -m 0644 "$work/$kind.manifest" "$work/$kind-image/manifest.json"
        tar --sort=name --format=gnu --mtime='@1' --owner=0 --group=0 --numeric-owner \
            -czf "$out/payload/veles-$kind-$version-linux-amd64.tar.gz" -C "$work/$kind-image" .
        load_output=$(podman load --input "$out/payload/veles-$kind-$version-linux-amd64.tar.gz")
        loaded=$(printf '%s\n' "$load_output" | sed -n 's/^Loaded image: //p')
        [[ -n $loaded && $(printf '%s\n' "$loaded" | wc -l) -eq 1 ]] || { echo "cannot identify built $kind image" >&2; exit 1; }
        revision_label=$(podman image inspect "$loaded" --format '{{index .Config.Labels "org.opencontainers.image.revision"}}')
        source_check=$(podman image inspect "$loaded" --format '{{index .Config.Labels "org.opencontainers.image.source"}}')
        [[ $revision_label == "$sha" && $source_check == "$source_label" ]] || { echo "$kind image labels failed provenance verification" >&2; exit 1; }
        [[ $(podman image inspect "$loaded" --format '{{.Id}}') == "$config_digest" ]] || { echo "$kind config digest does not match its manifest" >&2; exit 1; }
        podman image inspect "$loaded" --format '{{index .Config.Labels "org.opencontainers.image.title"}}' | grep -qx "veles-$kind"
    done
fi
if has images; then
    # The sandbox intentionally carries the amd64 Chrome/toolchain image;
    # there is no silent arm64 substitution. The other three image lanes
    # are also built only on the x86_64 runner in this release matrix.
    if [[ $cpu == x86_64 ]]; then
        command -v podman >/dev/null || { echo "podman is required for the OCI lane" >&2; exit 1; }
        # Fully qualified digest-pinned bases still require registries.conf
        # v2. Do not read or modify a host's legacy v1-only configuration.
        cat > "$work/registries.conf" <<'REGISTRIES'
unqualified-search-registries = ["docker.io"]
[[registry]]
prefix = "docker.io"
location = "docker.io"
REGISTRIES
        export CONTAINERS_REGISTRIES_CONF="$work/registries.conf"
        unset CONTAINER_HOST CONTAINER_CONNECTION
        mkdir -p "$work/source"
        mkdir -p "$work/source/deploy/vps"
        if [[ ${VELES_RELEASE_DRAFT:-0} == 1 ]]; then
            cp -a sandbox "$work/source/"
            cp -a deploy/vps/classifier "$work/source/deploy/vps/"
        else
            git -C "$repo" archive "$sha" sandbox deploy/vps/classifier | tar -x -C "$work/source"
        fi
        stamp=$(git -C "$repo" show -s --format=%ct "$sha")
        for kind in sandbox classifier; do
            tag="localhost/veles-$kind:$sha"
            case $kind in
                sandbox) context=sandbox; file=sandbox/Dockerfile ;;
                classifier) context=deploy/vps/classifier; file=deploy/vps/classifier/Containerfile ;;
            esac
            labels=(--label "org.opencontainers.image.revision=$sha" --label "org.opencontainers.image.title=veles-$kind")
            if [[ -n ${VELES_RELEASE_REPOSITORY:-} ]]; then
                labels+=(--label "org.opencontainers.image.source=https://github.com/$VELES_RELEASE_REPOSITORY")
            fi
            # Base images are digest-pinned and output mtimes are fixed.
            # Mutable apt/pip repository contents are NOT claimed hermetic.
            podman build --platform linux/amd64 --pull=always --no-cache \
                --timestamp "$stamp" "${labels[@]}" \
                --tag "$tag" -f "$work/source/$file" "$work/source/$context"
            rm -rf "$work/$kind.oci"
            podman save --format oci-dir --output "$work/$kind.oci" "$tag"
            tar --sort=name --format=gnu --mtime="@$stamp" --owner=0 --group=0 --numeric-owner \
                -cf "$out/payload/veles-$kind-$version-linux-amd64.oci.tar" -C "$work/$kind.oci" .
            # Only the revision tag is created. The smoke harness loads this
            # archive into isolated storage; no mutable production/latest tag
            # or deployment is changed by artifact construction.
        done
    else
        echo "OCI image lane is amd64-only; skipping on $cpu" >&2
    fi
fi

printf '%s\n' "$sha" > "$out/SOURCE_COMMIT"
printf '%s\n' "$version" > "$out/SOURCE_VERSION"
printf '%s\n' "$flake" > "$out/SOURCE_FLAKE_REF"
printf '%s\n' "$system" > "$out/SOURCE_SYSTEM"
printf '%s\n' "${VELES_RELEASE_DRAFT:-0}" > "$out/DRAFT"
# Lanes upload this checksum inventory beside their payload. The assembler
# verifies all three before the first registry write, not after signing.
bash "$(dirname "$0")/payload-inventory.sh" seal "$out"
echo "built requested R10 lanes into $out/payload"
