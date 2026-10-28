#!/usr/bin/env bash
# Validate the R10 lanes and image evidence needed BEFORE any upload. This
# deliberately does not require a manifest generated after registry signing.
set -euo pipefail
out=${1:?usage: preflight-images.sh ARTIFACT_DIR OWNER/REPO SHA TAG}
repo=${2:?owner/repo required}
sha=${3:?full source SHA required}
tag=${4:?release tag required}
[[ $repo =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || { echo "bad repository" >&2; exit 2; }
[[ $sha =~ ^[0-9a-f]{40}$ ]] || { echo "bad source SHA" >&2; exit 2; }
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$ ]] || { echo "bad tag" >&2; exit 2; }
for tool in jq sha256sum stat tar zstd podman; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }; done
[[ -s $out/SOURCE_COMMIT && -s $out/SOURCE_VERSION && -s $out/DRAFT ]] || { echo "missing inventory provenance" >&2; exit 1; }
version=$(<"$out/SOURCE_VERSION")
[[ $(<"$out/SOURCE_COMMIT") == "$sha" && $(<"$out/DRAFT") == 0 ]] || { echo "inventory is not for this source release" >&2; exit 1; }
[[ $tag == "v$version" || $tag == "v$version"-* ]] || { echo "tag/version mismatch" >&2; exit 1; }
mapfile -t lanes < "$out/SOURCE_SYSTEMS"
[[ ${lanes[*]} == "x86_64-linux aarch64-linux" ]] || { echo "incomplete architecture lanes" >&2; exit 1; }
if find "$out/unsigned" -type f -print -quit 2>/dev/null | grep -q .; then
    echo "unsigned Android intermediates remain" >&2; exit 1
fi
if find "$out/payload" -maxdepth 1 -type f \
    \( -name '*unsigned*' -o -name 'SHA256SUMS*' -o -name '*.sigstore.json' -o -name 'release-manifest.json' \) -print -quit | grep -q .; then
    echo "unsigned or processed input in pre-publication payload" >&2; exit 1
fi
count=$(find "$out/payload" -maxdepth 1 -type f | wc -l)
[[ $count == 18 ]] || { echo "expected 18 pre-publication artifacts and Android evidence, got $count" >&2; exit 1; }
for cpu in x86_64 aarch64; do
    base="veles-$version-${sha:0:7}-linux-$cpu"
    for suffix in tar.zst AppImage json; do [[ -s $out/payload/$base.$suffix ]] || { echo "missing $base.$suffix" >&2; exit 1; }; done
    metadata=$(tar --zstd -xOf "$out/payload/$base.tar.zst" ./BUNDLE-INFO.json)
    [[ $metadata == "$(<"$out/payload/$base.json")" ]] || { echo "$cpu bundle metadata mismatch" >&2; exit 1; }
    jq -e --arg version "$version" --arg rev "${sha:0:7}" --arg cpu "$cpu" '
        .name == "veles" and .version == $version and .revision == $rev and .architecture == $cpu
    ' <<<"$metadata" >/dev/null || { echo "$cpu bundle provenance mismatch" >&2; exit 1; }
    for plugin in echo atproto; do [[ -s $out/payload/veles-plugin-$plugin-$version-linux-$cpu.tar.zst ]] || { echo "missing $plugin/$cpu" >&2; exit 1; }; done
done
for file in "veles-$version-android-arm64-v8a-release.apk" "veles-userland-$version-android-arm64-v8a-release.apk" "veles-$version-android-arm64-v8a-release.aab" android-signing.json; do
    [[ -s $out/payload/$file ]] || { echo "missing signed Android input: $file" >&2; exit 1; }
done
jq -e --arg gateway "veles-$version-android-arm64-v8a-release.apk" --arg userland "veles-userland-$version-android-arm64-v8a-release.apk" --arg bundle "veles-$version-android-arm64-v8a-release.aab" '
    .kind == "android-upload-key" and (.certificateSha256 | test("^[0-9a-f]{64}$")) and
    (.certificateSha256 as $certificate | .packages as $packages |
        ($packages | length == 3) and
        ($packages | map(.artifact) | sort == ([$gateway,$userland,$bundle] | sort)) and
        all($packages[]; .certificateSha256 == $certificate)) and
    all(.packages[] | select(.artifact | endswith(".apk")); .verifiedSchemes.v2 == true and .verifiedSchemes.v3 == true) and
    any(.packages[] | select(.artifact | endswith(".aab")); .verifiedSchemes.jar == true)
' "$out/payload/android-signing.json" >/dev/null || { echo "invalid Android signing evidence" >&2; exit 1; }
# Evidence is not a signature. Re-verify every package byte before an
# upload, even if a previous lane wrote this JSON.
bash "$(dirname "$0")/payload-inventory.sh" verify "$out"
bash "$(dirname "$0")/verify-android.sh" "$out"
if [[ -e $out/payload/oci-image-digests.txt ]]; then
    echo "refusing to overwrite an existing image digest inventory" >&2
    exit 1
fi
for kind in gateway signer sandbox classifier; do
    suffix=oci.tar
    [[ $kind == gateway || $kind == signer ]] && suffix=tar.gz
    archive="$out/payload/veles-$kind-$version-linux-amd64.$suffix"
    [[ -s $archive ]] || { echo "missing $kind image archive" >&2; exit 1; }
    load_output=$(podman load --input "$archive")
    loaded=$(printf '%s\n' "$load_output" | sed -n 's/^Loaded image: //p')
    [[ -n $loaded && $(printf '%s\n' "$loaded" | wc -l) -eq 1 ]] || { echo "cannot identify loaded $kind" >&2; exit 1; }
    podman image inspect "$loaded" --format '{{.Os}}/{{.Architecture}}' | grep -qx 'linux/amd64'
    actual_revision=$(podman image inspect "$loaded" --format '{{index .Config.Labels "org.opencontainers.image.revision"}}')
    actual_source=$(podman image inspect "$loaded" --format '{{index .Config.Labels "org.opencontainers.image.source"}}')
    actual_title=$(podman image inspect "$loaded" --format '{{index .Config.Labels "org.opencontainers.image.title"}}')
    [[ $actual_revision == "$sha" && $actual_source == "https://github.com/$repo" && $actual_title == "veles-$kind" ]] || {
        echo "$kind image provenance/title does not match the release" >&2; exit 1
    }
done
echo "PASS: all required artifacts and image provenance verified before any upload"
