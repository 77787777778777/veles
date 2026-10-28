#!/usr/bin/env bash
# scripts/precommit.sh — run everything `nix flake check` runs, locally,
# in the same order, and fail on the first problem.
#
# WHY THIS EXISTS
#
# `nix flake check` is the gate, but it takes minutes and rebuilds from
# scratch in a sandbox. So the temptation is to run one local check —
# usually `cargo test` — and commit. Twice in one session that produced a
# red gate over something trivial:
#
#   * a trailing blank line, because a test was added AFTER `cargo fmt`;
#   * clippy::type_complexity, because clippy ran BEFORE the store
#     methods were written and not after.
#
# Both times every test passed. The checks are cheap; running only some
# of them is what costs time.
#
# Usage:  bash scripts/precommit.sh
set -uo pipefail
cd "$(dirname "$0")/.."

# ONE scratch directory for this run — every step, the tests, the Tcl
# suites and the tools they spawn — on tmpfs, removed when the gate
# exits. The test step used to run with TMPDIR=/dev/shm itself, so
# anything a test or a killed child failed to delete (tcl-lsp's
# per-process dirs, fixtures that leaked on purpose) accumulated at the
# top of /dev/shm: 2.3 GB of RAM by 2026-09-28. A per-run directory
# makes the gate clean up after everything it ran, leaky or not.
shm=/dev/shm
{ [ -d "$shm" ] && [ -w "$shm" ]; } || shm="${TMPDIR:-/tmp}"
RUN_TMP=$(mktemp -d "$shm/veles-gate.XXXXXX") || exit 1
trap 'rm -rf "$RUN_TMP"' EXIT
export TMPDIR="$RUN_TMP"

fail=0
step() {
    printf '%-28s' "$1"
    shift
    if "$@" >"$RUN_TMP/step.log" 2>&1; then
        echo "OK"
    else
        echo "FAIL"
        tail -25 "$RUN_TMP/step.log" | sed 's/^/    /'
        fail=1
    fi
    rm -f "$RUN_TMP/step.log"
}

# Formatting FIRST, and as a check rather than a rewrite: if it would
# change something, that is a finding, not a fixup to do silently.
step "rustfmt"  cargo fmt --all --check
step "clippy"   cargo clippy --workspace --all-features --all-targets -- -D warnings
# The binary as it ships: DEFAULT features. Every other step here turns
# on --all-features, which compiles the `test-support` modules into the
# library too, so code that calls one (crate::scratch) passed the whole
# gate while the shipped `veles` did not build (2026-10-04).
step "default build" cargo build -p veles-cli --bin veles
# The Phase B integration test (veles-core/tests/plugin_so.rs) loads REAL
# out-of-tree .so files. cargo test gives no build-order guarantee for a
# sibling cdylib with no dependency edge, so build them explicitly —
# otherwise the tests would skip and the ABI could rot untested.
#
# TWO artifacts, both load-bearing: the sample is the positive case
# (hook, tool, filter, command, host_call round trip) and badabi is the
# negative one — it dlopens cleanly and then REFUSES, which is what an
# ABI version mismatch looks like from the host's side and is the whole
# reason a version bump is safe. That path had no test until 2026-09-04.
step "plugin .so" cargo build -p veles-plugin-sample-so -p veles-plugin-badabi-so -p veles-plugin-anthropic-so
# A real executable for cross-platform process-plugin discovery and
# installer describe tests. It is a sibling, not a library dependency.
step "process plugin fixture" cargo build -p veles-plugin-echo
# `--exclude veles-runtime`: the runtime dylib must be built as a
# DEPENDENCY. Cargo passes `-C prefer-dynamic` to a dylib only then;
# named on the command line (`--workspace` names it) it embeds a static
# std, and every test binary that links it then fails with "cannot
# satisfy dependencies so `std` only shows up once". Its own (empty)
# tests are nothing to lose.
step "cargo test" env VELES_REQUIRE_PLUGIN_SO=1 cargo test --workspace --exclude veles-runtime --all-features
# The docs site agrees with the published extension API spec (its
# sources live outside this repo, so no cargo test can read them; the
# spec, changelog and docs/extending.md are held by tests/ext_api.rs in
# the step above). PLAN-extension-api.md D7.
step "extension API site" bash scripts/check-ext-api-site.sh
# The hub's WireGuard peer helper runs as root on what the gateway
# queues (pool pairing, deploy/vps/wg): its refusals are pinned here.
step "wg peers helper" bash scripts/test-wg-peers.sh
# The Windows gate (PLAN-windows.md §7): clippy, the build and the unit
# tests on the Windows build host, over ssh (~15 min warm; the tests
# alone are ~9.5 min). Only when a host is named — the host is a
# workstation that is "not on very often", and a tree is not red
# because a machine is off. `scripts/win/check.sh` skips (green) when
# the host does not answer.
if [ -n "${VELES_WIN_HOST:-}" ]; then
    # A host that does not answer is a green SKIP (it is ephemeral by
    # design), but it must read as one: the step said "OK" for a run that
    # never reached the host (2026-10-14, a transient after a reboot) and
    # only the Windows host's check.log showed no new START.
    printf '%-28s' "windows check"
    if bash scripts/win/check.sh "$VELES_WIN_HOST" all >"$RUN_TMP/step.log" 2>&1; then
        if grep -q "does not answer — skipped" "$RUN_TMP/step.log"; then
            echo "skipped ($VELES_WIN_HOST does not answer)"
        else
            echo "OK"
        fi
    else
        echo "FAIL"
        tail -25 "$RUN_TMP/step.log" | sed 's/^/    /'
        fail=1
    fi
    rm -f "$RUN_TMP/step.log"
else
    printf '%-28s%s\n' "windows check" "skipped (VELES_WIN_HOST unset)"
fi

# The Tcl suites enforce their own tally format; flake.nix greps
# "^<suite>: <n> passed, 0 failed", which scripts/tcl_tests.sh does NOT.
printf '%-28s' "tcl suites"
tcl_out=$(./scripts/tcl_tests.sh 2>&1)
tcl_rc=$?
tcl_bad=0
# The runner ALREADY knows: `run_all` does `wish "$t" || rc=1`, so a
# suite that dies makes tcl_tests.sh exit non-zero. That status was
# discarded here and the whole verdict came from scraping text, which is
# why an aborting suite could only be caught by the tally check below.
# Trust the exit code first; keep the text checks as the finer-grained
# report, not as the only evidence.
if [ "$tcl_rc" -ne 0 ]; then
    tcl_bad=1
fi
if echo "$tcl_out" | grep -qE "✗|[1-9][0-9]* failed"; then
    tcl_bad=1
fi
# A suite that ABORTS prints no tally at all, and reports neither a ✗ nor
# a failure count — so the check above passes it. That is how a suite
# that died on a missing stub proc looked green locally and red in CI,
# which greps for the tally. Assert every suite in the runner reported
# one, exactly as flake.nix does.
no_tally=""
for t in $(grep '^TESTS=' scripts/tcl_tests.sh | sed 's/.*="//;s/"//'); do
    suite=$(basename "$t" .tcl)
    # HERESTRING, NOT `echo "$tcl_out" | grep -q`.
    #
    # That pipeline made this gate fail at random on green code. `grep
    # -q` exits the instant it matches; `echo` is a forked subshell
    # still writing ~90 KB into a 64 KB pipe, so it dies of SIGPIPE, the
    # pipeline exits 141, and `set -o pipefail` (line 20) hands that to
    # the `if !` — which reads as "this suite printed no tally".
    #
    # It named a different suite every time, because which one loses the
    # race depends on timing, and it never reproduced in a handful of
    # runs: measured at 3 misses in 1080 checks, i.e. ~7% of full gate
    # runs, and 0 in the first 50 tried. Three gate failures on
    # 2026-09-04 were this, not the suites — `nix flake check` ran the
    # same suites green in the same minutes.
    #
    # A herestring is one process with no pipe, so there is nothing to
    # get SIGPIPE. (The `| head` pipelines below are display-only; their
    # status is never read.)
    if ! grep -qE "^$suite: [0-9]+ passed, [0-9]+ failed" <<<"$tcl_out"; then
        tcl_bad=1
        no_tally="$no_tally $suite"   # report ALL of them, not the first
    fi
done
if [ "$tcl_bad" = 1 ]; then
    # Unconditionally, because the old guard keyed off $suite — which the
    # loop above always leaves set — so a genuine suite FAILURE with every
    # tally present printed no "FAIL" at all and ran its diagnostic onto
    # the label line.
    echo "FAIL"
    for suite in $no_tally; do
        echo "    $suite reported no tally — it aborted before finishing"
        # And WHY. wish's own error lands between this suite's "── path"
        # header and the next one; without it the gate says a suite died
        # and leaves you to reproduce an intermittent to find out how.
        echo "$tcl_out" | awk -v s="$suite" '
            $0 ~ ("── .*/" s "\\.tcl$") { on = 1; next }
            on && /^── / { exit }
            on && NF { print "        " $0 }
        ' | tail -12
    done
    # The diagnostic must not drown in ordinary output. The old pattern
    # matched the bare word "failed", so every green tally
    # ("mdtest: 42 passed, 0 failed") and every test NAMED for failure
    # ("✓ a failed write is reported") matched it — twenty lines of green
    # that pushed the actual cause past head -20. Match real failures.
    echo "$tcl_out" | grep -E "✗|Error in startup|[1-9][0-9]* failed" \
        | head -40 | sed 's/^/    /'
    fail=1
else
    echo "OK"
    echo "$tcl_out" | grep -E "^[a-z]+test: " | sed 's/^/    /'
fi

# A desktop edit must rebuild only the APK's assembly (15 s), never the
# Android engine's cross-build (219 s; flake.nix engineSrc). Evaluates the
# derivations of a copy of the tree before and after an edit; builds
# nothing, ~25 s.
step "android rebuild scope" bash scripts/android-rebuild-scope.sh

# Every path a test reaches OUTSIDE its own crate must be named in
# flake.nix's `rustSrc` fileset, or the test runs here and silently
# skips — or fails — inside the nix sandbox.
#
# This is not hypothetical. On 2026-09-04 `plugins/examples` was reached
# by tclrt.rs through CARGO_MANIFEST_DIR/../.. and was NOT in the
# fileset, so `nix flake check` failed on a directory that plainly
# exists in the tree. guards.rs walks veles-desktop and plugins the same
# way, and its walk returns empty on a missing dir — so those assertions
# passed vacuously in CI instead of failing loudly.
#
# The fileset is opt-IN by design (it is what keeps 36 GB of target/ out
# of the store). Opt-in needs an enforcer or it becomes opt-forgotten.
printf '%-28s' "fileset covers test paths"
escapes=$(grep -rhoE 'CARGO_MANIFEST_DIR"\)\)?[^;]*\.\./\.\./[A-Za-z0-9_./-]+|repo\.join\("[A-Za-z0-9_.-]+"\)' \
              --include='*.rs' crates 2>/dev/null \
          | grep -oE '(\.\./\.\./|join\(")[A-Za-z0-9_.-]+' \
          | sed 's|.*/||; s|join("||' | sort -u)
uncovered=""
for p in $escapes; do
    # A fileset entry `./x` covers `./x/anything`, so match the top
    # component only — that is exactly the granularity nix copies at.
    grep -qE "^\s*\./$p(\s|/|$)" flake.nix || uncovered="$uncovered $p"
done
if [ -n "$uncovered" ]; then
    echo "FAIL"
    echo "    reached by a test but absent from flake.nix rustSrc:$uncovered"
    echo "    add it to the fileset, or the check that reaches it is a no-op in CI"
    fail=1
else
    echo "OK"
fi

# A `[patch.crates-io]` path dependency has to be BOTH named in the
# fileset and tracked by git, or the nix build cannot see it.
#
# For a flake in a git repository nix copies the tracked tree, so an
# untracked `vendor/<crate>` is simply absent no matter what the fileset
# says — and cargo, which reads the working tree, is perfectly happy.
# The build then dies with
#
#   error: failed to load source for dependency `clib`
#     failed to read /build/source/vendor/clib/Cargo.toml
#     No such file or directory
#
# about a file that plainly exists. Hit 2026-09-04 adding vendor/clib.
printf '%-28s' "vendored crates visible"
badpatch=""
for p in $(sed -n '/^\[patch\.crates-io\]/,/^\[/p' Cargo.toml \
           | grep -oE 'path = "[^"]+"' | sed 's/path = "//; s/"//'); do
    top="${p%%/*}"
    grep -qE "^\s*\./$top(\s|/|$)" flake.nix \
        || badpatch="$badpatch $p(not-in-fileset)"
    git ls-files --error-unmatch "$p/Cargo.toml" >/dev/null 2>&1 \
        || badpatch="$badpatch $p(untracked)"
done
if [ -n "$badpatch" ]; then
    echo "FAIL"
    echo "    patch.crates-io paths nix cannot see:$badpatch"
    echo "    git add the crate, and name its top directory in rustSrc"
    fail=1
else
    echo "OK"
fi


# The plugin author's guide lists the ops a plugin can call. That list is
# `bridgeops::OP_NAMES`, and a doc that drifts from it is worse than no
# doc: docs/plugins.md is what somebody reads before writing code
# against a command that does not exist, or misses one that does.
#
# The cargo side already cross-checks OP_NAMES against the live Tcl
# interpreter, plugins/veles_api.tcl and what host_call accepts. This is
# the same taxonomy, one hop further out.
printf '%-28s' "plugin docs list the ops"
ops_code=$(sed -n '/pub const OP_NAMES/,/^\];/p' crates/veles-core/src/bridgeops.rs \
           | grep -oE '"[a-z_]+"' | tr -d '"' | sort | tr '\n' ' ')
ops_docs=$(sed -n '/^call_llm/,/^usage_totals/p' docs/plugins.md \
           | tr -s ' ' '\n' | grep -E '^[a-z_]+$' | sort | tr '\n' ' ')
if [ "$ops_code" != "$ops_docs" ]; then
    echo "FAIL"
    echo "    bridgeops::OP_NAMES: $ops_code"
    echo "    docs/plugins.md:     $ops_docs"
    echo "    update the op block in docs/plugins.md to match"
    fail=1
else
    echo "OK"
fi
# Every suite in the runner must also be in flake.nix, or it passes here
# and never runs in CI.
printf '%-28s' "suites wired into flake"
missing=""
for t in $(grep '^TESTS=' scripts/tcl_tests.sh | sed 's/.*="//;s/"//' | tr ' ' '\n'); do
    name=$(basename "$t" .tcl)
    grep -q "for suite in .*\b$name\b" flake.nix || missing="$missing $name"
done
if [ -n "$missing" ]; then
    echo "FAIL"
    echo "    not in flake.nix:$missing"
    fail=1
else
    echo "OK"
fi

echo
if [ $fail -eq 0 ]; then
    echo "all local checks pass — nix flake check should be green"
else
    echo "FIX THE ABOVE BEFORE COMMITTING" >&2
fi
exit $fail
