#!/usr/bin/env bash
# Independent, read-only verification of a completed R10 inventory.
# No signing, publication, image loading, or mutation belongs here.
set -euo pipefail
out=${1:?usage: verify-inventory.sh ARTIFACT_DIR OWNER/REPO SHA TAG}
repo=${2:?owner/repo required}
sha=${3:?full source SHA required}
tag=${4:?release tag required}
[[ $out = /* ]] || out=$PWD/$out
[[ $repo =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || { echo "bad repository" >&2; exit 2; }
[[ $sha =~ ^[0-9a-f]{40}$ ]] || { echo "bad source SHA" >&2; exit 2; }
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$ ]] || { echo "bad release tag" >&2; exit 2; }
for f in SOURCE_COMMIT SOURCE_VERSION DRAFT; do
    [[ -s $out/$f ]] || { echo "missing $f" >&2; exit 1; }
done
version=$(<"$out/SOURCE_VERSION")
[[ $(<"$out/SOURCE_COMMIT") == "$sha" && $(<"$out/DRAFT") == 0 ]] || {
    echo "inventory source or draft state does not match the release" >&2; exit 1
}
[[ $tag == "v$version" || $tag == "v$version"-* ]] || { echo "tag/version mismatch" >&2; exit 1; }
for tool in jq sha256sum stat tar zstd; do
    command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }
done
[[ -s $out/payload/release-manifest.json ]] || { echo "missing release manifest" >&2; exit 1; }
if find "$out/unsigned" -type f -print -quit 2>/dev/null | grep -q .; then
    echo "unsigned Android intermediates remain" >&2; exit 1
fi
if find "$out/payload" -maxdepth 1 -type f \
    \( -name '*unsigned*' -o -name '*.sigstore.json' \) -print -quit | grep -q .; then
    echo "unsigned or already signed input in the payload" >&2; exit 1
fi
jq -e --arg repo "$repo" --arg sha "$sha" --arg tag "$tag" --arg version "$version" '
    .schemaVersion == 1 and .repository == $repo and .commit == $sha and
    .tag == $tag and .version == $version and
    .source == ("git+https://github.com/" + $repo + "?rev=" + $sha) and
    .systems == ["x86_64-linux","aarch64-linux"] and
    .signing == {files:"sigstore-keyless-cosign",oci:"cosign-by-digest",android:"explicit-upload-key"} and
    (.artifacts | type == "array" and length == 19)
' "$out/payload/release-manifest.json" >/dev/null || { echo "manifest release identity is invalid" >&2; exit 1; }
jq -e '
    (.artifacts | map(.name) | unique | length == 19) and
    all(.artifacts[];
        (.name | type == "string" and test("^[A-Za-z0-9][A-Za-z0-9._-]*$")) and
        (.sha256 | type == "string" and test("^[0-9a-f]{64}$")) and
        (.size | type == "number" and . > 0 and floor == .) and
        (.kind | type == "string" and length > 0))
' "$out/payload/release-manifest.json" >/dev/null || { echo "invalid artifact metadata" >&2; exit 1; }
manifest_tmp=$(mktemp)
actual_tmp=$(mktemp)
sums_tmp=$(mktemp)
trap 'rm -f "$manifest_tmp" "$actual_tmp" "$sums_tmp"' EXIT
jq -r '.artifacts[].name' "$out/payload/release-manifest.json" | LC_ALL=C sort > "$manifest_tmp"
find "$out/payload" -maxdepth 1 -type f ! -name release-manifest.json ! -name SHA256SUMS -printf '%f\n' | LC_ALL=C sort > "$actual_tmp"
cmp -s "$manifest_tmp" "$actual_tmp" || {
    echo "payload filenames differ from the manifest" >&2; diff -u "$manifest_tmp" "$actual_tmp" >&2; exit 1
}
while read -r name digest size; do
    actual=$(sha256sum "$out/payload/$name" | cut -d' ' -f1)
    [[ $actual == "$digest" && $(stat -c %s "$out/payload/$name") == "$size" ]] || {
        echo "payload bytes differ from the manifest: $name" >&2; exit 1
    }
done < <(jq -r '.artifacts[] | [.name,.sha256,(.size|tostring)] | @tsv' "$out/payload/release-manifest.json")
if [[ -e $out/payload/SHA256SUMS ]]; then
    (cd "$out/payload" && xargs -a "$manifest_tmp" sha256sum && sha256sum release-manifest.json) > "$sums_tmp"
    cmp -s "$sums_tmp" "$out/payload/SHA256SUMS" || {
        echo "SHA256SUMS does not describe the manifest and its exact payload" >&2; exit 1
    }
fi
for system in x86_64 aarch64; do
    name="veles-$version-${sha:0:7}-linux-$system"
    for file in "$name.tar.zst" "$name.AppImage" "$name.json"; do
        [[ -s $out/payload/$file ]] || { echo "missing $file" >&2; exit 1; }
    done
    embedded=$(tar --zstd -xOf "$out/payload/$name.tar.zst" ./BUNDLE-INFO.json)
    [[ $embedded == "$(<"$out/payload/$name.json")" ]] || { echo "archive metadata differs for $system" >&2; exit 1; }
    jq -e --arg version "$version" --arg revision "${sha:0:7}" --arg cpu "$system" '
        .name == "veles" and .version == $version and .revision == $revision and .architecture == $cpu
    ' <<<"$embedded" >/dev/null || { echo "archive provenance differs for $system" >&2; exit 1; }
    if tar --zstd -tf "$out/payload/$name.tar.zst" | grep -E '(^/|(^|/)\.\.(/|$))'; then
        echo "unsafe archive path for $system" >&2; exit 1
    fi
done
for plugin in echo atproto; do
    for cpu in x86_64 aarch64; do
        [[ -s $out/payload/veles-plugin-$plugin-$version-linux-$cpu.tar.zst ]] || {
            echo "missing $plugin $cpu archive" >&2; exit 1
        }
    done
done
owner=${repo%/*}
registry_host=${VELES_RELEASE_EVIDENCE_REGISTRY:-ghcr.io}
[[ $registry_host =~ ^[A-Za-z0-9.-]+(:[0-9]+)?$ ]] || { echo "invalid evidence registry host" >&2; exit 2; }
if [[ $registry_host != ghcr.io ]]; then
    [[ ${VELES_RELEASE_REHEARSAL:-0} == 1 && $registry_host =~ ^(localhost|127\.0\.0\.1):[0-9]+$ ]] || {
        echo "non-GHCR evidence is allowed only for an explicit loopback rehearsal" >&2; exit 2
    }
fi
jq -s -e --arg owner "${owner,,}" --arg sha "$sha" --arg version "$version" --arg registry_host "$registry_host" '
    length == 4 and
    (map(.kind) | sort == ["classifier","gateway","sandbox","signer"]) and
    all(.[];
        (.kind as $kind | .image | startswith($registry_host + "/" + $owner + "/veles-" + (if $kind == "gateway" then "agent" else $kind end) + "@sha256:")) and
        (.image | test("sha256:[0-9a-f]{64}$")) and
        .archive == ("veles-" + .kind + "-" + $version + "-linux-amd64." + (if .kind == "gateway" or .kind == "signer" then "tar.gz" else "oci.tar" end)) and
        .sourceCommit == $sha and .platform == "linux/amd64" and
        (.archiveSha256 | test("^[0-9a-f]{64}$")))
' "$out/payload/oci-image-digests.txt" >/dev/null || { echo "four-image source-bound inventory is invalid" >&2; exit 1; }
while read -r archive digest; do
    [[ $(sha256sum "$out/payload/$archive" | cut -d' ' -f1) == "$digest" ]] || {
        echo "image archive hash differs from registry evidence: $archive" >&2; exit 1
    }
done < <(jq -r '[.archive,.archiveSha256] | @tsv' "$out/payload/oci-image-digests.txt")
gateway=veles-$version-android-arm64-v8a-release.apk
userland=veles-userland-$version-android-arm64-v8a-release.apk
bundle=veles-$version-android-arm64-v8a-release.aab
jq -e --arg gateway "$gateway" --arg userland "$userland" --arg bundle "$bundle" '
    .schemaVersion == 1 and .kind == "android-upload-key" and
    (.certificateSha256 | test("^[0-9a-f]{64}$")) and
    (.certificateSha256 as $certificate | .packages as $packages |
        ($packages | length == 3) and
        ($packages | map(.artifact) | sort == ([$gateway,$userland,$bundle] | sort)) and
        all($packages[]; .certificateSha256 == $certificate)) and
    (all(.packages[] | select(.artifact | endswith(".apk")); .verifiedSchemes.v2 == true and .verifiedSchemes.v3 == true)) and
    (any(.packages[] | select(.artifact | endswith(".aab")); .verifiedSchemes.jar == true))
' "$out/payload/android-signing.json" >/dev/null || { echo "Android certificate/scheme evidence is invalid" >&2; exit 1; }
echo "PASS: inventory, payload bytes, source provenance and signing evidence match $tag@$sha"
