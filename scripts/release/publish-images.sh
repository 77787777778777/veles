#!/usr/bin/env bash
# Publish the four amd64 OCI images and sign each registry CONTENT DIGEST.
# Production writes to GHCR; the only supported non-production destination
# is an explicit loopback rehearsal with cosign stubbed by the fixture.
# Do not describe the matrix as multi-architecture: only the x86_64 lane
# produces images in R10 (the sandbox's Chrome/toolchain image is amd64).
set -euo pipefail
out=${1:?usage: publish-images.sh ARTIFACT_DIR OWNER/REPO SHA TAG}
repo=${2:?owner/repo required}
sha=${3:?commit SHA required}
tag=${4:?release tag required}
[[ $out = /* ]] || out=$PWD/$out
[[ $repo =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || { echo "bad repository" >&2; exit 2; }
[[ $sha =~ ^[0-9a-f]{40}$ ]] || { echo "bad commit" >&2; exit 2; }
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$ ]] || { echo "bad tag" >&2; exit 2; }
[[ ${GITHUB_ACTIONS:-} == true && ${GITHUB_EVENT_NAME:-} == push ]] || {
    echo "registry signing requires a tag-push GitHub workflow" >&2; exit 2
}
[[ ${GITHUB_REPOSITORY:-} == "$repo" ]] || {
    echo "registry signing repository does not match this run" >&2; exit 2
}
[[ ${VELES_RELEASE_WORKFLOW_SHA:-} =~ ^[0-9a-f]{40}$ ]] || {
    echo "VELES_RELEASE_WORKFLOW_SHA is required for the certificate identity" >&2; exit 2
}
[[ ${GITHUB_REF:-} == "refs/tags/$tag" ]] || {
    echo "registry signing ref does not match the release tag" >&2; exit 2
}
[[ -n ${ACTIONS_ID_TOKEN_REQUEST_URL:-} && -n ${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-} ]] || {
    echo "GitHub Actions OIDC is required for registry signing" >&2; exit 2
}
[[ $(cat "$out/SOURCE_COMMIT") == "$sha" ]] || {
    echo "registry artifacts are not from the requested source commit" >&2; exit 1
}
[[ $(cat "$out/DRAFT") == 0 ]] || { echo "refusing to publish draft artifacts" >&2; exit 2; }
for tool in podman jq sha256sum tar zstd; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }; done
# Complete every offline guard before the FIRST external write. In
# particular, do not push two images and only then discover a missing APK,
# architecture lane, or mislabelled third image.
bash "$(dirname "$0")/preflight-images.sh" "$out" "$repo" "$sha" "$tag"
command -v cosign >/dev/null || { echo "cosign is required" >&2; exit 1; }
export COSIGN_YES=true
identity="https://github.com/$repo/.github/workflows/release.yml@refs/tags/$tag"
issuer=https://token.actions.githubusercontent.com
owner=${repo%/*}
registry=${VELES_RELEASE_REGISTRY:-ghcr.io}/${owner,,}
[[ $registry =~ ^[A-Za-z0-9.-]+(:[0-9]+)?/[a-z0-9_.-]+$ ]] || { echo "invalid release registry" >&2; exit 2; }
push_options=()
if [[ ${VELES_RELEASE_REGISTRY_INSECURE:-0} == 1 ]]; then
    [[ $registry =~ ^(localhost|127\.0\.0\.1):[0-9]+/[a-z0-9_.-]+$ ]] || {
        echo "insecure registry rehearsal is loopback-only" >&2; exit 2
    }
    push_options=(--tls-verify=false)
elif [[ ${VELES_RELEASE_REGISTRY_INSECURE:-0} != 0 ]]; then
    echo "VELES_RELEASE_REGISTRY_INSECURE must be 0 or 1" >&2; exit 2
fi
# Refuse to replace an earlier signing attempt silently. The caller must
# inspect/reconcile any partial publication rather than clearing its evidence.
[[ ! -e $out/payload/oci-image-digests.txt ]] || { echo "refusing to overwrite an existing image digest inventory" >&2; exit 1; }
if [[ -d $out/image-digests ]] && find "$out/image-digests" -type f -print -quit | grep -q .; then
    echo "existing image digest files need an explicit recovery review" >&2; exit 1
fi
mkdir -p "$out/image-digests"
: > "$out/payload/oci-image-digests.txt"

for pair in gateway:agent signer:signer sandbox:sandbox classifier:classifier; do
    kind=${pair%%:*}; image=${pair#*:}
    name="$registry/veles-$image"
    mapfile -t archives < <(find "$out/payload" -maxdepth 1 -type f \
        \( -name "veles-$kind-*-linux-amd64.tar.gz" -o -name "veles-$kind-*-linux-amd64.oci.tar" \) -print)
    ((${#archives[@]} == 1)) || { echo "expected one amd64 image archive for $kind, got ${#archives[@]}" >&2; exit 1; }
    archive=${archives[0]}
    expected=$(sha256sum "$archive" | cut -d' ' -f1)
    # Podman 5.8 has no `load --format json`. Parse its documented
    # human-readable Loaded image line and verify the actual loaded ID.
    load_output=$(podman load --input "$archive")
    loaded=$(printf '%s\n' "$load_output" | sed -n 's/^Loaded image: //p')
    [[ -n $loaded && $(printf '%s\n' "$loaded" | wc -l) -eq 1 ]] || {
        echo "could not identify the single loaded $kind image" >&2; exit 1
    }
    podman image inspect "$loaded" --format '{{.Os}}/{{.Architecture}}' | grep -qx 'linux/amd64'
    expected_archive="veles-$kind-$(<"$out/SOURCE_VERSION")-linux-amd64"
    [[ $kind == gateway || $kind == signer ]] && expected_archive+=.tar.gz || expected_archive+=.oci.tar
    [[ $(basename "$archive") == "$expected_archive" ]] || { echo "unexpected $kind archive name" >&2; exit 1; }
    revision=$(podman image inspect "$loaded" --format '{{index .Config.Labels "org.opencontainers.image.revision"}}')
    source=$(podman image inspect "$loaded" --format '{{index .Config.Labels "org.opencontainers.image.source"}}')
    title=$(podman image inspect "$loaded" --format '{{index .Config.Labels "org.opencontainers.image.title"}}')
    [[ $revision == "$sha" && $source == "https://github.com/$repo" && $title == "veles-$kind" ]] || {
        echo "$kind archive has the wrong release identity" >&2; exit 1
    }
    destination="docker://$name:$tag"
    # This is a single image, not a manifest list. Push its complete OCI
    # manifest and layers; sign the registry's returned content digest,
    # never a mutable tag. Podman 5.8 has no push --all option.
    podman push --format oci "${push_options[@]}" --digestfile "$out/image-digests/$kind.digest" "$loaded" "$destination"
    digest=$(cat "$out/image-digests/$kind.digest")
    [[ $digest == sha256:* ]]
    ref="$name@$digest"
    cosign sign --oidc-provider github-actions "$ref"
    cosign verify --certificate-identity "$identity" \
        --certificate-oidc-issuer "$issuer" \
        --certificate-github-workflow-ref "refs/tags/$tag" \
        --certificate-github-workflow-sha "$VELES_RELEASE_WORKFLOW_SHA" "$ref"
    jq -cn --arg kind "$kind" --arg ref "$ref" --arg archive "$(basename "$archive")" --arg sha256 "$expected" --arg sha "$sha" \
        '{kind:$kind,image:$ref,archive:$archive,archiveSha256:$sha256,sourceCommit:$sha,platform:"linux/amd64"}' \
        >> "$out/payload/oci-image-digests.txt"
done
echo "PASS pushed and verified all four amd64 OCI images"
