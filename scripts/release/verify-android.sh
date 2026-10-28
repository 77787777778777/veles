#!/usr/bin/env bash
# Verify the signed Android BYTES against their public evidence. This runs
# before the first registry write and again before file signing, so a
# changed APK/AAB cannot inherit an old certificate record.
set -euo pipefail
out=${1:?usage: verify-android.sh ARTIFACT_DIR}
[[ $out = /* ]] || out=$PWD/$out
payload=$out/payload
evidence=$payload/android-signing.json
[[ -s $evidence ]] || { echo "missing Android signing evidence" >&2; exit 1; }
for tool in java jarsigner keytool jq; do
    command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }
done
[[ -n ${VELES_BUILD_TOOLS:-} && -x $VELES_BUILD_TOOLS/apksigner ]] || {
    echo "VELES_BUILD_TOOLS with apksigner is required" >&2; exit 2
}
expected=$(jq -er '.certificateSha256' "$evidence")
[[ $expected =~ ^[0-9a-f]{64}$ ]] || { echo "invalid Android evidence certificate" >&2; exit 1; }
mapfile -t packages < <(jq -r '.packages[].artifact' "$evidence")
((${#packages[@]} == 3)) || { echo "expected three signed Android packages" >&2; exit 1; }
work=$(mktemp -d "${TMPDIR:-/tmp}/veles-android-verify.XXXXXX")
trap 'rm -rf "$work"' EXIT
for name in "${packages[@]}"; do
    [[ $name =~ ^[A-Za-z0-9._-]+$ && -s $payload/$name ]] || {
        echo "invalid signed Android package path: $name" >&2; exit 1
    }
    fingerprint=
    case $name in
        *.apk)
            if ! "$VELES_BUILD_TOOLS/apksigner" verify --verbose --print-certs "$payload/$name" > "$work/$name.log" 2>&1; then
                cat "$work/$name.log" >&2
                echo "$name failed APK signature verification" >&2
                exit 1
            fi
            fingerprint=$(sed -n 's/^Signer #1 certificate SHA-256 digest: //p' "$work/$name.log")
            grep -q 'Verified using v2 scheme (APK Signature Scheme v2): true' "$work/$name.log" || {
                echo "$name has no verified APK v2 signature" >&2; exit 1
            }
            grep -q 'Verified using v3 scheme (APK Signature Scheme v3): true' "$work/$name.log" || {
                echo "$name has no verified APK v3 signature" >&2; exit 1
            }
            ;;
        *.aab)
            jarsigner -verify -verbose -certs "$payload/$name" > "$work/$name.log" 2>&1
            grep -q 'jar verified\.' "$work/$name.log" || {
                echo "$name has no verified JAR signature" >&2; exit 1
            }
            keytool -printcert -jarfile "$payload/$name" > "$work/$name.cert" 2>&1
            mapfile -t certificates < <(sed -n 's/^[[:space:]]*SHA256: //p' "$work/$name.cert" | tr -d ':' | tr 'A-F' 'a-f')
            ((${#certificates[@]} == 1)) || { echo "$name must have one signer certificate" >&2; exit 1; }
            fingerprint=${certificates[0]}
            ;;
        *) echo "unexpected Android package $name" >&2; exit 1 ;;
    esac
    [[ $fingerprint == "$expected" ]] || {
        echo "$name signer differs from the release evidence" >&2; exit 1
    }
done
echo "PASS: actual APK and AAB signatures match the release evidence"
