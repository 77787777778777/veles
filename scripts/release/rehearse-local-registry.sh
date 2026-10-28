#!/usr/bin/env bash
# R10 rehearsal on a LOOPBACK registry only. Real OCI archives and Podman
# transfers, real bundle metadata, real APK/AAB verification with a disposable
# key. Cosign is stubbed: no GHCR, GitHub release, or Sigstore transaction.
set -euo pipefail
cd "$(dirname "$0")/../.."
sha=$(git rev-parse HEAD)
version=$(nix eval --raw '.#packages.x86_64-linux.default.version')
tag=v$version
[[ $sha =~ ^[0-9a-f]{40}$ ]]
work=$(mktemp -d "${TMPDIR:-/tmp}/veles-r10-registry.XXXXXX")
pids=()
cleanup() {
    local pid rc=$?
    for pid in "${pids[@]}"; do kill -TERM "$pid" 2>/dev/null || true; done
    if ((rc == 0)); then
        chmod -R u+w "$work/payload" "$work/unsigned" "$work/image-digests" 2>/dev/null || true
        if [[ -n ${VELES_R10_EVIDENCE:-} ]]; then
            mkdir -p "$VELES_R10_EVIDENCE"
            cp -a "$work/payload/." "$VELES_R10_EVIDENCE/"
            for name in source-input.sha256 cosign.log registry.log tamper.log; do
                [[ ! -f $work/$name ]] || cp "$work/$name" "$VELES_R10_EVIDENCE/$name"
            done
        fi
        chmod -R u+w "$work" 2>/dev/null || true
        rm -rf "$work"
    else
        echo "retained failed loopback fixture: $work" >&2
    fi
    # The loopback registry is a native, fixture-owned process; pids above
    # stop it. No system service, GHCR credential, or production store.
}
trap cleanup EXIT
for tool in podman jq curl sha256sum tar zstd java jarsigner keytool; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }; done
[[ -n ${VELES_BUILD_TOOLS:-} && -x $VELES_BUILD_TOOLS/apksigner ]] || { echo "VELES_BUILD_TOOLS is required" >&2; exit 2; }
input=${VELES_R10_INPUT_ROOT:?VELES_R10_INPUT_ROOT must name a pinned source inventory}
[[ -f $input/SOURCE_COMMIT && $(<"$input/SOURCE_COMMIT") == "$sha" &&
    -f $input/SOURCE_VERSION && $(<"$input/SOURCE_VERSION") == "$version" &&
    -f $input/DRAFT && $(<"$input/DRAFT") == 0 &&
    -d $input/payload && -d $input/unsigned ]] || {
    echo "VELES_R10_INPUT_ROOT is not a pinned inventory for $sha" >&2; exit 2
}
bash scripts/release/payload-inventory.sh verify "$input"
mkdir -p "$work/payload" "$work/unsigned" "$work/registry" "$work/bin"
printf '%s\n' "$sha" > "$work/SOURCE_COMMIT"
printf '%s\n' "$version" > "$work/SOURCE_VERSION"
printf '%s\n' 0 > "$work/DRAFT"
printf '%s\n' x86_64-linux aarch64-linux > "$work/SOURCE_SYSTEMS"
# Consume the exact source-matched payload. Do not rewrite the archive's
# revision, rebuild Android, or replace the x86 plugins with fixtures.
# Their independent distro/rebuild checks remain the execution evidence.
for name in "veles-$version-${sha:0:7}-linux-x86_64.tar.zst" \
    "veles-$version-${sha:0:7}-linux-x86_64.AppImage" \
    "veles-$version-${sha:0:7}-linux-x86_64.json" \
    "veles-gateway-$version-linux-amd64.tar.gz" \
    "veles-signer-$version-linux-amd64.tar.gz" \
    "veles-sandbox-$version-linux-amd64.oci.tar" \
    "veles-classifier-$version-linux-amd64.oci.tar"; do
    [[ -s $input/payload/$name ]] || { echo "missing exact input payload/$name" >&2; exit 1; }
    install -m 0644 "$input/payload/$name" "$work/payload/$name"
done
for cpu in x86_64 aarch64; do
    for plugin in echo atproto; do
        name="veles-plugin-$plugin-$version-linux-$cpu.tar.zst"
        if [[ -s $input/payload/$name ]]; then
            install -m 0644 "$input/payload/$name" "$work/payload/$name"
        elif [[ $cpu == aarch64 ]]; then
            echo "FIXTURE: no native arm64 $plugin archive; exercising inventory structure only"
            printf 'UNVERIFIED native arm64 plugin fixture %s\n' "$plugin" > "$work/payload/$name"
        else
            echo "missing exact input payload/$name" >&2; exit 1
        fi
    done
done
# Reuse actual native aarch64 artifacts when present. Otherwise keep the
# placeholders explicit and preserve this limitation in the final manifest.
if [[ -s $input/payload/veles-$version-${sha:0:7}-linux-aarch64.tar.zst &&
    -s $input/payload/veles-$version-${sha:0:7}-linux-aarch64.AppImage &&
    -s $input/payload/veles-$version-${sha:0:7}-linux-aarch64.json ]]; then
    for suffix in tar.zst AppImage json; do
        name="veles-$version-${sha:0:7}-linux-aarch64.$suffix"
        install -m 0644 "$input/payload/$name" "$work/payload/$name"
    done
else
    echo "FIXTURE: no successful native aarch64 artifacts; exercising only inventory guards"
    mkdir -p "$work/arm"
    jq -cn --arg version "$version" --arg revision "${sha:0:7}" '{name:"veles",version:$version,revision:$revision,architecture:"aarch64"}' > "$work/arm/BUNDLE-INFO.json"
    printf '#!/bin/sh\necho unverified arm64 fixture\n' > "$work/arm/veles"
    chmod +x "$work/arm/veles"
    tar --zstd -cf "$work/payload/veles-$version-${sha:0:7}-linux-aarch64.tar.zst" -C "$work/arm" .
    printf 'UNVERIFIED aarch64 AppImage fixture\n' > "$work/payload/veles-$version-${sha:0:7}-linux-aarch64.AppImage"
    install -m 0644 "$work/arm/BUNDLE-INFO.json" "$work/payload/veles-$version-${sha:0:7}-linux-aarch64.json"
fi
(($(find "$work/unsigned" -maxdepth 1 -type f | wc -l) == 0))
for name in "veles-$version-android-arm64-v8a-release-unsigned.apk" \
    "veles-userland-$version-android-arm64-v8a-release-unsigned.apk" \
    "veles-$version-android-arm64-v8a-release-unsigned.aab"; do
    [[ -s $input/unsigned/$name ]] || { echo "missing exact input unsigned/$name" >&2; exit 1; }
    install -m 0644 "$input/unsigned/$name" "$work/unsigned/$name"
done
# Independent input hashes survive the rehearsal. They are not signing
# claims, and the fixture-only arm64 rows must be read with that limit.
cp "$input/PREPUBLISH-SHA256SUMS" "$work/source-input.sha256"
keytool -genkeypair -keystore "$work/rehearsal.p12" -storetype PKCS12 \
    -alias rehearsal-upload -keyalg RSA -keysize 2048 -validity 2 \
    -storepass rehearsal-pass -keypass rehearsal-pass \
    -dname 'CN=Veles disposable rehearsal' >/dev/null 2>&1
ANDROID_UPLOAD_KEYSTORE="$work/rehearsal.p12" ANDROID_UPLOAD_STOREPASS=rehearsal-pass \
    ANDROID_UPLOAD_KEYPASS=rehearsal-pass ANDROID_UPLOAD_ALIAS=rehearsal-upload \
    bash scripts/release/sign-android.sh "$work"
# Exercise a real checksum failure before the first registry write, then
# restore the original signed bytes and inventory for the positive path.
cp "$work/payload/veles-$version-android-arm64-v8a-release.apk" "$work/original.apk"
cp "$work/PREPUBLISH-SHA256SUMS" "$work/original.checksums"
printf 'tampered before publish\n' >> "$work/payload/veles-$version-android-arm64-v8a-release.apk"
if PATH="$work/bin:$PATH" bash scripts/release/preflight-images.sh "$work" 77787777778777/veles "$sha" "$tag" > "$work/tamper.log" 2>&1; then
    echo "tampered payload must not reach a registry write" >&2; exit 1
fi
grep -F 'payload differs from PREPUBLISH-SHA256SUMS' "$work/tamper.log"
cp "$work/original.apk" "$work/payload/veles-$version-android-arm64-v8a-release.apk"
cp "$work/original.checksums" "$work/PREPUBLISH-SHA256SUMS"
bash scripts/release/payload-inventory.sh verify "$work"
# The pinned nixpkgs Distribution registry owns only the scratch directory
# and a high loopback port. No host registry or credentials are used.
registry_bin=$(nix build 'nixpkgs#distribution' --no-link --print-out-paths)/bin/registry
cat > "$work/registry.yml" <<YAML
version: 0.1
log:
  level: error
http:
  addr: 127.0.0.1:15931
storage:
  filesystem:
    rootdirectory: $work/registry
YAML
"$registry_bin" serve "$work/registry.yml" > "$work/registry.log" 2>&1 &
pids+=("$!")
for _ in $(seq 1 100); do
    curl -fsS --max-time 2 http://127.0.0.1:15931/v2/ >/dev/null 2>&1 && break
    sleep 0.1
done
curl -fsS --max-time 5 http://127.0.0.1:15931/v2/ >/dev/null
cat > "$work/bin/cosign" <<'COSIGN'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "${FAKE_COSIGN_LOG:?}"
[[ $1 == sign ]] || {
    [[ $1 == verify ]]
    args=" $* "
    [[ $args == *" --certificate-identity https://github.com/77787777778777/veles/.github/workflows/release.yml@refs/tags/${EXPECTED_TAG:?} "* ]]
    [[ $args == *" --certificate-oidc-issuer https://token.actions.githubusercontent.com "* ]]
    [[ $args == *" --certificate-github-workflow-ref refs/tags/${EXPECTED_TAG} "* ]]
    [[ $args == *" --certificate-github-workflow-sha ${EXPECTED_SHA:?} "* ]]
}
COSIGN
chmod +x "$work/bin/cosign"
export FAKE_COSIGN_LOG="$work/cosign.log" EXPECTED_TAG="$tag" EXPECTED_SHA="$sha"
export GITHUB_ACTIONS=true GITHUB_EVENT_NAME=push GITHUB_REF="refs/tags/$tag"
export GITHUB_REF_NAME="$tag" GITHUB_SHA="$sha" GITHUB_REPOSITORY=77787777778777/veles
export VELES_RELEASE_WORKFLOW_SHA="$sha"
export ACTIONS_ID_TOKEN_REQUEST_URL=placeholder ACTIONS_ID_TOKEN_REQUEST_TOKEN=placeholder
export VELES_RELEASE_REGISTRY=127.0.0.1:15931 VELES_RELEASE_REGISTRY_INSECURE=1
# The registry evidence names this LOCAL endpoint; the production default
# remains ghcr.io. These digests are not claimed to exist at GHCR.
export VELES_RELEASE_EVIDENCE_REGISTRY=127.0.0.1:15931 VELES_RELEASE_REHEARSAL=1
export PATH="$work/bin:$PATH"
bash scripts/release/publish-images.sh "$work" 77787777778777/veles "$sha" "$tag"
(($(jq -s 'length' "$work/payload/oci-image-digests.txt") == 4))
while read -r image name; do
    name=${name#127.0.0.1:15931/}
    digest=${image##*@}
    evidence=${name//\//_}
    curl -fsS --max-time 15 -D "$work/$evidence.headers" -o "$work/$evidence.remote.json" \
        -H 'Accept: application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json' \
        "http://127.0.0.1:15931/v2/$name/manifests/$digest"
    remote=$(sed -n 's/^[Dd]ocker-[Cc]ontent-[Dd]igest: *//p' "$work/$evidence.headers" | tr -d '\r')
    [[ $remote == "$digest" ]] || { echo "registry content digest differs: $name $remote $digest" >&2; exit 1; }
done < <(jq -r '[.image, (.image | capture("^127[.]0[.]0[.]1:15931/(?<name>.*)@sha256:[0-9a-f]{64}$").name)] | @tsv' "$work/payload/oci-image-digests.txt")
VELES_RELEASE_REPOSITORY=77787777778777/veles VELES_RELEASE_TAG="$tag" bash scripts/release/manifest.sh "$work"
if [[ ! -s $input/payload/veles-$version-${sha:0:7}-linux-aarch64.tar.zst ]]; then
    jq '.limits += ["REHEARSAL ONLY: native arm64 archive, AppImage, and plugins are unverified placeholders."]' \
        "$work/payload/release-manifest.json" > "$work/payload/release-manifest.json.new"
    mv "$work/payload/release-manifest.json.new" "$work/payload/release-manifest.json"
    digest=$(sha256sum "$work/payload/release-manifest.json" | cut -d' ' -f1)
    sed -i "s/^[0-9a-f]\\{64\\}  release-manifest.json$/$digest  release-manifest.json/" \
        "$work/payload/SHA256SUMS"
    (cd "$work/payload" && sha256sum -c SHA256SUMS >/dev/null)
fi
bash scripts/release/verify-inventory.sh "$work" 77787777778777/veles "$sha" "$tag"
# Replace the registry-only stub with the combined, argument-validating
# file-signing stub used by the general offline rehearsal.
export FAKE_EXPECTED_SHA="$sha"
cat > "$work/bin/cosign" <<'ALL'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "${FAKE_COSIGN_LOG:?}"
if [[ $1 == sign-blob ]]; then
    [[ $2 == --bundle && $4 == --oidc-provider && $5 == github-actions ]]
    printf '{"fixture":true}\n' > "$3"
    exit 0
fi
[[ $1 == verify-blob ]]
args=" $* "
[[ $args == *" --certificate-identity https://github.com/77787777778777/veles/.github/workflows/release.yml@refs/tags/${EXPECTED_TAG:?} "* ]]
[[ $args == *" --certificate-github-workflow-ref refs/tags/${EXPECTED_TAG} "* ]]
[[ $args == *" --certificate-github-workflow-sha ${EXPECTED_SHA:?} "* ]]
ALL
chmod +x "$work/bin/cosign"
bash scripts/release/sign-artifacts.sh "$work/payload" 77787777778777/veles "$sha" "$tag"
(($(find "$work/payload" -maxdepth 1 -name '*.sigstore.json' | wc -l) == 21))
# A wrong workflow SHA must not be accepted by the verification wiring.
cat > "$work/bin/cosign" <<'FAIL'
#!/usr/bin/env bash
exit 1
FAIL
chmod +x "$work/bin/cosign"
if bash scripts/release/sign-artifacts.sh "$work/payload" 77787777778777/veles "$sha" "$tag"; then
    echo "a second signing run must not silently overwrite evidence" >&2; exit 1
fi
echo "LOOPBACK_REGISTRY_REHEARSAL_PASS"
echo "real archive loads/pushes and digest inventory: PASS"
echo "Sigstore calls: STUBBED, not a signature or remote identity proof"
echo "retained evidence: $work"
