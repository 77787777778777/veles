#!/usr/bin/env bash
# Fail before any upload if this is not the tag-push release identity, or
# if a required Android upload-key/OIDC credential is absent. Never print
# a credential. This does not generate or discover an Android key.
set -euo pipefail
repo=${1:?usage: require-release-credentials.sh OWNER/REPO SHA TAG}
sha=${2:?commit SHA required}
tag=${3:?release tag required}
[[ $repo =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || { echo "bad repository" >&2; exit 2; }
[[ $sha =~ ^[0-9a-f]{40}$ ]] || { echo "bad commit" >&2; exit 2; }
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$ ]] || { echo "bad tag" >&2; exit 2; }
[[ ${GITHUB_ACTIONS:-} == true && ${GITHUB_EVENT_NAME:-} == push ]] || {
    echo "only a tag-push workflow has the required signing identity" >&2; exit 2
}
[[ ${GITHUB_REF:-} == "refs/tags/$tag" && ${GITHUB_REF_NAME:-} == "$tag" ]] || {
    echo "workflow ref is not the requested release tag" >&2; exit 2
}
[[ ${GITHUB_REPOSITORY:-} == "$repo" ]] || {
    echo "repository identity mismatch" >&2; exit 2
}
# A lightweight tag and an annotated tag can give the push event different
# object IDs. Bind the certificate to the event's object ID while separately
# checking that this object still resolves to the authenticated source SHA.
[[ ${GITHUB_SHA:-} =~ ^[0-9a-f]{40}$ ]] || { echo "missing workflow object SHA" >&2; exit 2; }
printf 'VELES_RELEASE_WORKFLOW_SHA=%s\n' "$GITHUB_SHA" >> "${GITHUB_ENV:-/dev/null}"
if [[ ${VELES_RELEASE_CHECK_TAG_OBJECT:-1} == 1 ]]; then
    [[ -n ${GH_TOKEN:-}${GITHUB_TOKEN:-} ]] || { echo "GitHub token is required to authenticate the tag object" >&2; exit 1; }
    command -v gh >/dev/null || { echo "gh is required to authenticate the tag object" >&2; exit 1; }
    ref=$(gh api "repos/$repo/git/ref/tags/$tag")
    object_sha=$(jq -r '.object.sha' <<<"$ref")
    kind=$(jq -r '.object.type' <<<"$ref")
    [[ $object_sha == "$GITHUB_SHA" ]] || { echo "tag object differs from the workflow event" >&2; exit 1; }
    while [[ $kind == tag ]]; do
        object=$(gh api "repos/$repo/git/tags/$object_sha")
        object_sha=$(jq -r '.object.sha' <<<"$object")
        kind=$(jq -r '.object.type' <<<"$object")
    done
    [[ $kind == commit && $object_sha == "$sha" ]] || {
        echo "tag object does not resolve to the authenticated release commit" >&2; exit 1
    }
fi
required=(
    ANDROID_UPLOAD_KEYSTORE_B64
    ANDROID_UPLOAD_STOREPASS
    ANDROID_UPLOAD_KEYPASS
    ANDROID_UPLOAD_ALIAS
    ACTIONS_ID_TOKEN_REQUEST_URL
    ACTIONS_ID_TOKEN_REQUEST_TOKEN
)
for name in "${required[@]}"; do
    [[ -n ${!name:-} ]] || { echo "$name is missing from the protected release environment" >&2; exit 1; }
done
[[ $ANDROID_UPLOAD_ALIAS != sideload ]] || { echo "sideload is not an upload key" >&2; exit 2; }
bytes=$(printf '%s' "$ANDROID_UPLOAD_KEYSTORE_B64" | base64 --decode | wc -c)
((bytes > 0)) || { echo "Android upload keystore decodes to an empty file" >&2; exit 1; }
echo "PASS: tag identity, OIDC, and explicit Android upload-key inputs are present"
