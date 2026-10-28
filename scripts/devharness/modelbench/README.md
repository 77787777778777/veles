# modelbench — comparing engines and models on the same work

Fixtures and a runner for asking two questions that token counts cannot
answer: **is the work correct**, and **is it correct for the right
reasons**. Built 2026-09-02 while comparing veles against hermes.

Not part of any gate. It costs real API quota; run it deliberately.

## The read-this-first caveat

USER, 2026-09-02:

> glm is basically a mindflayer that has consumed your entire being, so
> it does exceptionally well on tasks designed by Claude opus, even
> though it's not as smart a model.

Every fixture here was designed by Claude, and every result so far was
measured on GLM-5.3 — a model distilled from Claude. A task whose
phrasing, structure and idea of "a clear spec" come from Claude is
exactly the task a Claude-distilled model is best at. **Treat any
single-model result from this harness as provisional until the same
fixture has run on an unrelated lineage.**

Two of the fixtures resist that bias better than the others, and it is
worth knowing which:

| fixture | who defines "correct" | bias risk |
|---|---|---|
| `jsonkit` | **IETF** — RFC 8259 / 6901 / 6902 / 7386 | LOW: the right answer is published, externally, and predates every model here |
| `rename-crossfile` | the Rust compiler | LOW |
| `hidden-spec-test` | an existing test in the fixture | LOW |
| `exprlang` | **invented by Claude** | HIGH: precedence table, error wording and API shape are all one author's taste |

Prefer `jsonkit` for cross-model comparison. `exprlang` is a
length/cost fixture, not a fairness one.

## The fixtures

- **`rename-crossfile`** — rename a function in the file you were told
  about; the break lands in a file you were not. `cargo build` decides.
- **`hidden-spec-test`** — add an ellipsis to `truncate`. The obvious
  implementation is off by one; the real spec is in a test the model has
  no reason to open.
- **`exprlang`** — 55 failing conformance tests for an expression
  language: lexer, precedence, associativity, short-circuit, positioned
  errors. A length and cost fixture.
- **`jsonkit`** — five interlocking RFCs (parse, serialize, JSON
  Pointer, JSON Patch, JSON Merge Patch). **45 visible tests the model
  iterates against, and 36 HIDDEN tests it never sees**, in
  `jsonkit-hidden/`. The hidden suite is the quality measure: an
  implementation fitted to the visible tests scores badly on it, one
  that actually read `SPEC.md` does not. A model that reshaped the
  public API fails to compile against the hidden suite, which scores 0 —
  that is a real answer, not a harness failure.

## Running it

    bash scripts/devharness/modelbench/run.sh <fixture> <engine> <reps>

`engine` is `veles` (uses `$VELES_HOME`, default the XDG data dir `~/.local/share/veles`) or `hermes`.
Set `MB_MODEL` / `MB_PROVIDER` to pick a route; `MB_PRICE_IN`,
`MB_PRICE_CACHED` and `MB_PRICE_OUT` (USD per million) to price it.

Defaults are GLM-5.3 on the z.ai coding plan at $1.40 / $0.26 / $4.40.

## Grading traps this harness has already fallen into

All four were real. Three scored correct work as failure; the fourth let
two runs cheat. The first two share a shape — reading one number out of
multi-line output without checking which line it was:

1. `cargo test` prints one `N passed` line **per test binary** (lib,
   each integration target, doc-tests). `head -1` picks the empty lib
   target. Take the max; this harness does.
2. hermes reports `input_tokens` **excluding** cache reads (Anthropic
   convention), veles reports the total. Mixing the two conventions
   produced a negative cost. `total = input + cache_read + output` is
   the check.
3. **A corrupted assertion in the hidden suite caps every score.**
   `jsonkit-hidden/hidden.rs` held a literal `0x01` byte inside a raw
   string, so `h_ser_control_char` demanded an UNESCAPED control
   character — the opposite of RFC 8259 §7. No correct implementation
   can pass it. Five runs across two engines and three loadouts all
   scored exactly 35/36, and *that* was the tell: independent 700-line
   implementations do not converge on one identical failure. Repaired,
   every one of them is 36/36.

   **An identical score across arms is a harness smell, not a finding.**
   Before reporting one, diff which tests failed — if it is the same
   test everywhere, suspect the test. `run.sh` now aborts rather
   than grade a hidden suite containing raw control bytes.

4. **A build artifact in the seed leaks the entire hidden suite.**
   Grading compiles the hidden tests to
   `$WORK/target/debug/deps/hidden-*`. A seed refreshed from an
   already-graded tree ships that 8 MB binary — and **all 36 hidden
   test names plus their expected literals** (`/a~1b`, `{"a":1,"a":2}`,
   `-2.5E-2`, …) come straight back out with `strings`. It happened.
   Two of six runs mined the binary instead of reading `SPEC.md`:
   veles-fat rep 1 spent **8 of its 52 calls** on it, veles-fair rep 2
   spent 2. Their hidden scores are worthless, and fat rep 1's cost —
   the most expensive veles run, the one that carried the "fat loadout
   is expensive" conclusion — is inflated by a rabbit hole that had
   nothing to do with its loadout.

   `run.sh` now aborts if the seed contains `target/`, and scrubs the
   hidden binary and `tests/hidden.rs` from the tree before keeping it.
   **A fixture directory must contain only sources.**

Each run's finished tree is kept at `$MB_KEEP/kept-<fixture>-<engine>-<rep>`
(default `/dev/shm`) so a grading dispute can be settled by re-running
the grader instead of re-running the model.
