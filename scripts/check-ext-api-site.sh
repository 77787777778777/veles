#!/usr/bin/env bash
# scripts/check-ext-api-site.sh: the docs site agrees with the repo's
# published extension API spec (PLAN-extension-api.md D7).
#
# The docs site's sources live outside this repo (~/veles-docs-work,
# USER 2026-10-03), so no cargo test can read them. This step, run by
# scripts/precommit.sh, fails unless:
#
#   1. the site's ref/extension-api.json (copied there by its
#      build/refresh-ext-api.sh or refresh-refs.sh) is byte-identical to docs/extension-api.json;
#   2. the site page src/en/extending.md carries the spec's stamp in its
#      frontmatter: extension_api: "<version> sha256:<16 hex>".
#
# So an API change cannot pass the gate until the site page has been
# read and updated too, which is the point.
#
# VELES_DOCS_SITE names the site tree (default ~/veles-docs-work). A
# missing tree FAILS: a skip that looks green is how a gate stops
# checking anything. Only VELES_DOCS_SITE=none skips, and says so.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
spec="$repo/docs/extension-api.json"
site="${VELES_DOCS_SITE:-$HOME/veles-docs-work}"

if [ "$site" = none ]; then
    echo "SKIP: VELES_DOCS_SITE=none, the docs site is not checked"
    exit 0
fi
if [ ! -d "$site" ]; then
    echo "FAIL: no docs site at $site (set VELES_DOCS_SITE, or VELES_DOCS_SITE=none to skip on purpose)"
    exit 1
fi
if [ ! -f "$spec" ]; then
    echo "FAIL: $spec is missing"
    exit 1
fi

version="$(sed -n 's/^  "api_version": "\([^"]*\)",\{0,1\}$/\1/p' "$spec")"
hash="$(sha256sum "$spec" | cut -c1-16)"
want="$version sha256:$hash"
if [ -z "$version" ]; then
    echo "FAIL: no api_version in $spec"
    exit 1
fi

fail=0
if ! cmp -s "$spec" "$site/ref/extension-api.json"; then
    echo "FAIL: $site/ref/extension-api.json differs from the repo's spec: run build/refresh-ext-api.sh in $site"
    fail=1
fi
page="$site/src/en/extending.md"
if [ ! -f "$page" ]; then
    echo "FAIL: $page is missing"
    fail=1
else
    got="$(sed -n '1,/^---$/{/^extension_api:/s/^extension_api: *"\{0,1\}\([^"]*\)"\{0,1\} *$/\1/p}' "$page" | head -1)"
    if [ "$got" != "$want" ]; then
        echo "FAIL: $page says extension_api: \"$got\", the spec is \"$want\": read the change, update the page, then its stamp"
        fail=1
    fi
fi
if [ "$fail" = 0 ]; then
    echo "OK: the docs site matches extension API $want"
fi
exit "$fail"
