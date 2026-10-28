#!/usr/bin/env bash
# Merge the three CI artifact uploads without letting same-named metadata
# from parallel jobs overwrite a different source or platform. The caller
# supplies the already-authenticated tag commit and package version.
set -euo pipefail
out=${1:?usage: assemble-artifacts.sh OUT SHA VERSION LANE_DIR...}
sha=${2:?expected tag commit required}
version=${3:?expected package version required}
[[ $sha =~ ^[0-9a-f]{40}$ ]] || { echo "bad commit" >&2; exit 2; }
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "bad version" >&2; exit 2; }
shift 3
(($# == 3)) || { echo "expected x86_64 Linux, aarch64 Linux, and Android lanes" >&2; exit 2; }
[[ $out = /* ]] || out=$PWD/$out
if [[ -e $out ]] && find "$out" -type f -print -quit | grep -q .; then
    echo "refusing to merge into a nonempty inventory: $out" >&2; exit 1
fi
mkdir -p "$out/payload" "$out/unsigned"
declare -A seen=()
for lane in "$@"; do
    name=$(basename "$lane")
    case $name in
        release-x86_64-linux) system=x86_64-linux ;;
        release-aarch64-linux) system=aarch64-linux ;;
        release-android) system=x86_64-linux ;;
        *) echo "unexpected artifact lane $name" >&2; exit 1 ;;
    esac
    [[ -z ${seen[$name]:-} && -d $lane/payload && -d $lane/unsigned ]] || {
        echo "missing or duplicate lane $name" >&2; exit 1
    }
    seen[$name]=1
    for pair in "SOURCE_COMMIT:$sha" "SOURCE_VERSION:$version" "SOURCE_SYSTEM:$system" "SOURCE_FLAKE_REF:." "DRAFT:0"; do
        key=${pair%%:*}; expected=${pair#*:}
        [[ -f $lane/$key && $(cat "$lane/$key") == "$expected" ]] || {
            echo "$name/$key is not $expected" >&2; exit 1
        }
    done
    if [[ $name != release-android ]] && find "$lane/unsigned" -type f -print -quit | grep -q .; then
        echo "unexpected Android intermediates in $name" >&2; exit 1
    fi
    bash "$(dirname "$0")/payload-inventory.sh" verify "$lane"
    if find "$lane/payload" -type f -name '*unsigned*' -print -quit | grep -q .; then
        echo "unsigned artifact in $name payload" >&2; exit 1
    fi
    for kind in payload unsigned; do
        while IFS= read -r -d '' source; do
            base=${source#"$lane/$kind/"}
            [[ $base != */* && $base != *$'\n'* ]] || { echo "unsafe artifact name in $name" >&2; exit 1; }
            dest=$out/$kind/$base
            if [[ -e $dest ]]; then
                cmp -s "$source" "$dest" || { echo "conflicting duplicate artifact $base" >&2; exit 1; }
            else
                install -m 0644 "$source" "$dest"
            fi
        done < <(find "$lane/$kind" -maxdepth 1 -type f -print0)
    done
done
printf '%s\n' "$sha" > "$out/SOURCE_COMMIT"
printf '%s\n' "$version" > "$out/SOURCE_VERSION"
printf '%s\n' 0 > "$out/DRAFT"
printf '%s\n' x86_64-linux aarch64-linux > "$out/SOURCE_SYSTEMS"
bash "$(dirname "$0")/payload-inventory.sh" seal "$out"
echo "PASS: assembled one source-matched x86_64/aarch64/Android inventory"
