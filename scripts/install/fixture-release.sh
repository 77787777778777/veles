#!/usr/bin/env bash
# A GitHub-shaped release server on loopback, for the installer tests.
# No Python: socat accepts, this script answers one request per process.
#
#   fixture-release.sh serve ROOT PORT     # foreground; ROOT/LATEST names the latest tag
#   fixture-release.sh handle ROOT         # (socat's per-connection child)
#
# Serves, like https://github.com/OWNER/REPO/releases:
#   /releases/latest                  302 -> /releases/tag/$(cat ROOT/LATEST)
#   /releases/tag/vX                  200
#   /releases/download/vX/FILE        ROOT/download/vX/FILE (302 via /objects/ first,
#                                     as GitHub redirects assets to its CDN)
set -euo pipefail
mode=${1:?usage: fixture-release.sh serve ROOT PORT | handle ROOT}
root=${2:?release root required}
case $mode in
serve)
    port=${3:?port required}
    exec socat "TCP-LISTEN:$port,bind=127.0.0.1,reuseaddr,fork" EXEC:"bash $0 handle $root"
    ;;
handle) ;;
*) echo "unknown mode $mode" >&2; exit 2 ;;
esac
reply() { # STATUS [HEADER...]
    local status=$1; shift
    printf 'HTTP/1.1 %s\r\nConnection: close\r\n' "$status"
    local h; for h in "$@"; do printf '%s\r\n' "$h"; done
}
IFS=' ' read -r method path _ || exit 0
path=${path%$'\r'}; path=${path%%\?*}
while IFS= read -r line; do line=${line%$'\r'}; [[ -z $line ]] && break; done
[[ $method == GET || $method == HEAD ]] || { reply '405 Method Not Allowed' 'Content-Length: 0'; printf '\r\n'; exit 0; }
case $path in
    *..*) reply '400 Bad Request' 'Content-Length: 0'; printf '\r\n' ;;
    /releases/latest)
        reply '302 Found' "Location: /releases/tag/$(cat "$root/LATEST")" 'Content-Length: 0'; printf '\r\n' ;;
    /releases/tag/v*)
        reply '200 OK' 'Content-Type: text/html' 'Content-Length: 3'; printf '\r\nok\n' ;;
    /releases/download/*)
        reply '302 Found' "Location: /objects/${path#/releases/download/}" 'Content-Length: 0'; printf '\r\n' ;;
    /objects/*)
        file=$root/download/${path#/objects/}
        if [[ -f $file ]]; then
            reply '200 OK' 'Content-Type: application/octet-stream' "Content-Length: $(stat -c %s "$file")"
            printf '\r\n'
            [[ $method == HEAD ]] || cat "$file"
        else
            reply '404 Not Found' 'Content-Length: 10'; printf '\r\nnot found\n'
        fi ;;
    *) reply '404 Not Found' 'Content-Length: 10'; printf '\r\nnot found\n' ;;
esac
