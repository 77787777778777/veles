#!/usr/bin/env bash
# The browser tab bridge end to end in real Chrome and Firefox, off-screen.
#
#   scripts/browser-bridge-e2e.sh [chrome|firefox|edge ...]
#
# Uses the veles at $VELES_BIN (default target/debug/veles; build it first)
# and the first browser found of: $CHROME_BIN, google-chrome-stable,
# google-chrome, chromium; $FIREFOX_BIN, firefox-devedition, firefox;
# $EDGE_BIN, microsoft-edge-stable, microsoft-edge. With no browser named,
# one that is not installed is skipped, and with none installed the script
# prints SKIP and exits 0; a browser NAMED here that is not installed, or
# a name it does not know, fails the run. (Every install registers Edge:
# the run copies the manifest the install wrote for the browser it drives
# into its profile.) The Windows run is scripts/win/browser-bridge-e2e.ps1.
#
# Everything is throwaway: a scratch HOME and VELES_HOME per browser (the
# install writes its native-host manifests and launcher there), a fresh
# browser profile, a local test page on a loopback port >= 19100. Both
# browsers run headless; nothing touches a display, a real profile, the
# owner's veles state or a running gateway. The tool talks to the
# browser-started native host directly, as a gateway session's call does,
# so no gateway is needed.
#
# BX_IDLE_SECS (default 35) is how long the bridge sits idle before one
# more snapshot, to show the background (Chrome's service worker,
# Firefox's event page) is not suspended under an open native port. Set it
# to 0 for a quick run.
set -euo pipefail
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
veles=${VELES_BIN:-$repo/target/debug/veles}
if ! command -v node >/dev/null 2>&1; then
    echo "browser bridge e2e: SKIP (node is not installed)"
    exit 0
fi
if [ ! -x "$veles" ]; then
    echo "browser bridge e2e: no veles binary at $veles; build it or set VELES_BIN" >&2
    exit 2
fi
case "$(uname -s)" in
    Linux) ;;
    # Windows: scripts/win/browser-bridge-e2e.ps1 on the host, or
    # scripts/win/browser-bridge-e2e.sh <host> from Linux. macOS: the
    # install writes the documented directories, but this driver's Chrome
    # and Firefox setup has never run on a Mac.
    *) echo "browser bridge e2e: SKIP (Linux here; Windows is scripts/win/browser-bridge-e2e.ps1; macOS is not run)"; exit 0 ;;
esac
exec node "$repo/scripts/browser-bridge-e2e.mjs" "$veles" "$repo/browser-extension" "$@"
