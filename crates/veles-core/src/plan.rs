//! Plan mode — research freely, change nothing, come back with a plan.
//!
//! USER: *"plan mode"* — the claude-code sense. The agent reads and
//! researches as much as it likes, changes nothing, and presents a plan
//! that has to be approved before anything executes.
//!
//! WHAT SHAPE THAT ACTUALLY IS
//!
//! Taken from the mode this was asked to copy, and worth stating because
//! two of these are easy to get backwards:
//!
//!   - **Reads are free.** Search, file reads, web research, status. The
//!     mode exists to research; a mode that cannot research is a
//!     different, worse mode.
//!   - **Shell is allowed for READ-ONLY commands, not banned.** `git
//!     log`, `grep`, `sed -n`, `ls` are how you find anything out about
//!     a repository. Banning `terminal` outright would gut the feature.
//!   - **Nothing that changes the world.** No edits, no commits, no
//!     installs, no cron, no delegation, no spend.
//!   - **Exactly one writable path**: the session's plan file.
//!   - **The restriction supersedes instructions.** The model cannot be
//!     argued out of it mid-turn, and no tool it can call lifts it
//!     except the one that asks a human.
//!
//! WHY THIS IS NOT `approval::classify`
//!
//! `approval.rs` has a table of dangerous-command patterns, and it is
//! the WRONG POLARITY for this. "Not in the dangerous list" is not
//! "read-only": `git commit`, `npm install`, `mv`, `cargo build` and
//! `echo x > f` are all absent from that table and all change things.
//! A deny-list cannot answer this question; only an allow-list can.
//!
//! So the allow-list below is deliberately NARROW AND REFUSING rather
//! than clever. A false refusal costs the user one `/plan off`. A false
//! allow is a mutation in a mode that promised there would be none, and
//! that is the only failure here that matters.

use serde_json::Value;

/// Session meta key prefix, exactly the shape `/failover` uses.
pub const META_PREFIX: &str = "plan_mode:";
/// Where the last PRESENTED plan text lives (meta key prefix + sid).
/// `present_plan` is an ask-channel message: once answered it exists in
/// no history row, so a surface that wants "what was the plan" has
/// nothing to read. Writing it here is what lets the desktop's Tasks
/// pane show it after the fact.
pub const PLAN_TEXT_PREFIX: &str = "presented_plan:";

/// The last plan this session presented, if any.
pub fn presented_plan(store: &crate::store::Store, session_id: &str) -> Option<String> {
    store.meta_get(&format!("{PLAN_TEXT_PREFIX}{session_id}"))
}

// ── `/plan <verb>` and the tool's `action` ───────────────────────────

/// What `/plan` (and the read-back tool) was ASKED to do, past the mode
/// words. Distinct from [`Mode`] because `on`/`off` MOVE the session and
/// these READ it — a verb that changes something belongs to `set_plan_mode`,
/// which announces the transition to the transcript; a read does not, and
/// must not be made to look like it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// Show the plan this session presented.
    Show,
    /// Show the TODO list instead (the model-facing name).
    Tasks,
    /// Show the presented plan AND the TODO list.
    Status,
}

impl Verb {
    /// Parse a `/plan` argument that is not a [`Mode`] word.
    ///
    /// Deliberately keyed on whole trimmed strings, not prefixes: the
    /// command door asks THIS table first and falls through to
    /// `set_plan_mode`, so a verb that claimed a mode spelling (or a
    /// prefix overlapping one) would turn `/plan off` into a read and
    /// leave the session in the mode the user meant to leave.
    ///
    /// `Mode::parse` claims "plan" (the mode word for ON), so this does
    /// NOT claim it: `/plan plan` stays a mode set even though the
    /// tool's `action` enum — which has no mode path to collide with —
    /// reads "plan" as the plan. Each table is complete for the door it
    /// serves, and the disjointness is pinned by
    /// `the_verb_table_and_the_mode_table_are_disjoint`.
    pub fn parse(s: &str) -> Option<Verb> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "show" | "view" | "read" | "current" | "what" | "the-plan" | "show-plan"
            | "showplan" => Verb::Show,
            "tasks" | "todo" | "todos" | "task" | "list" => Verb::Tasks,
            "status" | "all" | "both" | "state" | "summary" => Verb::Status,
            _ => return None,
        })
    }
}

// ── the read-back (one renderer, two doors) ──────────────────────────

/// Render what a session's planning state IS: the plan it presented,
/// the TODO list, and which mode it sits in.
///
/// ONE function serves the model-facing tool (`show_plan`) and the
/// human-facing `/plan show`, for the same reason `CompactOutcome::report`
/// exists: two spellings of "what was the plan" drift, and the drift is
/// invisible until a model reads one renderer's answer and a user reads
/// the other's.
///
/// The header names the session when `session_id` is NOT the caller's
/// own (`session` was passed, or `/plan show <id>`): an answer about
/// another conversation must say so, or it reads as an answer about
/// this one.
pub fn render_state(
    store: &crate::store::Store,
    session_id: &str,
    own_session: &str,
    verb: Verb,
) -> String {
    let mut out = if session_id == own_session {
        String::from("this session — ")
    } else {
        format!("session {session_id} — ")
    };
    out.push_str(match mode_for(store, session_id) {
        Mode::Plan => "plan mode is ON (nothing may change yet).\n",
        Mode::Act => "plan mode is off.\n",
    });

    if verb != Verb::Tasks {
        let plan = store.meta_get_pair(PLAN_TEXT_PREFIX, session_id);
        match plan.as_deref().map(str::trim) {
            Some(p) if !p.is_empty() => {
                out.push_str(
                    "\nTHE PRESENTED PLAN (what `present_plan` last showed the user):\n\n",
                );
                out.push_str(p);
                out.push('\n');
            }
            _ => out.push_str(
                "\nno plan has been PRESENTED in this session — `present_plan` is what \
                 records one. If the work was planned in prose instead, it is in the \
                 transcript, not here.\n",
            ),
        }
    }

    if verb != Verb::Show {
        let items = store.todos_list(session_id).unwrap_or_default();
        out.push_str("\nTHE TASK LIST (the `todo` tool's list):\n\n");
        out.push_str(&crate::todo::render_list(&items));
        out.push('\n');
    }
    out
}

// ── the read-only command allow-list ─────────────────────────────────
//
// WHY A SPLITTER (USER 2026-10-10: "Why does plan mode unconditionally
// disable terminal? Some commands seem to work though"). The first cut
// refused any command that CONTAINED `&&`, `|`, `;`, `>` or `<` before it
// read a single word. Models write `cd ~/repo && git log --oneline | head`
// almost every time, so in practice nearly every call was refused: all 45
// plan-mode `terminal` refusals in the live state.db had one of those
// shapes, and the refusal said only "not a read-only command". The models
// concluded the terminal was off; the bare `git log -5` they sometimes
// tried was the part that "seemed to work".
//
// So the command is now SPLIT the way a shell splits it — quotes, escapes,
// `&&`/`||`/`;`/`|`/newline — and every part must be a read on its own.
// What cannot be judged by reading words stays refused outright: `$(…)`,
// backticks, process substitution, heredocs, subshells, a lone `&`, and
// any redirect that is not to /dev/null or between descriptors.

/// Command heads that cannot change anything on their own.
///
/// Absent on purpose, and each for a reason someone will eventually want
/// to argue with: `env` and `xargs` and `sh`/`bash` (wrappers: whatever
/// they run is the real command and this function would be judging the
/// wrapper), `curl`/`wget` (a GET is a read, but the same binary writes
/// files with `-o` and posts bodies with `-d`; web_search and web_extract
/// are the sanctioned way to reach the network), `awk` (`system()` and
/// `print > file`), `tee` (writes), `xxd` (a second positional is an
/// output file), `ldd` (may execute the binary it inspects).
///
/// `echo` and `printf` are here since 2026-10-10: they were kept out as
/// "the natural half of `echo x > f`", but redirects are now judged one by
/// one (`redirect_ok`), so `echo x > f` is refused for its redirect.
const READ_ONLY_HEADS: &[&str] = &[
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "stat",
    "file",
    "du",
    "df",
    "pwd",
    "whoami",
    "date",
    "uname",
    "hostname",
    "printenv",
    "which",
    "type",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "grep",
    "egrep",
    "fgrep",
    "zgrep",
    "rg",
    "ag",
    "ack",
    "find",
    "fd",
    "locate",
    "tree",
    "diff",
    "cmp",
    "comm",
    "sort",
    "uniq",
    "cut",
    "tr",
    "column",
    "nl",
    "tac",
    "rev",
    "paste",
    "join",
    "strings",
    "od",
    "hexdump",
    "zcat",
    "jq",
    "yq",
    "md5sum",
    "sha1sum",
    "sha256sum",
    "sha512sum",
    "b2sum",
    "cksum",
    "echo",
    "printf",
    "test",
    "[",
    "seq",
    "true",
    "false",
    "ps",
    "pgrep",
    "id",
    "groups",
    "uptime",
    "free",
    "nproc",
    "lsblk",
    "findmnt",
    "lscpu",
    "journalctl",
    "ss",
];

/// Flags that turn a read-only head into a mutation. Checked for every
/// head above, whatever position they land in: `find` is listed because
/// its DEFAULT is a read, but `-delete` removes files and `-exec` runs
/// arbitrary commands — the allow-list's "certain" standard cannot
/// admit either. Matching is [`flag_hit`]'s: `--long=value` and a short
/// flag inside a cluster (`-ro`) count, as the program would read them.
const HEAD_WRITE_FLAGS: &[(&str, &[&str])] = &[
    (
        "find",
        &[
            "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fdelete", "-fprint", "-fprint0",
            "-fprintf", "-fls",
        ],
    ),
    // `fd`/`fdfind` execute per hit with -x/--exec, exactly find's shape.
    ("fd", &["-x", "-X", "--exec", "--exec-batch"]),
    ("yq", &["-i", "--inplace"]),
    ("sort", &["-o", "--output"]),
    ("date", &["-s", "--set"]),
    // ripgrep runs a preprocessor command per file.
    ("rg", &["--pre"]),
    // tree writes its listing to a file.
    ("tree", &["-o"]),
    // file compiles a magic database next to its input.
    ("file", &["-C", "--compile"]),
    // ss kills the sockets it matches.
    ("ss", &["-K", "--kill"]),
    (
        "journalctl",
        &[
            "--vacuum-size",
            "--vacuum-time",
            "--vacuum-files",
            "--rotate",
            "--flush",
            "--sync",
            "--relinquish-var",
            "--smart-relinquish-var",
            "--setup-keys",
            "--update-catalog",
        ],
    ),
];

/// Subcommand allow-lists for tools whose head says nothing about
/// whether the call mutates. `git` is the obvious one: `git log` and
/// `git push --force` share a binary.
const SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        "git",
        &[
            "log",
            "show",
            "status",
            "diff",
            "blame",
            "rev-parse",
            "rev-list",
            "merge-base",
            "describe",
            "cat-file",
            "ls-files",
            "ls-tree",
            "ls-remote",
            "shortlog",
            "grep",
            "config",
            "branch",
            "tag",
            "remote",
            "reflog",
            "stash",
        ],
    ),
    (
        "cargo",
        &["tree", "metadata", "search", "--version", "help"],
    ),
    (
        "nix",
        &["eval", "show-derivation", "path-info", "why-depends"],
    ),
    (
        "systemctl",
        &[
            "status",
            "show",
            "list-units",
            "list-unit-files",
            "is-active",
            "cat",
        ],
    ),
    ("docker", &["ps", "images", "inspect", "logs"]),
    (
        "kubectl",
        &["get", "describe", "logs", "explain", "api-resources"],
    ),
    ("npm", &["ls", "list", "view", "outdated"]),
    ("pip", &["list", "show", "freeze"]),
];

/// Subcommands that read in one form and write in another. The
/// allow-list above lets the head through; these flags take it back.
const SUBCOMMAND_WRITE_FLAGS: &[(&str, &str, &[&str])] = &[
    (
        "git",
        "config",
        &["--unset", "--add", "--replace-all", "--edit", "-e"],
    ),
    (
        "git",
        "branch",
        &[
            "-d",
            "-D",
            "-m",
            "-M",
            "--delete",
            "--move",
            "--set-upstream-to",
        ],
    ),
    ("git", "tag", &["-d", "-a", "-s", "--delete"]),
    (
        "git",
        "remote",
        &["add", "remove", "rm", "rename", "set-url", "prune"],
    ),
    ("git", "reflog", &["delete", "expire"]),
    (
        "git",
        "stash",
        &["push", "pop", "apply", "drop", "clear", "save", "store"],
    ),
    // `--output=<file>` writes the log/diff to a file.
    ("git", "log", &["--output"]),
    ("git", "show", &["--output"]),
    ("git", "diff", &["--output"]),
    // `git grep -O<cmd>` opens the hits in a command it runs.
    ("git", "grep", &["-O", "--open-files-in-pager"]),
];

/// Does `word` invoke `flag`, the way the program would read it?
///
/// - exact: `-delete`, `--output`;
/// - a long flag with its value attached: `--output=f`;
/// - a two-character short flag with its value attached (`-ofile`) or
///   inside a cluster of short flags (`-ro`, `-nO`).
///
/// The cluster rule over-matches (`date -Iseconds` reads as carrying
/// `-s`). That is the cheap direction: a false refusal costs a reworded
/// command, a missed write costs the mode its promise.
fn flag_hit(word: &str, flag: &str) -> bool {
    if word == flag {
        return true;
    }
    if let Some(long) = flag.strip_prefix("--") {
        return word
            .strip_prefix("--")
            .and_then(|w| w.strip_prefix(long))
            .is_some_and(|tail| tail.starts_with('='));
    }
    let short = flag.len() == 2 && flag.starts_with('-');
    if short && word.starts_with('-') && !word.starts_with("--") {
        if word.starts_with(flag) {
            return true;
        }
        let letter = flag.as_bytes()[1] as char;
        let cluster = &word[1..];
        return cluster.chars().all(|c| c.is_ascii_alphabetic()) && cluster.contains(letter);
    }
    false
}

/// One shell word after quote removal. A `$VAR` stays in the text as
/// written: harmless in an argument, and a command NAME holding one never
/// equals an allowed head, so it is refused like any unknown command.
#[derive(Debug)]
struct Word {
    text: String,
}

/// Split a command line into its simple commands, the way a shell would,
/// or say why it cannot be judged from its words at all.
///
/// Separators are `&&`, `||`, `;`, `|`, `|&` and newlines; every part they
/// separate must pass [`judge_segment`] on its own. Quotes and backslashes
/// are honoured, so `grep -E "a|b"` is one command, not two.
fn split_command(cmd: &str) -> Result<Vec<Vec<Word>>, String> {
    let s = cmd.replace("\\\r\n", " ").replace("\\\n", " ");
    let c: Vec<char> = s.chars().collect();
    let mut segs: Vec<Vec<Word>> = vec![Vec::new()];
    let mut cur = String::new();
    let mut in_word = false;
    let mut i = 0;

    fn flush(segs: &mut [Vec<Word>], cur: &mut String, in_word: &mut bool) {
        if *in_word {
            if let Some(seg) = segs.last_mut() {
                seg.push(Word {
                    text: std::mem::take(cur),
                });
            }
        }
        cur.clear();
        *in_word = false;
    }

    while i < c.len() {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        match ch {
            ' ' | '\t' | '\r' => {
                flush(&mut segs, &mut cur, &mut in_word);
                i += 1;
            }
            '\n' | ';' => {
                flush(&mut segs, &mut cur, &mut in_word);
                segs.push(Vec::new());
                i += 1;
            }
            '#' if !in_word => {
                // A comment runs to the end of the line; the newline
                // itself still separates.
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '\'' => {
                in_word = true;
                i += 1;
                loop {
                    match c.get(i) {
                        None => return Err("an unclosed quote".into()),
                        Some('\'') => {
                            i += 1;
                            break;
                        }
                        Some(&x) => {
                            cur.push(x);
                            i += 1;
                        }
                    }
                }
            }
            '"' => {
                in_word = true;
                i += 1;
                loop {
                    match c.get(i) {
                        None => return Err("an unclosed quote".into()),
                        Some('"') => {
                            i += 1;
                            break;
                        }
                        Some('\\') => match c.get(i + 1) {
                            Some(&x) if matches!(x, '"' | '\\' | '$' | '`') => {
                                cur.push(x);
                                i += 2;
                            }
                            _ => {
                                cur.push('\\');
                                i += 1;
                            }
                        },
                        Some('`') => {
                            return Err("a backtick runs a command inside the command".into())
                        }
                        Some('$') => {
                            if c.get(i + 1) == Some(&'(') {
                                return Err("`$(…)` runs a command inside the command".into());
                            }
                            cur.push('$');
                            i += 1;
                        }
                        Some(&x) => {
                            cur.push(x);
                            i += 1;
                        }
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(x) = next {
                    cur.push(x);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            '`' => return Err("a backtick runs a command inside the command".into()),
            '$' => {
                if next == Some('(') {
                    return Err("`$(…)` runs a command inside the command".into());
                }
                in_word = true;
                cur.push('$');
                i += 1;
            }
            '(' | ')' => return Err("a subshell `( … )` cannot be judged from its words".into()),
            '|' => {
                flush(&mut segs, &mut cur, &mut in_word);
                segs.push(Vec::new());
                i += if matches!(next, Some('|') | Some('&')) {
                    2
                } else {
                    1
                };
            }
            '&' if next == Some('&') => {
                flush(&mut segs, &mut cur, &mut in_word);
                segs.push(Vec::new());
                i += 2;
            }
            '&' if next == Some('>') => {
                flush(&mut segs, &mut cur, &mut in_word);
                i = redirect(&c, i, "")?;
            }
            '&' => return Err("a lone `&` sends part of the command to the background".into()),
            '>' | '<' => {
                // `2>` — a word of digits right before the operator is the
                // descriptor, not an argument.
                let fd = if in_word && !cur.is_empty() && cur.chars().all(|d| d.is_ascii_digit()) {
                    let fd = std::mem::take(&mut cur);
                    in_word = false;
                    fd
                } else {
                    flush(&mut segs, &mut cur, &mut in_word);
                    String::new()
                };
                i = redirect(&c, i, &fd)?;
            }
            _ => {
                in_word = true;
                cur.push(ch);
                i += 1;
            }
        }
    }
    flush(&mut segs, &mut cur, &mut in_word);
    segs.retain(|s| !s.is_empty());
    Ok(segs)
}

/// Judge one redirect starting at `c[i]` (`>`, `<` or the `&` of `&>`)
/// and return the index just past its target. Only the shapes that move
/// output away or between descriptors pass: `2>&1`, `>&2`, `N>/dev/null`,
/// `&>/dev/null`, `</dev/null`. Anything else would write a file, or feed
/// the command from somewhere this check does not read.
fn redirect(c: &[char], mut i: usize, fd: &str) -> Result<usize, String> {
    let at = |k: usize| c.get(k).copied();
    let mut op = String::from(fd);
    let input = at(i) == Some('<');
    if at(i) == Some('&') {
        op.push('&');
        i += 1;
    }
    let first = at(i).unwrap_or('>');
    op.push(first);
    i += 1;
    match (first, at(i)) {
        (_, Some('(')) => {
            return Err("process substitution runs a command inside the command".into())
        }
        ('<', Some('<')) => {
            return Err("a heredoc or here-string cannot be judged from its words".into())
        }
        ('>', Some('>')) | ('>', Some('|')) => {
            op.push(at(i).unwrap_or('>'));
            i += 1;
        }
        (_, Some('&')) if !op.starts_with('&') => {
            // Descriptor duplication: `2>&1`, `>&2`, `<&0`, `>&-`.
            op.push('&');
            i += 1;
            let start = i;
            while matches!(at(i), Some(d) if d.is_ascii_digit()) {
                i += 1;
            }
            if i > start || at(i) == Some('-') {
                if at(i) == Some('-') {
                    i += 1;
                }
                return Ok(i);
            }
            // `>& file` is "both streams to a file": judged by its target.
        }
        _ => {}
    }
    while matches!(at(i), Some(' ') | Some('\t')) {
        i += 1;
    }
    let mut target = String::new();
    while let Some(x) = at(i) {
        if x.is_whitespace() || matches!(x, ';' | '|' | '&' | '<' | '>' | '(' | ')') {
            break;
        }
        if x == '\'' || x == '"' {
            i += 1;
            while let Some(y) = at(i) {
                i += 1;
                if y == x {
                    break;
                }
                target.push(y);
            }
            continue;
        }
        target.push(x);
        i += 1;
    }
    if target == "/dev/null" {
        return Ok(i);
    }
    if target.is_empty() {
        return Err(format!("the redirect `{op}` has no target"));
    }
    Err(if input {
        format!(
            "the redirect `{op} {target}` feeds the command from a file (pass the file as an \
             argument instead)"
        )
    } else {
        format!("the redirect `{op} {target}` writes a file")
    })
}

/// A segment, shown back in a refusal: its words, clipped so a long
/// pipeline does not swamp the message.
fn show(words: &[&str]) -> String {
    let s = words.join(" ");
    if s.chars().count() > 80 {
        format!("{}…", s.chars().take(80).collect::<String>())
    } else {
        s
    }
}

/// `sed`, but only as a printer: `-n` set, no in-place edit, no script
/// file, and every script a print of lines or ranges — `N,Mp`, `N,$p`,
/// `/re/p`, `/a/,/b/p`, several joined by `;`. sed's `w` and `e`
/// commands write files and run commands, and a grammar this narrow
/// cannot contain either.
fn sed_print_only(rest: &[&str]) -> bool {
    static PRINT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = PRINT.get_or_init(|| {
        let addr = r"(?:[0-9]+|\$|/[^/]*/)";
        regex::Regex::new(&format!(r"^\s*(?:{addr}(?:\s*,\s*{addr})?)?\s*p\s*$")).expect("valid")
    });
    let mut quiet = false;
    let mut scripts: Vec<&str> = Vec::new();
    let mut positionals: Vec<&str> = Vec::new();
    let mut k = 0;
    while k < rest.len() {
        let w = rest[k];
        if w == "-e" || w == "--expression" {
            match rest.get(k + 1) {
                Some(s) => scripts.push(s),
                None => return false,
            }
            k += 2;
            continue;
        }
        if let Some(s) = w.strip_prefix("--expression=") {
            scripts.push(s);
        } else if w == "-n" || w == "--quiet" || w == "--silent" {
            quiet = true;
        } else if w.starts_with("--in-place") || w == "-f" || w.starts_with("--file") {
            return false;
        } else if w.starts_with("--") {
            // --regexp-extended, --null-data, --separate, --debug…
        } else if let Some(cluster) = w.strip_prefix('-').filter(|c| !c.is_empty()) {
            if cluster.contains('i') || cluster.contains('f') {
                return false;
            }
            if let Some(after) = cluster
                .strip_prefix('e')
                .or_else(|| cluster.find('e').map(|p| &cluster[p + 1..]))
            {
                // `-ne SCRIPT` or `-e1p`: the script is attached or next.
                if after.is_empty() {
                    match rest.get(k + 1) {
                        Some(s) => scripts.push(s),
                        None => return false,
                    }
                    k += 1;
                } else {
                    scripts.push(after);
                }
            }
            if cluster.contains('n') {
                quiet = true;
            }
        } else {
            positionals.push(w);
        }
        k += 1;
    }
    if scripts.is_empty() {
        // Without -e the first positional is the script, the rest files.
        match positionals.first() {
            Some(s) => scripts.push(s),
            None => return false,
        }
    }
    quiet
        && scripts.iter().all(|s| {
            s.split(';')
                .filter(|p| !p.trim().is_empty())
                .all(|p| re.is_match(p))
        })
}

/// Strip git's global options so the subcommand is the first word, or
/// refuse the ones that can run something: `-c` sets config for the
/// call, and config can name a command (`alias.x=!…`, `core.pager`);
/// `--exec-path=` points git at other binaries.
fn git_without_globals<'a>(rest: &[&'a str]) -> Result<Vec<&'a str>, String> {
    let mut k = 0;
    while k < rest.len() {
        let w = rest[k];
        if !w.starts_with('-') {
            break;
        }
        if w == "-c" || w.starts_with("--config-env") || (w.starts_with("-c") && w.len() > 2) {
            return Err(
                "`git -c` sets config for the call, and config can name a command to run".into(),
            );
        }
        if w.starts_with("--exec-path=") {
            return Err("`git --exec-path=` points git at other programs".into());
        }
        k += if matches!(w, "-C" | "--git-dir" | "--work-tree" | "--namespace") {
            2
        } else {
            1
        };
    }
    Ok(rest.get(k..).unwrap_or(&[]).to_vec())
}

/// Is this one simple command (no separators, no redirects left) a read?
/// `Err` carries the reason, naming the command.
fn judge_segment(words: &[Word]) -> Result<(), String> {
    let Some(head_word) = words.first() else {
        return Ok(());
    };
    // `command grep …` is grep with shell functions and aliases bypassed
    // (zsh users write it to dodge an aliased grep); `command -v x` only
    // looks a name up. Judge what it runs, not the builtin.
    if head_word.text == "command" {
        let mut k = 1;
        while let Some(w) = words.get(k) {
            match w.text.as_str() {
                "-v" | "-V" | "-pv" | "-vp" | "-pV" | "-Vp" => return Ok(()),
                "-p" => k += 1,
                _ => break,
            }
        }
        return judge_segment(&words[k..]);
    }
    let all: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
    let not_read_only = || Err(format!("`{}` is not a read-only command", show(&all)));
    let head = head_word.text.as_str();
    // A path-qualified head (./script, /usr/bin/rm) is not judgeable
    // from its name — refuse. `/usr/bin/ls` losing to this is the
    // false-refusal the docstring accepts.
    if head.contains('/') {
        return Err(format!(
            "`{head}` is a path, and a command named by its path cannot be judged by its name"
        ));
    }
    let mut rest: Vec<&str> = all[1..].to_vec();
    // `cd` changes nothing but this call's own shell: the terminal tool
    // runs one shell per command.
    if head == "cd" {
        return if rest.iter().filter(|w| !w.starts_with('-')).count() <= 1 {
            Ok(())
        } else {
            not_read_only()
        };
    }
    if head == "sed" {
        return if sed_print_only(&rest) {
            Ok(())
        } else {
            Err(format!(
                "`{}` is not a print-only sed (plan mode takes `sed -n 'N,Mp' file` and \
                 `sed -n '/re/p' file`)",
                show(&all)
            ))
        };
    }
    if head == "git" {
        rest = git_without_globals(&rest)?;
        // `git --version`, `git --exec-path`, `git -C x`: global flags
        // alone print something and run no subcommand.
        if rest.is_empty() {
            return Ok(());
        }
    }
    // Flags that turn a read-only head into a mutation — whatever
    // position they land in (`find . -name x -delete` is still a
    // delete).
    for (h, flags) in HEAD_WRITE_FLAGS {
        if *h == head {
            if let Some(w) = rest.iter().find(|w| flags.iter().any(|f| flag_hit(w, f))) {
                return Err(format!(
                    "`{w}` makes `{}` write a file or run a command",
                    show(&all)
                ));
            }
        }
    }
    // Positional arguments that are writes by shape:
    // - `hostname newname` sets the hostname (a flag-less positional);
    // - `uniq in out` writes `out`;
    // - `git branch newfeature` / `git tag v1` CREATE (their read forms
    //   are flag-shaped: --list / -l);
    // - `git config key value` writes (one positional reads, two write).
    let positionals: Vec<&str> = rest
        .iter()
        .copied()
        .filter(|w| !w.starts_with('-'))
        .collect();
    if head == "hostname" && !positionals.is_empty() {
        return not_read_only();
    }
    if head == "uniq" && positionals.len() > 1 {
        return Err(format!("`{}` writes its second file", show(&all)));
    }
    if READ_ONLY_HEADS.contains(&head) {
        return Ok(());
    }
    let Some((_, allowed)) = SUBCOMMANDS.iter().find(|(h, _)| *h == head) else {
        return not_read_only();
    };
    // The first word that is not a flag is the subcommand.
    let Some(&sub) = positionals.first() else {
        // `cargo --version` — a bare flag can still be listed.
        return if rest.iter().any(|w| allowed.contains(w)) {
            Ok(())
        } else {
            not_read_only()
        };
    };
    if !allowed.contains(&sub) {
        return not_read_only();
    }
    // …and the write-flag takebacks.
    for (h, s, flags) in SUBCOMMAND_WRITE_FLAGS {
        if *h == head && *s == sub {
            if let Some(w) = rest.iter().find(|w| flags.iter().any(|f| flag_hit(w, f))) {
                return Err(format!(
                    "`{w}` makes `{}` write a file or run a command",
                    show(&all)
                ));
            }
        }
    }
    // Positional-write shapes for subcommand tools: a word AFTER the
    // subcommand that is not a flag is a target, not a read.
    match (head, sub) {
        ("git", "branch" | "tag") if positionals.len() > 1 => return not_read_only(),
        // `git config key` reads; `git config key value` writes.
        ("git", "config") if positionals.len() > 2 => return not_read_only(),
        // Bare `git stash` stashes the working tree — a write. The read
        // forms all carry a subcommand word (list/show), which keeps
        // positionals ≥ 2.
        ("git", "stash") if positionals.len() <= 1 => return not_read_only(),
        _ => {}
    }
    Ok(())
}

/// Why this shell command is NOT certain to be read-only, or `Ok` when
/// every part of it is.
///
/// Answers `Ok` only when it is CERTAIN. Everything unrecognised is an
/// `Err`, which in plan mode means "refused, use `/plan off`" — and the
/// reason names the part that was refused, so the model can redo just
/// that part as a read instead of concluding the terminal is off.
pub fn judge_command(cmd: &str) -> Result<(), String> {
    let segs = split_command(cmd)?;
    if segs.is_empty() {
        return Err("the command is empty".into());
    }
    for seg in &segs {
        judge_segment(seg)?;
    }
    Ok(())
}

/// Is this shell command incapable of changing anything?
pub fn read_only_command(cmd: &str) -> bool {
    judge_command(cmd).is_ok()
}

// ── per-tool policy ──────────────────────────────────────────────────

/// Tools plan mode never allows, whatever their arguments: they change
/// the world, cost money, or both.
///
/// `present_plan` IS DELIBERATELY ABSENT and must stay absent: it is the
/// one tool whose job is to ask a human to lift the mode, and refusing
/// it would make plan mode inescapable except by typing `/plan off`.
/// Adding it here is the way to break the approval exit.
const REFUSED_TOOLS: &[(&str, &str)] = &[
    ("cronjob", "it would schedule work"),
    ("delegate_task", "it would start another agent"),
    ("a2a", "it would talk to a peer agent"),
    ("generate_image", "it would spend on generation"),
    ("generate_video", "it would spend on generation"),
    ("text_to_speech", "it would spend on synthesis"),
    ("computer_use", "it would drive the desktop"),
    ("skill_manage", "it would change the skill set"),
    ("open_preview", "it would change what is on screen"),
    ("focus_pane", "it would change what is on screen"),
];

/// Browser verbs that act on the page rather than read it.
const REFUSED_BROWSER: &[&str] = &["browser_click", "browser_type", "browser_press"];

/// Would plan mode refuse this call? `Some(reason)` if so.
///
/// `plan_path` is the ONE writable path — the session's plan file, the
/// direct analogue of the single file claude-code's plan mode may edit.
/// `None` means there is no plan file yet, so nothing is writable.
pub fn refuses(tool: &str, args: &Value, plan_path: Option<&str>) -> Option<String> {
    if let Some((_, why)) = REFUSED_TOOLS.iter().find(|(t, _)| *t == tool) {
        return Some((*why).to_string());
    }
    if REFUSED_BROWSER.contains(&tool) {
        return Some("it would act on the page rather than read it".into());
    }
    match tool {
        "write_file" | "patch" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            match plan_path {
                Some(p) if paths_match(p, path) => None,
                _ => Some("it would edit a file".into()),
            }
        }
        "terminal" | "execute_code" => {
            let cmd = args
                .get("command")
                .or_else(|| args.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if tool == "execute_code" {
                // Arbitrary code in an interpreter: there is no head to
                // read and no allow-list that would mean anything.
                return Some("it would run arbitrary code".into());
            }
            judge_command(cmd).err().map(|why| {
                format!(
                    "{why}. Read-only shell DOES run in plan mode, chains and pipes included \
                     (`cd repo && git log --oneline | head`, `rg -n x src 2>/dev/null`, \
                     `sed -n '10,40p' f`); only the part named here was refused, so do that \
                     part with a read instead"
                )
            })
        }
        // The memory tool re-checks direction itself (see acl.rs); a
        // write action is refused, a read is not.
        "memory" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("");
            if matches!(action, "set" | "append" | "delete" | "write" | "update") {
                Some("it would write to memory".into())
            } else {
                None
            }
        }
        // The tools that act on the world by ACTION, added after the table
        // above (2026-10-11/12) and gated here since 2026-10-13: a plan
        // session may look (list, read, screenshot) and may not send,
        // change, run or make the phone do anything.
        "sessions" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("");
            (action == "send").then(|| "it would send a message into another session".into())
        }
        "config" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("");
            matches!(action, "set" | "unset").then(|| "it would change a setting".into())
        }
        "app" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("");
            match action {
                "list" | "logs" | "screenshot" => None,
                _ => Some("it would make, change, run or stop an app".into()),
            }
        }
        "device" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("");
            match action {
                "list" | "info" | "battery" | "notifications" => None,
                _ => Some("it would make a device do something".into()),
            }
        }
        _ => None,
    }
}

/// Compare two paths without pretending to resolve symlinks: normalize
/// both the way approval.rs does, then require equality.
fn paths_match(a: &str, b: &str) -> bool {
    !a.is_empty() && crate::approval::normalize_command(a) == crate::approval::normalize_command(b)
}

/// The refusal handed back to the model, as a tool RESULT.
///
/// It NAMES PLAN MODE and never a capability. PENDING is explicit about
/// this: it has to read as a mode the user chose, not as a permission
/// error or a breakage — otherwise the model apologises for a bug that
/// does not exist, and the user reads a scary ACL message they did not
/// cause.
pub fn denial(tool: &str, why: &str) -> String {
    format!(
        "error: plan mode is on, so `{tool}` was not run — {why}. This is a MODE, not a \
         failure: the session is researching and must not change anything yet. Keep reading \
         and searching, and when you have a plan, present it. The user can leave plan mode \
         with `/plan off`."
    )
}

// ── the mode value ───────────────────────────────────────────────────

/// `plan` or `act`. Parsed leniently, like `failover::Mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Plan,
    Act,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "plan" | "on" | "true" | "yes" => Some(Mode::Plan),
            "act" | "off" | "false" | "no" | "clear" | "default" => Some(Mode::Act),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Plan => "plan",
            Mode::Act => "act",
        }
    }
}

// ── reading and writing the mode ─────────────────────────────────────

/// Is this session in plan mode? Reads the same meta row `/plan` writes.
pub fn mode_for(store: &crate::store::Store, session_id: &str) -> Mode {
    match store.meta_get(&format!("{META_PREFIX}{session_id}")) {
        Some(v) => Mode::parse(&v).unwrap_or(Mode::Act),
        None => Mode::Act,
    }
}

/// Set (or clear) a session's mode. `Mode::Act` deletes the row rather
/// than storing "act", so the absence of a row means the default —
/// exactly how `/failover` treats its own key.
pub fn set_mode(store: &crate::store::Store, session_id: &str, mode: Mode) -> anyhow::Result<()> {
    let key = format!("{META_PREFIX}{session_id}");
    match mode {
        Mode::Act => store.meta_delete(&key).map(|_| ()),
        Mode::Plan => store.meta_set(&key, mode.as_str()),
    }
}

// ── the approval exit ────────────────────────────────────────────────

/// `present_plan` — show the plan, ask a human, and leave plan mode only
/// if they say so.
///
/// Modelled on `ClarifyTool`: it rides the M11 ask channel, so it renders
/// on every surface that can answer an approval (CLI prompt, TUI modal,
/// telegram buttons, IRC numbered reply, Tk dialog) without this file
/// knowing about any of them.
///
/// THE ONE THING THAT MATTERS HERE: a timeout leaves plan mode **ON**.
/// `clarify` treats a timeout as "proceed with your best judgment", which
/// is right for a question and catastrophic for a permission. A mode the
/// model can escape by waiting is not a mode, so the default on every
/// non-answer — timed out, no interactive user, declined — is to stay
/// exactly where we are.
pub struct PresentPlanTool {
    pub store: std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
    pub session_id: String,
    pub timeout_secs: u64,
}

pub const CHOICE_APPROVE: &str = "approve";
pub const CHOICE_KEEP_PLANNING: &str = "keep-planning";

#[async_trait::async_trait]
impl crate::tools::Tool for PresentPlanTool {
    fn name(&self) -> &str {
        "present_plan"
    }
    fn description(&self) -> &str {
        "Present your finished plan to the user for approval. Only useful in plan mode: \
approving it leaves plan mode so the next turn can actually do the work. Call it once, \
when the plan is complete — not to ask a question (use `clarify` for that) and not to \
get permission for a single step."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The plan, in full. This is what the user reads before deciding."
                },
                "title": {
                    "type": "string",
                    "description": "One line naming what the plan does"
                }
            },
            "required": ["plan"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> String {
        let plan = args
            .get("plan")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if plan.is_empty() {
            return "error: plan is required and must not be empty".into();
        }
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Plan ready for approval")
            .trim()
            .to_string();

        // Not in plan mode? Then there is nothing to be let out of, and
        // saying so beats raising an approval the user did not ask for.
        if let Ok(s) = self.store.lock() {
            if mode_for(&s, &self.session_id) == Mode::Act {
                return "this session is not in plan mode, so there is nothing to approve — \
just do the work"
                    .into();
            }
        }

        // Interactive-check via the registry (fast path, no ask): a
        // session with no interactive asker attached cannot approve a
        // plan, so the honest stays-in-plan answer fires before the
        // plan text is even persisted.
        if !crate::ask::session_askers(&self.session_id)
            .iter()
            .any(|a| a.interactive())
        {
            return STAYS_IN_PLAN_MODE.into();
        }
        // Persist BEFORE the ask consumes the text: an answered ask
        // lives in no history row, so meta is the only copy a surface
        // can read afterwards. Newest plan wins; the old one is stale
        // the moment a new one is presented.
        if let Ok(s) = self.store.lock() {
            let _ = s.meta_set(&format!("{PLAN_TEXT_PREFIX}{}", self.session_id), &plan);
        }
        let req = crate::ask::AskRequest::new(
            &self.session_id,
            crate::ask::AskKind::Approval,
            &title,
            vec![
                crate::ask::AskChoice::new(CHOICE_APPROVE, "approve and start work"),
                crate::ask::AskChoice::new(CHOICE_KEEP_PLANNING, "keep planning"),
            ],
        )
        .with_detail(plan)
        .with_timeout(self.timeout_secs);

        // Relayed to every attached surface (see `ask::ask_session`);
        // `None` = nobody interactive, the honest stays-in-plan answer.
        let Some(answer) = crate::ask::ask_session(&self.session_id, req).await else {
            return STAYS_IN_PLAN_MODE.into();
        };
        // EVERY non-answer stays in plan mode. Listed rather than
        // defaulted, so a future edit has to decide deliberately.
        if answer.timed_out {
            return "nobody answered, so plan mode is still on — the plan was not approved. \
Wait for the user rather than working around it."
                .into();
        }
        // The note typed beside the choice (USER 2026-10-10: "Plan
        // approval ignores notes? Does the input textbox even do
        // anything?" — it did not: this read only the choice). A note
        // travels with EITHER verdict, and a note with no choice at all
        // is a request to revise: a note is feedback, never a grant.
        let note = answer
            .free_text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        // A plain "yes" typed on a chat surface or the TUI's `y` arrives
        // as `once` (the approval contract, `ask::parse_reply`): it never
        // approved a plan, whose own key is `approve`, so a "yes" on IRC
        // or Telegram left the session planning. `deny` stays a no.
        let approved = matches!(
            answer.first(),
            Some(CHOICE_APPROVE) | Some(crate::ask::CHOICE_ONCE)
        );
        if !approved {
            return match note {
                Some(n) => format!(
                    "the user chose to keep planning — plan mode is still on — and wrote:\n\n\
                     {n}\n\nRevise the plan to address that, then present it again. Do not \
                     start work."
                ),
                None => "the user chose to keep planning — plan mode is still on. Revise the \
plan; do not start work."
                    .into(),
            };
        }
        // Approved: this is the ONLY path out of plan mode besides the
        // user typing `/plan off`.
        let with_note = |msg: String| match &note {
            Some(n) => {
                format!("{msg}\n\nThe user added this with the approval:\n\n{n}\n\nFollow it.")
            }
            None => msg,
        };
        match self.store.lock() {
            Ok(s) => match set_mode(&s, &self.session_id, Mode::Act) {
                Ok(()) => with_note(
                    "the user APPROVED the plan — plan mode is off and this session can now \
                     make changes. Carry out the plan you presented."
                        .into(),
                ),
                Err(e) => with_note(format!(
                    "the user approved the plan, but the mode could not be saved ({e}) — \
plan mode is still on. Tell them to run `/plan off`."
                )),
            },
            Err(_) => with_note(
                "the user approved the plan, but the session store was locked — plan \
mode is still on. Tell them to run `/plan off`."
                    .into(),
            ),
        }
    }
}

const STAYS_IN_PLAN_MODE: &str = "no interactive user is available to approve a plan in this \
session, so plan mode stays on. Present the plan as your answer instead.";

// ── the read-back tool ───────────────────────────────────────────────

/// `show_plan` — the model's door onto "what is the plan".
///
/// THE GAP THIS CLOSES (PENDING 2026-09-18): `presented_plan` was read
/// by the desktop session listing and by `compact_impl`, and by NOTHING
/// the model can call — so the only way for a model to ask was to shell
/// out to sqlite, which USER had already watched qwen burn a lot of
/// steps on after a compaction. The compaction re-injection (`8fa7493`)
/// made the plan SURVIVE; this makes it ASKABLE, which is the half the
/// residual named. A session that starts in plan mode with no history
/// (a new Telegram session with the mode preset is not hypothetical)
/// also needs the question answerable before any compaction fires.
///
/// Why its OWN tool rather than an `action` on `present_plan`: the two
/// are opposite in kind — one is an ask that parks the turn on a human
/// and can leave plan mode, the other is a read that changes nothing.
/// A tool that sometimes raises an approval and sometimes answers a
/// question teaches the model to be afraid of its own read; and
/// `present_plan` refusing outside plan mode (an intentional airtight
/// property) would make a read-back refuse too. The one-arg no-arg-reads
/// shape copies `todo`, which models handle without confusion.
///
/// It rides `base_registry` — unlike `present_plan`, which cron and A2A
/// sessions never get because there is no human there to approve. A
/// background job waking a session that was mid-plan is exactly who
/// needs to read the plan, and a cron session with plan mode set must
/// not be told "unknown tool" by the tool whose job is saying what the
/// plan is.
pub struct ShowPlanTool {
    pub store: std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
    pub session_id: String,
}

#[async_trait::async_trait]
impl crate::tools::Tool for ShowPlanTool {
    fn name(&self) -> &str {
        "show_plan"
    }
    fn description(&self) -> &str {
        "Read back the planning state: the plan `present_plan` last presented, and the task \
list. Call with no arguments for both. Use it after a compaction, or whenever you need the \
plan you are carrying rather than reconstructing it from memory. It changes nothing — \
approving a plan is `present_plan`, not this."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["plan", "tasks", "status"],
                    "description": "plan = the presented plan, tasks = the todo list, \
        status = both (default)"
                },
                "session": {
                    "type": "string",
                    "description": "Another session to read (id or unique prefix). Omit for \
        this one."
                }
            }
        })
    }

    async fn call(&self, args: serde_json::Value) -> String {
        // A null `action` is the bare call, and a bad one falls to
        // `status` rather than erroring — the whole point of this tool
        // is answering, and every plausible intent is visible in the
        // status render anyway.
        let verb = args
            .get("action")
            .and_then(Value::as_str)
            .and_then(Verb::parse_action)
            .unwrap_or(Verb::Status);
        let raw = args
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        // A named session is a READ OF SOMEONE ELSE'S CONVERSATION, so
        // it pays what `session_search` pays — checked HERE rather than
        // by the ACL layer, which sees one tool name and cannot tell
        // which direction the read goes. Exactly the shape `memory`
        // uses for its read/write split. Checked BEFORE resolution: a
        // resolver error for a below-cap caller would say whether a
        // prefix matched anything, which is itself an answer about
        // sessions that are not theirs. The function also owns the
        // self-reference decision, so the gate and the resolver cannot
        // disagree about where "another session" begins.
        if let Some(denial) = cross_session_read_denial(
            raw,
            crate::acl::session_principal(&self.session_id).as_deref(),
        ) {
            return denial;
        }
        let Ok(s) = self.store.lock() else {
            return "error: store unavailable".into();
        };
        // Same resolution the `/plan show <id>` command arm uses, in one
        // place so the two doors cannot disagree about what a session
        // reference means.
        let target = match resolve_read_target(&s, raw, &self.session_id) {
            Ok(t) => t,
            Err(e) => return format!("error: {e:#}"),
        };
        render_state(&s, &target, &self.session_id, verb)
    }
}

/// The ACL shared by every door that reads ANOTHER session's plan (the
/// tool's `session` argument and `/plan show <id>`), so the two cannot
/// disagree about who may — including on the point that decides the
/// answer: what counts as "another session" in the first place. A
/// self-reference (`is_self_reference`) needs no capability because it
/// reads the caller's own conversation. `None` principal = no gate (an
/// owner surface, or a registry with no session), the pre-ACL behaviour
/// every owner path relies on.
pub fn cross_session_read_denial(raw: &str, who: Option<&crate::acl::Principal>) -> Option<String> {
    if is_self_reference(raw) {
        return None;
    }
    let who = who?;
    if who.has(crate::acl::Cap::SessionHistory) {
        return None;
    }
    Some(format!(
        "error: access denied — reading ANOTHER session's plan requires the \
         `session-history` capability, which {} does not have. This session's own plan is \
         readable; leave the session out.",
        who.label()
    ))
}

/// `action`-spellings for the tool argument. The `show`/`read` family is
/// accepted because a model reaching for a read names it that way; an
/// unrecognized word is NOT an error (see `call`).
impl Verb {
    fn parse_action(s: &str) -> Option<Verb> {
        let s = s.trim().to_ascii_lowercase();
        if s.starts_with("show") {
            return Some(Verb::Show);
        }
        match s.as_str() {
            "plan" | "the_plan" | "the-plan" | "view" | "read" | "current" => Some(Verb::Show),
            "tasks" | "todo" | "todos" | "task" | "list" => Some(Verb::Tasks),
            "status" | "both" | "all" | "state" => Some(Verb::Status),
            _ => None,
        }
    }
}

/// Does this `session` argument name the CALLER rather than another
/// session? Empty does, and so do four words models write when they
/// mean "mine" (Telegram's `/plan show this`, a model's
/// `{session:"current"}`). ONE definition, used by the resolver AND by
/// the capability gate, because the two must agree exactly on where
/// "another session" begins — a word the resolver treats as self and
/// the gate treats as cross would deny a read it then answers, and one
/// the gate treats as self and the resolver treats as cross would skip
/// the check for exactly the read that needs it.
fn is_self_reference(raw: &str) -> bool {
    let raw = raw.trim();
    raw.is_empty()
        || matches!(
            raw.to_ascii_lowercase().as_str(),
            "this" | "current" | "self" | "here"
        )
}

/// The `session` argument, resolved: empty (or a self-reference,
/// `is_self_reference`) means the calling session; anything else must be
/// a real session id or a UNIQUE prefix — the `/resume` and cron
/// `session` spelling, via the same resolver, because two
/// session-reference syntaxes in one product is the taxonomy collision
/// the 2026-09-17 round existed to close. Resolved to a full id, so the
/// answer's header cannot show a prefix that names two sessions
/// half-over.
pub fn resolve_read_target(
    store: &crate::store::Store,
    raw: &str,
    own_session: &str,
) -> anyhow::Result<String> {
    let raw = raw.trim();
    if is_self_reference(raw) {
        return Ok(own_session.to_string());
    }
    crate::cron::resolve_session(store, raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── the allow-list ───────────────────────────────────────────────

    #[test]
    fn ordinary_research_commands_are_read_only() {
        for c in [
            "ls -la",
            "cat Cargo.toml",
            "grep -rn foo src/",
            "rg --files",
            "find . -name '*.rs'",
            "wc -l src/main.rs",
            "head -20 README.md",
            "jq '.data' file.json",
            "diff a b",
            "git log --oneline -20",
            "git show HEAD",
            "git status",
            "git diff --stat",
            "git rev-parse HEAD",
            "git merge-base a b",
            "cargo tree",
            "systemctl status foo",
        ] {
            assert!(read_only_command(c), "should be read-only: {c}");
        }
    }

    #[test]
    fn commands_that_change_things_are_not() {
        for c in [
            "git commit -m x",
            "git push",
            "git checkout -- .",
            "git reset --hard",
            "rm -rf build",
            "mv a b",
            "cp a b",
            "touch f",
            "mkdir d",
            "npm install",
            "cargo build",
            "cargo test",
            "pip install x",
            "systemctl restart foo",
            "chmod +x f",
            "sed -i s/a/b/ f",
        ] {
            assert!(!read_only_command(c), "must NOT be read-only: {c}");
        }
    }

    /// The whole reason a deny-list could not do this job: none of these
    /// appear in `approval::classify`'s dangerous-pattern table, and all
    /// of them change something.
    #[test]
    fn the_deny_list_blind_spots_are_covered() {
        for c in ["git commit -m x", "npm install", "mv a b", "cargo build"] {
            assert!(
                crate::approval::classify("terminal", &json!({"command": c})).is_none(),
                "precondition: {c} is invisible to the dangerous-command table"
            );
            assert!(
                !read_only_command(c),
                "…but plan mode still refuses it: {c}"
            );
        }
    }

    #[test]
    fn chaining_defeats_a_read_only_head() {
        for c in [
            "ls && rm -rf /",
            "ls; rm x",
            "ls || rm x",
            "ls | tee f",
            "cat f > g",
            "cat f >> g",
            "cat < f",
            "echo `rm x`",
            "echo $(rm x)",
            "ls &",
            "ls\nrm x",
        ] {
            assert!(!read_only_command(c), "chaining must refuse: {c}");
        }
    }

    #[test]
    fn a_path_qualified_head_is_refused() {
        assert!(!read_only_command("/usr/bin/ls"));
        assert!(!read_only_command("./configure"));
        assert!(
            read_only_command("ls"),
            "the bare name still works — this is a narrowness cost, not a bug"
        );
    }

    #[test]
    fn a_wrapper_cannot_launder_a_command() {
        for c in ["env rm -rf x", "sh -c 'rm x'", "bash -c ls", "xargs rm"] {
            assert!(!read_only_command(c), "wrapper must refuse: {c}");
        }
    }

    #[test]
    fn a_read_only_git_subcommand_with_a_write_flag_is_refused() {
        assert!(read_only_command("git config --get user.name"));
        assert!(!read_only_command("git config --unset user.name"));
        assert!(read_only_command("git branch --list"));
        assert!(!read_only_command("git branch -D old"));
        assert!(read_only_command("git stash list"));
        assert!(!read_only_command("git stash pop"));
        assert!(read_only_command("git tag --list"));
        assert!(!read_only_command("git tag -d v1"));
    }

    #[test]
    fn positional_writes_in_read_shapes_are_refused() {
        // The holes the first-cut allow-list left open: commands whose
        // READ form is listed, but whose bare/positional form mutates.
        // Each of these WAS read_only_command() == true before the fix.
        for c in [
            "find . -name '*.rs' -delete", // find default is read; -delete is not
            "find . -exec rm {} ;",        // ...and -exec runs anything
            "fd -x rm",                    // fd's -x executes per hit
            "fd --exec-batch tar cf x.tar",
            "git branch newfeature",         // creates a branch
            "git tag v1.0",                  // creates a tag
            "git config user.name New Name", // two positionals = write
            "git stash",                     // bare stash stashes the tree
            "sort -o out.txt in.txt",        // -o writes the output file
            "yq -i '.x = 1' file.yaml",      // -i edits in place
            "date -s '2026-01-01'",          // sets the clock
            "hostname evil-host",            // bare positional sets the name
        ] {
            assert!(!read_only_command(c), "must NOT be read-only: {c}");
        }
        // …and the read shapes they guard stay allowed.
        for c in [
            "find . -name '*.rs'",
            "fd pattern",
            "git branch",
            "git tag",
            "git config user.name",
            "git stash list",
            "sort in.txt",
            "yq '.x' file.yaml",
            "date",
            "hostname",
        ] {
            assert!(read_only_command(c), "should be read-only: {c}");
        }
    }

    /// The live corpus (USER 2026-10-10: "Why does plan mode
    /// unconditionally disable terminal?"). These are the shapes of the
    /// 38 `terminal` calls plan mode refused in the live state.db, paths
    /// shortened. Every one was refused by the old blanket chain/redirect
    /// check; the reads among them must now run.
    #[test]
    fn the_live_refusals_that_were_reads_now_run() {
        for c in [
            r#"cd ~/repo && rg -n "tool_output_cap" src/config.rs | head; echo "=== default fn ==="; rg -n -B2 -A4 "fn default_cap" src/config.rs"#,
            r#"cd ~/repo && rg -n "messages_fts|fts5|CREATE TRIGGER|PRAGMA" src/store.rs | head -40"#,
            "cd ~/repo && git log --oneline -10 && git status --short | head -20",
            "ls -d ~/a* ~/.local/share/x ~/.config/x 2>&1; echo ---; ls ~/repo/scripts/precommit.sh",
            "cd ~/repo && git log --oneline -8 && git status --short | head -20 && sed -n '296,520p' PENDING.md",
            r#"grep -n "^proc ::vchat::" ~/repo/lib/chat.tcl | head -120"#,
            "ls ~/repo/ && echo ---- && ls ~/repo/crates/ 2>/dev/null && echo ---- && git -C ~/repo log --oneline -5",
            r#"cd ~/repo && ls; echo ===; find . -name "PLAN*" -o -name "*PENDING*" | head -30"#,
            "date '+%Y-%m-%d %H:%M %Z'; cd ~/repo && git log --oneline -8; git status --short",
            "ls -la ~/plans/ 2>/dev/null | tail -5; ls ~/notes/3b41* 2>/dev/null | head -3",
            r#"cd ~/repo && git log --oneline -8 && echo "=== stash ===" && git stash list | head -3"#,
            r"cd ~/repo && grep -n '^#\|^##\|^###' PENDING.md | head -60; echo ---; wc -l PENDING.md",
            r#"cd ~/repo && git status -sb | head -5 && ls crates/x 2>/dev/null || echo "NO x crate""#,
            r#"cd ~/slopdrop && grep -rn "api/procs\|api/vars" src/ 2>/dev/null | head -20"#,
            r#"wc -l ~/notes/s.jsonl && grep -c '"isCompactSummary":true' ~/notes/s.jsonl || true"#,
            "cd ~/notes && tail -1 s.jsonl | head -c 600",
            "cd ~/repo && git status --porcelain | wc -l",
            "cd ~/repo && sed -n '/^### Phase 4 /,/^### Phase 5 /p' PLAN.md",
            r"cd ~/repo/x && grep -rnoE 'veles_(core|frontends)::[a-z_:]+' src/ | sort | uniq -c | sort -rn | head -50",
            r"cd ~/repo && command grep -nE 'pub fn|pub struct' src/module.rs | head -30; wc -l src/module.rs",
        ] {
            assert!(
                judge_command(c).is_ok(),
                "a read must run in plan mode: {c}\n  → {:?}",
                judge_command(c)
            );
        }
        // …and the corpus's real non-reads stay refused.
        for c in [
            r#"for n in 1 2; do sed -n "${n}p" f; done"#,
            "cd ~/notes && python3 - <<'EOF'\nprint(1)\nEOF",
            r"find ~/notes -name 'x*' -exec ls -la {} \;",
        ] {
            assert!(judge_command(c).is_err(), "must stay refused: {c}");
        }
    }

    #[test]
    fn redirects_to_nowhere_and_between_descriptors_run() {
        for c in [
            "ls 2>/dev/null",
            "ls 2>&1",
            "ls >/dev/null 2>&1",
            "ls &>/dev/null",
            "ls 1>&2",
            "cat f </dev/null",
            "ls 2> /dev/null",
            "ls 2>&- ",
        ] {
            assert!(
                judge_command(c).is_ok(),
                "should run: {c} → {:?}",
                judge_command(c)
            );
        }
    }

    #[test]
    fn redirects_that_write_or_read_files_are_refused() {
        for c in [
            "ls > out",
            "ls >> out",
            "ls >| out",
            "ls 2> err.txt",
            "ls &> out",
            "ls &>> out",
            "ls >& out",
            "cat < f",
            "ls > /dev/nullx",
            "echo x>f",
        ] {
            let r = judge_command(c);
            assert!(r.is_err(), "must refuse: {c}");
            assert!(
                r.unwrap_err().contains("redirect"),
                "the reason names the redirect: {c}"
            );
        }
    }

    #[test]
    fn shapes_that_cannot_be_judged_from_their_words_are_refused() {
        for c in [
            "cat <<EOF\nx\nEOF",
            "cat <<< x",
            "diff <(ls a) <(ls b)",
            "ls >(cat)",
            "(cd x && rm y)",
            "$CMD x",
            "${CMD} x",
            "echo \"$(rm x)\"",
            "echo \"`rm x`\"",
            "echo 'unclosed",
            "echo \"unclosed",
            "ls & rm x",
        ] {
            assert!(judge_command(c).is_err(), "must refuse: {c}");
        }
        // The reasons say what the shape IS, so a model can tell a heredoc
        // from a typo (a later rule would refuse these too, with a
        // vaguer word).
        for (c, says) in [
            ("cat <<EOF\nx\nEOF", "heredoc"),
            ("cat <<< x", "here-string"),
            ("diff <(ls a) b", "process substitution"),
            ("ls >(cat)", "process substitution"),
            ("echo $(rm x)", "`$(…)`"),
            ("(cd x && rm y)", "subshell"),
            ("ls & rm x", "background"),
        ] {
            let why = judge_command(c).unwrap_err();
            assert!(why.contains(says), "{c}: {why}");
        }
    }

    #[test]
    fn quotes_keep_separators_inside_one_word() {
        for c in [
            "grep -E 'a|b;c&&d>e' f",
            "echo \"a > b; rm x\"",
            r"grep a\|b f",
            "rg -n 'x$' src",
            "echo \"$HOME\"",
        ] {
            assert!(
                judge_command(c).is_ok(),
                "one quoted word, a read: {c} → {:?}",
                judge_command(c)
            );
        }
        assert!(judge_command("grep 'x' f; rm y").is_err());
        assert!(judge_command("grep \"x\" f | sh").is_err());
    }

    /// Each new takeback, one refusing case apiece: these are the write
    /// paths a wider gate would otherwise have opened.
    #[test]
    fn the_write_paths_inside_read_commands_are_refused() {
        for c in [
            "git log --output=x",
            "git diff --output x",
            "git show --output=f HEAD",
            "git grep -O vim x",
            "git grep -nOvim x",
            "rg --pre cat x",
            "rg --pre=cat x",
            "find . -fprint out",
            "find . -fprintf out %p",
            "find . -fls out",
            "tree -o out",
            "file -C -m x",
            "uniq a b",
            "ss -K dst 1.2.3.4",
            "journalctl --vacuum-time=1s",
            "journalctl --rotate",
            "sort -ro out in",
            "git -c alias.x='!rm -rf y' x",
            "git -c core.pager=evil log",
            "git --exec-path=/tmp log",
            "git -ccore.pager=evil log",
            "git -C ~/repo branch -D old",
            "command rm x",
            "command -p rm x",
        ] {
            assert!(judge_command(c).is_err(), "must refuse: {c}");
        }
        for c in [
            "git -C ~/repo log -5",
            "git --no-pager log -5",
            "git -C ~/repo status --short",
            "git --exec-path",
            "uniq -c f",
            "sort -rn f",
            "journalctl -u x --since today",
            "ss -ltnp",
            "command -v rg",
            "command grep x f",
        ] {
            assert!(
                judge_command(c).is_ok(),
                "should run: {c} → {:?}",
                judge_command(c)
            );
        }
    }

    #[test]
    fn sed_runs_only_as_a_printer() {
        for c in [
            "sed -n '1,5p' f",
            "sed -n '10,$p' f",
            "sed -n '/re/p' f",
            "sed -n '/a/,/b/p' f",
            "sed -n '1p;3p' f",
            "sed -ne '2p' f",
            "sed -n -e '2p' -e '4p' f",
            "sed -nE '/x+/p' f",
            "sed --quiet 5p f",
        ] {
            assert!(
                judge_command(c).is_ok(),
                "a print-only sed: {c} → {:?}",
                judge_command(c)
            );
        }
        for c in [
            "sed -i 's/a/b/' f",
            "sed -n -i '1p' f",
            "sed -ni '1p' f",
            "sed --in-place=.bak -n 1p f",
            "sed 1p f",
            "sed -n 's/a/b/p' f",
            "sed -n '1w out' f",
            "sed -n '1e rm x' f",
            "sed -n -f script f",
            "sed -n",
        ] {
            assert!(judge_command(c).is_err(), "not a printer: {c}");
        }
    }

    #[test]
    fn a_flag_hits_in_every_form_a_program_reads() {
        assert!(flag_hit("--output", "--output"));
        assert!(flag_hit("--output=f", "--output"));
        assert!(!flag_hit("--output-indicator-new=x", "--output"));
        assert!(flag_hit("-o", "-o"));
        assert!(flag_hit("-ofile", "-o"));
        assert!(flag_hit("-ro", "-o"));
        assert!(!flag_hit("-rn", "-o"));
        assert!(
            !flag_hit("-name", "-delete"),
            "a find option is matched exactly"
        );
        assert!(flag_hit("-delete", "-delete"));
    }

    /// The refusal names the part it refused and says read-only shell
    /// still runs — the old "not a read-only command" is what taught the
    /// models the terminal was off.
    #[test]
    fn the_terminal_refusal_names_the_part_and_says_reads_run() {
        let why = refuses(
            "terminal",
            &json!({"command": "cd ~/repo && git log | head && rm -rf build"}),
            None,
        )
        .expect("refused");
        assert!(
            why.starts_with("`rm -rf build` is not a read-only command"),
            "names only the refused part, first: {why}"
        );
        assert!(
            why.contains("DOES run in plan mode"),
            "says reads run: {why}"
        );
        let why = refuses("terminal", &json!({"command": "ls > out"}), None).expect("refused");
        assert!(why.contains("> out"), "names the redirect: {why}");
    }

    #[test]
    fn an_unknown_command_is_refused_not_allowed() {
        assert!(!read_only_command("frobnicate --all"));
        assert!(!read_only_command(""));
        assert!(!read_only_command("   "));
    }

    // ── per-tool policy ──────────────────────────────────────────────

    #[test]
    fn reads_and_research_are_allowed() {
        for t in [
            "read_file",
            "search_files",
            "web_search",
            "web_extract",
            "skills_list",
            "skill_view",
            "todo",
            "clarify",
            "session_search",
            "session_export",
            "tool_search",
            "tool_describe",
            "browser_navigate",
            "browser_snapshot",
            "browser_scroll",
            "browser_console",
        ] {
            assert!(
                refuses(t, &json!({}), None).is_none(),
                "plan mode must allow research: {t}"
            );
        }
    }

    #[test]
    fn world_changing_tools_are_refused() {
        for t in [
            "cronjob",
            "delegate_task",
            "a2a",
            "generate_image",
            "generate_video",
            "text_to_speech",
            "computer_use",
            "skill_manage",
            "browser_click",
            "browser_type",
        ] {
            assert!(refuses(t, &json!({}), None).is_some(), "must refuse: {t}");
        }
    }

    #[test]
    fn the_plan_file_is_the_one_writable_path() {
        let plan = "/tmp/plans/current.md";
        assert!(
            refuses("write_file", &json!({"path": plan}), Some(plan)).is_none(),
            "the plan file is the single exception"
        );
        assert!(
            refuses("write_file", &json!({"path": "/tmp/other.md"}), Some(plan)).is_some(),
            "…and only it"
        );
        assert!(
            refuses("patch", &json!({"path": plan}), Some(plan)).is_none(),
            "patch reaches it too — the plan gets edited incrementally"
        );
        assert!(
            refuses("write_file", &json!({"path": plan}), None).is_some(),
            "no plan file designated means nothing is writable"
        );
    }

    #[test]
    fn terminal_is_gated_by_the_command_not_banned() {
        assert!(refuses("terminal", &json!({"command": "git log"}), None).is_none());
        assert!(refuses("terminal", &json!({"command": "git push"}), None).is_some());
    }

    #[test]
    fn execute_code_is_refused_outright() {
        assert!(
            refuses("execute_code", &json!({"code": "print(1)"}), None).is_some(),
            "there is no head to read and no allow-list that would mean anything"
        );
    }

    /// The tools added after the table (sessions, config, app, device)
    /// are gated by action: reads pass, anything that sends, changes,
    /// runs or makes a device act is refused. Before 2026-10-13 all four
    /// passed whole, because `refuses` lets an unlisted tool through.
    #[test]
    fn the_acting_tools_added_later_are_gated_by_action() {
        for (tool, read) in [
            ("sessions", "list"),
            ("sessions", "check"),
            ("config", "get"),
            ("app", "list"),
            ("app", "logs"),
            ("app", "screenshot"),
            ("device", "list"),
            ("device", "info"),
            ("device", "battery"),
            ("device", "notifications"),
        ] {
            assert!(
                refuses(tool, &json!({"action": read}), None).is_none(),
                "{tool} {read} only looks"
            );
        }
        for (tool, act) in [
            ("sessions", "send"),
            ("config", "set"),
            ("config", "unset"),
            ("app", "new"),
            ("app", "write"),
            ("app", "edit"),
            ("app", "run"),
            ("app", "stop"),
            ("app", "reload"),
            ("device", "notify"),
            ("device", "toast"),
            ("device", "vibrate"),
            ("device", "speak"),
            ("device", "open"),
            ("device", "share"),
            ("device", "location"),
            ("device", "clipboard"),
        ] {
            assert!(
                refuses(tool, &json!({"action": act}), None).is_some(),
                "{tool} {act} acts"
            );
        }
        // No action at all is not a read.
        assert!(refuses("app", &json!({}), None).is_some());
        assert!(refuses("device", &json!({}), None).is_some());
    }

    #[test]
    fn memory_reads_pass_and_memory_writes_do_not() {
        assert!(refuses("memory", &json!({"action": "get"}), None).is_none());
        assert!(refuses("memory", &json!({"action": "search"}), None).is_none());
        for a in ["set", "append", "delete", "write", "update"] {
            assert!(
                refuses("memory", &json!({"action": a}), None).is_some(),
                "memory {a} writes"
            );
        }
    }

    // ── wording ──────────────────────────────────────────────────────

    #[test]
    fn the_denial_names_the_mode_not_a_capability() {
        let d = denial("write_file", "it would edit a file");
        assert!(d.contains("plan mode"), "must name the mode: {d}");
        assert!(d.contains("/plan off"), "must say how to leave: {d}");
        assert!(
            !d.contains("capability") && !d.contains("access denied"),
            "must NOT read as a permission error: {d}"
        );
    }

    #[test]
    fn mode_parses_the_forms_a_user_would_type() {
        assert_eq!(Mode::parse("plan"), Some(Mode::Plan));
        assert_eq!(Mode::parse("on"), Some(Mode::Plan));
        assert_eq!(Mode::parse("off"), Some(Mode::Act));
        assert_eq!(Mode::parse("act"), Some(Mode::Act));
        assert_eq!(Mode::parse("clear"), Some(Mode::Act));
        assert_eq!(Mode::parse("sideways"), None);
    }

    // ── mode persistence ─────────────────────────────────────────────

    fn scratch_store() -> std::sync::Arc<std::sync::Mutex<crate::store::Store>> {
        let dir = crate::testenv::kept_dir();
        let s = crate::store::Store::open(&dir.join("state.db")).expect("store");
        std::sync::Arc::new(std::sync::Mutex::new(s))
    }

    #[test]
    fn the_mode_defaults_to_act_and_round_trips() {
        let store = scratch_store();
        let s = store.lock().unwrap();
        assert_eq!(mode_for(&s, "sess"), Mode::Act, "act is the default");
        set_mode(&s, "sess", Mode::Plan).unwrap();
        assert_eq!(mode_for(&s, "sess"), Mode::Plan);
        // Clearing DELETES the row, so absence means default — the same
        // contract /failover's key has.
        set_mode(&s, "sess", Mode::Act).unwrap();
        assert_eq!(mode_for(&s, "sess"), Mode::Act);
        assert!(s.meta_get(&format!("{META_PREFIX}sess")).is_none());
    }

    #[test]
    fn the_mode_is_per_session() {
        let store = scratch_store();
        let s = store.lock().unwrap();
        set_mode(&s, "a", Mode::Plan).unwrap();
        assert_eq!(mode_for(&s, "a"), Mode::Plan);
        assert_eq!(
            mode_for(&s, "b"),
            Mode::Act,
            "one session's mode is its own"
        );
    }

    // ── present_plan ─────────────────────────────────────────────────

    use crate::ask::{AskAnswer, AskRequest, Asker};
    use crate::tools::Tool;
    use std::sync::Arc;

    struct Answers(&'static str);
    #[async_trait::async_trait]
    impl Asker for Answers {
        async fn ask(&self, _req: AskRequest) -> AskAnswer {
            AskAnswer::choice(self.0)
        }
    }
    struct TimesOut;
    #[async_trait::async_trait]
    impl Asker for TimesOut {
        async fn ask(&self, _req: AskRequest) -> AskAnswer {
            AskAnswer::timeout()
        }
    }
    struct Captures(std::sync::Mutex<Option<AskRequest>>);
    #[async_trait::async_trait]
    impl Asker for Captures {
        async fn ask(&self, req: AskRequest) -> AskAnswer {
            *self.0.lock().unwrap() = Some(req);
            AskAnswer::choice(CHOICE_APPROVE)
        }
    }

    fn tool(store: &Arc<std::sync::Mutex<crate::store::Store>>, sid: &str) -> PresentPlanTool {
        PresentPlanTool {
            store: store.clone(),
            session_id: sid.into(),
            timeout_secs: 5,
        }
    }

    /// THE NON-NEGOTIABLE ONE. `clarify` reads a timeout as "proceed with
    /// your best judgment", which is right for a question and
    /// catastrophic for a permission: a mode the model can escape by
    /// waiting is not a mode.
    #[tokio::test]
    async fn a_timeout_leaves_plan_mode_on() {
        let store = scratch_store();
        set_mode(&store.lock().unwrap(), "pp-timeout", Mode::Plan).unwrap();
        crate::ask::set_session_asker("pp-timeout", Arc::new(TimesOut));
        let out = tool(&store, "pp-timeout")
            .call(json!({"plan": "do a thing"}))
            .await;
        assert_eq!(
            mode_for(&store.lock().unwrap(), "pp-timeout"),
            Mode::Plan,
            "waiting must never be a way out: {out}"
        );
        // The WORDING matters, not just the mode. TWO guards keep plan
        // mode on after a timeout — this arm, and the approve-key check
        // below it — so asserting only "still on" stays green with this
        // arm deleted, and the model is then told "the user chose to
        // keep planning" about a user who never answered.
        assert!(
            out.contains("nobody answered"),
            "a timeout must not be reported as a decision: {out}"
        );
        assert!(
            !out.contains("the user chose"),
            "nobody chose anything: {out}"
        );
    }

    #[tokio::test]
    async fn declining_leaves_plan_mode_on() {
        let store = scratch_store();
        set_mode(&store.lock().unwrap(), "pp-no", Mode::Plan).unwrap();
        crate::ask::set_session_asker("pp-no", Arc::new(Answers(CHOICE_KEEP_PLANNING)));
        let out = tool(&store, "pp-no")
            .call(json!({"plan": "do a thing"}))
            .await;
        assert_eq!(mode_for(&store.lock().unwrap(), "pp-no"), Mode::Plan);
        assert!(out.contains("keep planning"), "{out}");
    }

    #[tokio::test]
    async fn an_unrecognised_answer_leaves_plan_mode_on() {
        let store = scratch_store();
        set_mode(&store.lock().unwrap(), "pp-junk", Mode::Plan).unwrap();
        crate::ask::set_session_asker("pp-junk", Arc::new(Answers("something-else")));
        tool(&store, "pp-junk").call(json!({"plan": "x"})).await;
        assert_eq!(
            mode_for(&store.lock().unwrap(), "pp-junk"),
            Mode::Plan,
            "only the approve key approves"
        );
    }

    #[tokio::test]
    async fn approving_is_the_way_out() {
        let store = scratch_store();
        set_mode(&store.lock().unwrap(), "pp-yes", Mode::Plan).unwrap();
        crate::ask::set_session_asker("pp-yes", Arc::new(Answers(CHOICE_APPROVE)));
        let out = tool(&store, "pp-yes")
            .call(json!({"plan": "do a thing"}))
            .await;
        assert_eq!(mode_for(&store.lock().unwrap(), "pp-yes"), Mode::Act);
        assert!(out.contains("APPROVED"), "{out}");
    }

    /// An answer with a note typed beside it (`choice` may be empty: the
    /// note alone, which is what Enter in the desktop's box sends).
    struct AnswersWithNote(&'static str, &'static str);
    #[async_trait::async_trait]
    impl Asker for AnswersWithNote {
        async fn ask(&self, _req: AskRequest) -> AskAnswer {
            AskAnswer {
                selected: if self.0.is_empty() {
                    Vec::new()
                } else {
                    vec![self.0.to_string()]
                },
                free_text: Some(self.1.to_string()),
                ..Default::default()
            }
        }
    }

    /// USER 2026-10-10: "Plan approval ignores notes?" It did — the tool
    /// read only the choice. A note now reaches the model with either
    /// verdict, and a bare note never approves.
    #[tokio::test]
    async fn a_note_reaches_the_model_with_either_verdict_and_alone_never_approves() {
        let store = scratch_store();
        set_mode(&store.lock().unwrap(), "pp-note-yes", Mode::Plan).unwrap();
        crate::ask::set_session_asker(
            "pp-note-yes",
            Arc::new(AnswersWithNote(CHOICE_APPROVE, "skip the docs step")),
        );
        let out = tool(&store, "pp-note-yes")
            .call(json!({"plan": "1. code 2. docs"}))
            .await;
        assert_eq!(mode_for(&store.lock().unwrap(), "pp-note-yes"), Mode::Act);
        assert!(
            out.contains("APPROVED") && out.contains("skip the docs step"),
            "{out}"
        );

        set_mode(&store.lock().unwrap(), "pp-note-keep", Mode::Plan).unwrap();
        crate::ask::set_session_asker(
            "pp-note-keep",
            Arc::new(AnswersWithNote(CHOICE_KEEP_PLANNING, "split step 2 in two")),
        );
        let out = tool(&store, "pp-note-keep")
            .call(json!({"plan": "p"}))
            .await;
        assert_eq!(mode_for(&store.lock().unwrap(), "pp-note-keep"), Mode::Plan);
        assert!(
            out.contains("keep planning") && out.contains("split step 2 in two"),
            "{out}"
        );

        set_mode(&store.lock().unwrap(), "pp-note-only", Mode::Plan).unwrap();
        crate::ask::set_session_asker(
            "pp-note-only",
            Arc::new(AnswersWithNote("", "what about the phone?")),
        );
        let out = tool(&store, "pp-note-only")
            .call(json!({"plan": "p"}))
            .await;
        assert_eq!(
            mode_for(&store.lock().unwrap(), "pp-note-only"),
            Mode::Plan,
            "a note with no choice is feedback, never an approval"
        );
        assert!(out.contains("what about the phone?"), "{out}");
    }

    /// A "yes" typed on IRC or Telegram (or `y` in the TUI) arrives as
    /// `once`, the approval contract: it approves a plan. It used to
    /// leave the session planning.
    #[tokio::test]
    async fn a_typed_yes_approves_and_a_typed_no_keeps_planning() {
        let store = scratch_store();
        set_mode(&store.lock().unwrap(), "pp-y", Mode::Plan).unwrap();
        crate::ask::set_session_asker("pp-y", Arc::new(Answers(crate::ask::CHOICE_ONCE)));
        let out = tool(&store, "pp-y").call(json!({"plan": "p"})).await;
        assert_eq!(mode_for(&store.lock().unwrap(), "pp-y"), Mode::Act, "{out}");
        set_mode(&store.lock().unwrap(), "pp-n", Mode::Plan).unwrap();
        crate::ask::set_session_asker("pp-n", Arc::new(Answers(crate::ask::CHOICE_DENY)));
        let _ = tool(&store, "pp-n").call(json!({"plan": "p"})).await;
        assert_eq!(mode_for(&store.lock().unwrap(), "pp-n"), Mode::Plan);
    }

    #[tokio::test]
    async fn the_plan_text_reaches_the_human() {
        let store = scratch_store();
        set_mode(&store.lock().unwrap(), "pp-text", Mode::Plan).unwrap();
        let cap = Arc::new(Captures(std::sync::Mutex::new(None)));
        crate::ask::set_session_asker("pp-text", cap.clone());
        tool(&store, "pp-text")
            .call(json!({"plan": "step one, step two", "title": "Refactor the thing"}))
            .await;
        let req = cap.0.lock().unwrap().clone().expect("an ask was raised");
        assert_eq!(req.prompt, "Refactor the thing");
        assert!(
            req.detail.as_deref().unwrap_or("").contains("step one"),
            "the plan itself must be what they read: {req:?}"
        );
        // And the text outlives the ask: an answered question lives in
        // no history row, so the meta copy is what the Tasks pane reads.
        assert_eq!(
            presented_plan(&store.lock().unwrap(), "pp-text").as_deref(),
            Some("step one, step two"),
            "the presented plan must survive the ask that carried it"
        );
    }

    #[tokio::test]
    async fn with_no_human_it_stays_in_plan_mode() {
        let store = scratch_store();
        set_mode(&store.lock().unwrap(), "pp-nobody", Mode::Plan).unwrap();
        // No asker registered for this session at all.
        let out = tool(&store, "pp-nobody").call(json!({"plan": "x"})).await;
        assert_eq!(mode_for(&store.lock().unwrap(), "pp-nobody"), Mode::Plan);
        assert!(out.contains("plan mode stays on"), "{out}");
    }

    #[tokio::test]
    async fn outside_plan_mode_it_raises_nothing() {
        let store = scratch_store();
        crate::ask::set_session_asker("pp-act", Arc::new(Answers(CHOICE_APPROVE)));
        let out = tool(&store, "pp-act").call(json!({"plan": "x"})).await;
        assert!(out.contains("not in plan mode"), "{out}");
    }

    #[tokio::test]
    async fn an_empty_plan_is_refused() {
        let store = scratch_store();
        set_mode(&store.lock().unwrap(), "pp-empty", Mode::Plan).unwrap();
        let out = tool(&store, "pp-empty").call(json!({"plan": "   "})).await;
        assert!(out.starts_with("error:"), "{out}");
        assert_eq!(mode_for(&store.lock().unwrap(), "pp-empty"), Mode::Plan);
    }

    // ── the read-back (show_plan) ────────────────────────────────────

    fn reader(store: &Arc<std::sync::Mutex<crate::store::Store>>, sid: &str) -> ShowPlanTool {
        ShowPlanTool {
            store: store.clone(),
            session_id: sid.into(),
        }
    }

    /// THE RESIDUAL THIS EXISTS FOR (PENDING 2026-09-18): the plan
    /// survives compaction but had no tool-reachable reader, so the
    /// model's only way to ask was sqlite. This drives the tool through
    /// the same meta row `present_plan` writes.
    #[tokio::test]
    async fn the_plan_a_model_cannot_see_is_askable() {
        let store = scratch_store();
        set_mode(&store.lock().unwrap(), "sp-read", Mode::Plan).unwrap();
        store
            .lock()
            .unwrap()
            .meta_set(&format!("{PLAN_TEXT_PREFIX}sp-read"), "1. dig\n2. ship")
            .unwrap();
        let out = reader(&store, "sp-read").call(json!({})).await;
        assert!(out.contains("1. dig"), "the plan itself: {out}");
        assert!(out.contains("plan mode is ON"), "{out}");
        // The bare call answers BOTH questions, since the tool is what
        // a model reaches for when it has forgotten which it asked.
        assert!(out.contains("THE TASK LIST"), "{out}");
        // And reading changed nothing — not the mode, not the plan.
        assert_eq!(mode_for(&store.lock().unwrap(), "sp-read"), Mode::Plan);
    }

    #[tokio::test]
    async fn a_session_that_never_presented_says_so_rather_than_lieing() {
        let store = scratch_store();
        let out = reader(&store, "sp-none").call(json!({})).await;
        assert!(out.contains("no plan has been PRESENTED"), "{out}");
        assert!(out.contains("plan mode is off"), "{out}");
    }

    #[tokio::test]
    async fn the_actions_split_the_two_lists() {
        let store = scratch_store();
        let sid = "sp-actions";
        store
            .lock()
            .unwrap()
            .meta_set(&format!("{PLAN_TEXT_PREFIX}{sid}"), "THE-PLAN")
            .unwrap();
        store
            .lock()
            .unwrap()
            .todos_replace(
                sid,
                &[crate::store::TodoItem {
                    id: "a".into(),
                    content: "THE-TASK".into(),
                    status: "pending".into(),
                }],
            )
            .unwrap();
        let plan_only = reader(&store, sid).call(json!({"action": "plan"})).await;
        assert!(plan_only.contains("THE-PLAN"), "{plan_only}");
        assert!(!plan_only.contains("THE-TASK"), "{plan_only}");
        let tasks_only = reader(&store, sid).call(json!({"action": "tasks"})).await;
        assert!(tasks_only.contains("THE-TASK"), "{tasks_only}");
        assert!(!tasks_only.contains("THE-PLAN"), "{tasks_only}");
        let both = reader(&store, sid).call(json!({"action": "status"})).await;
        assert!(
            both.contains("THE-PLAN") && both.contains("THE-TASK"),
            "{both}"
        );
        // The model-facing spellings it will actually emit.
        for (a, want) in [
            ("show_plan", "THE-PLAN"),
            ("todo", "THE-TASK"),
            ("view", "THE-PLAN"),
        ] {
            let out = reader(&store, sid).call(json!({"action": a})).await;
            assert!(out.contains(want), "action {a}: {out}");
        }
        // An unrecognized action answers rather than erroring: every
        // intent is visible in the status render.
        let out = reader(&store, sid)
            .call(json!({"action": "sideways"}))
            .await;
        assert!(
            out.contains("THE-PLAN") && out.contains("THE-TASK"),
            "{out}"
        );
    }

    /// `/plan <verb>` must never swallow a mode word and `/plan <mode>`
    /// must never swallow a verb, or a keystroke silently stops meaning
    /// what the user typed. The two tables are disjoint BY TEST, not by
    /// keeping two literals lists in sync in two files.
    #[test]
    fn the_verb_table_and_the_mode_table_are_disjoint() {
        for m in [
            "plan", "on", "true", "yes", "act", "off", "false", "no", "clear", "default",
        ] {
            assert!(
                Verb::parse(m).is_none(),
                "`{m}` is a mode word; a verb claiming it turns `/plan {m}` into a read"
            );
        }
        for v in [
            "show",
            "view",
            "read",
            "current",
            "what",
            "the-plan",
            "show-plan",
            "showplan",
            "tasks",
            "todo",
            "todos",
            "task",
            "list",
            "status",
            "all",
            "both",
            "state",
            "summary",
        ] {
            assert!(
                Mode::parse(v).is_none(),
                "`{v}` is a read verb; Mode claiming it would MOVE the session on a question"
            );
        }
        assert_eq!(Verb::parse("SHOW"), Some(Verb::Show), "case folds");
        assert_eq!(Verb::parse("  tasks "), Some(Verb::Tasks), "trim folds");
    }

    #[tokio::test]
    async fn reading_another_session_names_it_and_uses_the_resume_spelling() {
        let store = scratch_store();
        {
            let s = store.lock().unwrap();
            s.create_session("sp-own").unwrap();
            s.create_session("20260918_other").unwrap();
            s.meta_set(&format!("{PLAN_TEXT_PREFIX}20260918_other"), "OTHER-PLAN")
                .unwrap();
        }
        let out = reader(&store, "sp-own")
            .call(json!({"session": "20260918_other"}))
            .await;
        assert!(out.contains("OTHER-PLAN"), "{out}");
        assert!(
            out.contains("session 20260918_other"),
            "names the subject: {out}"
        );
        assert!(
            !out.contains("session sp-own"),
            "the caller's own id must not appear as the subject: {out}"
        );
        // Unique prefix, the /resume and cron-session spelling.
        let out = reader(&store, "sp-own")
            .call(json!({"session": "20260918_othe"}))
            .await;
        assert!(
            out.contains("session 20260918_other"),
            "resolved to the full id: {out}"
        );
        // `this` is the caller, not a lookup.
        let out = reader(&store, "sp-own")
            .call(json!({"session": "this"}))
            .await;
        assert!(out.contains("this session"), "{out}");
        // An unknown id is refused rather than answered as the caller's.
        let out = reader(&store, "sp-own")
            .call(json!({"session": "no-such-session"}))
            .await;
        assert!(out.starts_with("error:"), "{out}");
    }

    /// A below-`session-history` caller may read THIS session's plan and
    /// not another's — the same floor `session_search` pays, checked
    /// inside the tool because the gate above cannot see the direction.
    #[tokio::test]
    async fn a_guest_reads_its_own_plan_and_no_others() {
        let store = scratch_store();
        {
            let s = store.lock().unwrap();
            s.create_session("sp-guest").unwrap();
            s.create_session("sp-owner").unwrap();
            s.meta_set(&format!("{PLAN_TEXT_PREFIX}sp-owner"), "PRIVATE-PLAN")
                .unwrap();
        }
        crate::acl::set_session_principal("sp-guest", crate::acl::Principal::guest());
        // Another session: refused, and the refusal says what would let it.
        let out = reader(&store, "sp-guest")
            .call(json!({"session": "sp-owner"}))
            .await;
        assert!(out.contains("access denied"), "{out}");
        assert!(
            out.contains("session-history"),
            "names the capability: {out}"
        );
        assert!(
            !out.contains("PRIVATE-PLAN"),
            "a denial must not leak the thing it denies: {out}"
        );
        // Its own: allowed — this is the tool the plan-mode model needs,
        // and a guest session in plan mode is a normal thing.
        let out = reader(&store, "sp-guest").call(json!({})).await;
        assert!(out.contains("this session"), "{out}");
        // `this` is not a session reference, so it is not a cross-read.
        let out = reader(&store, "sp-guest")
            .call(json!({"session": "this"}))
            .await;
        assert!(out.contains("this session"), "{out}");
        // The gate sits BEFORE resolution: an unknown id from a guest
        // gets the capability answer, not "does this prefix match".
        let out = reader(&store, "sp-guest")
            .call(json!({"session": "20260918"}))
            .await;
        assert!(out.contains("access denied"), "{out}");
        // No cleanup needed: the principal map is keyed by session id,
        // and "sp-guest" belongs to this test alone.
    }

    /// Plan mode freezes what CHANGES things. A read must survive the
    /// freeze or the mode makes the plan unaskable in exactly the mode
    /// where it is the only thing that matters.
    #[test]
    fn the_reader_is_not_a_way_plan_mode_breaks_itself() {
        assert!(refuses("show_plan", &json!({}), None).is_none());
        assert!(refuses("show_plan", &json!({"session": "x"}), None).is_none());
    }

    /// The registry half of the two claims in `ShowPlanTool`'s doc,
    /// pinned rather than asserted: READING a plan needs no human, so
    /// the tool rides `base_registry` (cron and A2A, the sessions that
    /// wake a plan-mode conversation) while `present_plan` deliberately
    /// does NOT (nobody there could approve), and it is RESIDENT, since
    /// the deferred tier is exactly where a tool nobody calls goes to
    /// hide (the `skill_manage` and `shell`/`monitor` lessons).
    #[test]
    fn the_reader_reaches_every_kind_of_session() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().expect("store dir");
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).expect("store"),
        ));
        let base = crate::tools::base_registry(store.clone(), "sp-base");
        let names = base.names();
        assert!(
            names.iter().any(|n| n == "show_plan"),
            "base_registry must carry the reader: {names:?}"
        );
        assert!(
            !base.deferred_names().contains(&"show_plan".to_string()),
            "resident, not deferred — a deferred reader is one a model will not pay to find"
        );
        assert!(
            !names.iter().any(|n| n == "present_plan"),
            "the ASK stays out of base_registry (precondition: it is builtin-only)"
        );
        let full = crate::tools::builtin_registry(store.clone(), None, "sp-full");
        assert!(full.names().iter().any(|n| n == "present_plan"));
        assert!(full.names().iter().any(|n| n == "show_plan"));
    }
}
