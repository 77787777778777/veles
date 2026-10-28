#!/usr/bin/env bash
# Seal or verify a build/assembly inventory BEFORE the first registry write.
# The final signed manifest is made later, after registry content digests
# exist. This file lives beside payload/, not in the signed payload itself.
set -euo pipefail
mode=${1:?usage: payload-inventory.sh seal|verify ARTIFACT_ROOT}
out=${2:?artifact root required}
[[ $out = /* ]] || out=$PWD/$out
inventory=$out/PREPUBLISH-SHA256SUMS
mkdir -p "$out/payload" "$out/unsigned"
mapfile -t files < <(find "$out/payload" "$out/unsigned" -maxdepth 1 -type f \
    ! -name 'oci-image-digests.txt' -printf '%P\n' | LC_ALL=C sort)
case $mode in
    seal)
        # Only the lane builder and the assembler may refresh this file.
        # verify-inventory.sh never reconciles a changed payload for you.
        ((${#files[@]} > 0)) || { rm -f "$inventory"; exit 0; }
        : > "$inventory.tmp"
        for base in "${files[@]}"; do
            [[ $base =~ ^[A-Za-z0-9._-]+$ ]] || { echo "unsafe artifact filename: $base" >&2; exit 1; }
            for kind in payload unsigned; do
                if [[ -f $out/$kind/$base ]]; then
                    digest=$(sha256sum "$out/$kind/$base" | cut -d' ' -f1)
                    printf '%s  %s/%s\n' "$digest" "$kind" "$base" >> "$inventory.tmp"
                fi
            done
        done
        mv "$inventory.tmp" "$inventory"
        ;;
    verify)
        [[ -s $inventory ]] || {
            if ((${#files[@]} == 0)); then exit 0; fi
            echo "missing $inventory before registry publication" >&2
            exit 1
        }
        mapfile -t recorded < "$inventory"
        ((${#recorded[@]} == ${#files[@]})) || {
            echo "payload set differs from $inventory" >&2; exit 1
        }
        for line in "${recorded[@]}"; do
            [[ $line =~ ^([0-9a-f]{64})[[:space:]][[:space:]](payload|unsigned)/([A-Za-z0-9._-]+)$ ]] || {
                echo "malformed pre-publication checksum record" >&2; exit 1
            }
            digest=${BASH_REMATCH[1]}
            path=${BASH_REMATCH[2]}/${BASH_REMATCH[3]}
            actual=$(sha256sum "$out/$path" | cut -d' ' -f1)
            [[ $actual == "$digest" ]] || {
                echo "payload differs from PREPUBLISH-SHA256SUMS: $path" >&2; exit 1
            }
        done
        ;;
    *) echo "unknown payload inventory operation: $mode" >&2; exit 2 ;;
esac
