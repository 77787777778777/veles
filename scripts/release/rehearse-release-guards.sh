#!/usr/bin/env bash
# Offline acceptance for R10's source, inventory, signing, and OIDC guards.
# It creates a disposable Android test key, never a production upload key.
# No registry, GitHub release, or Sigstore endpoint is written.
set -euo pipefail
cd "$(dirname "$0")/../.."
work=$(mktemp -d "${TMPDIR:-/tmp}/veles-r10-guards.XXXXXX")
cleanup() {
    local rc=$?
    if ((rc == 0)); then
        chmod -R u+w "$work" 2>/dev/null || true
        rm -rf "$work"
    else
        echo "retained failed offline fixture: $work" >&2
    fi
}
trap cleanup EXIT
pass=0
expect_failure() {
    local name=$1
    shift
    if "$@" >"$work/$name.log" 2>&1; then
        echo "GUARD FAILED (unexpected success): $name" >&2
        cat "$work/$name.log" >&2
        exit 1
    fi
    printf 'PASS guard rejects %s: ' "$name"
    tail -1 "$work/$name.log"
    pass=$((pass + 1))
}
pass_check() {
    local name=$1
    shift
    "$@" >"$work/$name.log" 2>&1 || { cat "$work/$name.log" >&2; return 1; }
    echo "PASS $name"
    pass=$((pass + 1))
}
expect_failure_with() {
    local name=$1 expected=$2
    shift 2
    expect_failure "$name" "$@"
    grep -F "$expected" "$work/$name.log" >/dev/null || {
        echo "guard failed for the wrong reason: $name" >&2; exit 1
    }
}
sha=$(git rev-parse HEAD)
version=$(nix eval --raw '.#packages.x86_64-linux.default.version')
tag=v$version
sha_bad=0000000000000000000000000000000000000000
# Most tests are offline and do not exercise tag resolution. Production
# defaults to checking it; the tag-specific tests below opt back in with a
# deterministic API fixture rather than contacting GitHub.
export VELES_RELEASE_CHECK_TAG_OBJECT=0
export GH_TOKEN=fixture-only-never-sent-to-github
for tool in git jq keytool jarsigner java tar zstd; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }; done
[[ -n ${VELES_BUILD_TOOLS:-} && -x $VELES_BUILD_TOOLS/apksigner ]] || { echo "VELES_BUILD_TOOLS is required" >&2; exit 2; }
expect_failure_with "loopback requires exact pinned source inventory" "not a pinned inventory" env \
    VELES_R10_INPUT_ROOT="$work/not-a-pinned-inventory" \
    bash scripts/release/rehearse-local-registry.sh
mkdir -p "$work/tag-api"
cat > "$work/tag-api/gh" <<'GH'
#!/usr/bin/env bash
set -euo pipefail
[[ $1 == api ]]
case $2 in
    repos/77787777778777/veles/git/ref/tags/*)
        jq -cn --arg sha "$GITHUB_SHA" '{object:{sha:$sha,type:(if env.FAKE_ANNOTATED == "1" then "tag" else "commit" end)}}'
        ;;
    repos/77787777778777/veles/git/tags/*)
        [[ ${FAKE_ANNOTATED:-0} == 1 ]]
        jq -cn --arg sha "$EXPECTED_COMMIT" '{object:{sha:$sha,type:"commit"}}'
        ;;
    *) exit 1 ;;
esac
GH
chmod 755 "$work/tag-api/gh"
pass_check "lightweight tag resolves to its source commit" env \
    PATH="$work/tag-api:$PATH" EXPECTED_COMMIT="$sha" VELES_RELEASE_CHECK_TAG_OBJECT=1 \
    GITHUB_ACTIONS=true GITHUB_EVENT_NAME=push GITHUB_REF="refs/tags/$tag" \
    GITHUB_REF_NAME="$tag" GITHUB_SHA="$sha" GITHUB_REPOSITORY=77787777778777/veles \
    ACTIONS_ID_TOKEN_REQUEST_URL=placeholder ACTIONS_ID_TOKEN_REQUEST_TOKEN=placeholder \
    ANDROID_UPLOAD_KEYSTORE_B64=cGxhY2Vob2xkZXI= ANDROID_UPLOAD_STOREPASS=placeholder \
    ANDROID_UPLOAD_KEYPASS=placeholder ANDROID_UPLOAD_ALIAS=upload \
    scripts/release/require-release-credentials.sh 77787777778777/veles "$sha" "$tag"
pass_check "annotated tag resolves to its peeled source commit" env \
    PATH="$work/tag-api:$PATH" EXPECTED_COMMIT="$sha" FAKE_ANNOTATED=1 \
    VELES_RELEASE_CHECK_TAG_OBJECT=1 \
    GITHUB_ACTIONS=true GITHUB_EVENT_NAME=push GITHUB_REF="refs/tags/$tag" \
    GITHUB_REF_NAME="$tag" GITHUB_SHA="$sha_bad" GITHUB_REPOSITORY=77787777778777/veles \
    ACTIONS_ID_TOKEN_REQUEST_URL=placeholder ACTIONS_ID_TOKEN_REQUEST_TOKEN=placeholder \
    ANDROID_UPLOAD_KEYSTORE_B64=cGxhY2Vob2xkZXI= ANDROID_UPLOAD_STOREPASS=placeholder \
    ANDROID_UPLOAD_KEYPASS=placeholder ANDROID_UPLOAD_ALIAS=upload \
    scripts/release/require-release-credentials.sh 77787777778777/veles "$sha" "$tag"
pass_check "tag and OIDC inputs accepted without reading or exposing a key" env \
    GITHUB_ACTIONS=true GITHUB_EVENT_NAME=push GITHUB_REF="refs/tags/$tag" \
    GITHUB_REF_NAME="$tag" GITHUB_SHA="$sha" GITHUB_REPOSITORY=77787777778777/veles \
    ACTIONS_ID_TOKEN_REQUEST_URL=placeholder ACTIONS_ID_TOKEN_REQUEST_TOKEN=placeholder \
    VELES_RELEASE_CHECK_TAG_OBJECT=0 \
    ANDROID_UPLOAD_KEYSTORE_B64=cGxhY2Vob2xkZXI= ANDROID_UPLOAD_STOREPASS=placeholder \
    ANDROID_UPLOAD_KEYPASS=placeholder ANDROID_UPLOAD_ALIAS=upload \
    scripts/release/require-release-credentials.sh 77787777778777/veles "$sha" "$tag"
expect_failure "missing Android upload key" env \
    GITHUB_ACTIONS=true GITHUB_EVENT_NAME=push GITHUB_REF="refs/tags/$tag" \
    GITHUB_REF_NAME="$tag" GITHUB_SHA="$sha" GITHUB_REPOSITORY=77787777778777/veles \
    ACTIONS_ID_TOKEN_REQUEST_URL=placeholder ACTIONS_ID_TOKEN_REQUEST_TOKEN=placeholder \
    ANDROID_UPLOAD_KEYSTORE_B64= ANDROID_UPLOAD_STOREPASS=placeholder \
    ANDROID_UPLOAD_KEYPASS=placeholder ANDROID_UPLOAD_ALIAS=upload \
    scripts/release/require-release-credentials.sh 77787777778777/veles "$sha" "$tag"
expect_failure "manual dispatch has the wrong signing ref" env \
    GITHUB_ACTIONS=true GITHUB_EVENT_NAME=workflow_dispatch GITHUB_REF=refs/heads/main \
    GITHUB_REF_NAME=main GITHUB_SHA="$sha" GITHUB_REPOSITORY=77787777778777/veles \
    ACTIONS_ID_TOKEN_REQUEST_URL=placeholder ACTIONS_ID_TOKEN_REQUEST_TOKEN=placeholder \
    VELES_RELEASE_CHECK_TAG_OBJECT=0 \
    ANDROID_UPLOAD_KEYSTORE_B64=cGxhY2Vob2xkZXI= ANDROID_UPLOAD_STOREPASS=placeholder \
    ANDROID_UPLOAD_KEYPASS=placeholder ANDROID_UPLOAD_ALIAS=upload \
    scripts/release/require-release-credentials.sh 77787777778777/veles "$sha" "$tag"
expect_failure_with "source SHA mismatch" "tag object does not resolve to the authenticated release commit" env \
    PATH="$work/tag-api:$PATH" EXPECTED_COMMIT=1111111111111111111111111111111111111111 FAKE_ANNOTATED=1 VELES_RELEASE_CHECK_TAG_OBJECT=1 \
    GITHUB_ACTIONS=true GITHUB_EVENT_NAME=push GITHUB_REF="refs/tags/$tag" \
    GITHUB_REF_NAME="$tag" GITHUB_SHA="$sha_bad" GITHUB_REPOSITORY=77787777778777/veles \
    ACTIONS_ID_TOKEN_REQUEST_URL=placeholder ACTIONS_ID_TOKEN_REQUEST_TOKEN=placeholder \
    ANDROID_UPLOAD_KEYSTORE_B64=cGxhY2Vob2xkZXI= ANDROID_UPLOAD_STOREPASS=placeholder \
    ANDROID_UPLOAD_KEYPASS=placeholder ANDROID_UPLOAD_ALIAS=upload \
    scripts/release/require-release-credentials.sh 77787777778777/veles "$sha" "$tag"
expect_failure "dirty release checkout is not CI source" env \
    VELES_RELEASE_EXPECTED_SHA="$sha" VELES_RELEASE_LANES=none \
    bash scripts/release/build-artifacts.sh "$work/dirty" .
expect_failure "mutable checkout cannot masquerade as a pinned release" env \
    -u VELES_RELEASE_EXPECTED_SHA -u VELES_RELEASE_DRAFT VELES_RELEASE_LANES=none \
    bash scripts/release/build-artifacts.sh "$work/unpinned" .
# Cheap source fixtures exercise selection/provenance independently of the
# expensive artifact builders. No generated artifact is called a release.
for lane in release-x86_64-linux release-aarch64-linux release-android; do
    mkdir -p "$work/lanes/$lane/payload" "$work/lanes/$lane/unsigned"
    # All three lanes may legitimately carry this same byte-identical
    # provenance fixture. The changed-lane case below is a separate copy.
    printf 'fixture lane\n' > "$work/lanes/$lane/payload/source.fixture"
    system=x86_64-linux
    [[ $lane == release-aarch64-linux ]] && system=aarch64-linux
    printf '%s\n' "$sha" > "$work/lanes/$lane/SOURCE_COMMIT"
    printf '%s\n' "$version" > "$work/lanes/$lane/SOURCE_VERSION"
    printf '%s\n' "$system" > "$work/lanes/$lane/SOURCE_SYSTEM"
    printf '.\n0\n' > "$work/lanes/$lane/provenance"
    head -1 "$work/lanes/$lane/provenance" > "$work/lanes/$lane/SOURCE_FLAKE_REF"
    tail -1 "$work/lanes/$lane/provenance" > "$work/lanes/$lane/DRAFT"
    bash scripts/release/payload-inventory.sh seal "$work/lanes/$lane"
done
cp -a "$work/lanes/." "$work/good-lanes/"
pass_check "three matched artifact lanes assemble" bash scripts/release/assemble-artifacts.sh \
    "$work/good" "$sha" "$version" \
    "$work/good-lanes/release-x86_64-linux" "$work/good-lanes/release-aarch64-linux" \
    "$work/good-lanes/release-android"
cp -a "$work/lanes/." "$work/bad-lanes/"
printf '%s\n' "$sha_bad" > "$work/bad-lanes/release-aarch64-linux/SOURCE_COMMIT"
expect_failure "mixed source lane" bash scripts/release/assemble-artifacts.sh \
    "$work/mixed" "$sha" "$version" \
    "$work/bad-lanes/release-x86_64-linux" "$work/bad-lanes/release-aarch64-linux" \
    "$work/bad-lanes/release-android"
printf '%s\n' "$sha" > "$work/lanes/release-aarch64-linux/SOURCE_COMMIT"
printf '%s\n' aarch64-linux > "$work/lanes/release-x86_64-linux/SOURCE_SYSTEM"
expect_failure "wrong lane architecture" bash scripts/release/assemble-artifacts.sh \
    "$work/wrong-arch" "$sha" "$version" \
    "$work/lanes/release-x86_64-linux" "$work/lanes/release-aarch64-linux" \
    "$work/lanes/release-android"
cp -a "$work/good-lanes" "$work/changed-lanes"
printf 'replacement payload bytes\n' >> "$work/changed-lanes/release-x86_64-linux/payload/source.fixture"
expect_failure_with "changed lane bytes cannot inherit its checksums" "payload differs from PREPUBLISH-SHA256SUMS" \
    bash scripts/release/assemble-artifacts.sh "$work/changed-lane" "$sha" "$version" \
    "$work/changed-lanes/release-x86_64-linux" "$work/changed-lanes/release-aarch64-linux" \
    "$work/changed-lanes/release-android"
# Construct only the manifest fixture. Large files are tiny test payloads;
# filenames and per-file SHA256 values still exercise the real inventory.
rm -f "$work/good/payload/source.fixture" "$work/good-lanes"/*/payload/source.fixture
for system in x86_64 aarch64; do
    mkdir -p "$work/bundle-$system/bin"
    jq -cn --arg version "$version" --arg revision "${sha:0:7}" --arg cpu "$system" \
        '{name:"veles",version:$version,revision:$revision,architecture:$cpu}' \
        > "$work/bundle-$system/BUNDLE-INFO.json"
    printf '#!/bin/sh\necho fixture %s\n' "$system" > "$work/bundle-$system/bin/veles"
    chmod +x "$work/bundle-$system/bin/veles"
    tar --sort=name --format=gnu --mtime='@1' --owner=0 --group=0 --numeric-owner \
        --zstd -cf "$work/good/payload/veles-$version-${sha:0:7}-linux-$system.tar.zst" \
        -C "$work/bundle-$system" .
    printf '%s %s\n' "$system" AppImage > "$work/good/payload/veles-$version-${sha:0:7}-linux-$system.AppImage"
    cp "$work/bundle-$system/BUNDLE-INFO.json" "$work/good/payload/veles-$version-${sha:0:7}-linux-$system.json"
    for plugin in echo atproto; do
        printf '%s %s\n' "$system" "$plugin" > "$work/good/payload/veles-plugin-$plugin-$version-linux-$system.tar.zst"
    done
done
for kind in gateway signer; do printf '%s\n' "$kind" > "$work/good/payload/veles-$kind-$version-linux-amd64.tar.gz"; done
for kind in sandbox classifier; do printf '%s\n' "$kind" > "$work/good/payload/veles-$kind-$version-linux-amd64.oci.tar"; done
: > "$work/good/payload/oci-image-digests.txt"
for kind in gateway signer sandbox classifier; do
    archive="veles-$kind-$version-linux-amd64.oci.tar"
    [[ $kind == gateway || $kind == signer ]] && archive="veles-$kind-$version-linux-amd64.tar.gz"
    image=$kind
    [[ $kind == gateway ]] && image=agent
    jq -cn --arg kind "$kind" --arg image "ghcr.io/77787777778777/veles-$image@sha256:$(printf '%064d' 0)" \
        --arg archive "$archive" --arg digest "$(sha256sum "$work/good/payload/$archive" | cut -d' ' -f1)" --arg sha "$sha" \
        '{kind:$kind,image:$image,archive:$archive,archiveSha256:$digest,sourceCommit:$sha,platform:"linux/amd64"}' >> "$work/good/payload/oci-image-digests.txt"
done
apkout=$(nix build .#veles-apk .#veles-userland-apk .#veles-aab --no-link --print-out-paths)
while IFS= read -r path; do
    for name in veles-release-unsigned.apk veles-userland-release-unsigned.apk veles-release-unsigned.aab; do
        if [[ -f $path/$name ]]; then
            base=$(basename "$name" -release-unsigned.${name##*.})
            [[ $base == veles ]] && base=veles-$version-android-arm64-v8a-release
            [[ $base == veles-userland ]] && base=veles-userland-$version-android-arm64-v8a-release
            install -m 0644 "$path/$name" "$work/good/unsigned/$base-unsigned.${name##*.}"
        fi
    done
done <<<"$apkout"
mapfile -t unsigned < <(find "$work/good/unsigned" -type f)
((${#unsigned[@]} == 3)) || { printf '%s\n' "${unsigned[@]}" >&2; exit 1; }
expect_failure "unsigned Android intermediates before signing" env \
    VELES_RELEASE_REPOSITORY=77787777778777/veles VELES_RELEASE_TAG="$tag" \
    bash scripts/release/manifest.sh "$work/good"
expect_failure "missing upload-key credentials" env -u ANDROID_UPLOAD_KEYSTORE \
    -u ANDROID_UPLOAD_STOREPASS -u ANDROID_UPLOAD_KEYPASS -u ANDROID_UPLOAD_ALIAS \
    VELES_BUILD_TOOLS="$VELES_BUILD_TOOLS" bash scripts/release/sign-android.sh "$work/good"
keytool -genkeypair -keystore "$work/rehearsal.p12" -storetype PKCS12 \
    -alias rehearsal-upload -keyalg RSA -keysize 2048 -validity 2 \
    -storepass rehearsal-pass -keypass rehearsal-pass \
    -dname 'CN=Veles disposable rehearsal' >/dev/null 2>&1
export ANDROID_UPLOAD_KEYSTORE="$work/rehearsal.p12"
export ANDROID_UPLOAD_STOREPASS=rehearsal-pass
export ANDROID_UPLOAD_KEYPASS=rehearsal-pass
export ANDROID_UPLOAD_ALIAS=rehearsal-upload
pass_check "both APKs and the AAB use one disposable explicit key" \
    bash scripts/release/sign-android.sh "$work/good"
cp -a "$work/good" "$work/partial"
mv "$work/partial/payload/veles-$version-${sha:0:7}-linux-aarch64.tar.zst" "$work/partial-missing.tar.zst"
expect_failure_with "partial architecture inventory cannot sign" "aarch64 archive" env \
    VELES_RELEASE_REPOSITORY=77787777778777/veles VELES_RELEASE_TAG="$tag" \
    bash scripts/release/manifest.sh "$work/partial"
pass_check "complete manifest and all file hashes" env \
    VELES_RELEASE_REPOSITORY=77787777778777/veles VELES_RELEASE_TAG="$tag" \
    bash scripts/release/manifest.sh "$work/good"
(cd "$work/good/payload" && sha256sum -c SHA256SUMS)
cp -a "$work/good/payload/release-manifest.json" "$work/manifest.good"
pass_check "manifest and every payload byte verify" \
    bash scripts/release/verify-inventory.sh "$work/good" 77787777778777/veles "$sha" "$tag"
cp -a "$work/good" "$work/tampered-android"
printf 'not a signed package\n' >> "$work/tampered-android/payload/veles-$version-android-arm64-v8a-release.apk"
# Reconcile the outer hashes so this reaches the cryptographic APK check,
# rather than passing on the independent byte-hash guard.
while IFS= read -r name; do
    jq --arg name "$name" --arg digest "$(sha256sum "$work/tampered-android/payload/$name" | cut -d' ' -f1)" --argjson size "$(stat -c %s "$work/tampered-android/payload/$name")" \
        '.artifacts |= map(if .name == $name then .sha256 = $digest | .size = $size else . end)' \
        "$work/tampered-android/payload/release-manifest.json" > "$work/android-manifest"
    mv "$work/android-manifest" "$work/tampered-android/payload/release-manifest.json"
done < <(jq -r '.artifacts[].name' "$work/tampered-android/payload/release-manifest.json")
expect_failure_with "APK bytes cannot inherit stale signing evidence" "failed APK signature verification" env \
    bash scripts/release/verify-android.sh "$work/tampered-android"
expect_failure "incomplete OIDC inputs cannot sign" env \
    GITHUB_ACTIONS=true ACTIONS_ID_TOKEN_REQUEST_URL=placeholder \
    bash scripts/release/sign-artifacts.sh "$work/good/payload" \
    77787777778777/veles "$sha" "$tag"
expect_failure_with "wrong tag cannot sign this manifest" "manifest source/repository/tag mismatch" env \
    FAKE_EXPECTED_SHA="$sha" VELES_RELEASE_WORKFLOW_SHA="$sha" GITHUB_ACTIONS=true ACTIONS_ID_TOKEN_REQUEST_URL=placeholder ACTIONS_ID_TOKEN_REQUEST_TOKEN=placeholder \
    GITHUB_EVENT_NAME=push GITHUB_REF="refs/tags/$tag" GITHUB_REPOSITORY=77787777778777/veles \
    PATH="$work/fakebin:$PATH" bash scripts/release/sign-artifacts.sh \
    "$work/good/payload" \
    77787777778777/veles "$sha" v0.1.0-wrong
expect_failure "local execution has no release OIDC identity" env -u GITHUB_ACTIONS \
    bash scripts/release/sign-artifacts.sh "$work/good/payload" \
    77787777778777/veles "$sha" "$tag"
# Fake only cosign locally to prove argument wiring, including workflow
# ref and commit SHA. This does NOT claim a signature or Sigstore success.
mkdir "$work/fakebin"
cat > "$work/fakebin/cosign" <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "${FAKE_COSIGN_LOG:?}"
if [[ $1 == sign-blob ]]; then
    while (($#)); do
        if [[ $1 == --bundle ]]; then printf '{}\n' > "$2"; exit 0; fi
        shift
    done
    exit 1
fi
[[ $1 == verify-blob ]] || exit 1
args=" $* "
[[ $args == *' --certificate-github-workflow-ref refs/tags/'* ]]
[[ $args == *" --certificate-github-workflow-sha ${FAKE_EXPECTED_SHA:?} "* ]]
FAKE
chmod 755 "$work/fakebin/cosign"
export FAKE_COSIGN_LOG="$work/cosign-args.log"
cp -a "$work/good" "$work/tampered"
rm -f "$work/tampered/payload/SHA256SUMS"
printf 'not the signed bytes\n' >> "$work/tampered/payload/veles-$version-${sha:0:7}-linux-x86_64.AppImage"
expect_failure_with "changed bytes cannot be signed by regenerating SHA256SUMS" "payload bytes differ from the manifest" env \
    FAKE_EXPECTED_SHA="$sha" VELES_RELEASE_WORKFLOW_SHA="$sha" GITHUB_ACTIONS=true ACTIONS_ID_TOKEN_REQUEST_URL=placeholder ACTIONS_ID_TOKEN_REQUEST_TOKEN=placeholder \
    GITHUB_EVENT_NAME=push GITHUB_REF="refs/tags/$tag" GITHUB_REPOSITORY=77787777778777/veles \
    PATH="$work/fakebin:$PATH" bash scripts/release/sign-artifacts.sh "$work/tampered/payload" 77787777778777/veles "$sha" "$tag"
cp -a "$work/good" "$work/missing-image"
rm "$work/missing-image/payload/veles-signer-$version-linux-amd64.tar.gz"
rm -f "$work/missing-image/payload/release-manifest.json" \
    "$work/missing-image/payload/SHA256SUMS" "$work/missing-image/payload/oci-image-digests.txt"
expect_failure_with "missing image is rejected before the first registry write" "expected 18 pre-publication artifacts and Android evidence, got 17" env \
    GITHUB_ACTIONS=true GITHUB_EVENT_NAME=push GITHUB_REF="refs/tags/$tag" GITHUB_REF_NAME="$tag" \
    GITHUB_REPOSITORY=77787777778777/veles GITHUB_SHA="$sha" VELES_RELEASE_WORKFLOW_SHA="$sha" \
    ACTIONS_ID_TOKEN_REQUEST_URL=placeholder ACTIONS_ID_TOKEN_REQUEST_TOKEN=placeholder \
    PATH="$work/fakebin:$PATH" bash scripts/release/publish-images.sh "$work/missing-image" 77787777778777/veles "$sha" "$tag"
cp -a "$work/good" "$work/tampered-before-publish"
rm -f "$work/tampered-before-publish/payload/release-manifest.json" \
    "$work/tampered-before-publish/payload/SHA256SUMS" "$work/tampered-before-publish/payload/oci-image-digests.txt"
printf 'changed after lane assembly\n' >> "$work/tampered-before-publish/payload/veles-$version-${sha:0:7}-linux-x86_64.AppImage"
expect_failure_with "changed bytes rejected before any registry write" "payload differs from PREPUBLISH-SHA256SUMS" env \
    GITHUB_ACTIONS=true GITHUB_EVENT_NAME=push GITHUB_REF="refs/tags/$tag" GITHUB_REF_NAME="$tag" \
    GITHUB_REPOSITORY=77787777778777/veles GITHUB_SHA="$sha" VELES_RELEASE_WORKFLOW_SHA="$sha" \
    ACTIONS_ID_TOKEN_REQUEST_URL=placeholder ACTIONS_ID_TOKEN_REQUEST_TOKEN=placeholder \
    PATH="$work/fakebin:$PATH" bash scripts/release/publish-images.sh "$work/tampered-before-publish" 77787777778777/veles "$sha" "$tag"
cp -a "$work/good" "$work/stale-image"
jq -c --arg bad "$sha_bad" '.sourceCommit = $bad' "$work/good/payload/oci-image-digests.txt" > "$work/stale-image/payload/oci-image-digests.txt.tmp"
mv "$work/stale-image/payload/oci-image-digests.txt.tmp" "$work/stale-image/payload/oci-image-digests.txt"
# Refresh only the outer hashes, so the negative case reaches the registry
# source check instead of stopping at the independent byte-integrity guard.
jq -r '.artifacts[].name' "$work/stale-image/payload/release-manifest.json" > "$work/stale-names"
while IFS= read -r name; do
    digest=$(sha256sum "$work/stale-image/payload/$name" | cut -d' ' -f1)
    jq --arg name "$name" --arg digest "$digest" --argjson size "$(stat -c %s "$work/stale-image/payload/$name")" \
        '.artifacts |= map(if .name == $name then .sha256 = $digest | .size = $size else . end)' \
        "$work/stale-image/payload/release-manifest.json" > "$work/stale-manifest.json"
    mv "$work/stale-manifest.json" "$work/stale-image/payload/release-manifest.json"
done < "$work/stale-names"
(cd "$work/stale-image/payload" && xargs -a "$work/stale-names" sha256sum && sha256sum release-manifest.json) > "$work/stale-image/payload/SHA256SUMS"
expect_failure_with "stale registry evidence cannot be signed" "four-image source-bound inventory is invalid" env \
    bash scripts/release/verify-inventory.sh "$work/stale-image" 77787777778777/veles "$sha" "$tag"
printf '%s\n' 1 > "$work/good/DRAFT"
expect_failure_with "draft registry publication" "refusing to publish draft artifacts" env \
    GITHUB_ACTIONS=true GITHUB_EVENT_NAME=push GITHUB_REF="refs/tags/$tag" \
    GITHUB_REPOSITORY=77787777778777/veles GITHUB_SHA="$sha" VELES_RELEASE_WORKFLOW_SHA="$sha" \
    ACTIONS_ID_TOKEN_REQUEST_URL=placeholder ACTIONS_ID_TOKEN_REQUEST_TOKEN=placeholder \
    bash scripts/release/publish-images.sh "$work/good" 77787777778777/veles "$sha" "$tag"
expect_failure "draft inventory cannot be signed into a manifest" env \
    VELES_RELEASE_REPOSITORY=77787777778777/veles VELES_RELEASE_TAG="$tag" \
    bash scripts/release/manifest.sh "$work/good"
# Keep the positive signing case last: all preceding negative fixtures must
# still contain unsigned inputs, never signature bundles from this rehearsal.
printf '%s\n' 0 > "$work/good/DRAFT"
pass_check "signing arguments include exact tag ref and source SHA" env \
    FAKE_EXPECTED_SHA="$sha" VELES_RELEASE_WORKFLOW_SHA="$sha" GITHUB_ACTIONS=true ACTIONS_ID_TOKEN_REQUEST_URL=placeholder \
    GITHUB_EVENT_NAME=push GITHUB_REF="refs/tags/$tag" GITHUB_REPOSITORY=77787777778777/veles \
    ACTIONS_ID_TOKEN_REQUEST_TOKEN=placeholder \
    PATH="$work/fakebin:$PATH" bash scripts/release/sign-artifacts.sh \
    "$work/good/payload" 77787777778777/veles "$sha" "$tag"
grep -F "certificate-github-workflow-sha $sha" "$work/cosign-args.log" >/dev/null
echo "OFFLINE_RELEASE_GUARDS_PASS=$pass"
