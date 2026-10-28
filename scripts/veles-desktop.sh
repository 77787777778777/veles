#!/usr/bin/env bash
# veles-desktop launcher — runs the CURRENT checkout's desktop against the
# gateway, outside any nix dev shell.
#
# Why this exists: `veles desktop` execs `wish`, which only exists inside
# the flake dev shell or a nix-built wrapper. This script pins the same
# store paths the flake's packages.veles-desktop uses, so double-clicking
# or running this from any shell just works.
#
# Rebuild note: the Rust side is target/release/veles from this checkout;
# after `git pull` run `nix develop -c cargo build --release -p veles-cli`
# (or just `veles-gateway-restart` for the service which uses target/debug).
set -euo pipefail

VELES_REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export VELES_DESKTOP_DIR="$VELES_REPO/veles-desktop"

# Pin the same tk/tcllib the flake uses (bump if these vanish from store).
WISH_BIN="$(ls -d /nix/store/*-tk-8.6.16/bin/wish 2>/dev/null | head -1 || true)"
TCLLIB_DIR="$(ls -d /nix/store/*-tcllib-2.0/lib 2>/dev/null | head -1 || true)"
# tkimg: JPEG/TIFF/BMP/ICO decoders for Tk photos. Without it on the
# path an attached .jpg falls back to an artifact card — correct, but not
# what the flake shell shows you, and that gap is exactly how "it works
# in the tests" turns into "it doesn't work in my app".
TKIMG_DIR="$(ls -d /nix/store/*-tkimg-*/lib 2>/dev/null | head -1 || true)"
if [[ -z "$WISH_BIN" ]]; then
  echo "veles-desktop: no tk wish in /nix/store — are you on the veles flake's nixpkgs?" >&2
  exit 1
fi
export WISH="$WISH_BIN"
TCLLIBPATH_PARTS=()
[[ -n "$TCLLIB_DIR" ]] && TCLLIBPATH_PARTS+=("$TCLLIB_DIR")
[[ -n "$TKIMG_DIR" ]] && TCLLIBPATH_PARTS+=("$TKIMG_DIR")
if [[ ${#TCLLIBPATH_PARTS[@]} -gt 0 ]]; then
  export TCLLIBPATH="${TCLLIBPATH_PARTS[*]}"
fi
if [[ -z "$TKIMG_DIR" ]]; then
  echo "veles-desktop: note — no tkimg in /nix/store; JPEG/TIFF attachments" >&2
  echo "  will show as artifact cards rather than inline pictures." >&2
  echo "  Fix: nix build .#veles-desktop, or run inside the dev shell." >&2
fi

# webp / svg / heic / avif have no Tk image handler at all — lib/media.tcl
# converts them to a cached PNG with whichever of these is on PATH. They
# are in the flake (imageDecoders), but this script runs outside it, so
# the same store paths get pinned here the way wish and tcllib do.
# Missing ones are not an error: the attachment falls back to a card.
for _dec in 'libwebp-*' 'resvg-*' 'libheif-*-bin'; do
  _dir="$(ls -d /nix/store/*-${_dec}/bin 2>/dev/null | head -1 || true)"
  [[ -n "$_dir" ]] && PATH="$_dir:$PATH"
done
export PATH

# ── is the GATEWAY binary older than the source? ────────────────────
#
# The desktop's Tcl is read live from this checkout, but the gateway is a
# compiled binary — and the systemd unit runs target/debug. So editing
# Rust and relaunching the desktop silently keeps testing the OLD engine:
# the window is new, the brain behind it is not.
#
# That cost a full spot-test round. Four "still broken" reports were all
# fixes sitting in source that the running gateway had never been built
# with. Say so, loudly, instead of letting it look like the fix failed.
newest_rust_source() {
  find "$VELES_REPO/crates" -name '*.rs' -newer "$1" -print -quit 2>/dev/null
}

# Which gateway are we actually pointed at?
#
# Both the staleness check and the autostart below assume the gateway is
# a process on THIS machine built from THIS checkout. With VELES_API
# pointing somewhere else, neither assumption holds: the staleness check
# would compare a local binary's mtime against a remote engine it has
# nothing to do with, and the autostart would boot a second, local
# gateway that nothing is going to talk to.
VELES_API_URL="${VELES_API:-http://127.0.0.1:8642}"
REMOTE_GATEWAY=1
case "$VELES_API_URL" in
  http://127.0.0.1:*|http://localhost:*|http://[::1]:*|https://127.0.0.1:*|https://localhost:*)
    REMOTE_GATEWAY=0 ;;
esac

# The ACTIVE unit's binary, not just "some unit that exists". Two bugs
# used to live here (2026-09-05, cost the launcher permanently):
#  - it read `veles-gateway` even when `veles-gateway-dev` was the unit
#    actually running, so the check compared the source against an
#    engine nobody was using;
#  - the nix-store binary it then checked has mtime = epoch 1, so EVERY
#    .rs file was "newer" and the script refused to start, always.
# The dev unit also wraps ExecStart in `/usr/bin/env PATH=…`, so the
# veles binary is pulled out by shape, not by first word.
gateway_unit_for_service() {
  local u
  for u in veles-gateway-dev veles-gateway; do
    if systemctl --user is-active "$u" >/dev/null 2>&1; then
      echo "$u"
      return 0
    fi
  done
  return 0  # nothing running
}
gateway_bin_for_service() {
  local unit exec tok
  unit="$(gateway_unit_for_service)"
  [[ -z "$unit" ]] && return 0
  exec="$(systemctl --user cat "$unit" 2>/dev/null | sed -n 's/^ExecStart=//p' | tail -1)"
  for tok in $exec; do
    case "$tok" in
      *target/debug/veles|*target/release/veles) echo "$tok"; return 0 ;;
    esac
  done
  return 0  # a nix-store unit: no checkout-built binary to check
}
newest_rust_source() {
  find "$VELES_REPO/crates" -name '*.rs' -newer "$1" -print -quit 2>/dev/null
}

GW_UNIT="$(gateway_unit_for_service)"
GW_BIN="$(gateway_bin_for_service)"
[[ -z "$GW_BIN" ]] && GW_BIN="$VELES_REPO/target/release/veles"
if [[ "$REMOTE_GATEWAY" == "1" ]]; then
  echo "veles-desktop: remote gateway $VELES_API_URL — skipping the local staleness check." >&2
  echo "  The engine there is whatever that host has built; this checkout does not say." >&2
elif [[ -z "$GW_UNIT" ]]; then
  # Nothing running: the autostart below decides what comes up. Check
  # against the release binary it would fall back to.
  GW_UNIT="(none — will autostart)"
elif [[ "$GW_UNIT" == "veles-gateway" ]]; then
  echo "veles-desktop: the running engine is the FLAKE build ($GW_BIN)." >&2
  echo "  That binary is frozen at nix build time — it is not this checkout, and" >&2
  echo "  its store mtime makes a source comparison meaningless. Starting anyway;" >&2
  echo "  use veles-gateway-dev if you want the engine under test." >&2
elif [[ -f "$GW_BIN" ]] && [[ -n "$(newest_rust_source "$GW_BIN")" ]]; then
  echo "veles-desktop: WARNING — the gateway binary is OLDER than the Rust source." >&2
  echo "  gateway: $GW_BIN (unit: $GW_UNIT)" >&2
  echo "  newer:   $(newest_rust_source "$GW_BIN")" >&2
  echo "  The desktop's Tcl is live from this checkout, but the ENGINE is not." >&2
  case "$GW_BIN" in
    *target/debug/*) echo "  Fix: nix develop -c cargo build -p veles-cli && systemctl --user restart $GW_UNIT" >&2 ;;
    *)               echo "  Fix: nix develop -c cargo build --release -p veles-cli, then restart $GW_UNIT" >&2 ;;
  esac
  if [[ "${VELES_DESKTOP_ALLOW_STALE:-}" != "1" ]]; then
    echo "  Refusing to start against a stale engine. Set VELES_DESKTOP_ALLOW_STALE=1 to override." >&2
    exit 1
  fi
  echo "  VELES_DESKTOP_ALLOW_STALE=1 — starting anyway." >&2
fi

# Make sure the gateway is up (the desktop is a window onto it).
#
# Only for a LOCAL one. Starting a local gateway because a remote is
# unreachable gives you a second engine on the wrong machine, which then
# answers /api/health and makes everything look fine.
#
# /api/health is unauthenticated by design (it is the container liveness
# probe), so no token is needed here — and, for the same reason, a 200
# from it does NOT mean the desktop's token will be accepted. main.tcl
# probes an authenticated route for that.
if [[ "$REMOTE_GATEWAY" == "0" ]]; then
  if ! curl -s --max-time 1 "$VELES_API_URL/api/health" >/dev/null 2>&1; then
    # Prefer whichever unit EXISTS — this machine runs veles-gateway-dev;
    # `start` on a missing unit is a hard error, not a no-op.
    if systemctl --user list-unit-files veles-gateway-dev.service --no-legend 2>/dev/null | grep -q veles-gateway-dev; then
      systemctl --user start veles-gateway-dev
    else
      systemctl --user start veles-gateway 2>/dev/null || \
        "$VELES_REPO/target/release/veles" gateway run &
    fi
  fi
elif ! curl -s --max-time 3 "$VELES_API_URL/api/health" >/dev/null 2>&1; then
  echo "veles-desktop: $VELES_API_URL is not answering /api/health." >&2
  echo "  Is the tunnel up? (ssh -L / WireGuard — see HANDOFF.md §16)" >&2
fi

exec "$VELES_REPO/target/release/veles" desktop "$@"
