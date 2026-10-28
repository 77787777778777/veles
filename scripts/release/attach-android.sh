#!/usr/bin/env bash
# Sign the release's Android APKs locally and attach them to its draft.
#
#   ANDROID_UPLOAD_KEYSTORE=/abs/key.p12 ANDROID_UPLOAD_ALIAS=... \
#   ANDROID_UPLOAD_STOREPASS=... ANDROID_UPLOAD_KEYPASS=... \
#     scripts/release/attach-android.sh v0.1.0
#
# The release workflow builds the gateway + userland APKs (and the gateway
# AAB) UNSIGNED and keeps them as the run's `android-unsigned` artifact; no
# key ever leaves this machine. This script downloads that artifact from
# the tag's release run, checks it was built from the tag's commit, signs
# it with scripts/release/sign-android.sh (same certificate for both APKs,
# verified with apksigner), and uploads the two APKs, android-signing.json
# and their own SHA256SUMS-android to the release. The cosign-signed
# SHA256SUMS is CI's and is not touched; the APK signature is what Android
# checks. The AAB (Play upload) is left in the work directory, not attached.
#
# Which key: the APK on the owner's phone was installed by
# scripts/android-install.sh, signed with ~/.config/veles-android/sideload.p12
# (alias `sideload`). Android upgrades an app in place only under the same
# certificate, so a release signed with a different key cannot replace that
# install without uninstalling (which deletes the app's files/home). Using
# the sideload key needs VELES_ANDROID_ALLOW_SIDELOAD_KEY=1 (sign-android.sh).
set -euo pipefail
tag=${1:?usage: attach-android.sh vX.Y.Z}
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$ ]] || { echo "bad tag $tag" >&2; exit 2; }
cd "$(dirname "$0")/../.."
repo=${VELES_REPOSITORY:-77787777778777/veles}
for tool in gh jq nix sha256sum git; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }; done
for var in ANDROID_UPLOAD_KEYSTORE ANDROID_UPLOAD_STOREPASS ANDROID_UPLOAD_KEYPASS ANDROID_UPLOAD_ALIAS; do
    [[ -n ${!var:-} ]] || { echo "$var is required (see the header of this script)" >&2; exit 2; }
done
gh release view "$tag" -R "$repo" --json isDraft --jq .isDraft | grep -qx true || {
    echo "$repo $tag is not a draft release; attach before publishing it" >&2; exit 1
}
commit=$(gh api "repos/$repo/commits/$tag" --jq .sha)
run=$(gh run list -R "$repo" --workflow release.yml --event push --branch "$tag" \
    --json databaseId,conclusion --jq '[.[] | select(.conclusion == "success")][0].databaseId')
[[ $run =~ ^[0-9]+$ ]] || { echo "no successful release.yml run for $tag" >&2; exit 1; }
work=$(mktemp -d "${TMPDIR:-/tmp}/veles-android-attach.XXXXXX")
trap 'rm -rf "$work/artifact"' EXIT
gh run download "$run" -R "$repo" -n android-unsigned -D "$work/artifact"
[[ $(cat "$work/artifact/SOURCE_COMMIT") == "$commit" ]] || {
    echo "android-unsigned of run $run was not built from $tag ($commit)" >&2; exit 1
}
bash scripts/release/payload-inventory.sh verify "$work/artifact"
sdk=$(nix build '.#android-sdk' --no-link --print-out-paths)
jdk=$(nix build 'nixpkgs#jdk17_headless' --no-link --print-out-paths)
VELES_BUILD_TOOLS=$(find -L "$sdk/libexec/android-sdk/build-tools" -mindepth 1 -maxdepth 1 -type d | sort -V | tail -1)
export VELES_BUILD_TOOLS PATH="$jdk/bin:$PATH"
bash scripts/release/sign-android.sh "$work/artifact"
mkdir -p "$work/attach"
cp "$work/artifact/payload/"*.apk "$work/artifact/payload/android-signing.json" "$work/attach/"
(cd "$work/attach" && sha256sum ./*.apk android-signing.json | sed 's# \./# #' > SHA256SUMS-android && cat SHA256SUMS-android)
gh release upload "$tag" -R "$repo" --clobber "$work/attach/"*
cp "$work/artifact/payload/"*.aab "$work/" 2>/dev/null || true
echo "attached the signed Android APKs to the $tag draft; the AAB (for Play) is in $work"
