#!/usr/bin/env bash
# Sign the gateway APK, matched userland APK, and gateway AAB with an
# EXPLICIT upload key. Never discover/create/reuse the sideload key.
# Public evidence contains certificate fingerprints, never key material.
set -euo pipefail
out=${1:?usage: sign-android.sh ARTIFACT_DIR}
[[ $out = /* ]] || out=$PWD/$out
for var in VELES_BUILD_TOOLS ANDROID_UPLOAD_KEYSTORE ANDROID_UPLOAD_STOREPASS ANDROID_UPLOAD_KEYPASS ANDROID_UPLOAD_ALIAS; do
    [[ -n ${!var:-} ]] || { echo "$var is required for Android release signing" >&2; exit 2; }
done
[[ $ANDROID_UPLOAD_KEYSTORE = /* && -f $ANDROID_UPLOAD_KEYSTORE ]] || {
    echo "upload keystore must be an absolute existing file path" >&2; exit 2
}
# The sideload key (scripts/android-install.sh) signs the APK already on the
# owner's phone; Android only upgrades in place under the same certificate.
# Using it for a release is an explicit owner decision, never a default.
if [[ ${VELES_ANDROID_ALLOW_SIDELOAD_KEY:-0} != 1 ]]; then
    [[ $ANDROID_UPLOAD_KEYSTORE != */sideload.p12 ]] || { echo "sideload is not a release upload key (VELES_ANDROID_ALLOW_SIDELOAD_KEY=1 to choose it deliberately)" >&2; exit 2; }
    [[ $ANDROID_UPLOAD_ALIAS != sideload ]] || { echo "refusing sideload signing identity (VELES_ANDROID_ALLOW_SIDELOAD_KEY=1 to choose it deliberately)" >&2; exit 2; }
fi
[[ -x $VELES_BUILD_TOOLS/apksigner ]] || { echo "apksigner missing from VELES_BUILD_TOOLS" >&2; exit 1; }
for tool in java jarsigner keytool jq; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }; done
[[ -d $out/unsigned && -d $out/payload ]] || { echo "expected unsigned/ and payload/" >&2; exit 1; }
mapfile -t inputs < <(find "$out/unsigned" -maxdepth 1 -type f -name '*-release-unsigned.apk' -print | LC_ALL=C sort)
((${#inputs[@]} == 2)) || { echo "expected gateway AND matched userland APKs, got ${#inputs[@]}" >&2; exit 1; }
mapfile -t aabs < <(find "$out/unsigned" -maxdepth 1 -type f -name '*-release-unsigned.aab' -print)
((${#aabs[@]} == 1)) || { echo "expected one gateway AAB, got ${#aabs[@]}" >&2; exit 1; }
work=$(mktemp -d "${TMPDIR:-/tmp}/veles-android-sign.XXXXXX")
trap 'rm -rf "$work"' EXIT
chmod 700 "$work"
for var in ANDROID_UPLOAD_STOREPASS ANDROID_UPLOAD_KEYPASS; do
    printf '%s' "${!var}" > "$work/$var.pass"
    chmod 600 "$work/$var.pass"
done
: > "$work/signers"
: > "$work/evidence"
record_fingerprint() {
    local fingerprint=$1
    [[ $fingerprint =~ ^[0-9a-f]{64}$ ]] || { echo "invalid signer fingerprint" >&2; exit 1; }
    if [[ -s $work/signers ]] && [[ $(head -1 "$work/signers") != "$fingerprint" ]]; then
        echo "gateway, userland, and AAB must use the SAME upload certificate" >&2; exit 1
    fi
    printf '%s\n' "$fingerprint" >> "$work/signers"
}
for input in "${inputs[@]}"; do
    name=$(basename "$input" -unsigned.apk).apk
    tmp=$work/$name
    "$VELES_BUILD_TOOLS/apksigner" sign \
        --ks "$ANDROID_UPLOAD_KEYSTORE" \
        --ks-pass "file:$work/ANDROID_UPLOAD_STOREPASS.pass" \
        --key-pass "file:$work/ANDROID_UPLOAD_KEYPASS.pass" \
        --ks-key-alias "$ANDROID_UPLOAD_ALIAS" \
        --v1-signing-enabled true --v2-signing-enabled true \
        --v3-signing-enabled true --out "$tmp" "$input"
    "$VELES_BUILD_TOOLS/apksigner" verify --verbose --print-certs "$tmp" > "$work/$name.verify"
    cat "$work/$name.verify"
    fingerprint=$(sed -n 's/^Signer #1 certificate SHA-256 digest: //p' "$work/$name.verify")
    record_fingerprint "$fingerprint"
    grep -q 'Verified using v2 scheme (APK Signature Scheme v2): true' "$work/$name.verify"
    grep -q 'Verified using v3 scheme (APK Signature Scheme v3): true' "$work/$name.verify"
    v1=$(sed -n 's/^Verified using v1 scheme (JAR signing): //p' "$work/$name.verify")
    [[ $v1 == true || $v1 == false ]] || { echo "unknown v1 result for $name" >&2; exit 1; }
    jq -cn --arg name "$name" --arg fingerprint "$fingerprint" --argjson v1 "$v1" \
        '{artifact:$name,certificateSha256:$fingerprint,verifiedSchemes:{v1:$v1,v2:true,v3:true}}' >> "$work/evidence"
    install -m 0644 "$tmp" "$out/payload/$name"
    rm -f "$input"
    echo "signed and verified Android artifact: $name"
done
input=${aabs[0]}
name=$(basename "$input" -unsigned.aab).aab
tmp=$work/$name
cp "$input" "$tmp"
jarsigner -keystore "$ANDROID_UPLOAD_KEYSTORE" -storetype PKCS12 \
    -storepass:file "$work/ANDROID_UPLOAD_STOREPASS.pass" \
    -keypass:file "$work/ANDROID_UPLOAD_KEYPASS.pass" \
    "$tmp" "$ANDROID_UPLOAD_ALIAS" > "$work/$name.sign" 2>&1
jarsigner -verify -verbose -certs "$tmp" > "$work/$name.verify" 2>&1
cat "$work/$name.verify"
grep -q 'jar verified\.' "$work/$name.verify"
keytool -printcert -jarfile "$tmp" > "$work/$name.cert" 2>&1
mapfile -t fingerprints < <(sed -n 's/^[[:space:]]*SHA256: //p' "$work/$name.cert" | tr -d ':' | tr 'A-F' 'a-f')
((${#fingerprints[@]} == 1)) || { echo "expected one AAB signer certificate" >&2; exit 1; }
record_fingerprint "${fingerprints[0]}"
jq -cn --arg name "$name" --arg fingerprint "${fingerprints[0]}" \
    '{artifact:$name,certificateSha256:$fingerprint,verifiedSchemes:{jar:true}}' >> "$work/evidence"
install -m 0644 "$tmp" "$out/payload/$name"
rm -f "$input"
jq -n --slurpfile artifacts "$work/evidence" \
    '{schemaVersion:1,kind:"android-upload-key",certificateSha256:$artifacts[0].certificateSha256,
      certificateSource:"configured release upload key",packages:$artifacts}' > "$out/payload/android-signing.json"
bash "$(dirname "$0")/payload-inventory.sh" seal "$out"
echo "PASS: both APKs and the AAB signed by certificate $(head -1 "$work/signers")"
