#!/usr/bin/env bash
# Create the one complete R10 inventory consumed by signing and release CI.
# Drafts, partial architecture sets, unsigned inputs, and stale signatures
# are refused. Registry evidence is structural; publish-images.sh performs
# the live digest signing and verification before this script is called.
set -euo pipefail
out=${1:?usage: manifest.sh OUT [x86_64-linux aarch64-linux]}
[[ $out = /* ]] || out=$PWD/$out
shift
if (($#)); then systems=("$@"); else mapfile -t systems < "$out/SOURCE_SYSTEMS"; fi
[[ ${systems[*]} == "x86_64-linux aarch64-linux" ]] || {
    echo "a signed R10 inventory requires BOTH x86_64-linux and aarch64-linux" >&2; exit 1
}
for tool in jq sha256sum stat tar zstd; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }; done
for f in SOURCE_COMMIT SOURCE_VERSION DRAFT; do [[ -s $out/$f ]] || { echo "missing $out/$f" >&2; exit 1; }; done
[[ $(cat "$out/DRAFT") == 0 ]] || { echo "draft output cannot become a signed release" >&2; exit 2; }
if find "$out/unsigned" -type f -print -quit 2>/dev/null | grep -q .; then
    echo "unsigned Android intermediates remain; sign both APKs and the AAB first" >&2; exit 1
fi
if find "$out/payload" -maxdepth 1 -type f \( -name '*unsigned*' -o -name 'SHA256SUMS*' -o -name '*.sigstore.json' -o -name 'release-manifest.json' \) -print -quit | grep -q .; then
    echo "unsigned or already processed file in the publish payload" >&2; exit 1
fi
repo=${VELES_RELEASE_REPOSITORY:?VELES_RELEASE_REPOSITORY is required}
tag=${VELES_RELEASE_TAG:?VELES_RELEASE_TAG is required}
sha=$(cat "$out/SOURCE_COMMIT")
version=$(cat "$out/SOURCE_VERSION")
[[ $repo =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || { echo "bad repository" >&2; exit 2; }
[[ $sha =~ ^[0-9a-f]{40}$ ]] || { echo "bad source commit" >&2; exit 2; }
[[ $tag == "v$version" || $tag == "v$version"-* ]] || { echo "tag $tag does not match package $version" >&2; exit 1; }
one() {
    local description=$1; shift
    mapfile -t matches < <(find "$out/payload" -maxdepth 1 -type f "$@" -printf '%f\n')
    ((${#matches[@]} == 1)) || { echo "expected exactly one $description, got ${#matches[@]}" >&2; exit 1; }
}
revision() {
    local cpu=$1 expected=${sha:0:7} metadata
    mapfile -t matches < <(find "$out/payload" -maxdepth 1 -type f -name "veles-$version-*-linux-$cpu.json" -printf '%f\n')
    ((${#matches[@]} == 1)) || { echo "missing bundle metadata for $cpu" >&2; exit 1; }
    [[ ${matches[0]} == "veles-$version-$expected-linux-$cpu.json" ]] || {
        echo "bundle filename is not from $expected: ${matches[0]}" >&2; exit 1
    }
    metadata=$(tar --zstd -xOf "$out/payload/veles-$version-$expected-linux-$cpu.tar.zst" ./BUNDLE-INFO.json)
    [[ $metadata == "$(<"$out/payload/${matches[0]}")" ]] || {
        echo "archive and sidecar metadata differ for $cpu" >&2; exit 1
    }
    jq -e --arg revision "$expected" --arg cpu "$cpu" --arg version "$version" \
        '.name == "veles" and .version == $version and .revision == $revision and .architecture == $cpu' \
        <<<"$metadata" >/dev/null || { echo "bundle provenance mismatch for $cpu" >&2; exit 1; }
        for file in "$out/payload/veles-$version-$expected-linux-$cpu.tar.zst" "$out/payload/veles-$version-$expected-linux-$cpu.AppImage"; do
        [[ -s $file ]] || { echo "missing $file" >&2; exit 1; }
    done
}
for system in "${systems[@]}"; do
    case $system in x86_64-linux) cpu=x86_64 ;; aarch64-linux) cpu=aarch64 ;; *) exit 2 ;; esac
    one "$cpu archive" -name "veles-$version-*-linux-$cpu.tar.zst"
    one "$cpu AppImage" -name "veles-$version-*-linux-$cpu.AppImage"
    one "$cpu bundle metadata" -name "veles-$version-*-linux-$cpu.json"
    one "$cpu echo plugin" -name "veles-plugin-echo-$version-linux-$cpu.tar.zst"
    one "$cpu atproto plugin" -name "veles-plugin-atproto-$version-linux-$cpu.tar.zst"
    revision "$cpu"
done
one "Android gateway APK" -name "veles-$version-android-arm64-v8a-release.apk"
one "Android userland APK" -name "veles-userland-$version-android-arm64-v8a-release.apk"
one "Android gateway AAB" -name "veles-$version-android-arm64-v8a-release.aab"
for pair in gateway:tar.gz signer:tar.gz sandbox:oci.tar classifier:oci.tar; do
    kind=${pair%%:*}; suffix=${pair#*:}
    one "$kind OCI archive" -name "veles-$kind-$version-linux-amd64.$suffix"
done
[[ -s $out/payload/oci-image-digests.txt ]] || { echo "missing signed OCI digest inventory" >&2; exit 1; }
[[ -s $out/payload/android-signing.json ]] || { echo "missing Android signing evidence" >&2; exit 1; }
owner=${repo%/*}
registry_host=${VELES_RELEASE_EVIDENCE_REGISTRY:-ghcr.io}
[[ $registry_host =~ ^[A-Za-z0-9.-]+(:[0-9]+)?$ ]] || { echo "invalid evidence registry host" >&2; exit 2; }
if [[ $registry_host != ghcr.io ]]; then
    [[ ${VELES_RELEASE_REHEARSAL:-0} == 1 && $registry_host =~ ^(localhost|127\.0\.0\.1):[0-9]+$ ]] || {
        echo "non-GHCR evidence is allowed only for an explicit loopback rehearsal" >&2; exit 2
    }
fi
jq -s -e --arg owner "${owner,,}" --arg sha "$sha" --arg registry_host "$registry_host" '
    length == 4 and (map(.kind) | sort == ["classifier","gateway","sandbox","signer"]) and
    all(.[]; .platform == "linux/amd64" and .sourceCommit == $sha and
        (.kind as $kind | .image | startswith($registry_host + "/" + $owner + "/veles-" + (if $kind == "gateway" then "agent" else $kind end) + "@sha256:")) and
        (.image | test("sha256:[0-9a-f]{64}$")) and
        (.archive | type == "string" and contains("/") | not) and
        (.archiveSha256 | test("^[0-9a-f]{64}$")))' "$out/payload/oci-image-digests.txt" >/dev/null || {
    echo "invalid or incomplete four-image digest inventory" >&2; exit 1
}
while read -r kind archive digest; do
    [[ $kind && $archive && $digest ]]
    [[ $(sha256sum "$out/payload/$archive" | cut -d' ' -f1) == "$digest" ]] || {
        echo "digest inventory does not match $archive" >&2; exit 1
    }
done < <(jq -r '.kind + " " + .archive + " " + .archiveSha256' "$out/payload/oci-image-digests.txt")
gateway=veles-$version-android-arm64-v8a-release.apk
userland=veles-userland-$version-android-arm64-v8a-release.apk
bundle=veles-$version-android-arm64-v8a-release.aab
jq -e --arg gateway "$gateway" --arg userland "$userland" --arg bundle "$bundle" '
    .kind == "android-upload-key" and (.certificateSha256 | test("^[0-9a-f]{64}$")) and
    (.certificateSha256 as $certificate | .packages as $packages |
        ($packages | length == 3) and
        ($packages | map(.artifact) | sort == ([$gateway,$userland,$bundle] | sort)) and
        all($packages[]; .certificateSha256 == $certificate))' "$out/payload/android-signing.json" >/dev/null || {
    echo "Android evidence does not cover one key and all three packages" >&2; exit 1
}
bash "$(dirname "$0")/verify-android.sh" "$out"
mapfile -t files < <(find "$out/payload" -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort)
((${#files[@]} == 19)) || { echo "expected the complete 19-file R10 payload, got ${#files[@]}" >&2; exit 1; }
manifest=$(mktemp "${TMPDIR:-/tmp}/veles-r10-manifest.XXXXXX")
trap 'rm -f "$manifest"' EXIT
for name in "${files[@]}"; do
    digest=$(sha256sum "$out/payload/$name" | cut -d' ' -f1)
    size=$(stat -c %s "$out/payload/$name")
    case $name in
        *.tar.zst) kind=archive ;; *.AppImage) kind=appimage ;;
        *.apk) kind=android-apk ;; *.aab) kind=android-bundle ;;
        *.tar.gz|*.oci.tar) kind=oci-archive ;;
        oci-image-digests.txt) kind=registry-digests ;; *.json) kind=metadata ;;
        *) echo "unrecognised artifact name: $name" >&2; exit 1 ;;
    esac
    jq -cn --arg name "$name" --arg digest "$digest" --argjson size "$size" --arg kind "$kind" \
        '{name:$name,sha256:$digest,size:$size,kind:$kind}' >> "$manifest"
done
jq -n --arg repo "$repo" --arg tag "$tag" --arg sha "$sha" --arg version "$version" \
    --arg source "git+https://github.com/$repo?rev=$sha" --slurpfile artifacts "$manifest" \
    --argjson systems "$(printf '%s\n' "${systems[@]}" | jq -R . | jq -sc .)" \
    '{schemaVersion:1,repository:$repo,tag:$tag,commit:$sha,version:$version,systems:$systems,
      source:$source,artifacts:$artifacts,
      signing:{files:"sigstore-keyless-cosign",oci:"cosign-by-digest",android:"explicit-upload-key"},
      evidence:{githubRun:((env.GITHUB_SERVER_URL // "") + "/" + $repo + "/actions/runs/" + (env.GITHUB_RUN_ID // "local"))},
      limits:["Signing metadata does not imply target execution or a hermetic build.",
              "APK and AAB signatures use a separately protected Android upload key, never the sideload key.",
              "Classifier model data is an external read-only mount, not embedded in the image.",
              "OCI images are amd64 in this matrix; the sandbox does not claim an arm64 Chrome substitute."]}' \
    > "$out/payload/release-manifest.json"
(cd "$out/payload" && sha256sum "${files[@]}" release-manifest.json > SHA256SUMS)
echo "PASS: complete 19-file payload plus release manifest for $tag@$sha"
