#!/bin/sh
# deploy/laptop/lib/searxng-idle-stop.sh — ~/.local/lib/veles/searxng-idle-stop.sh
#
# Run by searxng-idle.timer every 2 min: stop the local SearXNG quadlet
# (user/searxng.container) once it has been quiet for SEARXNG_IDLE_SECS
# (default 900).
#   - The signal is the mtime of the stamp web_search touches on every
#     attempt: $XDG_CACHE_HOME/veles/searxng-last-use
#     (crates/veles-core/src/webtools.rs searxng_stamp_path). The path
#     is a cross-language contract; the Rust test
#     stamp_path_follows_the_cache_dir_contract pins it.
#   - No stamp: leave it running. A hand-started unit is nobody else's
#     to stop (fail open).
#   - A unit started after the last use counts from its start, so a
#     hand start is not undone two minutes later by an old stamp.
# POSIX sh (the no-python policy, AGENTS.md); GNU date and stat.
set -u
idle=${SEARXNG_IDLE_SECS:-900}
unit=searxng.service
stamp=${XDG_CACHE_HOME:-$HOME/.cache}/veles/searxng-last-use

systemctl --user is-active --quiet "$unit" || exit 0
[ -f "$stamp" ] || exit 0
last=$(stat -c %Y "$stamp")
started=$(systemctl --user show -p ActiveEnterTimestamp --value "$unit")
if [ -n "$started" ]; then
    s=$(date -d "$started" +%s 2>/dev/null || echo 0)
    [ "$s" -gt "$last" ] && last=$s
fi
now=$(date +%s)
[ $((now - last)) -ge "$idle" ] || exit 0
systemctl --user stop "$unit"
