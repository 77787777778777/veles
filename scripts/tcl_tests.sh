#!/usr/bin/env bash
# scripts/tcl_tests.sh — run the desktop's Tcl tests without touching
# the user's desktop.
#
# `lib/mdtest.tcl` needs a mapped Tk window (bbox has no geometry
# otherwise), and a mapped window on a live DISPLAY pops up and steals
# focus. So: run it on a throwaway X server. If none is available the
# tests SKIP rather than hijacking the session — an earlier version
# mapped a window straight onto the developer's desktop.
#
# Usage: scripts/tcl_tests.sh
set -uo pipefail

cd "$(dirname "$0")/.."

# ── one run at a time ────────────────────────────────────────────────
#
# These suites need EXCLUSIVE use of an X server. They map real windows
# and assert on focus, cursor movement and key delivery. `xvfb-run -a`
# picks a display number by probing for a free one, and two concurrent
# runs can probe their way to the SAME number — at which point both
# sets of Tk windows land on one server, fight over focus, and unrelated
# suites fail with cursor assertions.
#
# That is not hypothetical: it produced three "tcl suites FAIL" gate
# runs on 2026-09-02 that were each green on the next attempt, which is
# the worst possible failure mode — a gate nobody can trust. Eight
# sequential runs never reproduced it; two CONCURRENT runs reproduced it
# on the first try, both failing, in different suites.
#
# Serialising is the correct semantics, not a workaround: a second run
# waits instead of corrupting the first. Fixed path on purpose — two
# runs with different TMPDIRs must still contend for the same lock.
if command -v flock >/dev/null 2>&1 && [ -z "${VELES_TCL_TESTS_LOCKED:-}" ]; then
    export VELES_TCL_TESTS_LOCKED=1
    exec flock /tmp/veles-tcl-tests.lock "$0" "$@"
fi
TESTS="veles-desktop/lib/i18ntest.tcl veles-desktop/lib/i18nkeysetest.tcl veles-desktop/lib/pathtest.tcl veles-desktop/lib/jointest.tcl veles-desktop/lib/mdtest.tcl veles-desktop/lib/mdspectest.tcl veles-desktop/lib/transcriptspectest.tcl veles-desktop/lib/uitest.tcl veles-desktop/lib/listtest.tcl veles-desktop/lib/emacstest.tcl veles-desktop/lib/titletest.tcl veles-desktop/lib/sessiontest.tcl veles-desktop/lib/texttest.tcl veles-desktop/lib/rendertest.tcl veles-desktop/lib/lsptest.tcl veles-desktop/lib/fgtest.tcl veles-desktop/lib/devtest.tcl veles-desktop/lib/mediatest.tcl veles-desktop/lib/chrometest.tcl veles-desktop/lib/statetest.tcl veles-desktop/lib/coachtest.tcl veles-desktop/lib/whichkeytest.tcl veles-desktop/lib/comptest.tcl veles-desktop/lib/tabtest.tcl veles-desktop/lib/irctest.tcl veles-desktop/lib/apittest.tcl veles-desktop/lib/insttest.tcl veles-desktop/lib/installtest.tcl veles-desktop/lib/sectest.tcl veles-desktop/lib/acltest.tcl veles-desktop/lib/setuptest.tcl veles-desktop/lib/cmdstest.tcl veles-desktop/lib/sandboxtest.tcl veles-desktop/lib/lspstarttest.tcl veles-desktop/lib/tiptest.tcl veles-desktop/lib/tipzonetest.tcl veles-desktop/lib/statustest.tcl veles-desktop/lib/statusbuildtest.tcl veles-desktop/lib/quotatest.tcl veles-desktop/lib/taskstest.tcl veles-desktop/lib/asktest.tcl veles-desktop/lib/panetest.tcl veles-desktop/lib/sidebartest.tcl veles-desktop/lib/sessionswintest.tcl veles-desktop/lib/overlaytest.tcl veles-desktop/lib/responsivitytest.tcl veles-desktop/lib/memfilestest.tcl veles-desktop/lib/streamtest.tcl veles-desktop/lib/androidtest.tcl veles-desktop/lib/cmdtreetest.tcl veles-desktop/lib/devicetest.tcl veles-desktop/lib/phonetest.tcl veles-desktop/lib/settingsformtest.tcl veles-desktop/lib/appstest.tcl"
# Kept for the messages below (both suites share the same requirements).
TEST="veles-desktop/lib/mdtest.tcl"

# ── the two lists must agree ─────────────────────────────────────────
#
# `nix flake check` runs its OWN hardcoded suite list (flake.nix), because
# it wants a per-suite log as an artifact. Two lists that must stay in sync
# is how a suite gets written, registered HERE, and never gates anything:
# tipzonetest was in one list and not the other until the diff was read.
# Cheap enough to just check, and a mismatch is a finding either way.
flake_suites=$(sed -n 's/.*for suite in \(.*\); do/\1/p' flake.nix 2>/dev/null | head -1)
if [ -n "$flake_suites" ]; then
    here=$(for t in $TESTS; do basename "$t" .tcl; done | sort)
    there=$(for s in $flake_suites; do echo "$s"; done | sort)
    drift=$(comm -3 <(echo "$here") <(echo "$there"))
    if [ -n "$drift" ]; then
        echo "FAIL: the Tcl suite list in scripts/tcl_tests.sh and the one in" >&2
        echo "      flake.nix's 'for suite in' differ. In one but not the other:" >&2
        echo "        $(echo $drift)" >&2
        echo "      A suite missing from flake.nix gates nothing; one missing" >&2
        echo "      here never runs locally. Fix both lists." >&2
        exit 1
    fi
fi

# ── and every suite FILE must be in them ────────────────────────────
#
# The drift check above only compares the two lists TO EACH OTHER — a
# suite in NEITHER list is invisible to it, and that is the sharper
# failure mode: a test that passes whenever anyone runs it directly and
# gates nothing, forever. Enumerate the files and require each name in
# TESTS. Exclusions are explicit and must carry their reason.
suite_exempt="captest"   # manual visual-capture harness, not a pass/fail suite
unregistered=""
for f in veles-desktop/lib/*test.tcl; do
    name=$(basename "$f" .tcl)
    case " $suite_exempt " in
        *" $name "*) continue ;;
    esac
    case " $TESTS " in
        *" $f "*) continue ;;
    esac
    unregistered="$unregistered $name"
done
if [ -n "$unregistered" ]; then
    echo "FAIL: suite file(s) exist but are registered in NO list:" >&2
    echo "      $unregistered" >&2
    echo "      A suite nobody runs is a test that gates nothing — add it to" >&2
    echo "      scripts/tcl_tests.sh TESTS AND flake.nix's 'for suite in'," >&2
    echo "      or add it to suite_exempt above with a reason." >&2
    exit 1
fi

# Run every suite, reporting all failures rather than stopping at the
# first — a red suite should tell you everything that is wrong at once.
run_all() {
    local rc=0
    for t in $TESTS; do
        echo "── $t"
        wish "$t" || rc=1
    done
    return $rc
}

if command -v xvfb-run >/dev/null; then
    echo "running Tcl tests under xvfb-run"
    XVFB_RUNNING=1 exec xvfb-run -a bash -c "$(declare -f run_all); TESTS=\"$TESTS\"; run_all"
fi

if command -v Xvfb >/dev/null; then
    # Pick a display nobody holds. `:99` was hardcoded, so a leftover
    # server whose EXIT trap never fired (a killed run, a crashed shell)
    # kept the number, the new Xvfb failed to bind, and the suites either
    # attached to a stranger's X or to none.
    DISP=""
    for n in $(seq 99 120); do
        if [ ! -e "/tmp/.X${n}-lock" ]; then DISP=":$n"; break; fi
    done
    if [ -z "$DISP" ]; then
        echo "no free X display between :99 and :120"
        exit 1
    fi
    Xvfb "$DISP" -screen 0 1024x768x24 >/dev/null 2>&1 &
    XPID=$!
    trap 'kill $XPID 2>/dev/null' EXIT
    # WAIT for the server, do not guess at it.
    #
    # This was `sleep 1`: a bet that Xvfb comes up within a second. Under
    # load it does not, and a suite that starts before the display is
    # accepting connections dies on `toplevel` with no tally at all —
    # which is precisely what an intermittent "reported no tally" in the
    # gate looks like, and why it never named the same suite twice.
    #
    # Xvfb creates the lock file when it binds the display, which is the
    # same signal `xvfb-run -a` uses to choose one. No extra tool needed
    # (there is no xdpyinfo in this devShell).
    ready=0
    for _ in $(seq 1 100); do
        if [ -e "/tmp/.X${DISP#:}-lock" ]; then ready=1; break; fi
        if ! kill -0 "$XPID" 2>/dev/null; then
            echo "Xvfb on $DISP exited while starting"
            exit 1
        fi
        sleep 0.1
    done
    if [ "$ready" != 1 ]; then
        echo "Xvfb on $DISP never became ready (10s)"
        exit 1
    fi
    echo "running Tcl tests on $DISP (Xvfb pid $XPID)"
    export DISPLAY="$DISP" XVFB_RUNNING=1
    run_all
    exit $?
fi

# No Xvfb: FAIL, don't skip. This used to `exit 0`, which meant CI (and
# anyone running it outside the dev shell) got a green tick for running
# nothing at all — the suite sat broken on a real `-rmargin1` bug for
# months behind that exit code. xorg.xvfb is in the flake devShell, so a
# missing Xvfb means the environment is wrong, which is worth failing on.
echo "FAIL: no Xvfb available — the Tcl tests need an offscreen X server." >&2
echo "      run inside the dev shell (nix develop / direnv), which provides it," >&2
echo "      or VELES_TK_TEST_FORCE=1 wish $TEST to accept a visible window." >&2
exit 1
