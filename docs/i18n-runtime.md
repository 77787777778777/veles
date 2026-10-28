# veles i18n — the runtime path

How a string gets from source to a translated UI, where the catalogs live,
and what to do when this design hits its limits. Written 2026-09-21 at the
user's request, after a build-memory problem made the compile-time load
visible; the engine catalogs became runtime files on 2026-09-28.

## The path, end to end

    source string  →  catalog  →  runtime lookup  →  player

There are two catalogs and two runtimes. They share a key space but not a
reader, which is the fact most of this document depends on.

### Engine (Rust: TUI, REPL, gateway, Telegram, IRC)

1. **Source.** A string in `crates/**/*.rs`, wrapped as
   `crate::i18n::lookup("English text", &[(name, val)])` or the
   `i18n!("key", name = val)` macro. The English text IS the key.
2. **Catalog.** `crates/veles-core/locales/<id>.yml` — flat `key: "value"`
   rows under `_version: 1`.
3. **Load — at runtime** (since 2026-09-28; before that, rust-i18n's
   `i18n!` macro compiled all 47 files into the binary). `i18n::locales_dir`
   finds the directory, first hit wins: `VELES_LOCALES_DIR` (the nix
   wrapper sets it) → `share/veles/locales` beside the binary's `bin/` (the
   installed layout, `$out/share/veles/locales`) → the source tree's
   `crates/veles-core/locales` (a working-tree build: `target/debug/veles`,
   the dev gateway, `cargo test`). A locale's file is read the first time
   something renders in it. **Only `en.yml` is compiled in**, so a binary
   that finds no directory still renders every symbolic key in English;
   `available()` (the picker's list) is the `.yml` files actually present
   plus `en`, so it never offers a locale it cannot load. A file that will
   not read or parse is logged once and that locale renders English — one
   bad catalog does not take the others down.
4. **Resolve.** `crate::i18n::lookup` (`src/i18n.rs`). Missing key → the
   locale's catalog, then English's, then the key itself, which for an
   English-as-key row is the English text (a graceful degrade) and for a
   symbolic id (`tasks_header`) is a loud gap.
5. **Locale — per REQUEST, not per process** (2026-09-25). A lookup renders
   in `i18n::effective()`: a thread-local scope (sync work — the shared
   command dispatcher enters it from the session's own language), else a
   task-local scope (async work — each desktop request carries the
   window's language in `X-Veles-Locale`; `Agent::run_turn` runs under
   the session's language), else the process default. The process
   default is `set_active` / `init` — config `tui.locale` → `VELES_LOCALE`
   → `LANG`/`LC_ALL` → `en`, normalized by `normalize` (region folds onto
   language; `zh_TW`→`zh_hant`, `sr_RS`→`sr_cyrl`; `C.UTF-8` = no
   preference) — and it follows a CHANGED `tui.locale` on config reload.

   Why: the gateway is one process answering IRC, Telegram, A2A and the
   desktop. The first `/locale` switched that process — every user of
   every surface at once. Now `/locale <id>` on a gateway-served surface
   sets `session_locale` for THAT session (the TUI and REPL, single-user,
   still switch the process), and `/locale primary|secondary` — a config
   write that sets everyone's default — is `cmd-admin`.

   Precedence: the session's own language → the request's (desktop
   window) → the process default. **Limitation:** `tokio::spawn` /
   `spawn_blocking` do not inherit a task-local; a lookup in spawned work
   falls back to the process default unless the spawner re-enters the
   scope (`i18n::scope(i18n::scoped(), fut)`, as the desktop's turn
   spawns do).

### Desktop (Tcl/Tk)

1. **Source.** `::vi18n::mc {English text}` — or `::vi18n::label $w {key}`
   for a widget created once, which registers it so `relabel` re-texts it
   after a swap. `::vi18n::aname $w {key}` is the same for an accessible
   name.
2. **Catalog.** `veles-desktop/msgs/<id>.msg` — msgcat format,
   `::msgcat::mcset <id> {key} {value}`.
3. **Load.** `package require msgcat` + `mclocale` at
   `::vi18n::init` (called from `main.tcl`). **This is a runtime load**:
   msgcat reads the file lazily on first lookup after a locale change, so
   swapping needs no restart.
4. **Resolve.** `::vi18n::mc` → msgcat. Missing key → the key itself, same
   convention as the engine.

### Both

One TSV per translator feeds both catalogs:
`scripts/i18n-build.sh <id>` reads `<id>-part-*.tsv`, then
`<id>-symbolic.tsv`, then any review corrections `<id>-fix-*.tsv` (last
wins), and writes `msgs/<id>.msg` AND `locales/<id>.yml` from the same rows.
The generators give every value the English source's edge spaces (layout,
not language).

**Where the TSVs live is a known gap:** `~/i18n-work`, outside the repo.
The shipped catalogs are complete on their own, but they cannot be rebuilt
from a fresh clone; `tsv2msg`/`tsv2yml` merge a partial TSV over the
shipped catalog, which is how a correction round is applied without them.

**Checking what a catalog SAYS:** `tclsh scripts/i18n-catalog-check.tcl`
compares every row of every shipped catalog with its English source —
en.yml's TEXT for a symbolic key, never the key's name — for placeholders
(`%s %d %N$s %{name}`), edge spaces and bracket balance. Gated by
`i18ntest` (desktop) and `i18n::tests::every_catalog_row_keeps_its_sources_
placeholders` (engine, in `cargo test`). A value identical to English is
counted, not failed.

## The asymmetry that matters

| | engine `.yml` | desktop `.msg` |
|---|---|---|
| load | runtime, lazy (en compiled in) | runtime, lazy |
| unreachable entry costs | a parse + memory in the process that loads the locale | nothing |
| can be swapped live | yes (per request, or `set_active`) | yes (`mclocale`) |
| trim safe? | yes, to engine-reachable keys | **no** — its completeness is pinned |

An entry no Rust code can reach is never read, so the engine catalog is
filtered to the engine-reachable key set (`scripts/i18n-tsv2yml.sh`, 4th
arg) while the `.msg` catalog keeps every key: **3.3 MB → 448 KB** of
catalogs on disk (42022 → 6144 entries across 47 locales). That trim was
first made to cut compile memory, back when every entry was compiled in.

**What the compile-time embed cost, measured 2026-09-28** (veles-core
alone, release, `-j 8`, nothing else building, per-rustc peak RSS from a
`RUSTC_WRAPPER` running GNU time): the 47 catalogs expanded into ~11,400
`map.insert` statements in one function, and cost **+0.34 GB** on the
library (3.57 vs 3.23 GB with en.yml alone; 48 vs 43 s) and **+0.60 GB**
on its test binary (5.79 vs 5.19 GB; 139 vs 137 s), in every build of the
crate. About a tenth of the largest compiles — real, but not what made
the flake check's 16-17 GB system peak; see `docs/build-memory.md` for
that.

## The rule that keeps the swap working

**If a `-text` (or an accessible name) carries format args, it must be
registered — `::vi18n::label` / `::vi18n::aname` — not assigned inline.**

The reason is mechanical rather than stylistic. A live swap re-texts
widgets by walking the tree and reverse-mapping each widget's *current
text* back to a catalog key. That only works while the text on screen IS
a catalog value:

| shape | on screen | sweep finds it? |
|---|---|---|
| `-text [::vi18n::mc {Send}]` | `Send` (a key) | yes |
| `-text [::vi18n::mc {failed: %s} $e]` | `failed: timeout` | **no** |
| `-text [format [::vi18n::mc {failed: %s}] $e]` | `failed: timeout` | **no** |

`failed: timeout` is not a key and never will be, so a widget written
that way keeps the old language for the rest of the session — silently,
because the English fallback looks correct while only English is in play.

`::vi18n::label $w {failed: %s} $e` avoids that: it records the key and
its args, asks msgcat for the text, and remembers what it wrote.
`relabel()` re-renders from the key on a swap.

Two consequences worth knowing before you touch this:

- **A registered widget can be overwritten.** That is a status label's
  normal life: registered with a catalog message, then overwritten with
  live state by a later turn. `relabel()` re-texts a widget only while it
  still holds the text `label()` wrote, and drops the registration once
  something else claims it. Without that guard a swap resurrects a stale
  message over whatever the user was reading.
- **One owner per widget.** `resweep()` skips a registered widget —
  `relabel()` already handled it, and letting both run would map
  new-locale text back through the old locale's index.

Both rules are checked rather than remembered: `i18ntest` fails on any
new `-text`/`va11y::name` that carries format args without registering,
and on a registered widget that gets resurrected. A site that genuinely
cannot register — a **treeview item** (`$t insert … -text`) is not a
widget option and has no registration channel — states its reason with
`;# i18n-exempt: <why>` and is listed in the scan's exemptions.

## The runtime load, and the failure mode it traded for

This section used to list escape hatches for the compile-time embed —
trim harder, split the crate, load at runtime — and warned against the
third as "a memory optimization alone", because it turns a build-time cost
into a runtime failure mode: a shipped binary could now be missing its
strings. The user took it on 2026-09-28 ("embedding this in the binaries
47 times seems objectively stupid"), and the failure mode is handled this
way rather than avoided:

- **Missing directory:** English, from the compiled-in `en.yml` — every
  symbolic key still renders as text, never as its id. `available()` is
  then just `en`, so the picker offers nothing it cannot load.
- **Missing or corrupt file:** that locale renders English per key; a
  warning names the file once. Other locales are unaffected.
- **Installed builds:** the flake installs the catalogs at
  `$out/share/veles/locales` and the wrapper sets `VELES_LOCALES_DIR` to
  them; a bare `$out/bin/veles` (the unwrapped binary) finds the same
  directory through the `share/` fallback.
- **Pinned by** `i18n::tests::catalogs_are_read_at_runtime_and_english_is_built_in`
  (a catalog written after the build is read; a broken one renders English;
  no directory renders English) and
  `the_catalog_directory_follows_env_then_install_then_source`.

The Android build ships the directory in its assets and sets
`VELES_LOCALES_DIR` to where it unpacks them.

### Related gotcha: the extraction is the other half

A trim is only safe if the key set it filters against is complete. The
extractor (`scripts/i18n-keys.tcl`) finds keys four ways, and each was
added because the previous set silently lost translations:

- string literals in `lookup(...)` / `i18n!(...)` calls;
- the `help_entries` table's descriptions (reached as `lookup(desc, …)`) —
  `rust_help_descs_in`, since 2026-09-25. Before that this line was
  wrong: the descriptions were only in the ENGINE-key superset the trim
  filters against, never in the translator key set, so all 76 sat in no
  catalog and `/help` rendered English in every locale;
- **`en.yml`'s symbolic keys**, which reach `lookup()` through a match arm
  (`("tasks_header", "tasks_none")`) and which no call-site walk can see;
- the desktop's `::vi18n::label` and `::vi18n::aname` calls (added
  2026-09-21, when the runtime-text round introduced those shapes — without
  the clause the keys they carry drop out of the set the moment a site is
  converted).

**The extractor matches `mc\s*{`, so a key must sit on the SAME LINE as
its call.** Not a style rule: a multi-line `[format [::vi18n::mc \` +
newline + `{key}]` was invisible for weeks, and the key it hid
(`admin default zone: …`) was in no catalog in 47 locales until 2026-09-21.
A key the extractor cannot see is not a missing translation anyone reports —
it renders English through the fallback, which looks correct while only
English is in play.

That is why the checks are worth more than the rules: `uitest` compares the
catalog against the extractor in BOTH directions (a key with no entry, and
an entry no key uses), and `i18ntest`'s completeness contract fails if any
locale is short of `en.msg`. Either one catches a collapse; neither depends
on someone remembering to look.

The second rule, for the same reason: an extractor that misses keys and a
generator that filters against them are one system. If a future key shape is
added, it needs a clause here, and the guard that catches a regression is
`i18n::tests::catalog_keys_and_code_do_not_drift` plus the count guard in
`i18n-build.sh` (a collapsed key set is refused rather than applied).