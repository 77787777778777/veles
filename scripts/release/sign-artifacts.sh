#!/usr/bin/env bash
# Sign and verify every release file using GitHub's Sigstore OIDC identity.
# This is intentionally CI-only: a local browser login is not the release
# workflow identity and must not be presented as such.
set -euo pipefail
out=${1:?usage: sign-artifacts.sh ARTIFACT_DIR OWNER/REPO SHA TAG}
repo=${2:?owner/repo required}
sha=${3:?commit SHA required}
tag=${4:?release tag required}
[[ $repo =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || { echo "bad repository" >&2; exit 2; }
[[ $sha =~ ^[0-9a-f]{40}$ ]] || { echo "bad commit" >&2; exit 2; }
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$ ]] || { echo "bad tag" >&2; exit 2; }
[[ ${GITHUB_ACTIONS:-} == true && -n ${ACTIONS_ID_TOKEN_REQUEST_URL:-} && -n ${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-} ]] || {
    echo "GitHub Actions OIDC is required; local signing is not release signing" >&2; exit 2
}
[[ ${VELES_RELEASE_WORKFLOW_SHA:-} =~ ^[0-9a-f]{40}$ ]] || {
    echo "VELES_RELEASE_WORKFLOW_SHA is required for certificate verification" >&2; exit 2
}
for tool in realpath jq sha256sum tar zstd; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }; done
out=$(realpath "$out")
jq -e --arg sha "$sha" --arg repo "$repo" --arg tag "$tag" \
    '.commit == $sha and .repository == $repo and .tag == $tag' \
    "$out/release-manifest.json" >/dev/null || { echo "manifest source/repository/tag mismatch" >&2; exit 1; }
[[ ${GITHUB_EVENT_NAME:-} == push && ${GITHUB_REF:-} == "refs/tags/$tag" && ${GITHUB_REPOSITORY:-} == "$repo" ]] || {
    echo "file signing requires the matching tag-push workflow identity" >&2; exit 2
}
# Re-read the payload before signing. An attacker cannot substitute bytes,
# drop a platform, or retain an old registry digest beside a new manifest.
bash "$(dirname "$0")/verify-inventory.sh" "$(dirname "$out")" "$repo" "$sha" "$tag"
bash "$(dirname "$0")/verify-android.sh" "$(dirname "$out")"
command -v cosign >/dev/null || { echo "cosign is required" >&2; exit 1; }
if find "$out" -maxdepth 1 -type f -name '*unsigned*' -print -quit | grep -q .; then
    echo "refusing to sign an unsigned Android intermediate" >&2; exit 1
fi
ref="refs/tags/$tag"
identity="https://github.com/$repo/.github/workflows/release.yml@$ref"
issuer=https://token.actions.githubusercontent.com
export COSIGN_YES=true
mapfile -t files < <(find "$out" -maxdepth 1 -type f \
    ! -name 'SHA256SUMS' ! -name '*.sigstore.json' -printf '%f\n' | LC_ALL=C sort)
((${#files[@]})) || { echo "no release files in $out" >&2; exit 1; }
for f in "${files[@]}"; do [[ -s $out/$f ]] || { echo "empty release file: $f" >&2; exit 1; }; done
if [[ -f $out/SHA256SUMS ]]; then (cd "$out" && sha256sum -c SHA256SUMS); fi
(cd "$out" && sha256sum "${files[@]}" > SHA256SUMS)
files+=(SHA256SUMS)
for f in "${files[@]}"; do
    echo "signing $f"
    cosign sign-blob --bundle "$out/$f.sigstore.json" --oidc-provider github-actions "$out/$f"
    cosign verify-blob --bundle "$out/$f.sigstore.json" \
        --certificate-identity "$identity" --certificate-oidc-issuer "$issuer" \
        --certificate-github-workflow-ref "$ref" --certificate-github-workflow-sha "$VELES_RELEASE_WORKFLOW_SHA" "$out/$f"
done
(cd "$out" && sha256sum -c SHA256SUMS)
echo "PASS signed and verified ${#files[@]} files as $identity ($ref)"
