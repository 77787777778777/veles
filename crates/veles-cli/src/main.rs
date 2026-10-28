// The composition root binary: its spawn sites drive its own runtime.
// The M3 hop (clippy.toml) is core/frontends business.
#![allow(clippy::disallowed_methods)]

//! veles CLI — surface parity target: `hermes` command family.

// The shared crates come through the `veles-runtime` dylib, so core's
// statics exist once in the process (PLAN-plugins-installer.md M1; lib.rs
// says the same).
use veles_runtime::veles_core;
use veles_runtime::veles_frontends;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "veles",
    version,
    about = "Veles Agent — Rust core + Tcl scripting"
)]
struct Cli {
    /// Named profile: an isolated config/state root under the default
    /// data directory's profiles/; default keeps its original layout.
    /// See crates/veles-core/src/profiles.rs for selection and safety.
    #[arg(long, global = true)]
    profile: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum InstallAction {
    State,
    Preview {
        source: std::path::PathBuf,
    },
    Bundle {
        source: std::path::PathBuf,
        #[arg(long)]
        yes: bool,
    },
    /// Download and verify a release before installing its core bundle
    Download {
        /// GitHub release asset directory for this exact repository and tag
        #[arg(long)]
        release: String,
        #[arg(long, default_value = "")]
        repository: String,
        #[arg(long, default_value = "")]
        tag: String,
        /// Full source commit expected in the signed release manifest
        #[arg(long, default_value = "")]
        commit: String,
        /// Full tag-object SHA expected in the signing certificate
        #[arg(long, default_value = "")]
        workflow_sha: String,
        #[arg(long)]
        yes: bool,
    },
    Configure {
        name: String,
        #[arg(long)]
        answers: std::path::PathBuf,
        #[arg(long, default_value = "")]
        instance: String,
        #[arg(long)]
        yes: bool,
    },
    Component {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Preview or apply a named installation profile from typed answers
    /// (TOML or JSON). `--template <profile>` prints a commented answers
    /// file to fill in: desktop, server, container-host or nixos.
    Profile {
        #[arg(long, required_unless_present = "template")]
        answers: Option<std::path::PathBuf>,
        #[arg(long, conflicts_with_all = ["answers", "core", "yes"])]
        template: Option<String>,
        #[arg(long)]
        core: Option<std::path::PathBuf>,
        #[arg(long)]
        yes: bool,
    },
    /// Prepare a reviewed request for host components (own PDS, local
    /// media, pool hub, pool member). Applied only by `profile-handoff`.
    /// `--template <component>` prints a commented answers file: pds,
    /// media, pool-hub or pool-member.
    HostComponent {
        #[arg(long, required_unless_present = "template")]
        answers: Option<std::path::PathBuf>,
        #[arg(long, conflicts_with_all = ["answers", "yes"])]
        template: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Prepare a confined container-host request. This never executes it.
    ContainerHost {
        #[arg(long)]
        answers: std::path::PathBuf,
        #[arg(long)]
        storage_root: std::path::PathBuf,
        #[arg(long)]
        gateway_user: String,
        #[arg(long)]
        sandbox_user: String,
        #[arg(long)]
        network: String,
        #[arg(long)]
        gateway_image: String,
        #[arg(long)]
        gateway_image_digest: String,
        #[arg(long, default_value = "gateway")]
        gateway_name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Execute a previously reviewed system/container profile request.
    /// This is intentionally CLI-only; the API and Tk only create it.
    ProfileHandoff {
        #[arg(long)]
        request: std::path::PathBuf,
        #[arg(long)]
        yes: bool,
    },
    Rollback {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    Verify,
    History,
    #[command(hide = true)]
    Serve,
}

#[derive(Subcommand)]
enum Command {
    /// Interactive chat (default surface)
    Chat {
        /// Single query, exit after answering (hermes chat -q parity)
        #[arg(short = 'q', long)]
        query: Option<String>,
        /// Model override
        #[arg(short = 'm', long)]
        model: Option<String>,
        /// Provider override (custom provider name, with or without 'custom:')
        #[arg(short = 'p', long)]
        provider: Option<String>,
        /// Reasoning effort override
        #[arg(short = 'r', long)]
        reasoning: Option<String>,
        /// Quiet one-shot: print ONLY the final reply (delegate_task children)
        #[arg(short = 'Q', long)]
        quiet: bool,
        /// Resume the latest session (hermes --continue parity)
        #[arg(long)]
        r#continue: bool,
        /// Resume a specific session by id or prefix (hermes --resume parity)
        #[arg(long)]
        resume: Option<String>,
        /// Headless JSON I/O: line-delimited events on stdout; answer
        /// lines on stdin (ending with a line "EOF"). S5 stream-json.
        #[arg(short = 'j', long)]
        io_json: bool,
        /// Force the ratatui TUI (also: --tui)
        #[arg(long)]
        tui: bool,
        /// Force the plain line-oriented REPL: no alternate screen, no
        /// spinners, semantic text markers (a11y linear mode — also the
        /// best surface for screen readers and SSH-from-phone).
        #[arg(long)]
        plain: bool,
        /// Force the plain readline REPL (the TUI is the default on a TTY)
        #[arg(long)]
        repl: bool,
    },
    /// Start the veles-desktop GUI window (Tcl/Tk; auto-starts this
    /// profile's gateway if it isn't already running)
    #[command(alias = "desktop")]
    Gui,
    /// Start the gateway (telegram/irc adapters, A2A + desktop API +
    /// OpenAI proxy servers when their platform flags are enabled)
    Gateway {
        #[command(subcommand)]
        action: GatewayAction,
    },
    /// Setup wizard for provider, model, network and secrets
    Setup,
    /// Preview/install/upgrade/rollback components and reviewed local bundles
    Install {
        #[command(subcommand)]
        action: InstallAction,
    },
    /// Health check (hermes doctor parity)
    Doctor {
        /// Inspect the fixed prefix: the system prompt + tool schemas
        /// every LLM round pays before history, per-block sizes, and
        /// the BLAKE3 hash the prompt-cache story turns on.
        #[arg(long)]
        prefix: bool,
        /// With --prefix: dump the system prompt verbatim instead of
        /// summarizing it.
        #[arg(long)]
        raw: bool,
        /// List every automatic mechanism: whether it is wired, whether
        /// it is on, and the config key that changes it.
        #[arg(long)]
        mechanisms: bool,
        /// Resolve the configured [sandbox] backend, report the binary
        /// and artifacts it needs, and RUN a script through it. The one
        /// command that proves the fence exists — used by every install
        /// script and by scripts/sandbox_vm_check.sh.
        #[arg(long)]
        sandbox: bool,
        /// List the wires this build speaks, which of them a PLUGIN
        /// supplied (api_mode = "plugin"), and ask each route in the
        /// chain for one word until one answers. A driver registers
        /// when its plugin loads, which is after config validation — so
        /// this is the only surface that can say whether a
        /// `driver = "..."` line names something real. The asking half
        /// costs real tokens.
        #[arg(long)]
        providers: bool,
        /// With --providers: skip the live "one word" turn entirely —
        /// drivers and the route chain are read-only. Without it each
        /// route is asked until one answers.
        #[arg(long)]
        no_probe: bool,
    },
    /// Pick model/provider (hermes model parity)
    Model,
    /// Live Tcl interpreter surfaces (eval/repl/reload/file) — the
    /// runtime's own lisp, inspectable at runtime
    Tcl {
        #[command(subcommand)]
        action: TclAction,
    },
    /// R1 spike: load a shared-core module (.so) and run the go/no-go
    /// checks against it (PLAN-plugins-installer.md R1). Dev-only and
    /// hidden: the checks prove the M1/M2/M3 architecture before R3
    /// builds the real module host, which replaces this.
    #[command(hide = true)]
    ModuleSpike {
        /// Which check to run: statics, contained, unload, pinned,
        /// rebuild, panic. `all` runs the non-destructive sequence.
        #[arg(long, default_value = "all")]
        check: String,
        /// Path to the module .so (default: the workspace's echo
        /// module in target/debug/deps).
        #[arg(long)]
        so: Option<std::path::PathBuf>,
    },
    /// R3: run one module-host test case in this binary (the runtime
    /// dylib is linked here; a cargo test binary links core statically
    /// and cannot load modules). Hidden: driven by
    /// crates/veles-cli/tests/module_host.rs.
    #[command(hide = true)]
    ModuleHostTest {
        /// load-unload | stray-spawn | spin-keeps-mapped | quit-cut-short |
        /// standby-quit | stale-call | key-mismatch | rescan | irc-module |
        /// irc-live (manual: $VELES_TEST_IRC=host:port)
        case: String,
    },
    /// Manage plugins (hermes plugins parity)
    Plugins {
        #[command(subcommand)]
        action: Option<PluginsAction>,
    },
    /// Running subagents, background shells and monitors (asks the
    /// running instance over loopback -- the registry is in-process)
    Tasks {
        #[command(subcommand)]
        action: Option<TasksAction>,
    },
    /// Manage cron jobs (hermes cron parity)
    Cron {
        #[command(subcommand)]
        action: Option<CronAction>,
    },
    /// Manage channel-event triggers (slopdrop bridge + vision)
    Triggers {
        #[command(subcommand)]
        action: Option<TriggersAction>,
    },
    /// Back up the brain — BOTH roots since the XDG split — to a
    /// timestamped directory. Config root: config.toml, .env, commands,
    /// plugins, skills. Data root: state.db (sessions, memory, cron),
    /// notes, checkpoints, scripts, plans, certs, plugin-state,
    /// module-state.
    Backup {
        /// Destination directory (default:
        /// $XDG_DATA_HOME/veles/backups/<ts>, i.e.
        /// ~/.local/share/veles/backups/<ts> — $VELES_HOME overrides)
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
    },
    /// Restore a backup (refuses while the gateway is running)
    Restore {
        /// Backup directory produced by `veles backup`
        dir: std::path::PathBuf,
    },
    /// Rebuild this node from a pool peer after its home was lost
    /// (PLAN-recover.md, docs/recovery.md). On the arbiter: `veles pool
    /// invite <name> --recover`; here, on the bare home: `veles recover
    /// '<invite>' --as <name>`. It takes the old identity back, re-links
    /// every member with new tokens, and pulls this node's sessions (the
    /// peer's replica of it), memory, config, the peer's skills and
    /// commands, and asks the vault to admit new keys — then lists what
    /// it could not restore. Refuses while a gateway runs on this home.
    Recover {
        /// The recovery invite: its text, a file holding it, or a photo
        /// of its QR code. Omit it to run the pulls again over the links
        /// a first recovery wrote (each step is idempotent).
        invite: Option<String>,
        /// Take back the identity the invite names: its node id, a prefix
        /// of 8 or more, or its name.
        #[arg(long = "as")]
        as_node: Option<String>,
        /// Pull from this peer (an [a2a_agents] name) instead of the
        /// arbiter.
        #[arg(long)]
        from: Option<String>,
        /// Steps to leave out: store, memory, config, files, vault
        /// (repeat it, or a comma list).
        #[arg(long, value_delimiter = ',')]
        skip: Vec<String>,
        /// Recover without showing what will be written first.
        #[arg(long)]
        yes: bool,
    },
    /// Migrate hermes config.yaml to veles config.toml
    Migrate,
    /// Migrate the hermes data config does not cover: skills, cron jobs
    /// and usage history. Idempotent, and never writes to ~/.hermes.
    ///
    /// Skills matter most: veles READS ~/.hermes/skills at runtime, so
    /// until they are copied the hermes install cannot be deleted.
    MigrateData {
        /// Report what would move, write nothing
        #[arg(long)]
        dry_run: bool,
        /// Only this step: skills | cron | usage (default: all three)
        #[arg(long)]
        only: Option<String>,
    },
    /// Run one tool by name and print its result (execute_code bridge)
    Tool {
        /// Tool name (e.g. read_file, terminal, search_files)
        #[arg(long)]
        name: String,
        /// Tool arguments as a JSON object
        #[arg(long)]
        args: String,
        /// Use the FULL session registry instead of the base one.
        ///
        /// The default is base_registry — no delegate_task, no
        /// cronjob, no clarify — so a script or a plugin reaching for
        /// `veles tool` cannot spawn agents or schedule jobs by
        /// accident. `--full` is the owner's own escalation: it adds
        /// nothing this shell could not already do through
        /// `veles chat -q`, and it names the step.
        #[arg(long)]
        full: bool,
    },
    /// Tool residency: what rides in every prompt vs what is
    /// discoverable on demand, with each schema's byte cost
    Tools {
        #[command(subcommand)]
        action: Option<ToolsAction>,
    },
    /// How often each tool is actually called — the regression watch
    /// behind tool-tier decisions (a resident tool nobody uses is a
    /// demotion candidate; a deferred tool at zero is the skill_manage /
    /// delegate_task story)
    Toolstats {
        /// Only calls in the last N days (default: all time)
        #[arg(long)]
        days: Option<i64>,
        /// Break ONE tool out by day instead of the totals table
        #[arg(long)]
        tool: Option<String>,
        /// How many rows of the totals table to print (default 40)
        #[arg(long, default_value_t = 40)]
        limit: usize,
    },
    /// Import hermes session history into veles' store (one-shot
    /// migration aid; refuses while the gateway runs; idempotent)
    ImportHermes {
        /// Hermes state.db (default ~/.hermes/state.db)
        #[arg(long)]
        db: Option<std::path::PathBuf>,
        /// Only import sessions from this source (telegram, desktop, cli, ...)
        #[arg(long)]
        source: Option<String>,
        /// Only import sessions with activity in the last N days
        #[arg(long)]
        since_days: Option<i64>,
        /// Report what would be imported, write nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Undo points for the agent's file edits
    Checkpoints {
        #[command(subcommand)]
        action: Option<CheckpointAction>,
    },
    /// Manage secrets: stored in the encrypted vault (`veles vault`),
    /// used by the engine, and by the model as $NAME in commands or
    /// through api_request, as each secret's policy allows. Values are
    /// never printed and are scrubbed from all tool output.
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// The encrypted vault this node shares with the rest of the pool:
    /// keys, members, sync, and importing the old env files
    Vault {
        #[command(subcommand)]
        action: VaultAction,
    },
    /// Tickets: the authorization for a call another node asks to run
    /// beyond the User role — signed on a device, never by a gateway
    Ticket {
        #[command(subcommand)]
        action: TicketAction,
    },
    /// The headless ticket signer for IRC and Telegram: its own key and
    /// its own bot/nick; asks the owner, signs on an explicit yes
    Signer {
        #[command(subcommand)]
        action: SignerAction,
    },
    /// Memory — every file, and its history, in state.db: list, read,
    /// edit, search, diff, restore, replicate
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// Config replication (B2): log this node's config writes, offer
    /// them to a peer, apply what a peer sent. No master — every node
    /// merges the union of everyone's log.
    Configsync {
        #[command(subcommand)]
        action: ConfigsyncAction,
    },
    /// Store replication (B5): ship this node's session data to peers
    /// as read-only replica files (`replicas/<node>.db`), and fold what
    /// peers sent. One writer per session stays — a replica is the
    /// owner's image, never a second writer.
    Replicas {
        #[command(subcommand)]
        action: ReplicasAction,
    },
    /// Session migration between pool nodes (B6): move a session (and
    /// its subtree) from this node to a registry peer, at a turn
    /// boundary — quiesce, copy, adopt there, fence + tombstone here.
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Joining the pool by QR (PLAN-sessions-and-phone.md P2d): on the
    /// arbiter, mint a one-time invite; on the new node, join with it.
    /// The arbiter introduces the newcomer to every member.
    Pool {
        #[command(subcommand)]
        action: PoolAction,
    },
    /// Pairing codes (S9): let a friend use the gateway bot without
    /// sharing keys. Owner-side management; redemption happens in-chat
    /// via `/pair <CODE>`.
    Pair {
        #[command(subcommand)]
        action: PairAction,
    },
    /// Access control: who may do what, and why. `acl explain` answers
    /// "why does this person have the role they have" — the question
    /// specificity precedence creates, since it is no longer about a
    /// line number in config.toml.
    Acl {
        /// Subcommand and its arguments, exactly as a chat surface
        /// spells them: `list`, `explain <id|mask> [net] [chan]`,
        /// `grant <id|mask> <role>`, `revoke <id|mask>`,
        /// `user save|rm …`, `limits`, `export`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The named sandboxes: which exist, who lands in which, and
    /// purging one. Same verbs as `/sandbox` on every other surface.
    Sandbox {
        /// `list`, `show <name>`, `create <name> [--image …]`,
        /// `assign <name> --role guest …`, `unassign …`, `drop <name>`,
        /// `purge <name> [--work]`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The emergency brake: refuse every connector, serve only the
    /// local frontends. This IS a local frontend, so it is one of the
    /// three places that can LIFT one.
    Lockdown {
        /// Nothing (status), `on [reason]`, or `off`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Per-role context ceilings (`[acl.limits]`): read them, or set
    /// one live.
    Limits {
        /// Nothing (list), `set <role|default> <tokens>`, or
        /// `clear <role|default>`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The compute pool (Plan B): this node's identity/capabilities and
    /// every registered node, from the cluster registry in state.db.
    Nodes {
        #[command(subcommand)]
        action: Option<NodesAction>,
    },
    /// Dreaming — background reflection that consolidates recent
    /// sessions into durable memory lessons (S2)
    Dream,
    /// The option surface: every key this binary reads, what your config
    /// sets, and what it holds that nothing reads
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Install a skill from a git URL (S13; lands in the veles skills
    /// root — `~/.config/veles/skills/`, or `$XDG_CONFIG_HOME/veles/skills`)
    Skills {
        #[command(subcommand)]
        action: SkillsAction,
    },
    /// Show version and build info
    Version,
}

#[derive(Subcommand)]
enum ToolsAction {
    /// Every tool with its tier (core = resident in every prompt,
    /// deferred = discoverable via tool_search) and its schema size.
    List,
}

#[derive(Subcommand)]
enum NodesAction {
    /// Every registered node (default). Self is marked; stale nodes
    /// (heartbeat older than [cluster].stale_after_secs) are flagged.
    List,
    /// This node's own identity and probed capabilities — what it
    /// advertises to the pool.
    Whoami,
    /// One node in full, by id or unique id prefix.
    Show {
        /// Node id or unique prefix
        id: String,
    },
    /// Re-run the capability probe and re-register this node (what the
    /// gateway does at boot), without restarting the gateway.
    Probe,
    /// Remove a node from the registry (explicit decommission — a
    /// node that simply went quiet goes stale, it is not deleted).
    Forget {
        /// Node id or unique prefix
        id: String,
    },
}

#[derive(Subcommand)]
enum SkillsAction {
    /// Install a skill from a git URL (shallow clone + validate SKILL.md)
    Install {
        /// Git URL of the skill repo
        url: String,
        /// Install under this name instead of the repo/frontmatter name
        #[arg(long)]
        name: Option<String>,
        /// Replace an already-installed skill of the same name
        #[arg(long)]
        overwrite: bool,
    },
}

#[derive(Subcommand)]
enum CheckpointAction {
    /// Count, size and the five most recent (default)
    Status,
    /// List checkpoints, newest first
    List {
        /// Only this session
        #[arg(long)]
        session: Option<String>,
    },
    /// Show one checkpoint and a preview of the saved content
    Show { id: i64 },
    /// Put a file back the way it was (itself checkpointed first)
    Restore { id: i64 },
    /// Apply age/size retention now
    Prune,
}

#[derive(Subcommand)]
enum SecretAction {
    /// Store a secret in the vault. Omit the value to be prompted: it
    /// does not echo, and it stays out of your shell history.
    Set {
        name: String,
        value: Option<String>,
        /// provider | platform | user (default: what the config uses it for)
        #[arg(long)]
        kind: Option<String>,
        /// Hosts api_request may send it to, comma-separated
        #[arg(long)]
        hosts: Option<String>,
        /// Keep it out of commands: the engine and api_request only
        #[arg(long)]
        no_shell: bool,
        /// One line on what it is for
        #[arg(long)]
        note: Option<String>,
    },
    /// List secrets: names, what each may be used for, where it lives
    /// (never values)
    List,
    /// Remove a secret (from the vault, else from secrets.env)
    Unset { name: String },
    /// Change what the model may do with a stored secret
    Policy {
        name: String,
        /// on | off — `$NAME` in commands
        #[arg(long)]
        shell: Option<String>,
        /// Hosts api_request may send it to, comma-separated ("" = none)
        #[arg(long)]
        hosts: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
}

#[derive(Subcommand)]
enum VaultAction {
    /// Create the vault with this node as its first member (a first
    /// `veles secret set` does this too)
    Init,
    /// This node's fingerprint and public keys — what the other machine
    /// compares before an admit or a first sync
    Whoami,
    /// Members, the secrets this node can read, and the inbox
    Status,
    /// Ask to join a vault: send this node's public keys to a member's
    /// inbox (--push), or print the request to carry over by hand
    Join {
        /// A member's A2A URL, or a peer name from [a2a_agents]
        #[arg(long)]
        push: Option<String>,
        #[arg(long)]
        bearer: Option<String>,
        /// Write the request to this file instead of stdout
        #[arg(long)]
        out: Option<String>,
    },
    /// This node's vault keys were lost (or must be replaced): set the
    /// old key files aside, mint new ones, and send the join request a
    /// member admits with `veles vault admit <node> --fingerprint <new>`
    Reidentify {
        /// A member's A2A URL, or a peer name from [a2a_agents]
        #[arg(long)]
        push: Option<String>,
        #[arg(long)]
        bearer: Option<String>,
        /// Write the request to this file instead of stdout
        #[arg(long)]
        out: Option<String>,
    },
    /// Admit a node whose join request is in the inbox (or a file),
    /// after comparing its fingerprint with `veles vault whoami` there
    Admit {
        /// Node id (or a unique prefix) or name
        node: String,
        #[arg(long)]
        fingerprint: String,
        /// Read the request from this file instead of the inbox
        #[arg(long)]
        file: Option<String>,
    },
    /// Remove a member. Prints the secrets it could read: replace those
    /// at their providers, because it may have kept them
    Revoke { node: String },
    /// Re-encrypt every value for exactly the current members
    Rekey,
    /// Write this node's vault as an offer (stdout, or --out FILE)
    Offer {
        #[arg(long)]
        out: Option<String>,
    },
    /// Send this node's vault to a peer's inbox
    Push {
        /// A2A URL, or a peer name from [a2a_agents]
        peer: String,
        #[arg(long)]
        bearer: Option<String>,
    },
    /// List what is waiting in the vault inbox
    Inbox,
    /// Apply an offer: a file (`-` = stdin), or every offer in the inbox.
    /// A first sync needs --trust with the root's fingerprint
    Apply {
        file: Option<String>,
        #[arg(long)]
        inbox: bool,
        #[arg(long)]
        trust: Option<String>,
    },
    /// Copy the credentials in .env and secrets.env into the vault. Never
    /// edits either file: it lists the lines you can then delete
    Import {
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ConfigsyncAction {
    /// Scan config.toml and log this node's replicated-key changes
    /// (the append-only config_log). Every write path lands here,
    /// because detection is on the file, not on a setter.
    Scan,
    /// Where replication stands: rows in the log, per-node high-water
    /// marks, pending inbox offers.
    Status,
    /// Write this node's rows past a cursor to a file (or stdout) as a
    /// config sync offer, for moving it by hand.
    Offer {
        /// Local rowid cursor: offer rows PAST this (0 = everything).
        #[arg(long, default_value_t = 0)]
        since: i64,
        /// Output file (`-` = stdout, the default).
        #[arg(long)]
        out: Option<String>,
    },
    /// Push this node's rows past a cursor to a peer's configsync
    /// inbox over the A2A link, by hand (the gateways do it on their
    /// own with `[cluster] configsync_auto`). The peer stores it — only
    /// if it names this node in its `[cluster] memsync_peers` — and
    /// applies it on arrival when its own configsync_auto is on, else
    /// with `veles configsync apply --inbox`.
    Push {
        /// Peer's A2A base URL (e.g. http://vps:9900/a2a/).
        peer: String,
        /// Local rowid cursor (0 = everything).
        #[arg(long, default_value_t = 0)]
        since: i64,
        /// Bearer token for the peer, when it wants one (else the env).
        #[arg(long)]
        bearer: Option<String>,
    },
    /// List the config sync offers waiting in this node's inbox.
    Inbox,
    /// Apply a config sync offer: a file (`-` = stdin), or with
    /// `--inbox` every offer waiting, oldest first. Imports the rows,
    /// merges EVERYTHING (local history included), and writes the
    /// merged state into config.toml (with a backup). Exit 2 when a
    /// cross-node conflict remains — the LWW winner is already
    /// written; the banner demands the human decision.
    Apply {
        /// Offer file (`-` = stdin). Omit with --inbox.
        file: Option<String>,
        /// Apply every pending inbox offer in arrival order; clean ones
        /// move to inbox/applied/, conflicting ones stay listed.
        #[arg(long)]
        inbox: bool,
        /// Show what applying would change, and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ReplicasAction {
    /// Install the store log + triggers on this node's state.db and
    /// stamp the owner marker; a fresh install also backfills the
    /// sessions active in `[cluster] storesync_backfill_days`. The
    /// gateway does this at boot when `storesync_peers` names anyone;
    /// this is the by-hand path.
    InstallLog,
    /// Put the current rows of every session active in the last DAYS into
    /// the store log, so the next push carries them whole — for a replica
    /// that must be able to take those sessions over.
    Backfill {
        /// Days of session activity (default: `[cluster]
        /// storesync_backfill_days`, 14 unless set)
        #[arg(long)]
        days: Option<u32>,
    },
    /// Where replication stands: this node's log cursor, replica files
    /// held, pending inbox offers.
    Status,
    /// Push this node's log rows past a cursor to a peer's storesync
    /// inbox over the A2A link (chunked). The peer stores it — only if
    /// it names this node in its `[cluster] storesync_peers`; the
    /// operator applies it there with `veles replicas apply --inbox`.
    Push {
        /// Peer's A2A base URL (e.g. http://vps:9900/a2a/).
        peer: String,
        /// Local log cursor (0 = everything; the peer's `veles replicas
        /// status` names the cursor it holds).
        #[arg(long, default_value_t = 0)]
        since: i64,
        /// Bearer token for the peer, when it wants one (else the env).
        #[arg(long)]
        bearer: Option<String>,
    },
    /// List the store sync offers waiting in this node's inbox.
    Inbox,
    /// Apply a store sync offer: a file (`-` = stdin), or with
    /// `--inbox` every offer waiting, oldest first. Folds rows into the
    /// sender's replica file; gaps refuse the offer (re-push from zero).
    Apply {
        /// Offer file (`-` = stdin). Omit with --inbox.
        file: Option<String>,
        /// Apply every pending inbox offer in arrival order; applied
        /// ones move to inbox/applied/.
        #[arg(long)]
        inbox: bool,
    },
    /// The replica files this node holds, with cursors and row counts.
    List,
    /// FTS search across every replica file, hits tagged with the
    /// owning node — history stays findable when the owner is down.
    Search {
        /// Search terms.
        query: String,
        /// Max hits (default 20).
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

#[derive(Subcommand)]
enum SignerAction {
    /// Unlock the key, connect the signer's own bot/nick, serve gateways.
    ///
    /// Reads DIR/signer.toml (key, listen, [telegram] owner_id,
    /// [irc] server/nick/owner_nick/owner_account) and DIR/signer.env
    /// (SIGNER_GATEWAY_TOKENS=name:token,…, SIGNER_TELEGRAM_TOKEN,
    /// SIGNER_IRC_PASSWORD). DIR defaults to the key directory `veles
    /// ticket keygen` writes. The key's passphrase is read from the file
    /// SIGNER_PASSPHRASE_FILE names (a systemd credential, say), else
    /// asked for on the terminal. A gateway reaches it through its own
    /// `[cluster] signers = ["http://…"]` and VELES_SIGNER_TOKEN.
    Run {
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum TicketAction {
    /// Make this device a ticket signer: a new key, encrypted to a
    /// passphrase, and the lines each gateway that should trust it adds
    Keygen {
        /// A name for the key (e.g. laptop-desktop, phone)
        name: String,
    },
    /// Show a ticket request in full and sign it if you say yes
    Sign {
        /// The request (`veles-ticket-request:1.…`), as posted
        request: String,
        /// Which key (default: the only one in the signer directory)
        #[arg(long)]
        key: Option<String>,
        /// Sign without asking (the call is still printed)
        #[arg(long)]
        yes: bool,
    },
    /// Write a request by hand — the emergency path, and a test aid
    Request {
        /// The node that will run the call
        #[arg(long)]
        target: String,
        #[arg(long)]
        tool: String,
        /// The call's arguments, as JSON
        #[arg(long, default_value = "{}")]
        args: String,
        #[arg(long, default_value = "")]
        session: String,
        /// The role it runs as: user, trusted, admin, owner
        #[arg(long, default_value = "owner")]
        grant: String,
        /// The node making the call (default: this machine)
        #[arg(long)]
        origin: Option<String>,
    },
    /// Check a ticket against its request, as the request's target would
    Check { ticket: String, request: String },
}

#[derive(Subcommand)]
enum PoolAction {
    /// On the arbiter: a one-time invite (10 minutes, single use) for a
    /// node the pool will call NAME. Prints the WireGuard QR first when
    /// `[pool.wireguard]` is set (scan it with the WireGuard app), then
    /// the veles QR (scan it on the Phone page, or paste its text into
    /// `veles pool join`).
    Invite {
        /// What the pool will call the new node: a-z, 0-9 and -.
        name: String,
        /// How the pool reaches it (default: its minted tunnel address).
        #[arg(long)]
        url: Option<String>,
        /// Seconds the invite is good for.
        #[arg(long, default_value_t = veles_core::pool::DEFAULT_TTL_SECS)]
        ttl: u64,
        /// Also write the codes as PNGs into this directory.
        #[arg(long)]
        png: Option<std::path::PathBuf>,
        /// Print the invite's text only (no drawn codes): for a script,
        /// or for pasting.
        #[arg(long)]
        text: bool,
        /// Also write the WireGuard client config (the first QR's text) to
        /// this file, mode 0600: a Linux node installs it with the
        /// `pool-member` host component. It holds the newcomer's private key.
        #[arg(long)]
        wireguard_file: Option<std::path::PathBuf>,
        /// A recovery invite: re-admit the member NAME, whose home was
        /// lost, under the node id it had (`veles recover --as` on it).
        /// Its links are rotated when it redeems.
        #[arg(long)]
        recover: bool,
        /// With --recover: the node id (or a unique prefix) to re-admit,
        /// when this node cannot tell which node NAME is.
        #[arg(long, requires = "recover")]
        node: Option<String>,
    },
    /// On the new node: join the pool with an invite — its text, a file
    /// holding it, or a photo (JPEG/PNG) of its QR code.
    Join {
        /// The invite text, or a path to a text file or a photo.
        invite: String,
        /// Join without showing what will be written first.
        #[arg(long)]
        yes: bool,
    },
    /// The invites this arbiter has minted, open and used.
    Invites {
        /// Drop the open invite for this name (and its tunnel).
        #[arg(long)]
        revoke: Option<String>,
    },
    /// On the arbiter: introduce two members to each other with a fresh
    /// token pair — for a member that was off when the other joined
    /// (a join introduces the newcomer only to who answers), and to
    /// rotate a pair's tokens (a member that has the peer already takes
    /// the new tokens in place of the old). Both must answer.
    Introduce {
        /// A member's name, as this node's [a2a_agents] has it.
        a: String,
        /// The other member's name.
        b: String,
    },
}

#[derive(Subcommand)]
enum SessionAction {
    /// Migrate a session (and its whole subtree — question 1) to a
    /// registry node. Refuses while a turn is live or the queue is
    /// mid-drain (the turn-boundary protocol); names what does NOT
    /// travel and what the target's first turn re-pays.
    Migrate {
        /// The session id to move (the subtree root).
        id: String,
        /// Target node: registry name, id, or unique id prefix (the
        /// B4 placement spelling).
        node: String,
        /// Migrate a CHILD whose parent stays behind, orphaning its
        /// parent link honestly (refused by default — the link would
        /// cross files).
        #[arg(long)]
        force: bool,
    },
    /// Finish or undo a PARKED migration (`migrating_to`) whose push
    /// never confirmed: queries the target, retires here if it
    /// adopted, un-parks if it confirmed not-adopted. Never both —
    /// the query decides.
    Reclaim {
        /// The parked session id.
        id: String,
    },
    /// Where a session lives: this store's stamp, or which replica
    /// holds it (B5's `session_owner`), and its migration state.
    Owner {
        /// The session id to ask about.
        id: String,
    },
    /// Take over a session whose node stopped answering, from this node's
    /// replica of it. The arbiter allows it once that node's lease has run
    /// out; when the arbiter is the node that died, a ticket signed with
    /// any trusted key does (the command prints the request to sign).
    Takeover {
        /// The session id (the subtree root).
        id: String,
        /// A ticket authorizing it (`veles-ticket:1.…`)
        #[arg(long)]
        ticket: Option<String>,
    },
    /// Continue a replica's session HERE under a new id. Needs nobody's
    /// permission: the owner's session is untouched.
    ForkReplica {
        /// The session id in the replica.
        id: String,
    },
    /// Takeovers of THIS node's sessions that other nodes recorded while
    /// it was away: each is checked and settled (the session is handed
    /// over, and anything written here since continues as a fork).
    Takeovers,
    /// The sessions some veles process on this machine holds open or is
    /// running a turn in — what `session send` can reach right away.
    List {
        /// Also the live sessions on the pool nodes this one reaches
        /// (`[a2a_agents]`), addressed `<node>/<session>`.
        #[arg(long)]
        remote: bool,
    },
    /// Send a session mail: the text runs as that session's next turn,
    /// marked as mail from the command line (not from its user). A
    /// session nobody has open keeps it until it is next opened.
    Send {
        /// A session id, a unique id prefix, or an exact title — or
        /// `<node>/<session>` for a session on another pool node.
        to: String,
        /// The message.
        text: String,
        /// Wait up to this many seconds for the turn's answer, and print it.
        #[arg(long, default_value_t = 0)]
        wait: u64,
    },
    /// A sent mail's state, and its answer once there is one.
    Mail {
        /// The mail id `session send` printed (`<node>/<id>` for mail sent
        /// to another node).
        id: String,
    },
}

#[derive(Subcommand)]
enum MemoryAction {
    /// List memory files: path, size, newest revision, when, and by what
    Ls {
        /// Only paths starting with this (e.g. `reference/`)
        prefix: Option<String>,
    },
    /// Print a memory file (or one of its revisions)
    Cat {
        path: String,
        /// A revision id from `veles memory log` (r12 or 12)
        #[arg(long)]
        rev: Option<String>,
    },
    /// Edit a memory file in $VISUAL/$EDITOR — a new path creates it.
    /// Saved as one revision; if the file changed while you were editing
    /// (the agent wrote it), nothing is overwritten and your text is kept
    /// in the temp file, whose path is printed.
    Edit { path: String },
    /// Set a memory file's content from a file or stdin (scripts)
    Write {
        path: String,
        /// Read the content from this file (default: stdin)
        #[arg(long)]
        file: Option<String>,
    },
    /// Delete a memory file (a revision — `restore` brings it back)
    Rm { path: String },
    /// Rename a memory file (refuses an existing target)
    Mv { from: String, to: String },
    /// Grep every memory file (case-insensitive)
    Search { query: String },
    /// Unified diff between two revisions of a file (default: the last
    /// change — the previous revision against the newest)
    Diff {
        path: String,
        /// Older revision (r12 or 12)
        old: Option<String>,
        /// Newer revision (default: the newest)
        new: Option<String>,
    },
    /// Revision log, newest first
    Log {
        #[arg(default_value_t = 20)]
        n: usize,
        /// Only this file's revisions
        #[arg(long)]
        path: Option<String>,
    },
    /// Make an old revision of a file current again (as a new revision)
    Restore {
        path: String,
        /// The revision to bring back (r12 or 12)
        rev: String,
    },
    /// Roll back a memory file N of its own revisions
    Rollback {
        path: String,
        #[arg(default_value_t = 1)]
        n: usize,
    },
    /// Write every memory file into a new directory, as plain files
    Export { dir: String },
    /// Record every .md file under a directory as a memory revision
    /// (same relative paths; unchanged files record nothing)
    Import { dir: String },
    /// Show the memory tree index (what the prompt carries)
    Tree,
    /// S8: doctor — size, duplication, token share over the tree
    Doctor,
    /// Split oversized system/core.md: keep a resident head that fits
    /// `--resid` (default: the configured [memory] core_char_limit),
    /// move the overflow into reference/topics/*.md, one file per
    /// topic, and regenerate the tree index. Repeatable — merges into
    /// the same topic files rather than overwriting them.
    Curate {
        /// Resident budget in chars (default: core_char_limit)
        #[arg(long)]
        resid: Option<usize>,
    },
    /// Push memory revisions to a peer's memsync INBOX over the A2A link
    /// (Plan B memory replication). Chunked below the A2A request limit;
    /// paths holding credential-shaped strings are withheld and named.
    /// The peer stores the offer — only if it names this node in its
    /// `[cluster] memsync_peers` — and applies nothing until its operator
    /// runs `veles memory apply --inbox`.
    Push {
        /// Peer name (from config a2a_agents) or direct URL
        peer: String,
        /// Only revisions newer than this cursor (default: all, i.e. a
        /// full sync). An incremental offer carries each path's base, so
        /// the peer can fast-forward from where it is.
        #[arg(long, default_value_t = 0)]
        since: i64,
    },
    /// Write a sync offer to a file (or stdout) instead of pushing it —
    /// the manual transport: copy the file to the other node and run
    /// `veles memory apply <file>` there.
    Offer {
        /// Only revisions newer than this cursor (default: all).
        #[arg(long, default_value_t = 0)]
        since: i64,
        /// Output file (default: stdout).
        #[arg(short, long)]
        output: Option<String>,
    },
    /// List the sync offers waiting in this node's memsync inbox.
    Inbox,
    /// One-line cluster read: how many offers are waiting on a decision
    /// (held conflicts included — they stay in the inbox) and how many
    /// cleanly-applied offers the archive holds. `apply --inbox` is what
    /// names the conflicts; the count exists where the fact exists.
    Status,
    /// Remove applied-offer files older than a week from
    /// `inbox/applied/` (the gateway does this automatically). Pending
    /// offers and held conflicts are never touched. Honest by
    /// construction: an unprovable mtime is kept, not deleted.
    PruneApplied {
        /// Files younger than this many days are kept (default 7).
        #[arg(long, default_value_t = 7)]
        days: i64,
    },
    /// Apply a memory sync offer: a file (`-` = stdin), or with
    /// `--inbox` every offer waiting in the inbox, oldest first. Reports
    /// applied/unchanged/conflicts per offer; conflicting paths keep
    /// local content — pick a winner by hand. Exit 2 when any conflict
    /// remains.
    Apply {
        /// Offer file (`-` = stdin). Omit with --inbox.
        file: Option<String>,
        /// Apply every pending inbox offer in arrival order; clean ones
        /// move to inbox/applied/, conflicting ones stay listed.
        #[arg(long)]
        inbox: bool,
    },
}

/// The blob options core cannot derive: each connector lists what it
/// reads out of its own `[platforms.*]` block, and pins that list
/// against its own source.
fn blob_surface() -> anyhow::Result<Vec<veles_core::config_surface::Knob>> {
    let mut out = veles_core::config_surface::platform_surface();
    out.extend(veles_irc::config_surface());
    out.extend(veles_telegram::config_surface());
    out.extend(veles_slopdrop::config_surface());
    out.extend(process_plugin_surfaces_from(
        &veles_core::pluginhost::default_roots(),
    )?);
    Ok(out)
}

/// Read each discovered process plugin's own manifest. This synchronous
/// boundary is called before the gateway's runtime starts, or by the CLI
/// (which is not inside a Tokio runtime). It never invents source-tree
/// plugin roots or silently substitutes a stale executable.
fn process_plugin_surfaces_from(
    roots: &[std::path::PathBuf],
) -> anyhow::Result<Vec<veles_core::config_surface::Knob>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let discovered = veles_core::pluginhost::discover(roots);
    runtime.block_on(async {
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeMap::new();
        for plugin in discovered {
            let manifest = veles_core::pluginhost::describe(&plugin.exe)
                .await
                .map_err(|e| anyhow::anyhow!("{} --describe: {e}", plugin.exe.display()))?;
            for knob in manifest.knobs {
                if seen
                    .insert(knob.path.clone(), plugin.name.clone())
                    .is_some()
                {
                    anyhow::bail!(
                        "process plugins declare config option {} more than once",
                        knob.path
                    );
                }
                out.push(knob);
            }
        }
        Ok(out)
    })
}

/// The vars the connectors read, for the `.env` half of the surface.
/// Derived from each connector's own config surface, so the two cannot
/// drift about which keys have twins.
fn env_blob() -> Vec<veles_core::config_surface::EnvKnob> {
    let mut out = veles_irc::env_surface();
    out.extend(veles_telegram::env_surface());
    out
}

/// `veles config …` — the option surface, and merging it into a file.
///
/// Both halves answer one question from opposite ends: what does this
/// binary read, and what does my config say about it. An installation that
/// never wrote `[sandbox]` is fenced `native` and has no way to ask; that
/// silence is the gap this closes.
fn config_family(action: ConfigAction) -> anyhow::Result<()> {
    use veles_core::config_surface as surface;
    let extras = blob_surface()?;
    let env_extras = env_blob();
    match action {
        ConfigAction::Defaults => {
            print!("{}", surface::defaults_text_with(&extras)?);
            Ok(())
        }
        ConfigAction::EnvDefaults => {
            print!("{}", surface::env_defaults_text(&env_extras));
            Ok(())
        }
        ConfigAction::Tidy { apply, path } => {
            let path = path.unwrap_or_else(veles_core::configedit::config_path);
            let (report, backup) = surface::tidy_file(&path, apply)?;
            if report.is_noop() {
                println!("{}: no live line restates its default", path.display());
            } else {
                println!(
                    "{}: {} line(s) restate their default{}",
                    path.display(),
                    report.commented.len(),
                    if apply { " — commented out" } else { "" }
                );
                for (key, before, after) in &report.commented {
                    println!("  {key}\n    - {before}\n    + {after}");
                }
            }
            for (key, why) in &report.kept {
                println!("  kept live: {key} — {why}");
            }
            if !apply && !report.is_noop() {
                println!("\nwrite it: veles config tidy --apply");
            } else if let Some(bak) = backup {
                println!(
                    "\nwrote {} (previous kept at {})",
                    path.display(),
                    bak.display()
                );
            }
            Ok(())
        }
        ConfigAction::Fill { dry_run, path } => {
            let path = path.unwrap_or_else(veles_core::configedit::config_path);
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            let report = surface::fill_with(&existing, &extras)?;
            print_fill_report(&path, &report, &extras, dry_run);

            // The `.env` beside it. Half of what this installation
            // actually runs on lives there — the IRC identity is eleven
            // vars and no `[platforms.irc]` block at all — so a fill that
            // only touched config.toml would document one file out of two.
            let env_path = path
                .parent()
                .map(|d| d.join(".env"))
                .unwrap_or_else(|| std::path::PathBuf::from(".env"));
            let env_existing = std::fs::read_to_string(&env_path).unwrap_or_default();
            let env_report = surface::env_fill(&env_existing, &env_extras);
            println!();
            print_env_report(&env_path, &env_report, dry_run);

            if dry_run {
                return Ok(());
            }
            if !report.is_noop() {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| anyhow::anyhow!("creating {}: {e}", parent.display()))?;
                }
                if !existing.is_empty() {
                    let bak = backup_path(&path);
                    std::fs::write(&bak, &existing)
                        .map_err(|e| anyhow::anyhow!("writing backup {}: {e}", bak.display()))?;
                    println!("\nprevious config kept at {}", bak.display());
                }
                std::fs::write(&path, &report.text)
                    .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
                println!("wrote {}", path.display());
                println!(
                    "(most sections are read on every use; a restart is only \
                     needed for the ones held from boot — `veles config fill --dry-run` \
                     after editing shows what is left)"
                );
            }
            if !env_report.is_noop() {
                if let Some(parent) = env_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| anyhow::anyhow!("creating {}: {e}", parent.display()))?;
                }
                if !env_existing.is_empty() {
                    // The backup holds the same credentials as the file, so
                    // it gets the same bits: a 644 `.env.bak` next to a 600
                    // `.env` is the secret leaking with an extra step.
                    let bak = backup_path(&env_path);
                    veles_cli::write_private(&bak, &env_existing)?;
                    println!("previous .env kept at {}", bak.display());
                }
                // 0600: this file holds credentials, and a fill that
                // created it with the umask's default would be the reason
                // a token became world-readable.
                veles_cli::write_private(&env_path, &env_report.text)?;
                println!("wrote {}", env_path.display());
            }
            Ok(())
        }
    }
}

/// The backup name for a file: its own name with `.bak` after it.
///
/// `with_extension` is the wrong tool for both files a fill writes. On
/// `config.toml` it happens to produce `config.toml.bak`, but on `.env`
/// there is no extension to replace — a leading dot makes it a hidden
/// file, not a name with an extension — so the backup came out as
/// `.env.env.bak`.
fn backup_path(path: &std::path::Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "config".to_string());
    path.with_file_name(format!("{name}.bak"))
}

/// The `.env` half of the report. What a fill did NOT touch is as much
/// the point as what it added: this file holds live credentials, so
/// "your token is still your token" is a line worth printing.
fn print_env_report(
    path: &std::path::Path,
    report: &veles_core::config_surface::EnvFill,
    dry_run: bool,
) {
    println!(
        "{}{}",
        if dry_run { "audit of " } else { "" },
        path.display()
    );
    if report.added.is_empty() {
        println!("every var is already named — nothing to add");
    } else {
        println!(
            "\n{} vars documented as comments (uncommenting one assigns it):",
            report.added.len()
        );
        println!("  {}", report.added.join(", "));
    }
    if !report.live.is_empty() {
        println!(
            "\n{} vars already carry a value and were left byte-for-byte alone:",
            report.live.len()
        );
        println!("  {}", report.live.join(", "));
    }
    if !report.misplaced.is_empty() {
        println!(
            "\n{} lines that decide where this file is read from, and so cannot \
             be read from it:",
            report.misplaced.len()
        );
        for (name, what) in &report.misplaced {
            println!("  {name} — {what}");
        }
        println!(
            "  export these in the unit file or the shell instead; by the time \
             .env is open, the directory was chosen"
        );
    }
}

/// The report is the point: writing keys is easy, and a config nobody can
/// read back is how an installation ends up with a fence it cannot
/// explain. So every line says what changed *and* what the file was
/// already doing.
fn print_fill_report(
    path: &std::path::Path,
    report: &veles_core::config_surface::Fill,
    extras: &[veles_core::config_surface::Knob],
    dry_run: bool,
) {
    let (total, unset, shown) = veles_core::config_surface::counts(extras);
    println!(
        "{}{} ({total} options: {unset} unset by default, {shown} shown but not written)",
        if dry_run { "audit of " } else { "" },
        path.display()
    );

    if report.documented.is_empty() {
        println!("every option is already named — nothing to add");
    }
    if !report.documented.is_empty() {
        println!(
            "\n{} options documented in comments (`# key = value  # default`; nothing pinned):",
            report.documented.len()
        );
        // Grouped by section: 74 dotted paths in a column is a wall, and
        // the section is the unit a person decides about.
        let mut sections: Vec<(String, Vec<String>)> = Vec::new();
        for full in &report.documented {
            let (section, key) = match full.rsplit_once('.') {
                Some((s, k)) => (s.to_string(), k.to_string()),
                None => (String::new(), full.clone()),
            };
            match sections.iter_mut().find(|(s, _)| *s == section) {
                Some((_, keys)) => keys.push(key),
                None => sections.push((section, vec![key])),
            }
        }
        for (section, keys) in sections {
            let head = if section.is_empty() {
                "(top level)".to_string()
            } else {
                format!("[{section}]")
            };
            println!("  {head} {}", keys.join(", "));
        }
    }
    if !report.dead.is_empty() {
        println!("\n{} keys nothing in this build reads:", report.dead.len());
        for key in &report.dead {
            println!("  {key}");
        }
        // The footnote is the crate's, not the printer's: it used to say
        // "the desktop reads some of them", which was false for every key
        // above it and is why they survived.
        if let Some(note) = report.dead_key_note() {
            println!("  {note}");
        }
    }
    if !report.differing.is_empty() {
        println!(
            "\n{} options set to something other than the default:",
            report.differing.len()
        );
        for (key, got, want) in &report.differing {
            // "(default unset by default)" says one thing twice. The sentinel
            // means there is no default to compare against — print that.
            let want = if want == veles_core::config_surface::UNSET_DEFAULT {
                "unset"
            } else {
                want.as_str()
            };
            println!("  {key} = {got}   (default {want})");
        }
    }
}

/// One node by id or unique prefix, or an error that says WHICH way it
/// failed: no match and an ambiguous prefix are different mistakes.
fn resolve_node(
    store: &veles_core::store::Store,
    id: &str,
) -> anyhow::Result<veles_core::store::NodeRow> {
    use veles_core::store::NodeMatch;
    match store.node_resolve(id)? {
        NodeMatch::One(n) => Ok(*n),
        NodeMatch::None => anyhow::bail!("no node matches '{id}'"),
        NodeMatch::Ambiguous(n) => {
            anyhow::bail!("'{id}' matches {n} nodes — give more of the id")
        }
    }
}

/// A comma list of hosts, lowercased; "" is none.
fn host_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect()
}

/// `veles secret …` — the vault, one name at a time.
fn secret_family(action: SecretAction) -> anyhow::Result<()> {
    use veles_core::vault::{Kind, Policy};
    let cfg = veles_core::config::load().unwrap_or_default();
    match action {
        SecretAction::Set {
            name,
            value,
            kind,
            hosts,
            no_shell,
            note,
        } => {
            let name = name.trim_start_matches('$').to_string();
            if !veles_core::secrets::valid_name(&name) {
                anyhow::bail!(
                    "invalid secret name '{name}': letters, digits and underscores, starting with a letter or _"
                );
            }
            let value = match value {
                Some(v) => {
                    eprintln!(
                        "note: a value on the command line is in your shell history now — \
                         next time omit it and you will be prompted without echo"
                    );
                    v
                }
                None => veles_cli::read_secret(&format!("value for ${name}: "))?,
            };
            if value.is_empty() {
                eprintln!("empty value — nothing stored");
                return Ok(());
            }
            let mut policy = veles_core::secrets::policy_of(&name, &cfg);
            if let Some(k) = kind.as_deref() {
                let k = match k {
                    "provider" => Kind::Provider,
                    "platform" => Kind::Platform,
                    "user" => Kind::User,
                    other => anyhow::bail!("unknown kind '{other}': provider, platform or user"),
                };
                let keep_hosts = policy.hosts.clone();
                policy = Policy::default_for(k, keep_hosts);
            }
            if let Some(h) = hosts.as_deref() {
                policy.hosts = host_list(h);
            }
            if no_shell {
                policy.shell = false;
            }
            if let Some(n) = note {
                policy.note = n;
            }
            // One node's own credential (its A2A bearer, its desktop
            // token): this node's secrets.env, never the shared vault.
            if veles_core::secrets::is_node_secret(&name) {
                veles_core::secrets::set(&name, &value)?;
                println!(
                    "stored ${name} in secrets.env — this node's own credential, never in the \
                     shared vault"
                );
                return Ok(());
            }
            veles_core::vault::set_local(&name, &value, policy.clone())?;
            let l = veles_core::secrets::Listed {
                name: name.clone(),
                policy,
                source: "vault",
            };
            println!("stored {}", l.line());
            Ok(())
        }
        SecretAction::List => {
            let all = veles_core::secrets::listing();
            if all.is_empty() {
                println!("no secrets stored");
            }
            for l in all {
                println!("{}", l.line());
            }
            let unreadable = veles_core::vault::local_unreadable();
            if !unreadable.is_empty() {
                println!("{}", veles_core::vault::unreadable_line(&unreadable));
            }
            Ok(())
        }
        SecretAction::Unset { name } => {
            let name = name.trim_start_matches('$').to_string();
            if veles_core::vault::local_values().contains_key(&name) {
                veles_core::vault::unset_local(&name)?;
                println!("${name} removed from the vault (the removal syncs with it)");
            } else {
                veles_core::secrets::unset(&name)?;
                println!("${name} removed from secrets.env");
            }
            Ok(())
        }
        SecretAction::Policy {
            name,
            shell,
            hosts,
            note,
        } => {
            let name = name.trim_start_matches('$').to_string();
            let mut v = veles_core::vault::Vault::open_local()?;
            let line = v.update(|v| {
                let cur = v.get(&name)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "{name} is not in the vault — `veles vault import` or `veles secret set` first"
                    )
                })?;
                let mut p = cur.policy.clone();
                match shell.as_deref() {
                    None => {}
                    Some("on") | Some("true") | Some("yes") => p.shell = true,
                    Some("off") | Some("false") | Some("no") => p.shell = false,
                    Some(o) => anyhow::bail!("--shell takes on or off, not '{o}'"),
                }
                if let Some(h) = hosts.as_deref() {
                    p.hosts = host_list(h);
                }
                if let Some(n) = note {
                    p.note = n;
                }
                v.set_policy(&name, p.clone())?;
                Ok(veles_core::secrets::Listed {
                    name: name.clone(),
                    policy: p,
                    source: "vault",
                }
                .line())
            })?;
            println!("{line}");
            Ok(())
        }
    }
}

/// A peer given as a URL or as a name in `[a2a_agents]`, with the
/// bearer to use for it.
fn vault_peer(peer: &str, bearer: Option<String>) -> anyhow::Result<(String, Option<String>)> {
    if peer.contains("://") {
        return Ok((
            peer.to_string(),
            bearer.or_else(|| veles_core::secrets::resolve("A2A_BEARER_TOKEN")),
        ));
    }
    let cfg = veles_core::config::load()?;
    let p = cfg.a2a_agents.get(peer).ok_or_else(|| {
        anyhow::anyhow!("no peer named '{peer}' in [a2a_agents] — give its A2A URL instead")
    })?;
    let tok = bearer.or_else(|| (!p.auth.token.is_empty()).then(|| p.auth.token.clone()));
    Ok((p.url.clone(), tok))
}

/// Send (or print) this node's join request — `vault join` and `vault
/// reidentify` both end here.
fn send_join(
    req: &veles_core::vault::JoinRequest,
    push: Option<String>,
    bearer: Option<String>,
    out: Option<String>,
) -> anyhow::Result<()> {
    use veles_core::vault;
    let msg = vault::Message::Join(req.clone());
    println!(
        "this node: {} ({}), fingerprint {}",
        req.name,
        req.node,
        req.fingerprint()
    );
    match (push, out) {
        (Some(peer), _) => {
            let (url, tok) = vault_peer(&peer, bearer)?;
            let rt = tokio::runtime::Runtime::new()?;
            let reply = rt.block_on(vault::push_to_peer(&msg, &url, tok.as_deref()))?;
            println!("{reply}");
        }
        (None, Some(path)) => {
            std::fs::write(&path, vault::encode_message(&msg)?)?;
            println!("request written to {path}");
        }
        (None, None) => print!("{}", vault::encode_message(&msg)?),
    }
    println!(
        "\nOn the member: `veles vault admit {} --fingerprint {}` — after reading that \
         fingerprint off THIS screen. Then push its vault here and run `veles vault \
         apply --inbox --trust <its root fingerprint>`.",
        req.node,
        req.fingerprint()
    );
    Ok(())
}

/// `veles vault …` — keys, members, sync. No Agent is built.
fn vault_family(action: VaultAction) -> anyhow::Result<()> {
    use veles_core::vault;
    // Each arm opens the vault itself: the read-only ones must not mint
    // this node's keys as a side effect of a question.
    match action {
        VaultAction::Init => {
            let mut v = vault::Vault::open_or_create_local()?;
            v.update(|v| v.init())?;
            println!(
                "vault created; this node ({}) is its only member\nfingerprint {}",
                v.keys.name,
                v.keys.fingerprint()
            );
            Ok(())
        }
        VaultAction::Whoami => {
            let id =
                veles_core::node::NodeIdentity::load_or_create(&veles_core::config::config_dir())?;
            let dir = vault::vault_dir();
            let status = vault::identity_status_in(&dir, &id.id, &id.name)?;
            if status == vault::IdentityStatus::MissingKeys {
                anyhow::bail!("{}", status.summary(&id.id, &dir));
            }
            let Some(v) = vault::Vault::open_local_if_any()? else {
                println!(
                    "node        {} ({})\nno vault keys on this node yet — `veles vault join` \
                     (or `veles vault init` on the first node) creates them",
                    id.name, id.id
                );
                return Ok(());
            };
            println!("node        {} ({})", v.keys.name, v.keys.node);
            println!("fingerprint {}", v.keys.fingerprint());
            println!("recipient   {}", v.keys.recipient());
            println!("keys        {}", v.dir.display());
            if v.is_initialized() {
                println!("identity    {}", status.summary(&id.id, &dir));
            }
            Ok(())
        }
        VaultAction::Status => {
            let Some(id) = veles_core::node::NodeIdentity::load(&veles_core::config::config_dir())?
            else {
                println!("no vault on this node yet — `veles secret set` or `veles vault init` makes one");
                return Ok(());
            };
            for line in vault::status_lines(&vault::vault_dir(), &id.id, &id.name)? {
                println!("{line}");
            }
            let pending = vault::inbox_pending(&vault::inbox_dir())?;
            println!("inbox: {} waiting", pending.len());
            Ok(())
        }
        VaultAction::Join { push, bearer, out } => {
            let v = vault::Vault::open_or_create_local()?;
            send_join(&v.join_request(), push, bearer, out)
        }
        VaultAction::Reidentify { push, bearer, out } => {
            let r = vault::reidentify_local()?;
            let req = r.vault.join_request();
            for p in &r.backed_up {
                println!("old key file set aside: {}", p.display());
            }
            match (&r.old_fp, &r.recorded_fp) {
                (Some(old), _) => println!("old fingerprint {old}"),
                (None, Some(rec)) => println!("the vault's record of this node: {rec}"),
                (None, None) => {}
            }
            println!("new fingerprint {}", req.fingerprint());
            if r.recorded_fp.is_none() {
                println!(
                    "(the vault on this node has no member row for {} — this is a plain join)",
                    req.node
                );
            }
            send_join(&req, push, bearer, out)
        }
        VaultAction::Admit {
            node,
            fingerprint,
            file,
        } => {
            let mut v = vault::Vault::open_local()?;
            // The inbox entry this request came from, moved to applied/
            // only AFTER the admit succeeds: a mistyped fingerprint must
            // leave the request there to try again.
            let mut from_inbox: Option<std::path::PathBuf> = None;
            let req = match file {
                Some(f) => {
                    let raw = if f == "-" {
                        let mut s = String::new();
                        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                        s
                    } else {
                        std::fs::read_to_string(&f)?
                    };
                    match vault::decode_message(&raw)? {
                        vault::Message::Join(r) => r,
                        _ => anyhow::bail!("{f} is an offer, not a join request"),
                    }
                }
                None => {
                    let mut found = Vec::new();
                    for p in vault::inbox_pending(&vault::inbox_dir())? {
                        if let vault::Message::Join(r) = vault::inbox_read(&p)?.message {
                            if r.node.starts_with(&node) || r.name == node {
                                found.push((p, r));
                            }
                        }
                    }
                    match found.len() {
                        0 => anyhow::bail!("no join request from '{node}' in the inbox"),
                        1 => {
                            let (p, r) = found.remove(0);
                            from_inbox = Some(p);
                            r
                        }
                        n => {
                            anyhow::bail!("{n} join requests match '{node}' — give more of the id")
                        }
                    }
                }
            };
            let out = v.update(|v| v.admit(&req, &fingerprint))?;
            if let Some(p) = from_inbox {
                let done = vault::inbox_dir().join("applied");
                std::fs::create_dir_all(&done)?;
                std::fs::rename(&p, done.join(p.file_name().unwrap_or_default()))?;
            }
            if let Some((old, new)) = &out.replaced {
                println!("replacing key for {} ({old} → {new})", req.name);
            }
            println!(
                "admitted {} ({}); re-encrypted {} value(s) for it",
                req.name,
                req.node,
                out.rekeyed.len()
            );
            println!(
                "Next: `veles vault push <its peer>` — and it applies with --trust {}",
                v.membership()
                    .root
                    .map(|r| vault::fingerprint(&r.recipient, &r.sign_pk))
                    .unwrap_or_default()
            );
            Ok(())
        }
        VaultAction::Revoke { node } => {
            let mut v = vault::Vault::open_local()?;
            let m = v.membership();
            let target = m
                .current
                .keys()
                .filter(|id| id.starts_with(&node) || m.current[*id].name == node)
                .cloned()
                .collect::<Vec<_>>();
            let id = match target.as_slice() {
                [one] => one.clone(),
                [] => anyhow::bail!("no member matches '{node}'"),
                _ => anyhow::bail!("'{node}' matches several members — give more of the id"),
            };
            let rotate = v.update(|v| v.revoke(&id))?;
            println!("revoked {id}; values re-encrypted for the remaining members");
            if !rotate.is_empty() {
                println!(
                    "It could read these, and may have kept them — replace each at its provider, \
                     then `veles secret set` the new one:"
                );
                for n in rotate {
                    println!("  ${n}");
                }
            }
            Ok(())
        }
        VaultAction::Rekey => {
            let mut v = vault::Vault::open_local()?;
            let (done, unreadable) = v.update(|v| v.rekey())?;
            println!("re-encrypted {} value(s)", done.len());
            if !unreadable.is_empty() {
                println!(
                    "could not read (another member must rekey): {}",
                    unreadable.join(", ")
                );
            }
            Ok(())
        }
        VaultAction::Offer { out } => {
            let v = vault::Vault::open_local()?;
            let msg = vault::Message::Offer {
                from_node: v.keys.node.clone(),
                from_name: v.keys.name.clone(),
                file: v.file.clone(),
            };
            let text = vault::encode_message(&msg)?;
            match out.as_deref() {
                None | Some("-") => print!("{text}"),
                Some(p) => std::fs::write(p, text)?,
            }
            Ok(())
        }
        VaultAction::Push { peer, bearer } => {
            let v = vault::Vault::open_local()?;
            if !v.is_initialized() {
                anyhow::bail!("this node has no vault to push");
            }
            let (url, tok) = vault_peer(&peer, bearer)?;
            let msg = vault::Message::Offer {
                from_node: v.keys.node.clone(),
                from_name: v.keys.name.clone(),
                file: v.file.clone(),
            };
            let rt = tokio::runtime::Runtime::new()?;
            println!(
                "{}",
                rt.block_on(vault::push_to_peer(&msg, &url, tok.as_deref()))?
            );
            Ok(())
        }
        VaultAction::Inbox => {
            let pending = vault::inbox_pending(&vault::inbox_dir())?;
            if pending.is_empty() {
                println!("nothing waiting");
            }
            for p in pending {
                let e = vault::inbox_read(&p)?;
                let what = match &e.message {
                    vault::Message::Offer {
                        from_name, file, ..
                    } => format!(
                        "offer from {from_name}: {} member row(s), {} write(s)",
                        file.members.len(),
                        file.records.len()
                    ),
                    vault::Message::Join(r) => format!(
                        "join request from {} ({}), fingerprint {}",
                        r.name,
                        r.node,
                        r.fingerprint()
                    ),
                };
                println!(
                    "{}  via {}  {what}",
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    e.peer
                );
            }
            Ok(())
        }
        VaultAction::Apply { file, inbox, trust } => {
            let mut v = vault::Vault::open_local()?;
            let mut offers: Vec<(Option<std::path::PathBuf>, vault::VaultFile)> = Vec::new();
            if inbox {
                for p in vault::inbox_pending(&vault::inbox_dir())? {
                    if let vault::Message::Offer { file, .. } = vault::inbox_read(&p)?.message {
                        offers.push((Some(p), file));
                    }
                }
            } else {
                let f = file.ok_or_else(|| anyhow::anyhow!("give an offer file, or --inbox"))?;
                let raw = if f == "-" {
                    let mut s = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                    s
                } else {
                    std::fs::read_to_string(&f)?
                };
                match vault::decode_message(&raw)? {
                    vault::Message::Offer { file, .. } => offers.push((None, file)),
                    vault::Message::Join(_) => {
                        anyhow::bail!("that is a join request — `veles vault admit` takes it")
                    }
                }
            }
            if offers.is_empty() {
                println!("no offers to apply");
                return Ok(());
            }
            for (path, incoming) in offers {
                let report = v.update(|v| v.apply(&incoming, trust.as_deref()))?;
                println!("{}", report.summarize());
                if let Some(p) = path {
                    let done = vault::inbox_dir().join("applied");
                    std::fs::create_dir_all(&done)?;
                    std::fs::rename(&p, done.join(p.file_name().unwrap_or_default()))?;
                }
            }
            Ok(())
        }
        VaultAction::Import { dry_run } => {
            // No keys yet is fine (the first import creates the vault);
            // keys gone while the vault names this node is refused here,
            // before anything is compared.
            let v = vault::Vault::open_local_if_any()?;
            if v.is_none() {
                let id = veles_core::node::NodeIdentity::load_or_create(
                    &veles_core::config::config_dir(),
                )?;
                let dir = vault::vault_dir();
                let st = vault::identity_status_in(&dir, &id.id, &id.name)?;
                if st.is_problem() {
                    anyhow::bail!("{}", st.summary(&id.id, &dir));
                }
            }
            let cfg = veles_core::config::load().unwrap_or_default();
            let found = veles_core::secrets::env_file_secrets(&cfg);
            if found.is_empty() {
                println!("no credentials in .env or secrets.env");
                return Ok(());
            }
            let have: std::collections::HashMap<String, String> = v
                .map(|v| v.values().0)
                .unwrap_or_default()
                .into_iter()
                .map(|s| (s.name, s.value))
                .collect();
            let mut dup_lines = Vec::new();
            for (name, (value, source)) in &found {
                if veles_core::secrets::is_node_secret(name) {
                    println!(
                        "  ${name}: this node's own credential — stays in {source}, never in \
                         the shared vault"
                    );
                    continue;
                }
                let policy = veles_core::secrets::inferred_policy(name, &cfg);
                match have.get(name) {
                    Some(cur) if cur == value => {
                        println!("  ${name}: already in the vault ({source} line is a duplicate)");
                        dup_lines.push(format!("{source}: {name}"));
                    }
                    Some(_) => println!(
                        "  ${name}: the vault has a DIFFERENT value — kept the vault's; check \
                         which one is current"
                    ),
                    None => {
                        let l = veles_core::secrets::Listed {
                            name: name.clone(),
                            policy: policy.clone(),
                            source,
                        };
                        if dry_run {
                            println!("  would import {}", l.line());
                        } else {
                            vault::set_local(name, value, policy)?;
                            println!("  imported {}", l.line());
                            dup_lines.push(format!("{source}: {name}"));
                        }
                    }
                }
            }
            if !dry_run && !dup_lines.is_empty() {
                println!(
                    "\nThese lines are now duplicates of the vault. The vault wins over them, so \
                     they can be deleted once the gateway has been checked:"
                );
                for l in dup_lines {
                    println!("  {l}");
                }
            }
            Ok(())
        }
    }
}

/// `veles configsync …` — config replication (B2). No Agent is built
/// (same reasoning as `nodes_family`): these are registry-and-file
/// operations, not turns.
/// `veles replicas …` — store replication (B5). Same no-Agent posture
/// as configsync: file, log and A2A operations, not turns.
/// `veles ticket keygen|sign|request|check` (PLAN-pool-round.md D3). A
/// key lives on a DEVICE: this command writes it encrypted to a passphrase
/// in the signer directory (beside the gateway's config, never inside it)
/// and nothing here ever hands it to a gateway.
fn signer_family(action: SignerAction) -> anyhow::Result<()> {
    use anyhow::Context as _;
    match action {
        SignerAction::Run { dir } => {
            let dir = dir.unwrap_or_else(veles_core::ticket::key_dir);
            let cfg = veles_signer::load_config(&dir)?;
            let secrets = veles_signer::Secrets::load(&dir);
            anyhow::ensure!(
                !secrets.gateway_tokens.is_empty(),
                "no gateway may ask: set SIGNER_GATEWAY_TOKENS=name:token,… in {}",
                dir.join("signer.env").display()
            );
            let pass = match std::env::var("SIGNER_PASSPHRASE_FILE")
                .ok()
                .filter(|v| !v.is_empty())
            {
                Some(f) => std::fs::read_to_string(&f)
                    .with_context(|| format!("reading the passphrase file {f}"))?
                    .trim_end_matches(['\n', '\r'])
                    .to_string(),
                None => veles_cli::read_secret(&format!("passphrase for the key {}: ", cfg.key))?,
            };
            let key = veles_core::ticket::load_key(&dir.join(format!("{}.key", cfg.key)), &pass)?;
            drop(pass);
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let desk = veles_signer::Desk::new();
                if let Some(tg) = cfg.telegram.clone() {
                    let token = secrets.telegram_token.clone().ok_or_else(|| {
                        anyhow::anyhow!("[telegram] needs SIGNER_TELEGRAM_TOKEN in signer.env")
                    })?;
                    let bot = veles_signer::telegram::Bot::new(&tg.api_url, &token);
                    tokio::spawn(veles_signer::telegram::run(
                        bot,
                        tg.owner_id,
                        tg.chat_id,
                        desk.clone(),
                    ));
                }
                if let Some(irc) = cfg.irc.clone() {
                    tokio::spawn(veles_signer::irc_leg::run(
                        irc,
                        secrets.irc_password.clone(),
                        desk.clone(),
                    ));
                }
                let listener = tokio::net::TcpListener::bind(&cfg.listen)
                    .await
                    .with_context(|| format!("listening on {}", cfg.listen))?;
                eprintln!(
                    "veles signer: key {}, listening on {}",
                    key.key_id(),
                    cfg.listen
                );
                veles_signer::serve(cfg, secrets.gateway_tokens, key, desk, listener).await
            })
        }
    }
}

fn ticket_family(action: TicketAction) -> anyhow::Result<()> {
    use veles_core::ticket;
    let this_node = || {
        veles_core::node::NodeIdentity::load(&veles_core::config::config_dir())
            .ok()
            .flatten()
            .map(|i| i.id)
    };
    match action {
        TicketAction::Keygen { name } => {
            anyhow::ensure!(
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')),
                "a key name is letters, digits, - and _"
            );
            let path = ticket::key_dir().join(format!("{name}.key"));
            anyhow::ensure!(
                !path.exists(),
                "{} exists — pick another name",
                path.display()
            );
            let pass = veles_cli::read_secret("passphrase for the new key: ")?;
            let again = veles_cli::read_secret("again: ")?;
            anyhow::ensure!(pass == again, "the passphrases differ — nothing written");
            let (key, pkcs8) = ticket::SignerKey::generate()?;
            ticket::save_key(&path, &pkcs8, &pass)?;
            println!(
                "key written: {} (encrypted to your passphrase)

Add this to [cluster] on every gateway that should accept the tickets it signs:

[cluster.ticket_signers.{name}]
key = \"{}\"",
                path.display(),
                key.public_config()
            );
            Ok(())
        }
        TicketAction::Sign { request, key, yes } => {
            let req = ticket::TicketRequest::decode(&request)?;
            eprintln!("{}\n", req.describe(4000));
            if !yes {
                use std::io::Write as _;
                eprint!("sign this? [y/N] ");
                std::io::stderr().flush().ok();
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim(), "y" | "Y" | "yes") {
                    anyhow::bail!("not signed");
                }
            }
            let dir = ticket::key_dir();
            let path = match key {
                Some(k) => dir.join(format!("{k}.key")),
                None => {
                    let keys: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
                        .map(|rd| {
                            rd.filter_map(|e| e.ok().map(|e| e.path()))
                                .filter(|p| p.extension().is_some_and(|x| x == "key"))
                                .collect()
                        })
                        .unwrap_or_default();
                    match keys.as_slice() {
                        [one] => one.clone(),
                        [] => anyhow::bail!(
                            "no signing key in {} — `veles ticket keygen <name>` makes one",
                            dir.display()
                        ),
                        _ => {
                            anyhow::bail!("several keys in {} — pick one with --key", dir.display())
                        }
                    }
                }
            };
            let pass = veles_cli::read_secret("passphrase: ")?;
            let signer = ticket::load_key(&path, &pass)?;
            println!("{}", signer.sign(&req)?);
            Ok(())
        }
        TicketAction::Request {
            target,
            tool,
            args,
            session,
            grant,
            origin,
        } => {
            let origin = origin
                .or_else(this_node)
                .ok_or_else(|| anyhow::anyhow!("this machine has no node id — pass --origin"))?;
            let args: serde_json::Value = serde_json::from_str(&args)
                .map_err(|e| anyhow::anyhow!("--args is not JSON: {e}"))?;
            let req = ticket::TicketRequest::new(
                &origin,
                &target,
                &session,
                &tool,
                args,
                &grant,
                ticket::DEFAULT_TTL_SECS,
            )?;
            println!("{}", req.encode()?);
            Ok(())
        }
        TicketAction::Check { ticket: t, request } => {
            let req = ticket::TicketRequest::decode(&request)?;
            let parsed = ticket::Ticket::parse(&t)?;
            let cfg = veles_core::config::load()?;
            let v = ticket::verify(
                &parsed,
                &ticket::trusted_signers(&cfg),
                &ticket::CallContext {
                    me: &req.target,
                    origin: &req.origin,
                    session: &req.session,
                    tool: &req.tool,
                    args: &req.args,
                    now: chrono::Utc::now().timestamp(),
                },
            )?;
            println!(
                "valid: {} as {}, signed by {}, until {}",
                req.tool, v.grant, v.signer, v.exp
            );
            Ok(())
        }
    }
}

/// `veles session migrate|reclaim|owner` (B6). The migrate and
/// reclaim legs are async (the A2A push/query); `owner` is a pure
/// store read. Errors print and exit non-zero — a migration is an
/// operation, not a chat.
/// This data dir's store, shared the way the mail functions take it.
fn mail_store() -> anyhow::Result<Arc<std::sync::Mutex<veles_core::store::Store>>> {
    let home = veles_core::config::veles_home();
    Ok(Arc::new(std::sync::Mutex::new(
        veles_core::store::Store::open(&home.join("state.db"))?,
    )))
}

/// `veles session list` (session mail, PLAN-sessions-and-phone.md S5).
fn session_mail_list(remote: bool) -> anyhow::Result<()> {
    use veles_core::lockutil::LockOk;
    let store = mail_store()?;
    let live = veles_core::mail::live(&store);
    if live.is_empty() {
        println!("no live sessions on this machine");
    }
    for l in live {
        let state = match (l.running, l.held) {
            (true, true) => "running",
            (true, false) => "one-shot",
            (false, _) => "idle",
        };
        let pid = l.pid.map(|p| p.to_string()).unwrap_or_default();
        println!(
            "{}  {:<9} {:>8}  {:<8} {}",
            l.session_id, l.surface, pid, state, l.title
        );
    }
    if !remote {
        return Ok(());
    }
    let cfg = veles_core::config::load()?;
    let mut names: Vec<&String> = cfg.a2a_agents.keys().collect();
    names.sort();
    let rt = tokio::runtime::Runtime::new()?;
    for name in names {
        let Some(r) = veles_core::mailnet::remote(&store.lock_ok(), &cfg, name) else {
            continue;
        };
        match rt.block_on(veles_core::mailnet::list(&r)) {
            Ok(ls) => {
                for l in ls {
                    let state = if l.running { "running" } else { "idle" };
                    println!(
                        "{name}/{}  {:<9} {:>8}  {:<8} {}",
                        l.session_id, l.surface, "", state, l.title
                    );
                }
            }
            Err(e) => eprintln!("{name}: {e:#}"),
        }
    }
    Ok(())
}

/// Mail to `<node>/<session>` over the pool's A2A link.
fn session_mail_send_remote(node: &str, sid: &str, text: &str, wait: u64) -> anyhow::Result<()> {
    use veles_core::lockutil::LockOk;
    let principal = veles_core::acl::principal_from_env().map_err(|e| anyhow::anyhow!(e))?;
    let store = mail_store()?;
    let cfg = veles_core::config::load()?;
    let Some(remote) = veles_core::mailnet::remote(&store.lock_ok(), &cfg, node) else {
        anyhow::bail!("no pool node {node:?} — [a2a_agents] names the nodes this one reaches");
    };
    let from = veles_core::mail::Sender {
        session: None,
        title: None,
        principal,
    };
    let rt = tokio::runtime::Runtime::new()?;
    let r = rt.block_on(veles_core::mailnet::send(&remote, &from, sid, text, wait))?;
    let id = format!("{}/{}", remote.name, r.id);
    println!(
        "sent mail {id} to {}/{sid}{}",
        remote.name,
        if r.held {
            format!(" ({} has it open)", remote.name)
        } else {
            format!(
                "; nobody has it open on {}, so it runs when it is next opened",
                remote.name
            )
        }
    );
    match r.state.as_str() {
        "answered" => {
            println!("{}", r.reply.unwrap_or_default());
            Ok(())
        }
        "failed" => {
            eprintln!("the turn failed: {}", r.reply.unwrap_or_default());
            std::process::exit(2);
        }
        state if wait > 0 && r.held => {
            eprintln!("no answer within {wait}s (the mail is {state}); veles session mail {id} shows it later");
            std::process::exit(3);
        }
        _ => Ok(()),
    }
}

/// `veles session send <to> <text> [--wait N]`. The sender is the command
/// line — the owner surface, unless `VELES_SESSION_PRINCIPAL` names
/// someone (a delegate child shelling out): then the mail carries THEIR
/// rights, and the turn it starts runs with no more than those.
fn session_mail_send(to: &str, text: &str, wait: u64) -> anyhow::Result<()> {
    use veles_core::lockutil::LockOk;
    use veles_core::mail::{Delivery, Resolved, Waited};
    if let Some((node, sid)) = veles_core::mailnet::split_address(to) {
        return session_mail_send_remote(node, sid, text, wait);
    }
    let principal = veles_core::acl::principal_from_env().map_err(|e| anyhow::anyhow!(e))?;
    let store = mail_store()?;
    let open: Vec<String> = veles_core::mail::live(&store)
        .into_iter()
        .filter(|l| l.held)
        .map(|l| l.session_id)
        .collect();
    let target = match veles_core::mail::resolve(&store.lock_ok(), to, &open)? {
        Resolved::One(s) => s,
        Resolved::None => {
            anyhow::bail!("no session matches {to:?} (veles session list shows the live ones)")
        }
        Resolved::Many(ids) => {
            anyhow::bail!("{to:?} matches several sessions: {}", ids.join(", "))
        }
    };
    let from = veles_core::mail::Sender {
        session: None,
        title: None,
        principal,
    };
    let rt = tokio::runtime::Runtime::new()?;
    // Our own socket while we wait: the target's process tells us the
    // moment its turn ends instead of us polling the store for it.
    let _bound = wait > 0 && rt.block_on(async { veles_core::runsock::serve("cli").is_some() });
    let sent = veles_core::mail::send(&store, &from, &target, text)?;
    match sent.delivery {
        Delivery::Here | Delivery::Process(_) => {
            let pid = match sent.delivery {
                Delivery::Process(p) => p.to_string(),
                _ => std::process::id().to_string(),
            };
            println!(
                "sent mail {} to {target} (process {pid} has it open)",
                sent.id
            );
        }
        Delivery::Queued => {
            println!(
                "sent mail {} to {target}; nobody has it open, so it runs when that session is \
                 next opened (veles session mail {} shows its answer then)",
                sent.id, sent.id
            );
            veles_core::runsock::shutdown();
            return Ok(());
        }
    }
    if wait == 0 {
        return Ok(());
    }
    let out = rt.block_on(veles_core::mail::wait(
        &store,
        &sent.id,
        std::time::Duration::from_secs(wait),
    ));
    veles_core::runsock::shutdown();
    match out? {
        Waited::Answered(reply) => {
            println!("{reply}");
            Ok(())
        }
        Waited::Failed(e) => {
            eprintln!("the turn failed: {e}");
            std::process::exit(2);
        }
        Waited::Pending(state) => {
            eprintln!(
                "no answer within {wait}s (the mail is {state}); veles session mail {} shows it later",
                sent.id
            );
            std::process::exit(3);
        }
    }
}

/// `veles session mail <id>`.
fn session_mail_show(id: &str) -> anyhow::Result<()> {
    use veles_core::lockutil::LockOk;
    let store = mail_store()?;
    if let Some((node, mid)) = veles_core::mailnet::split_address(id) {
        let cfg = veles_core::config::load()?;
        let Some(remote) = veles_core::mailnet::remote(&store.lock_ok(), &cfg, node) else {
            anyhow::bail!("no pool node {node:?}");
        };
        let rt = tokio::runtime::Runtime::new()?;
        let (state, reply) = rt.block_on(veles_core::mailnet::check(&remote, mid))?;
        println!("mail {id}: {state}");
        if let Some(r) = reply.filter(|r| !r.is_empty()) {
            println!("{r}");
        }
        return Ok(());
    }
    let Some(row) = store.lock_ok().mail_get(id)? else {
        anyhow::bail!("no mail {id}");
    };
    let from = row.from_session.as_deref().unwrap_or("the command line");
    println!(
        "mail {id}: {from} → {} — {} (sent {}, last change {})",
        row.to_session, row.state, row.created_at, row.updated_at
    );
    if let Some(r) = row.reply.as_deref().filter(|r| !r.is_empty()) {
        println!("{r}");
    }
    Ok(())
}

/// An invite as `pool join` and `recover` take it: its text, a text file
/// holding it, or a photo (JPEG/PNG) of its QR code.
fn read_invite(arg: &str) -> anyhow::Result<veles_core::pool::Invite> {
    use veles_core::pool;
    let raw = match std::fs::read(arg) {
        Ok(bytes) => match std::str::from_utf8(&bytes) {
            Ok(t) if t.trim_start().starts_with(pool::QR_PREFIX) => t.to_string(),
            _ => pool::qr_read(&bytes)?,
        },
        Err(_) => arg.to_string(),
    };
    pool::decode(&raw)
}

/// `veles recover` — a node rebuilds itself from a pool peer after its
/// home was lost (`veles_core::recover`).
fn recover_cmd(
    invite: Option<String>,
    as_node: Option<String>,
    from: Option<String>,
    skip: Vec<String>,
    yes: bool,
) -> anyhow::Result<()> {
    use veles_core::recover;
    let invite = invite.as_deref().map(read_invite).transpose()?;
    match &invite {
        Some(inv) => {
            let whom = match &inv.recover {
                Some(r) => format!(
                    "node {} ({}), as {}'s registry knows it",
                    r.id, r.name, inv.arbiter.name
                ),
                None => format!("a NEW node the pool will call {:?}", inv.name),
            };
            println!(
                "This rebuilds this home as {whom}, from {} ({}).\n\
                 It writes node.toml, re-links every member with new tokens ([a2a_agents], \
                 [cluster] and A2A_PEER_TOKENS, as a join does), folds this node's sessions back \
                 from the peer's replica into state.db, applies its memory and config, copies its \
                 skills and commands where none are here, and mints vault keys for a member to \
                 admit.",
                inv.arbiter.name, inv.arbiter.url
            );
        }
        None => println!(
            "This runs the recovery's pulls again over this node's links (from {}): sessions, \
             memory, config, skills and commands, the vault request. Each is idempotent.",
            from.as_deref().unwrap_or("the arbiter")
        ),
    }
    if !yes {
        use std::io::Write as _;
        print!("Recover? [y/N] ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            println!("Nothing was written.");
            return Ok(());
        }
    }
    let opts = recover::Options {
        invite,
        as_node,
        from,
        skip,
    };
    let rt = tokio::runtime::Runtime::new()?;
    let say = |l: &str| println!("  {l}");
    let out = rt.block_on(recover::run(&opts, &say))?;
    println!("\nRestored:");
    for l in &out.done {
        println!("  ✓ {l}");
    }
    println!("\nNOT restored (nothing outside this home holds it, or a step could not):");
    for l in &out.not_restored {
        println!("  ✗ {l}");
    }
    println!("\nNext, by hand:");
    for l in &out.next {
        println!("  → {l}");
    }
    println!(
        "  → start the gateway (`veles gateway run`, or its service) so the pool can call this \
         node, and check `veles doctor` and `veles vault status`"
    );
    Ok(())
}

/// `veles pool …` — joining the pool by QR (`veles_core::pool`).
fn pool_family(action: PoolAction) -> anyhow::Result<()> {
    use veles_core::pool;
    match action {
        PoolAction::Invite {
            name,
            url,
            ttl,
            png,
            text,
            wireguard_file,
            recover,
            node,
        } => {
            let cfg = veles_core::config::load()?;
            let minted = if recover {
                if url.is_some() {
                    anyhow::bail!(
                        "--recover keeps the url {name} has here ([a2a_agents.{name}]): no --url"
                    );
                }
                let home = veles_core::config::veles_home();
                let store = veles_core::store::Store::open(&home.join("state.db"))?;
                pool::mint_recovery(&cfg, &store, &name, node.as_deref(), ttl)?
            } else {
                pool::mint(&cfg, &name, url.as_deref(), ttl)?
            };
            let invite_text = pool::encode(&minted.invite)?;
            if let Some(path) = &wireguard_file {
                match &minted.wg_client {
                    Some(wg) => {
                        veles_core::platform::write_private(path, wg.as_bytes())?;
                        eprintln!(
                            "WireGuard client config for {name} written to {} (0600; it holds the private key)",
                            path.display()
                        );
                    }
                    None => eprintln!(
                        "no WireGuard config was minted: [pool.wireguard] is not set on this hub"
                    ),
                }
            }
            if text {
                println!("{invite_text}");
                return Ok(());
            }
            if let Some(r) = &minted.invite.recover {
                println!(
                    "A recovery invite for {name}: node {} ({}), known here by {}. On the bare \
                     home, with a veles binary: veles recover '<the text>' --as {name}\n\
                     (`veles pool invite {name} --recover --text` prints the text.) Good for {} \
                     minutes, once. It holds credentials: show it only on your own screen.\n",
                    r.id,
                    r.name,
                    minted.recover_via.unwrap_or("?"),
                    ttl / 60
                );
                println!("{}", pool::qr_text(&invite_text)?);
                return Ok(());
            }
            if let Some(dir) = &png {
                std::fs::create_dir_all(dir)?;
            }
            let mins = ttl / 60;
            if let Some(wg) = &minted.wg_client {
                println!("1. On the new phone, scan this with the WireGuard app (+, scan from QR code):\n");
                println!("{}", pool::qr_text(wg)?);
                if let Some(q) = &minted.wg_queued {
                    println!(
                        "   The hub's side is queued for its root helper: {}",
                        q.display()
                    );
                }
                if let Some(dir) = &png {
                    let p = dir.join(format!("wireguard-{name}.png"));
                    std::fs::write(&p, pool::qr_png(wg, 8)?)?;
                    println!("   (also {})", p.display());
                }
                println!(
                    "\n2. Then, in veles on the phone: Phone page, Join a pool, and scan this:\n"
                );
            } else {
                println!("In veles on the new node: Phone page, Join a pool, and scan this:\n");
            }
            println!("{}", pool::qr_text(&invite_text)?);
            if let Some(dir) = &png {
                let p = dir.join(format!("invite-{name}.png"));
                std::fs::write(&p, pool::qr_png(&invite_text, 8)?)?;
                println!("   (also {})", p.display());
            }
            println!(
                "\nOr on a laptop: veles pool join '<the text>' (`veles pool invite {name} --text` \
                 prints it). Good for {mins} minutes, once. It holds credentials: show it only \
                 on your own screen."
            );
            Ok(())
        }
        PoolAction::Join { invite, yes } => {
            let inv = read_invite(&invite)?;
            if let Some(r) = &inv.recover {
                anyhow::bail!(
                    "this is a recovery invite for {} ({}): run `veles recover '<the text>' --as \
                     {}` instead — it takes that identity back before it joins",
                    r.name,
                    r.id,
                    r.name
                );
            }
            println!(
                "This joins the pool of {} ({}) as {:?}, reached at {}.\n\
                 It writes [a2a_agents], [cluster] and this node's A2A bind host \
                 (platforms.a2a.extra.host) in config.toml and A2A_PEER_TOKENS in .env, for {} \
                 and every member it introduces.",
                inv.arbiter.name, inv.arbiter.url, inv.name, inv.url, inv.arbiter.name
            );
            if !yes {
                use std::io::Write as _;
                print!("Join? [y/N] ");
                std::io::stdout().flush()?;
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                if !matches!(line.trim(), "y" | "Y" | "yes") {
                    println!("Nothing was written.");
                    return Ok(());
                }
            }
            let home = veles_core::config::veles_home();
            let store = veles_core::store::Store::open(&home.join("state.db"))?;
            let rt = tokio::runtime::Runtime::new()?;
            let joined = rt.block_on(pool::join(&inv))?;
            pool::seed_registry(&store, &joined);
            println!(
                "Joined: {} and {} member(s): {}.",
                joined.arbiter.name,
                joined.members.len(),
                if joined.members.is_empty() {
                    "none yet".to_string()
                } else {
                    joined.members.join(", ")
                }
            );
            for n in &joined.not_introduced {
                println!(
                    "  not introduced to {}: {}",
                    n["name"].as_str().unwrap_or("?"),
                    n["error"].as_str().unwrap_or("?")
                );
            }
            if let Some(w) = &joined.written.warning {
                println!("  note: {w}");
            }
            println!("Restart veles so its A2A server starts and the pool can call it.");
            Ok(())
        }
        PoolAction::Introduce { a, b } => {
            let cfg = veles_core::config::load()?;
            let home = veles_core::config::veles_home();
            let store = veles_core::store::Store::open(&home.join("state.db"))?;
            let rt = tokio::runtime::Runtime::new()?;
            let r = rt.block_on(pool::introduce(&store, &cfg, &a, &b))?;
            println!(
                "introduced {a} and {b} to each other (lists: {}).",
                r["lists"]
                    .as_array()
                    .map(|l| l
                        .iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", "))
                    .unwrap_or_default()
            );
            for side in [&a, &b] {
                let w = &r[side.as_str()]["written"];
                println!(
                    "  {side}: wrote its side ({}){}",
                    w["peers"]
                        .as_array()
                        .map(|p| p
                            .iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", "))
                        .unwrap_or_default(),
                    w["warning"]
                        .as_str()
                        .map(|t| format!("\n    note: {t}"))
                        .unwrap_or_default()
                );
            }
            println!(
                "Each side's gateway accepts the new tokens at once (.env is read per call); \
                 restart one whose peer lists changed, so it gossips with its new peer."
            );
            Ok(())
        }
        PoolAction::Invites { revoke } => {
            if let Some(name) = revoke {
                if pool::revoke(&name)? {
                    println!("revoked the open invite for {name}");
                } else {
                    println!("no open invite for {name}");
                }
                return Ok(());
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            let rows = pool::ledger();
            if rows.is_empty() {
                println!("no invites");
            }
            for r in rows {
                let state = match r.redeemed_at {
                    Some(_) => "joined".to_string(),
                    None if r.expires <= now => "expired".to_string(),
                    None => format!("open, {} min left", (r.expires - now) / 60),
                };
                let tunnel =
                    r.wg.as_ref()
                        .map(|w| format!(" tunnel {}", w.address))
                        .unwrap_or_default();
                println!("{:<14} {:<28} {state}{tunnel}", r.name, r.url);
            }
            Ok(())
        }
    }
}

fn session_family(action: SessionAction) -> anyhow::Result<()> {
    // Session mail needs no node identity, and must not mint one.
    let action = match action {
        SessionAction::List { remote } => return session_mail_list(remote),
        SessionAction::Send { to, text, wait } => return session_mail_send(&to, &text, wait),
        SessionAction::Mail { id } => return session_mail_show(&id),
        other => other,
    };
    let home = veles_core::config::veles_home();
    let store = veles_core::store::Store::open(&home.join("state.db"))?;
    let me = veles_core::node::NodeIdentity::load_or_create(&veles_core::config::config_dir())?;
    // The two wire legs are async (the A2A push/query); run them on a
    // one-shot runtime the way the vault push does. `owner` is sync.
    let rt = tokio::runtime::Runtime::new()?;
    match action {
        SessionAction::Migrate { id, node, force } => {
            match rt.block_on(veles_core::migration::migrate(&me, &node, &id, force)) {
                Ok(line) => {
                    println!("{line}");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("migration failed: {e:#}");
                    std::process::exit(2);
                }
            }
        }
        SessionAction::Reclaim { id } => {
            match rt.block_on(veles_core::migration::reclaim(&me, &id)) {
                Ok(line) => {
                    println!("{line}");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("reclaim failed: {e:#}");
                    std::process::exit(2);
                }
            }
        }
        SessionAction::Takeover { id, ticket } => {
            use veles_core::takeover;
            let plan = takeover::plan(&store, &id)?;
            let authorized_by = match ticket {
                Some(t) => {
                    let cfg = veles_core::config::load()?;
                    takeover::spend_ticket(
                        &store,
                        &veles_core::ticket::trusted_signers(&cfg),
                        &me.id,
                        &plan,
                        &t,
                    )?
                }
                None => {
                    let asked = rt.block_on(takeover_grant(&me.id, &plan));
                    match asked {
                        Ok(by) => by,
                        Err(why) => {
                            eprintln!(
                                "the arbiter does not allow it: {why:#}\n\n\
                                 If {owner} is down and the arbiter cannot say so (it is the node \
                                 that died), one approval does: sign this with any trusted key, \
                                 on this device or another —\n  veles ticket sign '{req}'\nthen \
                                 run\n  veles \
                                 session takeover {id} --ticket '<the ticket>'\n\n\
                                 Or continue a copy here, which needs nobody: veles session \
                                 fork-replica {id}",
                                owner = plan.owner_name,
                                req = takeover::ticket_request(&me.id, &plan)?,
                            );
                            std::process::exit(2);
                        }
                    }
                }
            };
            let r = takeover::take_over(&store, &me.id, &me.name, &plan, &authorized_by)?;
            println!(
                "took {id} over from {} ({}): {} session(s), {} message(s), authorized by {}.\n\
                 The prompt and tool list came along, so the provider's cache can still be warm.\n\
                 not carried: {} checkpoint blob(s) (file undo) and {}'s background tasks.\n\
                 When {} is back it hands the session over; anything it wrote after your copy \
                 becomes a fork there.",
                plan.owner_name,
                plan.owner,
                r.sessions,
                r.messages,
                authorized_by.split(':').next().unwrap_or("?"),
                r.missing_blobs.len(),
                plan.owner_name,
                plan.owner_name,
            );
            Ok(())
        }
        SessionAction::ForkReplica { id } => {
            let (fork, r) = veles_core::takeover::fork_from_replica(&store, &me.id, &me.name, &id)?;
            println!(
                "{id} continues here as {fork}: {} session(s), {} message(s). The original is \
                 untouched. not carried: {} checkpoint blob(s) (file undo).",
                r.sessions,
                r.messages,
                r.missing_blobs.len()
            );
            Ok(())
        }
        SessionAction::List { .. } | SessionAction::Send { .. } | SessionAction::Mail { .. } => {
            unreachable!("session mail is answered before the node identity is loaded")
        }
        SessionAction::Takeovers => {
            drop(store);
            let home = veles_core::config::veles_home();
            let store = std::sync::Arc::new(std::sync::Mutex::new(veles_core::store::Store::open(
                &home.join("state.db"),
            )?));
            let cfg = std::sync::Arc::new(veles_core::config::load()?);
            let arbiter = veles_core::arbiter::StaticArbiter::new(store.clone(), cfg)
                .ok()
                .map(std::sync::Arc::new);
            let lines = rt.block_on(veles_core::takeover::learn(store, arbiter))?;
            if lines.is_empty() {
                println!("no takeovers of this node's sessions to settle");
            }
            for (_, l) in lines {
                println!("{l}");
            }
            Ok(())
        }
        SessionAction::Owner { id } => {
            let local = store.session_node_stamp(&id)?;
            // A tombstone outranks session_owner's local-row answer:
            // the row exists HERE, but the session lives on the target
            // — saying "owned by <this node>" would be the lie the
            // whole fence exists to prevent.
            if let Some(n) = local.as_deref() {
                if veles_core::migration::is_retired(Some(n)) {
                    let target = n
                        .trim_start_matches(veles_core::migration::RETIRED_PREFIX)
                        .split('@')
                        .next()
                        .unwrap_or("?");
                    println!(
                        "session {id}: RETIRED here — migrated to node {target} ({n}); \
                         the local rows are the archived original (readable, not writable)"
                    );
                    return Ok(());
                }
                if veles_core::migration::is_parked(Some(n)) {
                    let target =
                        veles_core::migration::park_target(Some(n)).unwrap_or_else(|| "?".into());
                    println!(
                        "session {id}: PARKED mid-migration to {target} ({n}) — \
                         `veles session reclaim {id}` finishes or undoes it"
                    );
                    return Ok(());
                }
            }
            let exists = store.session_exists(&id)?;
            match veles_core::storesync::session_owner(&store, &id)? {
                Some(owner) => {
                    println!("session {id}: owned by node {owner}");
                    Ok(())
                }
                None => {
                    if exists {
                        println!("session {id}: local, no owner stamp (pre-ownership era)");
                    } else {
                        println!("session {id}: unknown here and in every replica held");
                    }
                    Ok(())
                }
            }
        }
    }
}

/// Ask the pool's arbiter for the grant to take `plan` over. Ok = the
/// `authorized_by` a takeover records.
async fn takeover_grant(me: &str, plan: &veles_core::takeover::Plan) -> anyhow::Result<String> {
    use veles_core::arbiter::{Arbiter as _, LeaseState};
    let home = veles_core::config::veles_home();
    let store = std::sync::Arc::new(std::sync::Mutex::new(veles_core::store::Store::open(
        &home.join("state.db"),
    )?));
    let cfg = std::sync::Arc::new(veles_core::config::load()?);
    let arb = veles_core::arbiter::StaticArbiter::new(store, cfg)?;
    let key = veles_core::takeover::takeover_key(&plan.owner, &plan.root);
    match arb.try_acquire_lease(&key, me).await {
        LeaseState::Held(_) => Ok(format!("arbiter:{}", arb.arbiter_id())),
        LeaseState::HeldByOther { holder, .. } => {
            anyhow::bail!("node {holder} took it over already")
        }
        LeaseState::Refused(why) => anyhow::bail!("{why}"),
    }
}

fn replicas_family(action: ReplicasAction) -> anyhow::Result<()> {
    use veles_core::storesync;
    let home = veles_core::config::veles_home();
    let store = veles_core::store::Store::open(&home.join("state.db"))?;
    let me = veles_core::node::NodeIdentity::load_or_create(&veles_core::config::config_dir())?;
    match action {
        ReplicasAction::InstallLog => {
            let days = veles_core::config::load()
                .map(|c| c.cluster.storesync_backfill_days)
                .unwrap_or(14);
            let state = storesync::install_log(&store, days)?;
            println!("store log ready ({state})");
            Ok(())
        }
        ReplicasAction::Backfill { days } => {
            let days = days.unwrap_or_else(|| {
                veles_core::config::load()
                    .map(|c| c.cluster.storesync_backfill_days)
                    .unwrap_or(14)
            });
            let r = storesync::backfill(&store, days)?;
            println!(
                "backfilled {} row(s) of {} session(s) active in the last {} day(s) — \
                 they go out with the next push",
                r.rows, r.sessions, r.days
            );
            Ok(())
        }
        ReplicasAction::Status => {
            let max = storesync::max_seq(&store)?;
            println!("store log: {max} row(s) (cursor {max})");
            let auto = veles_core::config::load()
                .map(|c| c.cluster.storesync_auto)
                .unwrap_or(false);
            println!(
                "auto: {}",
                if auto {
                    "on (push + apply on the gossip cadence)"
                } else {
                    "off (push/apply by hand)"
                }
            );
            let replicas = storesync::list_replicas()?;
            if replicas.is_empty() {
                println!("no replica files held");
            } else {
                for r in &replicas {
                    println!(
                        "  replica of {}: cursor {}, {} session(s), {} message(s), {} bytes",
                        r.node, r.applied_seq, r.sessions, r.messages, r.bytes
                    );
                }
            }
            let pending = storesync::inbox_pending(&storesync::inbox_dir())?;
            println!(
                "inbox: {} offer(s) waiting{}",
                pending.len(),
                if pending.is_empty() {
                    String::new()
                } else {
                    " — apply with `veles replicas apply --inbox`".to_string()
                }
            );
            Ok(())
        }
        ReplicasAction::Push {
            peer,
            since,
            bearer,
        } => {
            let rt = tokio::runtime::Runtime::new()?;
            let out = rt.block_on(storesync::push_to_peer(
                &store,
                &me.id,
                &me.name,
                &peer,
                bearer.as_deref(),
                since,
            ))?;
            println!("{out}");
            Ok(())
        }
        ReplicasAction::Inbox => {
            let pending = storesync::inbox_pending(&storesync::inbox_dir())?;
            if pending.is_empty() {
                println!("no store sync offers waiting");
                return Ok(());
            }
            for p in pending {
                let entry = storesync::inbox_read(&p)?;
                println!(
                    "{}  from {} ({})  {} row(s) past {}",
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    entry.offer.from_name,
                    entry.offer.from_node,
                    entry.offer.rows.len(),
                    entry.offer.since_seq,
                );
            }
            Ok(())
        }
        ReplicasAction::Apply { file, inbox } => {
            if inbox {
                let dir = storesync::inbox_dir();
                let done = dir.join("applied");
                std::fs::create_dir_all(&done)?;
                let pending = storesync::inbox_pending(&dir)?;
                if pending.is_empty() {
                    println!("no store sync offers waiting");
                    return Ok(());
                }
                let mut failed = 0;
                for p in pending {
                    let entry = match storesync::inbox_read(&p) {
                        Ok(e) => e,
                        Err(e) => {
                            eprintln!("{}: {e:#}", p.display());
                            failed += 1;
                            continue;
                        }
                    };
                    let conn = storesync::open_replica(&entry.offer.from_node)?;
                    match storesync::apply_offer(&conn, &entry.offer) {
                        Ok(report) => {
                            println!(
                                "{}: {} row(s) applied, {} deleted, cursor {}{}",
                                p.file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                                report.applied_rows,
                                report.deleted_rows,
                                report.final_seq,
                                if report.withheld_secret_rows > 0 {
                                    format!(
                                        " ({} row(s) withheld: credential-shaped)",
                                        report.withheld_secret_rows
                                    )
                                } else {
                                    String::new()
                                }
                            );
                            if let Some(from) = report.rebased_from {
                                println!(
                                    "  skipped from cursor {from}: the sender's log no longer holds \
                                     those rows (its backfill re-sends the recent sessions)"
                                );
                            }
                            std::fs::rename(&p, done.join(p.file_name().unwrap_or_default()))?;
                        }
                        Err(e) => {
                            eprintln!("{}: {e:#}", p.display());
                            failed += 1;
                        }
                    }
                }
                if failed > 0 {
                    anyhow::bail!(
                        "{failed} offer(s) refused — see above (gaps need a re-push from zero)"
                    );
                }
                Ok(())
            } else {
                let path = file
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("give a file (or `-`), or --inbox"))?;
                let raw = if path == "-" {
                    use std::io::Read;
                    let mut s = String::new();
                    std::io::stdin().read_to_string(&mut s)?;
                    s
                } else {
                    std::fs::read_to_string(path)?
                };
                let entry = storesync::inbox_read(std::path::Path::new(path)).or_else(|_| {
                    Ok::<_, anyhow::Error>(storesync::InboxEntry {
                        received_at: String::new(),
                        peer: String::new(),
                        task_id: String::new(),
                        offer: storesync::decode_message(&raw)?,
                    })
                })?;
                let conn = storesync::open_replica(&entry.offer.from_node)?;
                let report = storesync::apply_offer(&conn, &entry.offer)?;
                println!(
                    "{} row(s) applied, {} deleted, {} up-to-date, cursor {}{}",
                    report.applied_rows,
                    report.deleted_rows,
                    report.skipped_uptodate,
                    report.final_seq,
                    if report.withheld_secret_rows > 0 {
                        format!(
                            " ({} row(s) withheld: credential-shaped)",
                            report.withheld_secret_rows
                        )
                    } else {
                        String::new()
                    }
                );
                if let Some(from) = report.rebased_from {
                    println!(
                        "  skipped from cursor {from}: the sender's log no longer holds those \
                         rows (its backfill re-sends the recent sessions)"
                    );
                }
                Ok(())
            }
        }
        ReplicasAction::List => {
            let replicas = storesync::list_replicas()?;
            if replicas.is_empty() {
                println!("no replica files held");
                return Ok(());
            }
            for r in replicas {
                println!(
                    "{}: cursor {}, {} session(s), {} message(s), {} bytes",
                    r.node, r.applied_seq, r.sessions, r.messages, r.bytes
                );
            }
            Ok(())
        }
        ReplicasAction::Search { query, limit } => {
            let hits = storesync::search_replicas(&query, limit)?;
            if hits.is_empty() {
                println!("no hits in any replica");
                return Ok(());
            }
            for h in hits {
                println!(
                    "[{}] {} {} {}: {}",
                    h.node, h.created_at, h.session_id, h.role, h.snippet
                );
            }
            Ok(())
        }
    }
}

fn configsync_family(action: ConfigsyncAction) -> anyhow::Result<()> {
    use veles_core::configsync;
    let home = veles_core::config::veles_home();
    let store = veles_core::store::Store::open(&home.join("state.db"))?;
    let me = veles_core::node::NodeIdentity::load_or_create(&veles_core::config::config_dir())?;
    match action {
        ConfigsyncAction::Scan => {
            let report = configsync::scan_file(&store, &me.id, &configsync::config_path())?;
            println!("{}", report.summarize());
            Ok(())
        }
        ConfigsyncAction::Status => {
            let rows = store.config_log_rows()?;
            let mut by_node: std::collections::BTreeMap<&str, (usize, i64, &str)> =
                std::collections::BTreeMap::new();
            for r in &rows {
                let e = by_node
                    .entry(r.node.as_str())
                    .or_insert((0, 0, r.at.as_str()));
                e.0 += 1;
                e.1 = e.1.max(r.node_seq);
                e.2 = r.at.as_str();
            }
            if rows.is_empty() {
                println!(
                    "config log is empty — run `veles configsync scan` to log this file's writes"
                );
            } else {
                println!("config log: {} row(s)", rows.len());
                for (node, (n, max, at)) in &by_node {
                    let self_mark = if *node == me.id { " (this node)" } else { "" };
                    println!("  {node}: {n} row(s), node_seq ≤ {max}, last at {at}{self_mark}");
                }
            }
            let pending = configsync::inbox_pending(&configsync::inbox_dir())?;
            println!(
                "inbox: {} offer(s) waiting{}",
                pending.len(),
                if pending.is_empty() {
                    String::new()
                } else {
                    " — apply with `veles configsync apply --inbox`".to_string()
                }
            );
            Ok(())
        }
        ConfigsyncAction::Offer { since, out } => {
            let (offer, withheld) = configsync::build_offer(&store, &me.id, &me.name, since)?;
            for k in &withheld {
                eprintln!("withheld {k}: credential-shaped value, secrets stay on their node");
            }
            if offer.rows.is_empty() {
                println!("nothing to offer since seq {since}");
                return Ok(());
            }
            let text = configsync::encode_message(&offer)?;
            match out.as_deref() {
                None | Some("-") => print!("{text}"),
                Some(path) => {
                    std::fs::write(path, &text).map_err(|e| anyhow::anyhow!("write {path}: {e}"))?
                }
            }
            Ok(())
        }
        ConfigsyncAction::Push {
            peer,
            since,
            bearer,
        } => {
            let rt = tokio::runtime::Runtime::new()?;
            let out = rt.block_on(configsync::push_to_peer(
                &store,
                &me.id,
                &me.name,
                &peer,
                bearer.as_deref(),
                since,
            ))?;
            println!("{out}");
            Ok(())
        }
        ConfigsyncAction::Inbox => {
            let pending = configsync::inbox_pending(&configsync::inbox_dir())?;
            if pending.is_empty() {
                println!("no config sync offers waiting");
                return Ok(());
            }
            for p in pending {
                let entry = configsync::inbox_read(&p)?;
                println!(
                    "{}  from {} ({})  {}",
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    entry.offer.from_name,
                    entry.offer.from_node,
                    configsync::summarize_offer(&entry.offer),
                );
            }
            Ok(())
        }
        ConfigsyncAction::Apply {
            file,
            inbox,
            dry_run: true,
        } => {
            let mut offers = Vec::new();
            if inbox {
                for p in configsync::inbox_pending(&configsync::inbox_dir())? {
                    let label = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    offers.push((label, configsync::inbox_read(&p)?.offer));
                }
            } else {
                let Some(file) = file else {
                    anyhow::bail!("give an offer file (`-` for stdin) or --inbox");
                };
                let raw = if file == "-" {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf
                } else {
                    std::fs::read_to_string(&file)?
                };
                offers.push((file, configsync::decode_message(&raw)?));
            }
            for (label, offer) in offers {
                let report = configsync::preview_offer(&store, &me.id, &offer)?;
                println!(
                    "— {label} from {} (dry run: nothing imported or written) —\n{}",
                    offer.from_name,
                    report.summarize()
                );
                // What auto-apply would hold: a security-section row that
                // CHANGES something here; one this node already agrees
                // with lands quietly.
                let (_, held) = configsync::split_held(&offer);
                if !held.rows.is_empty() {
                    let p = configsync::preview_offer(&store, &me.id, &held)?;
                    if p.changed.is_empty() && p.is_clean() {
                        println!(
                            "{} security-section row(s) agree with this node: with \
                             configsync_auto they land quietly",
                            held.rows.len()
                        );
                    } else {
                        println!(
                            "with configsync_auto, these wait for the owner: {}",
                            p.changed.join(", ")
                        );
                    }
                }
            }
            Ok(())
        }
        ConfigsyncAction::Apply { file, inbox, .. } => {
            let mut conflicted = false;
            if inbox {
                let dir = configsync::inbox_dir();
                let done = dir.join("applied");
                for p in configsync::inbox_pending(&dir)? {
                    let entry = configsync::inbox_read(&p)?;
                    let report = configsync::apply_offer(&store, &me.id, &entry.offer)?;
                    println!(
                        "{}",
                        if entry.offer.from_name.is_empty() {
                            report.summarize()
                        } else {
                            format!("— from {} —\n{}", entry.offer.from_name, report.summarize())
                        }
                    );
                    if report.is_clean() {
                        std::fs::create_dir_all(&done)?;
                        std::fs::rename(&p, done.join(p.file_name().unwrap_or_default()))?;
                    } else {
                        conflicted = true;
                    }
                }
                if configsync::inbox_pending(&dir)?.is_empty() && !conflicted {
                    // fallthrough: nothing waiting is not an error
                }
            } else {
                let Some(file) = file else {
                    anyhow::bail!("give an offer file (`-` for stdin) or --inbox");
                };
                let raw = if file == "-" {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf
                } else {
                    std::fs::read_to_string(&file)?
                };
                let offer = configsync::decode_message(&raw)?;
                let report = configsync::apply_offer(&store, &me.id, &offer)?;
                println!("{}", report.summarize());
                conflicted = !report.is_clean();
            }
            if conflicted {
                // std::process::exit bypasses destructors, but this is
                // the command's last act.
                std::process::exit(2);
            }
            Ok(())
        }
    }
}

/// ` (asserted)` / ` (observed)` after a registry row's static_ip, so a
/// reader can tell the operator's stability promise from a probe's
/// sighting; nothing for a row written before the source was recorded.
fn static_ip_source_mark(n: &veles_core::store::NodeRow) -> String {
    n.static_ip_source
        .as_deref()
        .map(|src| format!(" ({src})"))
        .unwrap_or_default()
}

/// `veles nodes …` — the cluster registry. Reads (and for `probe`,
/// writes) the same `nodes` table the gateway heartbeats into; no Agent
/// is built, for the same reason `acl_family` builds none — a question
/// about a registry row must not load providers and a session.
fn nodes_family(action: Option<NodesAction>) -> anyhow::Result<()> {
    let home = veles_core::config::veles_home();
    let store = veles_core::store::Store::open(&home.join("state.db"))?;
    let cfg = veles_core::config::load()?;
    // Reads never mint an identity; only whoami/probe may.
    let self_id = veles_core::node::NodeIdentity::load(&veles_core::config::config_dir())
        .ok()
        .flatten()
        .map(|i| i.id)
        .unwrap_or_default();
    let stale_after = cfg.cluster.stale_after_secs as i64;

    let render = |n: &veles_core::store::NodeRow| -> String {
        let stale = veles_core::node::is_stale(&n.heartbeat_at, stale_after);
        let me = if n.id == self_id { " (self)" } else { "" };
        let stale_mark = if stale { "  [stale]" } else { "" };
        let gpu = match &n.gpu {
            Some(g) => serde_json::from_str::<serde_json::Value>(g)
                .ok()
                .and_then(|v| v["name"].as_str().map(|s| s.to_string()))
                .map(|s| format!("  gpu={s}"))
                .unwrap_or_default(),
            None => String::new(),
        };
        // B4: placement metrics — normalized load ('?' when unknown:
        // unknown is not idle and must not read like 0) and lifetime
        // sessions owned.
        let load = match n.load1 {
            Some(l) => format!("  load={l}"),
            None => "  load=?".to_string(),
        };
        let served = format!("  served={}", n.sessions_served);
        // T2 (PLAN-phone-userland.md): what the node has, as it
        // advertised it — what automatic homes and `needs` route on. A
        // node on an older build advertises nothing and says so.
        let os = n.caps_os().map(|o| format!("  os={o}")).unwrap_or_default();
        let tools = match n.caps_tools() {
            Some(t) if t.is_empty() => "\n    tools     none".to_string(),
            Some(t) => format!("\n    tools     {}", t.join(" ")),
            None => "\n    tools     (not advertised — an older build)".to_string(),
        };
        // B3a left the format/arg counts drifted: `heartbeat_at` was
        // consumed by the inline group (so it printed after static_ip
        // on line 1) and the second line's literal `heartbeat` label
        // was followed by the observed address. B4's load/served made
        // it visible (`served=0` glued to the date). Line 1 = identity
        // + capabilities + placement; line 2 = liveness.
        format!(
            "{}  {}{}  host={}  v{}  {}{}{}{}{}{}\n    heartbeat {}{}{}{}",
            n.id,
            n.name,
            me,
            n.host,
            n.version,
            n.uptime_class,
            os,
            gpu,
            load,
            served,
            n.static_ip
                .as_deref()
                .map(|s| format!("  static_ip={s}{}", static_ip_source_mark(n)))
                .unwrap_or_default(),
            n.heartbeat_at,
            // B3a: the peer-observed address, labelled as a sighting —
            // one address says nothing about stability; the HISTORY is
            // the evidence and `nodes show` prints it.
            n.observed_addr
                .as_deref()
                .map(|a| format!("  seen={a}"))
                .unwrap_or_default(),
            stale_mark,
            tools
        )
    };

    match action.unwrap_or(NodesAction::List) {
        NodesAction::List => {
            let nodes = store.nodes_list()?;
            if nodes.is_empty() {
                println!("no nodes registered — start a gateway, or run `veles nodes probe`");
            } else {
                for n in &nodes {
                    println!("{}", render(n));
                }
            }
            // The singleton fence (B1.5): which node arbitrates cron/
            // telegram/IRC. Unset is fail-closed-by-default — say so,
            // loudly, every time the registry is read, because the
            // double-poller is the catastrophic failure mode.
            match cfg.cluster.arbiter.as_deref().map(str::trim) {
                None | Some("") => {
                    println!("\n{}", veles_core::node::arbiter_unset_status(&cfg.cluster))
                }
                Some(_) => match veles_core::node::validate_arbiter(&cfg.cluster, &store) {
                    Ok(full) => {
                        let who = nodes
                            .iter()
                            .find(|n| n.id == full)
                            .map(|n| n.name.as_str())
                            .unwrap_or("?");
                        println!("\narbiter: {who} ({full})");
                    }
                    Err(e) => println!("\narbiter: {e:#}"),
                },
            }
            Ok(())
        }
        NodesAction::Whoami => {
            let id =
                veles_core::node::NodeIdentity::load_or_create(&veles_core::config::config_dir())?;
            let caps = veles_core::node::NodeCaps::resolve(&cfg.node);
            println!("id           {}", id.id);
            println!(
                "name         {}",
                cfg.node.name.as_deref().unwrap_or(&id.name)
            );
            println!("host         {}", veles_core::node::hostname());
            println!("version      {}", veles_core::VERSION);
            println!("uptime_class {}", caps.uptime_class);
            println!("os           {}", caps.os);
            println!(
                "tools        {}",
                if caps.tools.is_empty() {
                    "none".to_string()
                } else {
                    caps.tools.join(" ")
                }
            );
            match &caps.gpu {
                Some(g) => println!(
                    "gpu          {} ({})",
                    g.name,
                    g.memory_total_mb
                        .map(|m| format!("{m} MB"))
                        .unwrap_or_else(|| "?".into())
                ),
                None => println!("gpu          none"),
            }
            if let Some(e) = cfg.node.endpoint.as_deref() {
                println!("endpoint     {e}");
            }
            // What the registry row records (asserted or nothing), then
            // the local route source labelled as what it is: a LAN-side
            // address that says nothing about the WAN IP or its stability.
            match veles_core::node::resolve_static_ip(&cfg.node) {
                Some((a, src)) => println!("static_ip    {a}  ({})", src.as_str()),
                None => println!(
                    "static_ip    none asserted — set [node] static_ip if this node's uplink has one"
                ),
            }
            if let Some(a) = veles_core::node::probe_route_source() {
                println!("route src    {a}  (local route source — behind NAT a LAN address, not the WAN IP)");
            }
            if !caps.extra.is_empty() {
                println!("caps         {}", serde_json::to_string(&caps.extra)?);
            }
            Ok(())
        }
        NodesAction::Show { id } => {
            let n = resolve_node(&store, &id)?;
            println!("{}", render(&n));
            if let Some(g) = &n.gpu {
                println!("    gpu        {g}");
            }
            if let Some(c) = &n.caps {
                println!("    caps       {c}");
            }
            if let Some(e) = &n.endpoint {
                println!("    endpoint   {e}");
            }
            if let Some(s) = &n.static_ip {
                println!("    static_ip  {s}{}", static_ip_source_mark(&n));
            }
            // B3a: the observation history — the evidence for whether
            // this node's uplink is stable is the SEQUENCE of addresses
            // its peers saw, not the latest one.
            if let Ok(hist) = store.node_observation_history(&n.id, 10) {
                if !hist.is_empty() {
                    println!("    observed (latest first):");
                    for (at, addr) in hist.iter().rev() {
                        println!("      {at}  {addr}");
                    }
                }
            }
            println!("    registered {}", n.registered_at);
            Ok(())
        }
        NodesAction::Probe => {
            let id = veles_core::node::register_self(&store, &cfg)?;
            println!("registered node {} ({})", id.id, id.name);
            Ok(())
        }
        NodesAction::Forget { id } => {
            let target = resolve_node(&store, &id)?;
            if store.node_forget(&target.id)? {
                println!("forgot node {} ({})", target.id, target.name);
            }
            Ok(())
        }
    }
}

/// `veles acl|sandbox|limits` — the same engine calls every chat
/// surface makes, against the same state.db.
///
/// NOT via an Agent: building one to answer `veles acl list` would
/// load providers, plugins and a session for a question about a rule
/// table, which is why `commands::AclCtx` exists.
fn acl_family(which: &str, args: &[String]) -> anyhow::Result<()> {
    let home = veles_core::config::veles_home();
    let store = std::sync::Arc::new(std::sync::Mutex::new(veles_core::store::Store::open(
        &home.join("state.db"),
    )?));
    veles_core::acl::set_runtime_pairing_store(store.clone());
    let fallback = veles_core::acl::live_config();
    // No principal: this is the owner's own shell. The human at this
    // keyboard already has the keys.
    let ctx = veles_frontends::commands::AclCtx {
        store: &store,
        fallback: &fallback,
        session_id: "",
        principal: None,
        sender: None,
    };
    // NOT `args.join(" ")`: the shell already split these correctly,
    // and a bare-space join undoes its quoting a second time — a
    // `arg=two words` element comes back out as two tokens. `rejoin_argv`
    // preserves the boundaries the shell established.
    let rest = veles_core::triggers::rejoin_argv(args);
    let out = match which {
        "acl" => veles_frontends::commands::acl_command(&ctx, &rest),
        "limits" => veles_frontends::commands::limits_command(&ctx, &rest),
        // `Surface::Repl`, and it is not a formality: a shell on this
        // box is exactly the evidence `/lockdown off` asks for, and
        // this is the surface an operator reaches for when the desktop
        // is not running.
        "lockdown" => veles_frontends::commands::lockdown_command(
            &ctx,
            veles_frontends::commands::Surface::Repl,
            &rest,
        ),
        _ => {
            let rest = if rest.trim().is_empty() {
                "list"
            } else {
                &rest
            };
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(veles_frontends::commands::sandbox_command(&ctx, rest))
        }
    };
    println!("{out}");
    Ok(())
}

#[derive(Subcommand)]
enum PairAction {
    /// Create a pairing code (default role=user, 24h expiry)
    New {
        /// Role to grant on redemption: trusted | user | guest
        #[arg(long, default_value = "user")]
        role: String,
        /// Code expiry in hours (default 24)
        #[arg(long, default_value_t = 24)]
        ttl_hours: u64,
    },
    /// List live pairing grants
    List,
    /// Revoke an identity's grant (e.g. telegram:12345678)
    Revoke { identity: String },
}

#[derive(Subcommand)]
enum TasksAction {
    /// List running subagents, background shells and monitors (default)
    List,
    /// Show captured output for a background shell
    Output { id: u32 },
    /// Stop one task
    Cancel { id: u32 },
}

#[derive(Subcommand)]
enum CronAction {
    /// List jobs (default)
    List,
    /// Create a local-delivery job (the same one /crons add creates)
    Add {
        /// Schedule: "30m", "2h", "1d", "cron:0 9 * * *", "once:<rfc3339>"
        schedule: String,
        /// Resume THIS session as a wakeup (id or unique prefix) instead
        /// of a fresh session
        #[arg(long)]
        session: Option<String>,
        /// The prompt the job runs
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    /// Fire a job immediately (manual run, no schedule slot consumed)
    Run { job_id: String },
    /// Pause a job
    Pause { job_id: String },
    /// Resume a paused job
    Resume { job_id: String },
    /// Delete a job and its run history
    Remove { job_id: String },
    /// Show recent run history for a job
    History { job_id: String },
}

#[derive(Subcommand)]
enum TriggersAction {
    /// List trigger rows (default)
    List,
    /// Create a trigger: veles triggers add <name> network=… channel=…
    /// event=TEXT match=… action=slopdrop [cooldown=30s] [cap=5]
    /// [min-role=trusted] [pattern=…] [arg=…]
    Add {
        name: String,
        #[arg(trailing_var_arg = true)]
        spec: Vec<String>,
    },
    /// Show one row
    Show { name: String },
    /// Enable a row
    Enable { name: String },
    /// Disable a row
    Disable { name: String },
    /// Remove a row and its fire history
    Rm { name: String },
    /// Recent fires/refusals for one trigger (all when omitted)
    Log { name: Option<String> },
    /// Would this text fire this trigger, without calling anything
    Test { name: String, text: String },
    /// Pin a channel's forward mask: EVENTS or `auto`
    Mask {
        network: String,
        channel: String,
        events: String,
    },
    /// Re-derive every channel's forward mask from slopdrop's bindings
    Refresh,
}

#[derive(Subcommand)]
enum TclAction {
    /// Evaluate one Tcl expression in the gateway's live interpreter
    Eval {
        /// Tcl code to evaluate
        code: String,
    },
    /// Interactive REPL against the live interpreter
    Repl,
    /// Re-source every plugin script (manual hot reload)
    Reload,
    /// Source a local file through the live interpreter
    File { path: String },
}

#[derive(Subcommand)]
enum GatewayAction {
    /// Run the gateway in the foreground (the service/task runs this)
    Run {
        /// Stop after this many seconds nobody has used the gateway (no
        /// desktop request, no inbound message, no turn running), with
        /// exit status 75 — the phone's app passes 300 and starts it
        /// again when it is opened.
        #[arg(long)]
        exit_when_idle: Option<u64>,
    },
    /// Show gateway state: running/stopped, pid, uptime, adapters,
    /// recent lifecycle records
    Status,
    /// Stop the running gateway (its service if installed, else a graceful
    /// stop over the local socket/pipe, then the signal)
    Stop,
    /// Stop then start the gateway again (its service if installed)
    Restart,
    /// Install the gateway as a service and start it: a systemd user unit
    /// (veles-gateway.service) on Linux, a logon task (veles-gateway) on Windows
    Install,
    /// Disable and remove the service (the unit, or the logon task)
    Uninstall,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print a complete config.toml at defaults — every option the binary
    /// reads, with the unset ones named in comments rather than written
    Defaults,
    /// Print every var this build reads from .env, at the value it falls
    /// back to — commented, the way `fill` writes them
    EnvDefaults,
    /// Document every missing option in config.toml as a comment showing
    /// its default (nothing is pinned), and every var it does not mention
    /// in the .env beside it. Never re-serializes either: comments, values,
    /// key order and live credentials survive, and the old files are kept
    /// as .bak. Also reports keys nothing reads.
    Fill {
        /// Audit only: print the report, write nothing
        #[arg(long)]
        dry_run: bool,
        /// File to fill (default: the live config.toml)
        #[arg(long)]
        path: Option<std::path::PathBuf>,
    },
    /// Comment out every live line that only restates its built-in
    /// default (`key = 4` → `# key = 4  # default`), so a default the code
    /// changes reaches this install. Shows the change; `--apply` writes it
    /// (with a .bak). Never changes what the config means — each line is
    /// checked before it is touched.
    Tidy {
        /// Write the change (default: show it only)
        #[arg(long)]
        apply: bool,
        /// File to tidy (default: the live config.toml)
        #[arg(long)]
        path: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum PluginsAction {
    /// List discovered plugins and their enable state
    List,
    /// Roll back plugin state to a git snapshot (`git log` in plugins dir)
    Rollback {
        /// Commit hash or ref from `git log` in the plugins dir
        /// ($XDG_CONFIG_HOME/veles/plugins, i.e. ~/.config/veles/plugins)
        commit: String,
    },
    /// Show the hook taxonomy: which hooks the engine fires, which are
    /// reserved for hermes compat, and which have handlers right now
    Hooks,
    /// Create a working plugin from a template
    New {
        /// Directory name, and the plugin's name in its manifest
        name: String,
        /// One line for the manifest and the file header
        #[arg(long, default_value = "")]
        description: String,
        /// The plugin's language: tcl (default), js (TypeScript, the
        /// lang-js host), racket (the lang-racket host) or lisp (Common
        /// Lisp, the lang-lisp host). `overlay` writes a Rust overlay
        /// module's own repository into ./<name> instead: a crate and a
        /// flake that builds veles with it (docs/extending.md, "Rust
        /// overlay modules"; not part of the stable API)
        #[arg(long, default_value = "tcl")]
        kind: String,
    },
    /// Clone a plugin repo into the plugins directory
    Install {
        /// Any git URL, including a local path
        url: String,
        /// Install under a different name than the manifest's
        #[arg(long = "as")]
        as_name: Option<String>,
    },
    /// Delete a plugin (the git snapshot taken first is the undo)
    Remove { name: String },
    /// Install a BUILT module file (a `veles_module_*` dylib from this
    /// build's target dir) under `<data>/modules/<name>/`, content-
    /// addressed with `current` pointing at it, and write its
    /// plugin.toml. The portable form of scripts/install-module-dev.sh;
    /// the gateway loads it at the next start or `plugins rescan`.
    InstallModule {
        /// The name the module DECLARES (`irc`, `lang-js`, `echo-module`
        /// for the echo module): the host refuses a manifest whose name
        /// differs from the module's own, and the name is only readable
        /// by loading it, which this command does not do
        name: String,
        /// The built file: target/<profile>/deps/libveles_module_<n>.so,
        /// or target\<profile>\veles_module_<n>.dll
        file: std::path::PathBuf,
    },
    /// The running gateway's plugins (process AND module tiers): state,
    /// pid/version, restarts, last error
    Status,
    /// Restart a plugin (`name` or `name:instance`); the gateway and
    /// every other plugin keep running. A module reloads in-process: the
    /// gateway PID stays. A Tcl plugin is sourced again.
    Reload { target: String },
    /// Unload a plugin: a module or process plugin is torn down and
    /// forgotten until a `rescan` (or restart) finds it again; a Tcl
    /// plugin's handlers and registrations are dropped until `start`.
    Unload { target: String },
    /// Stop a plugin until `start` (its messages wait, up to the queue's limits)
    Stop { target: String },
    /// Start a stopped plugin
    Start { target: String },
    /// The last lines a process plugin wrote to its log
    Logs { target: String },
    /// Re-discover plugins under the gateway's roots and start anything
    /// new: a first install needs no restart
    Rescan,
    /// The running gateway's plugin services: who provides each one, who
    /// declares and uses it, and which required use has no provider
    Services,
    /// The running gateway's override points: each decision core asks by
    /// name, and the plugins that answer it, top first (docs/extending.md)
    Overrides,
    /// The extension API a plugin may rely on across updates: its
    /// version and surfaces, as published in docs/extension-api.json
    /// (docs/extending.md)
    Api {
        /// Print the spec as JSON
        #[arg(long, conflicts_with_all = ["write", "stamp", "check"])]
        json: bool,
        /// Rewrite docs/extension-api.json from this build (run in the
        /// repository root)
        #[arg(long, conflicts_with_all = ["stamp", "check"])]
        write: bool,
        /// Print the changelog heading for the spec as this build states it
        #[arg(long, conflicts_with = "check")]
        stamp: bool,
        /// Compare the code, the spec, its changelog and docs/extending.md
        #[arg(long)]
        check: bool,
    },
}

fn prompt_profile_secrets(
    names: Option<&Vec<serde_json::Value>>,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    profile_secrets_with(names, veles_core::secrets::resolve, |name| {
        Ok(veles_cli::read_secret(&format!(
            "value for ${name} (hidden; Enter leaves it for later setup): "
        ))?)
    })
}

fn profile_secrets_with(
    names: Option<&Vec<serde_json::Value>>,
    mut stored: impl FnMut(&str) -> Option<String>,
    mut prompt: impl FnMut(&str) -> anyhow::Result<String>,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let mut values = std::collections::BTreeMap::new();
    for name in names
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
    {
        if let Some(value) = stored(name) {
            anyhow::ensure!(!value.is_empty(), "stored profile secret {name} is empty");
            values.insert(name.to_string(), value);
            continue;
        }
        let value = prompt(name)?;
        if !value.is_empty() {
            values.insert(name.to_string(), value);
        }
    }
    Ok(values)
}

#[cfg(test)]
mod profile_secret_tests {
    #[test]
    fn stored_profile_secrets_reach_the_handoff_without_a_prompt() {
        let names = vec![serde_json::json!("FIXTURE_PROFILE_SECRET")];
        let values = super::profile_secrets_with(
            Some(&names),
            |_| Some("stored-value".into()),
            |_| panic!("a stored secret must not be prompted for"),
        )
        .unwrap();
        assert_eq!(values["FIXTURE_PROFILE_SECRET"], "stored-value");
    }

    #[test]
    fn an_empty_prompt_does_not_invent_a_profile_secret() {
        let names = vec![serde_json::json!("FIXTURE_PROFILE_SECRET")];
        let values =
            super::profile_secrets_with(Some(&names), |_| None, |_| Ok(String::new())).unwrap();
        assert!(values.is_empty());
    }
}

fn run_install(action: InstallAction) -> anyhow::Result<()> {
    let service = veles_runtime::veles_install::InstallService::new(
        veles_runtime::veles_install::Paths::current(),
    );
    let confirm = |yes: bool| -> anyhow::Result<()> {
        if yes {
            return Ok(());
        }
        eprint!("Apply this plan? [y/N] ");
        use std::io::Write as _;
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        anyhow::ensure!(
            matches!(answer.trim(), "y" | "Y" | "yes"),
            "cancelled; nothing changed"
        );
        Ok(())
    };
    match action {
        InstallAction::State => println!("{}", serde_json::to_string_pretty(&service.state()?)?),
        InstallAction::Preview { source } => println!("{}", serde_json::to_string_pretty(&service.preview_bundle(&source)?.review())?),
        InstallAction::Bundle { source, yes } => {
            let plan = service.preview_bundle(&source)?;
            println!("{}", serde_json::to_string_pretty(&plan.review())?);
            confirm(yes)?;
            println!("{}", serde_json::to_string_pretty(&service.install_local_bundle(&source, &plan.plan_sig)?)?);
        }
        InstallAction::Download { release, repository, tag, commit, workflow_sha, yes } => {
            let request = veles_runtime::veles_install::ReleaseRequest {
                release_url: release,
                repository,
                tag,
                commit,
                workflow_sha,
            };
            // No ambient Tokio runtime here: component discovery uses
            // its own synchronous runtime in the existing CLI paths.
            let plan = service.preview_signed_bundle(&request)?;
            println!("{}", serde_json::to_string_pretty(&plan.review())?);
            confirm(yes)?;
            let result = service.apply_signed_bundle(plan)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        InstallAction::Configure { name, answers, instance, yes } => {
            let text = if answers == std::path::Path::new("-") {
                use std::io::Read;
                let mut text = String::new();
                std::io::stdin().read_to_string(&mut text)?;
                text
            } else {
                std::fs::read_to_string(&answers)?
            };
            let body: serde_json::Value = serde_json::from_str(&text)?;
            let values = body
                .get("options")
                .unwrap_or(&body)
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("answers must be a JSON object or contain an options object"))?
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let plan = service.preview_config(&name, &instance, &values)?;
            println!("{}", serde_json::to_string_pretty(&plan.review())?);
            if yes {
                println!("{}", serde_json::to_string_pretty(&service.apply_config(&plan)?)?);
            }
        }
        InstallAction::Profile { answers, template, core, yes } => {
            use veles_runtime::veles_install::{NamedProfile, ProfileRequest, ServiceScope};
            if let Some(name) = template {
                print!("{}", veles_runtime::veles_install::templates::profile_template(&name)?);
                return Ok(());
            }
            let answers = answers.context("--answers <file> (or --template <profile> to print one)")?;
            let answers: veles_runtime::veles_install::ProfileAnswers =
                veles_runtime::veles_install::templates::parse_answers(&std::fs::read_to_string(&answers)?)?;
            let host_scope = (answers.profile == NamedProfile::Server && answers.host.scope == ServiceScope::System) || answers.profile == NamedProfile::ContainerHost;
            let request = if host_scope {
                let container = answers.profile == NamedProfile::ContainerHost;
                anyhow::ensure!(container || core.is_some(), "a system server requires --core <unpacked bundle dir>; only container-host has no native core");
                let mut request = ProfileRequest::host(answers)?;
                if !container {
                    request.core = std::path::absolute(core.unwrap())?;
                }
                request
            } else if answers.profile == NamedProfile::Nixos {
                ProfileRequest::local(answers, std::path::PathBuf::new())?
            } else {
                let core = core.context("a native local profile requires --core <unpacked bundle dir>")?;
                ProfileRequest::local(answers, std::path::absolute(core)?)?
            };
            let plan = service.preview_profile(request.clone())?;
            println!("{}", serde_json::to_string_pretty(&plan.review())?);
            if yes {
                // A system or container request is only staged here; its
                // secrets are asked for by the root handoff that uses them.
                let secrets = if host_scope {
                    Default::default()
                } else {
                    prompt_profile_secrets(plan.review()["secrets"].as_array())?
                };
                println!("{}", serde_json::to_string_pretty(&service.apply_profile(request, &plan.base_sig, &plan.plan_sig, &secrets)?)?);
            }
        }
        InstallAction::HostComponent { answers, template, yes } => {
            use veles_runtime::veles_install::{templates, NamedProfile, ProfileAnswers, ProfileRequest};
            if let Some(name) = template {
                print!("{}", templates::component_template(&name)?);
                return Ok(());
            }
            let answers = answers.context("--answers <file> (or --template <component> to print one)")?;
            let parsed: templates::HostComponentAnswers =
                templates::parse_answers(&std::fs::read_to_string(&answers)?)?;
            anyhow::ensure!(!parsed.component.is_empty(), "the answers file names no [[component]]");
            let mut profile = ProfileAnswers {
                profile: NamedProfile::Desktop,
                ..Default::default()
            };
            profile.setup.skipped = veles_core::setup::SECTIONS.iter().map(|s| s.to_string()).collect();
            profile.host.host_components = parsed.component;
            let request = ProfileRequest::local(profile, std::path::PathBuf::new())?;
            let plan = service.preview_profile(request.clone())?;
            println!("{}", serde_json::to_string_pretty(&plan.review())?);
            if yes {
                println!("{}", serde_json::to_string_pretty(&service.apply_profile(request, &plan.base_sig, &plan.plan_sig, &Default::default())?)?);
            }
        }
        InstallAction::ContainerHost { answers, storage_root, gateway_user, sandbox_user, network, gateway_image, gateway_image_digest, gateway_name, yes } => {
            use veles_runtime::veles_install::{container_host as ch, NamedProfile, ProfileAnswers, ProfileRequest};
            let mut answers: ProfileAnswers = veles_runtime::veles_install::templates::parse_answers(&std::fs::read_to_string(&answers)?)?;
            anyhow::ensure!(answers.profile == NamedProfile::ContainerHost, "--answers must select container-host");
            let gateway = ch::RootlessUser::system(&gateway_user, storage_root.join(&gateway_user));
            let sandbox = ch::RootlessUser::system(&sandbox_user, storage_root.join(&sandbox_user));
            let image = ch::PinnedImage::new(&gateway_image, &gateway_image_digest)?;
            let mut options = ch::ContainerHostOptions::new(ch::StorageOptions::new(storage_root), gateway, sandbox);
            options.network = Some(network);
            options.quadlets.push(ch::QuadletSpec { user:gateway_user.clone(), name:gateway_name, container:ch::ContainerSpec { description:"veles gateway; configure its runtime separately".into(), image:image.clone(), extra:Vec::new() } });
            options.images.push(ch::PullRequest { user:gateway_user.clone(), image });
            answers.host.container = Some(options);
            let request = ProfileRequest::host(answers)?;
            let plan = service.preview_profile(request.clone())?;
            println!("{}", serde_json::to_string_pretty(&plan.review())?);
            if yes {
                println!("{}", serde_json::to_string_pretty(&service.apply_profile(request, &plan.base_sig, &plan.plan_sig, &Default::default())?)?);
            }
        }
        InstallAction::ProfileHandoff { request, yes } => {
            let handoff: veles_runtime::veles_install::ProfileHandoff = serde_json::from_slice(&std::fs::read(&request)?)?;
            let request_for_review = handoff.request_for_cli();
            let plan = service.preview_profile(request_for_review)?;
            anyhow::ensure!(plan.plan_sig == handoff.plan_sig && plan.base_sig == handoff.base_sig, "profile request, target state or artifact changed after preview; refusing handoff");
            println!("{}", serde_json::to_string_pretty(&plan.review())?);
            confirm(yes)?;
            let secrets = prompt_profile_secrets(plan.review()["secrets"].as_array())?;
            println!("{}", serde_json::to_string_pretty(&veles_runtime::veles_install::privileged_handoff(&handoff, &secrets)?)?);
        }
        InstallAction::Component { name, yes } => {
            let plan = service.preview_component(&name)?;
            println!("{}", serde_json::to_string_pretty(&plan)?);
            confirm(yes)?;
            let signature = plan["plan_sig"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("artifact preview lacks its plan signature"))?;
            println!("{}", serde_json::to_string_pretty(&service.install_component(&name, Some(signature))?)?);
        }
        InstallAction::Rollback { name, yes } => {
            let plan = service.preview_rollback(&name)?;
            println!("{}", serde_json::to_string_pretty(&plan)?);
            confirm(yes)?;
            let sig = plan["plan_sig"].as_str().context("rollback preview has no plan_sig")?;
            println!("{}", serde_json::to_string_pretty(&service.rollback(&name, sig)?)?);
        }
        InstallAction::Verify => {
            let result = service.verify()?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            anyhow::ensure!(result["ok"] == serde_json::Value::Bool(true), "component integrity verification failed");
        }
        InstallAction::History => println!("{}", serde_json::to_string_pretty(&service.record()?.history)?),
        InstallAction::Serve => anyhow::bail!("the standalone installer server is not implemented; use the authenticated gateway API or the CLI"),
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    use std::io::IsTerminal;
    let cli = Cli::parse();
    // --profile sets VELES_PROFILE BEFORE anything reads config or the
    // store: profiles::active_profile consults the env first, so every
    // downstream surface agrees on the profile.
    if let Some(p) = &cli.profile {
        std::env::set_var("VELES_PROFILE", p);
    }
    // A bad name must not fall back to the default's config/state. Do
    // this before tracing, .env loading, provider resolution or the store.
    veles_core::profiles::validate_startup()?;
    // ONE tracing init for the process, chosen by the surface about to
    // run. Two call sites used to install a global subscriber under
    // different conditions, so `veles chat -q "hi"` (no -Q) panicked on
    // the second install — the whole non-quiet one-shot path was dead.
    //
    //   quiet one-shot → stderr (stdout is delegate_task's contract)
    //   JSON one-shot  → stderr (stdout is the NDJSON machine stream)
    //   TUI            → CAPTURED: ring buffer + file, nothing on stdout,
    //                    because ratatui owns the terminal and fmt()'s
    //                    default writer would scribble over the layout
    //   everything else→ stdout, as before
    // Commands whose stdout IS the product: a one-shot's reply, a ticket
    // or ticket request (pasted as one line into a chat — a log line in
    // front of it is a line someone has to trim by hand), and the
    // extension API spec (`plugins api --json` is a JSON document; the
    // in-tree example plugin's boot lines printed in front of it).
    let stream_oneshot_stdout_reserved = matches!(
        &cli.command,
        Some(Command::Chat {
            query: Some(_),
            quiet: true,
            ..
        }) | Some(Command::Chat {
            query: Some(_),
            io_json: true,
            ..
        }) | Some(Command::Ticket { .. })
            | Some(Command::Plugins {
                action: Some(PluginsAction::Api { .. })
            })
            // `veles install` prints JSON reviews and answers templates that
            // are piped into jq or saved to a file: logs go to stderr.
            | Some(Command::Install { .. })
            // `veles pool invite --text` is meant for a file or a pipe.
            | Some(Command::Pool { .. })
    );
    let interactive_tty = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    // `tui_bound` decides BOTH where the logs go and which surface
    // actually starts. It used to decide only the first: the dispatch
    // below recomputed `is_terminal()` on its own and never looked at
    // `--plain` or VELES_PLAIN at all, so a11y linear mode got the
    // ratatui TUI on a real terminal — with the log lines this very
    // match had routed to stdout scribbling over it. One predicate,
    // both decisions.
    let plain_env = std::env::var("VELES_PLAIN")
        .map(|v| v == "1")
        .unwrap_or(false);
    let tui_bound = match &cli.command {
        None => interactive_tty && !plain_env,
        Some(Command::Chat {
            query: None,
            tui: true,
            ..
        }) => true,
        // a11y linear mode: --plain (or VELES_PLAIN=1) forces the REPL
        // even on a TTY. Same surface as the non-TTY fallback.
        Some(Command::Chat {
            query: None,
            plain: true,
            ..
        }) => false,
        Some(Command::Chat {
            query: None,
            repl: true,
            ..
        }) => false,
        Some(Command::Chat { query: None, .. }) => interactive_tty && !plain_env,
        _ => false,
    };
    use veles_core::logbuf::LogMode;
    // Long-running processes keep their info lines on stdout, where a
    // service manager collects them; everything a person runs once is
    // quiet unless something is wrong (or RUST_LOG says otherwise).
    let daemon = matches!(
        &cli.command,
        Some(Command::Gateway {
            action: GatewayAction::Run { .. }
        }) | Some(Command::Signer { .. })
            | Some(Command::ModuleHostTest { .. })
            | Some(Command::ModuleSpike { .. })
    );
    veles_core::logbuf::init(if stream_oneshot_stdout_reserved {
        LogMode::Stderr
    } else if tui_bound {
        LogMode::Captured
    } else if daemon {
        LogMode::Stdout
    } else {
        LogMode::Quiet
    });
    // i18n: install the process locale BEFORE any surface prints
    // (config `tui.locale` > VELES_LOCALE > LANG/LC_ALL > en). Boot
    // paths re-call init idempotently with the loaded config; this one
    // covers light CLI commands that never boot an agent (memory
    // doctor, cron, skills...).
    {
        let cfg_locale = veles_core::config::load().ok().and_then(|c| c.tui.locale);
        let _ = veles_core::i18n::init(cfg_locale.as_deref());
    }
    // In-tree Rust plugins (Phase A): register before any surface boots
    // an agent or the gateway — see register_builtin_rust_plugins. Not
    // in the signer: it holds a decrypted signing key, and nothing in
    // its process is there that it does not need.
    if !matches!(cli.command, Some(Command::Signer { .. })) {
        veles_cli::register_builtin_rust_plugins();
    }
    // A first run lands on the wizard, not on an engine error. One-shots
    // (`chat -q`, delegate children) are not asked: they fail with the
    // engine's own "run `veles setup`" message.
    if matches!(&cli.command, None | Some(Command::Chat { query: None, .. }))
        && !veles_cli::first_run_gate(interactive_tty)?
    {
        return Ok(());
    }
    match cli.command {
        None => {
            // Bare `veles` = the TUI when attached to a terminal
            // (hermes-parity interactive chat used to be a REPL; the
            // ratatui app is the real surface now). Off a TTY (pipes,
            // ssh-one-shot) fall back to the readline REPL.
            let ov = veles_core::agent::ModelOverrides::default();
            if tui_bound {
                return veles_frontends::tui::run(false, &ov);
            }
            veles_frontends::chat::run(false, None, &ov)
        }
        Some(Command::Gui) => veles_cli::run_gui(),
        // -m / -p / -r reach the INTERACTIVE surfaces too. They were
        // accepted here, matched with `..`, and dropped: `veles chat
        // --plain -m glm-5.3` ran on the config default while --help
        // promised otherwise, and the banner and /context both reported
        // the model it was not using.
        Some(Command::Chat {
            query: None,
            tui: true,
            r#continue,
            model,
            provider,
            reasoning,
            ..
        }) => veles_frontends::tui::run(
            r#continue,
            &veles_core::agent::ModelOverrides::from_flags(
                model.as_deref(),
                provider.as_deref(),
                reasoning.as_deref(),
            ),
        ),
        Some(Command::Chat {
            query: None,
            repl: true,
            r#continue,
            resume,
            model,
            provider,
            reasoning,
            ..
        }) => veles_frontends::chat::run(
            r#continue,
            resume.as_deref(),
            &veles_core::agent::ModelOverrides::from_flags(
                model.as_deref(),
                provider.as_deref(),
                reasoning.as_deref(),
            ),
        ),
        Some(Command::Chat {
            query: None,
            r#continue,
            resume,
            model,
            provider,
            reasoning,
            ..
        }) => {
            // Default interactive chat: TUI on a real terminal, REPL
            // otherwise (piped stdin, `--plain`, VELES_PLAIN=1, or a
            // script's incantation). `tui_bound` is the one predicate
            // that already answered this for the log routing.
            let ov = veles_core::agent::ModelOverrides::from_flags(
                model.as_deref(),
                provider.as_deref(),
                reasoning.as_deref(),
            );
            if tui_bound {
                veles_frontends::tui::run(r#continue, &ov)
            } else {
                veles_frontends::chat::run(r#continue, resume.as_deref(), &ov)
            }
        }
        Some(Command::Chat {
            query: Some(q),
            model,
            provider,
            reasoning,
            quiet: _,
            r#continue,
            resume,
            io_json,
            ..
        }) if io_json => veles_frontends::cli::run_oneshot_json(
            &q,
            model.as_deref(),
            provider.as_deref(),
            reasoning.as_deref(),
            r#continue,
            resume.as_deref(),
        ),
        Some(Command::Chat {
            query: Some(q),
            model,
            provider,
            reasoning,
            quiet,
            r#continue,
            resume,
            ..
        }) => veles_frontends::cli::run_oneshot(
            &q,
            model.as_deref(),
            provider.as_deref(),
            reasoning.as_deref(),
            quiet,
            r#continue,
            resume.as_deref(),
        ),
        Some(Command::Gateway { action }) => match action {
            GatewayAction::Run { exit_when_idle } => {
                if let Some(secs) = exit_when_idle.filter(|s| *s > 0) {
                    veles_core::idle::arm(secs);
                }
                // The connectors' own options, for the All settings page
                // (`/api/config/form`): core cannot derive them.
                veles_core::settings_form::register_knobs(blob_surface()?);
                veles_cli::run_gateway()
            }
            GatewayAction::Status => veles_cli::gateway_status(),
            GatewayAction::Stop => veles_cli::gateway_stop(),
            GatewayAction::Restart => veles_cli::gateway_restart(),
            GatewayAction::Install => veles_cli::gateway_install(),
            GatewayAction::Uninstall => veles_cli::gateway_uninstall(),
        },
        Some(Command::Setup) => veles_cli::run_setup(),
        Some(Command::Install { action }) => run_install(action),
        Some(Command::Doctor {
            prefix,
            raw,
            mechanisms,
            sandbox,
            providers,
            no_probe,
        }) => veles_frontends::cli::doctor(prefix, raw, mechanisms, sandbox, providers, no_probe),
        Some(Command::Model) => veles_cli::run_model(&[]),
        Some(Command::Tcl { action }) => match action {
            TclAction::Eval { code } => veles_cli::tclrepl::run_eval(&code),
            TclAction::Repl => veles_cli::tclrepl::run_repl(),
            TclAction::Reload => veles_cli::tclrepl::run_reload(),
            TclAction::File { path } => veles_cli::tclrepl::run_file(&path),
        },
        #[cfg(unix)]
        Some(Command::ModuleSpike { check, so }) => veles_cli::modulespike::run(&check, so),
        #[cfg(not(unix))]
        Some(Command::ModuleSpike { .. }) | Some(Command::ModuleHostTest { .. }) => {
            anyhow::bail!("the module-host test harness reads /proc; not on this platform yet")
        }
        #[cfg(unix)]
        Some(Command::ModuleHostTest { case }) => {
            let rt = tokio::runtime::Runtime::new()?;
            veles_core::rt::install_core_handle(rt.handle().clone());
            let r = rt.block_on(veles_cli::modulehosttest::run(&case));
            // A runtime dropped inside its own async context panics
            // (tokio's blocking-pool shutdown); block in a helper thread.
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                rt.shutdown_timeout(std::time::Duration::from_secs(5));
                let _ = tx.send(());
            });
            let _ = rx.recv();
            r
        }
        Some(Command::Plugins { action }) => {
            use veles_frontends::cli::PluginsCmd;
            // The process-plugin verbs act on the running gateway's host;
            // the rest manage the plugin directory in this process.
            let cmd = match action {
                Some(PluginsAction::Status) => return veles_cli::procplugins::status(),
                Some(PluginsAction::Reload { target }) => {
                    return veles_cli::procplugins::control("reload", &target)
                }
                Some(PluginsAction::Stop { target }) => {
                    return veles_cli::procplugins::control("stop", &target)
                }
                Some(PluginsAction::Start { target }) => {
                    return veles_cli::procplugins::control("start", &target)
                }
                Some(PluginsAction::Unload { target }) => {
                    return veles_cli::procplugins::control("unload", &target)
                }
                Some(PluginsAction::Logs { target }) => {
                    return veles_cli::procplugins::logs(&target)
                }
                Some(PluginsAction::Rescan) => return veles_cli::procplugins::rescan(),
                Some(PluginsAction::Services) => return veles_cli::procplugins::services(),
                Some(PluginsAction::Overrides) => return veles_cli::procplugins::overrides(),
                Some(PluginsAction::Api {
                    json,
                    write,
                    stamp,
                    check,
                }) => {
                    use veles_cli::extapi::{cli, ApiCmd};
                    return cli(if json {
                        ApiCmd::Json
                    } else if write {
                        ApiCmd::Write
                    } else if stamp {
                        ApiCmd::Stamp
                    } else if check {
                        ApiCmd::Check
                    } else {
                        ApiCmd::Summary
                    });
                }
                None => None,
                Some(PluginsAction::List) => Some(PluginsCmd::List),
                Some(PluginsAction::Rollback { commit }) => Some(PluginsCmd::Rollback(commit)),
                Some(PluginsAction::New {
                    name,
                    description,
                    kind,
                }) => Some(PluginsCmd::New(name, description, kind)),
                Some(PluginsAction::Install { url, as_name }) => {
                    Some(PluginsCmd::Install(url, as_name))
                }
                Some(PluginsAction::Remove { name }) => Some(PluginsCmd::Remove(name)),
                Some(PluginsAction::InstallModule { name, file }) => {
                    Some(PluginsCmd::InstallModule(name, file))
                }
                Some(PluginsAction::Hooks) => Some(PluginsCmd::Hooks),
            };
            veles_frontends::cli::plugins(cmd)
        }
        Some(Command::Tasks { action }) => veles_frontends::cli::tasks(action.map(|a| match a {
            TasksAction::List => veles_frontends::cli::TasksCmd::List,
            TasksAction::Output { id } => veles_frontends::cli::TasksCmd::Output(id),
            TasksAction::Cancel { id } => veles_frontends::cli::TasksCmd::Cancel(id),
        })),
        Some(Command::Cron { action }) => veles_frontends::cli::cron(action.map(|a| match a {
            CronAction::List => veles_frontends::cli::CronCmd::List,
            CronAction::Add {
                schedule,
                session,
                prompt,
            } => veles_frontends::cli::CronCmd::Add {
                schedule,
                prompt: prompt.join(" "),
                session,
            },
            CronAction::Run { job_id } => veles_frontends::cli::CronCmd::Run(job_id),
            CronAction::Pause { job_id } => veles_frontends::cli::CronCmd::Pause(job_id),
            CronAction::Resume { job_id } => veles_frontends::cli::CronCmd::Resume(job_id),
            CronAction::Remove { job_id } => veles_frontends::cli::CronCmd::Remove(job_id),
            CronAction::History { job_id } => veles_frontends::cli::CronCmd::History(job_id),
        })),
        Some(Command::Triggers { action }) => {
            veles_frontends::cli::triggers(action.map(|a| match a {
                TriggersAction::List => veles_frontends::cli::TriggersCmd::List,
                TriggersAction::Add { name, spec } => {
                    veles_frontends::cli::TriggersCmd::Add { name, spec }
                }
                TriggersAction::Show { name } => veles_frontends::cli::TriggersCmd::Show(name),
                TriggersAction::Enable { name } => veles_frontends::cli::TriggersCmd::Enable(name),
                TriggersAction::Disable { name } => {
                    veles_frontends::cli::TriggersCmd::Disable(name)
                }
                TriggersAction::Rm { name } => veles_frontends::cli::TriggersCmd::Rm(name),
                TriggersAction::Log { name } => veles_frontends::cli::TriggersCmd::Log(name),
                TriggersAction::Test { name, text } => {
                    veles_frontends::cli::TriggersCmd::Test { name, text }
                }
                TriggersAction::Mask {
                    network,
                    channel,
                    events,
                } => veles_frontends::cli::TriggersCmd::Mask {
                    network,
                    channel,
                    events,
                },
                TriggersAction::Refresh => veles_frontends::cli::TriggersCmd::Refresh,
            }))
        }
        Some(Command::Backup { dir }) => veles_cli::run_backup(dir.as_deref()),
        Some(Command::Restore { dir }) => veles_cli::run_restore(&dir),
        Some(Command::Recover {
            invite,
            as_node,
            from,
            skip,
            yes,
        }) => recover_cmd(invite, as_node, from, skip, yes),
        Some(Command::Migrate) => veles_cli::run_migrate(),
        Some(Command::Tool { name, args, full }) => veles_cli::run_tool(&name, &args, full),
        Some(Command::Tools { action }) => match action.unwrap_or(ToolsAction::List) {
            ToolsAction::List => veles_frontends::cli::tools_list(),
        },
        Some(Command::Toolstats { days, tool, limit }) => {
            veles_frontends::cli::toolstats(days, tool, limit)
        }
        Some(Command::MigrateData { dry_run, only }) => {
            veles_cli::run_migrate_data(dry_run, only.as_deref())
        }
        Some(Command::ImportHermes {
            db,
            source,
            since_days,
            dry_run,
        }) => veles_cli::run_import_hermes(db.as_deref(), source.as_deref(), since_days, dry_run),
        Some(Command::Checkpoints { action }) => {
            use veles_frontends::cli::CheckpointCmd as C;
            veles_frontends::cli::checkpoints(action.map(|a| match a {
                CheckpointAction::Status => C::Status,
                CheckpointAction::List { session } => C::List(session),
                CheckpointAction::Show { id } => C::Show(id),
                CheckpointAction::Restore { id } => C::Restore(id),
                CheckpointAction::Prune => C::Prune,
            }))
        }
        Some(Command::Secret { action }) => secret_family(action),
        Some(Command::Vault { action }) => vault_family(action),
        Some(Command::Ticket { action }) => ticket_family(action),
        Some(Command::Signer { action }) => signer_family(action),
        Some(Command::Memory { action }) => {
            let s = veles_core::memory::MemoryStore::new();
            s.ensure_hierarchy()?;
            match action {
                MemoryAction::Ls { prefix } => {
                    print!("{}", veles_cli::memory_ls(&s, prefix.as_deref())?);
                    Ok(())
                }
                MemoryAction::Cat { path, rev } => {
                    let text = match rev {
                        Some(r) => s.content_at(&path, veles_cli::parse_rev(&r)?)?,
                        None => s.read_file(&path)?,
                    };
                    print!("{text}");
                    if !text.is_empty() && !text.ends_with('\n') {
                        println!();
                    }
                    Ok(())
                }
                MemoryAction::Edit { path } => {
                    veles_cli::run_memory_edit(&s, &path, &veles_cli::editor_command())
                }
                MemoryAction::Write { path, file } => {
                    let content = match file.as_deref() {
                        None | Some("-") => {
                            let mut buf = String::new();
                            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                            buf
                        }
                        Some(f) => std::fs::read_to_string(f)
                            .map_err(|e| anyhow::anyhow!("read {f}: {e}"))?,
                    };
                    if s.write_as(&path, &content, "cli")? {
                        println!("{path}: recorded");
                    } else {
                        println!("{path}: unchanged");
                    }
                    Ok(())
                }
                MemoryAction::Rm { path } => {
                    s.delete_file(&path)?;
                    println!("{path}: deleted (`veles memory log --path {path}` has it)");
                    Ok(())
                }
                MemoryAction::Mv { from, to } => {
                    s.move_file(&from, &to)?;
                    println!("{from} → {to}");
                    Ok(())
                }
                MemoryAction::Search { query } => {
                    println!("{}", s.search(&query)?);
                    Ok(())
                }
                MemoryAction::Diff { path, old, new } => {
                    let old = old.map(|r| veles_cli::parse_rev(&r)).transpose()?;
                    let new = new.map(|r| veles_cli::parse_rev(&r)).transpose()?;
                    print!("{}", s.diff(&path, old, new)?);
                    Ok(())
                }
                MemoryAction::Log { n, path } => {
                    for row in s.log_rows(path.as_deref(), n)? {
                        let mut line = veles_core::memory::log_line(&row);
                        if row.deleted {
                            line.push_str("  (deleted)");
                        }
                        println!("{line}");
                    }
                    Ok(())
                }
                MemoryAction::Restore { path, rev } => {
                    let rev = veles_cli::parse_rev(&rev)?;
                    s.restore(&path, rev)?;
                    println!("{path}: r{rev} is current again");
                    Ok(())
                }
                MemoryAction::Rollback { path, n } => {
                    s.rollback(&path, n)?;
                    println!("rolled back {path} by {n} revision(s)");
                    Ok(())
                }
                MemoryAction::Export { dir } => {
                    let n = s.export_to(std::path::Path::new(&dir))?;
                    println!("exported {n} file(s) to {dir}");
                    Ok(())
                }
                MemoryAction::Import { dir } => {
                    let landed = s.import_from(std::path::Path::new(&dir), "import")?;
                    for p in &landed {
                        println!("  {p}");
                    }
                    println!("imported {} changed file(s) from {dir}", landed.len());
                    Ok(())
                }
                MemoryAction::Tree => {
                    print!("{}", s.tree_index());
                    Ok(())
                }
                MemoryAction::Doctor => {
                    // S8: read-only memory doctor over the hierarchy.
                    print!("{}", s.doctor());
                    Ok(())
                }
                MemoryAction::Curate { resid } => {
                    println!("{}", s.curate(resid.unwrap_or(0))?);
                    Ok(())
                }
                MemoryAction::Push { peer, since } => {
                    // Resolve the peer exactly as the `a2a` tool does:
                    // a configured a2a_agents name (with its bearer) or
                    // a direct URL.
                    let cfg = veles_core::config::load()?;
                    let (url, token) =
                        if peer.starts_with("http://") || peer.starts_with("https://") {
                            (peer.clone(), None)
                        } else {
                            let p = cfg.a2a_agents.get(&peer).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "unknown peer '{peer}' — not in a2a_agents and not a URL"
                                )
                            })?;
                            let tok = if p.auth.kind == "bearer" && !p.auth.token.is_empty() {
                                Some(p.auth.token.clone())
                            } else {
                                None
                            };
                            (p.url.clone(), tok)
                        };
                    // The offer names the sender by its stable id (what a
                    // registry row is keyed on) AND its display name (what
                    // a person reading the inbox recognises).
                    let me = veles_core::node::NodeIdentity::load_or_create(
                        &veles_core::config::config_dir(),
                    )?;
                    let name = cfg.node.name.clone().unwrap_or_else(|| me.name.clone());
                    let rt = tokio::runtime::Runtime::new()?;
                    let out = rt.block_on(veles_core::memsync::push_to_peer_scoped(
                        &s,
                        &me.id,
                        &name,
                        &url,
                        token.as_deref(),
                        since,
                        &cfg.cluster.local_memory_globs,
                    ))?;
                    println!("{out}");
                    Ok(())
                }
                MemoryAction::Offer { since, output } => {
                    let cfg = veles_core::config::load()?;
                    let me = veles_core::node::NodeIdentity::load_or_create(
                        &veles_core::config::config_dir(),
                    )?;
                    let name = cfg.node.name.clone().unwrap_or_else(|| me.name.clone());
                    let (offer, withheld) = veles_core::memsync::build_offer_scoped(
                        &s,
                        &me.id,
                        &name,
                        since,
                        &cfg.cluster.local_memory_globs,
                    )?;
                    for p in &withheld {
                        eprintln!(
                            "withheld {p}: node-local memory stays on its node \
                             (local_memory_globs / reference/nodes/)"
                        );
                    }
                    let json = serde_json::to_string_pretty(&offer)?;
                    match output {
                        Some(path) => {
                            std::fs::write(&path, json)?;
                            eprintln!(
                                "wrote {} to {path}",
                                veles_core::memsync::summarize_offer(&offer)
                            );
                        }
                        None => println!("{json}"),
                    }
                    Ok(())
                }
                MemoryAction::Status => {
                    let (pending, applied) = veles_core::memsync::status_counts();
                    println!(
                        "memsync: {pending} offer(s) awaiting/held, {applied} applied file(s) archived"
                    );
                    if pending > 0 {
                        println!(
                            "  run `veles memory apply --inbox` to settle (and name conflicts)"
                        );
                    }
                    Ok(())
                }
                MemoryAction::PruneApplied { days } => {
                    let dir = veles_core::memsync::inbox_dir();
                    let removed = veles_core::memsync::prune_applied(&dir, days * 24 * 3600);
                    let (pending, applied) = veles_core::memsync::status_counts();
                    if removed > 0 {
                        println!("pruned {removed} applied file(s) older than {days} day(s)");
                    } else {
                        println!("nothing older than {days} day(s) to prune");
                    }
                    println!(
                        "memsync: {pending} offer(s) awaiting/held, {applied} applied file(s) archived"
                    );
                    Ok(())
                }
                MemoryAction::Inbox => {
                    let dir = veles_core::memsync::inbox_dir();
                    let pending = veles_core::memsync::inbox_pending(&dir)?;
                    if pending.is_empty() {
                        println!("memsync inbox empty ({})", dir.display());
                    }
                    for p in pending {
                        let name = p.file_name().unwrap_or_default().to_string_lossy();
                        match veles_core::memsync::inbox_read(&p) {
                            Ok(e) => println!(
                                "{name}  from {} ({})  via {}  {}  — {}",
                                e.offer.from_name,
                                e.offer.from_node,
                                e.peer,
                                e.received_at,
                                veles_core::memsync::summarize_offer(&e.offer)
                            ),
                            Err(err) => println!("{name}  UNREADABLE: {err:#}"),
                        }
                    }
                    Ok(())
                }
                MemoryAction::Apply { file, inbox } => {
                    // A conflict is a non-zero exit: the apply did
                    // something other than a clean fast-forward, and a
                    // cron/automation driving this must be able to tell
                    // without parsing the prose.
                    let mut conflicted = false;
                    // THIS node's local paths: a peer filters by its own
                    // globs, and `[cluster]` never replicates.
                    let local_globs = veles_core::config::load()?.cluster.local_memory_globs;
                    if inbox {
                        if file.is_some() {
                            anyhow::bail!("give a file OR --inbox, not both");
                        }
                        let applied = veles_core::memsync::apply_inbox(&s, &local_globs)?;
                        if applied.is_empty() {
                            println!("memsync inbox empty — nothing to apply");
                        }
                        for (name, report) in applied {
                            println!("{name}: {}", report.summarize());
                            // A conflict stays in the inbox: it is still
                            // owed a decision, and a re-run reports it.
                            conflicted |= !report.is_clean();
                        }
                    } else {
                        let Some(file) = file else {
                            anyhow::bail!("give an offer file (`-` for stdin) or --inbox");
                        };
                        let raw = if file == "-" {
                            use std::io::Read;
                            let mut buf = String::new();
                            std::io::stdin().read_to_string(&mut buf)?;
                            buf
                        } else {
                            std::fs::read_to_string(&file)?
                        };
                        // A bare offer (`veles memory offer`), an inbox
                        // entry, or the text of an A2A offer message.
                        let offer = veles_core::memsync::decode_message(&raw)?;
                        let report =
                            veles_core::memsync::apply_offer_scoped(&s, &offer, &local_globs)?;
                        println!("{}", report.summarize());
                        conflicted = !report.is_clean();
                    }
                    if conflicted {
                        // std::process::exit bypasses destructors, but
                        // this is the command's last act.
                        std::process::exit(2);
                    }
                    Ok(())
                }
            }
        }
        Some(Command::Configsync { action }) => configsync_family(action),
        Some(Command::Replicas { action }) => replicas_family(action),
        Some(Command::Session { action }) => session_family(action),
        Some(Command::Pool { action }) => pool_family(action),
        Some(Command::Pair { action }) => {
            // S9: owner-side pairing management against the shared
            // state.db. The gateway must be restarted (or running with
            // this store) for runtime grants to resolve — same DB file,
            // so grants created here are visible to a running gateway.
            let home = veles_core::config::veles_home();
            let store = std::sync::Arc::new(std::sync::Mutex::new(veles_core::store::Store::open(
                &home.join("state.db"),
            )?));
            veles_core::acl::set_runtime_pairing_store(store.clone());
            match action {
                PairAction::New { role, ttl_hours } => {
                    let (code, exp) =
                        veles_core::pairing::create(&store, &role, Some(ttl_hours * 3600))?;
                    println!("pairing code: {code}");
                    println!("role: {role} · expires: {exp} (unix)");
                    println!("the recipient sends: /pair {code} to the bot");
                    Ok(())
                }
                PairAction::List => {
                    let grants = veles_core::pairing::list_grants(&store);
                    if grants.is_empty() {
                        println!("no live pairing grants");
                    } else {
                        for g in grants {
                            println!("{} → {} (granted_at {})", g.identity, g.role, g.granted_at);
                        }
                    }
                    Ok(())
                }
                PairAction::Revoke { identity } => {
                    if veles_core::pairing::revoke(&store, &identity) {
                        println!("revoked: {identity}");
                    } else {
                        println!("no grant found for {identity}");
                    }
                    Ok(())
                }
            }
        }
        // The ACL, the sandboxes and the ceilings — the same engine
        // calls every chat surface makes, against the same state.db.
        // NOT via an Agent: building one to answer `veles acl list`
        // would load providers, plugins and a session for a question
        // about a rule table, which is why `commands::AclCtx` exists.
        Some(Command::Acl { ref args }) => acl_family("acl", args),
        Some(Command::Sandbox { ref args }) => acl_family("sandbox", args),
        Some(Command::Limits { ref args }) => acl_family("limits", args),
        Some(Command::Lockdown { ref args }) => acl_family("lockdown", args),
        Some(Command::Nodes { action }) => nodes_family(action),
        Some(Command::Dream) => {
            // S2 dreaming: one reflection pass (may take a minute), then
            // print where the lessons landed.
            let cfg = Arc::new(veles_core::config::load()?);
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let lessons = rt.block_on(async { veles_core::dream::dream(cfg).await })?;
            println!("{lessons}");
            if lessons != "No new durable lessons" && !lessons.is_empty() {
                let date = chrono::Local::now().format("%Y-%m-%d");
                println!("\n(lessons appended to memory reference/lessons/{date}.md — `veles memory cat reference/lessons/{date}.md`; fold them into memory or leave them as reference)");
            }
            Ok(())
        }
        Some(Command::Config { action }) => config_family(action),
        Some(Command::Skills { action }) => match action {
            SkillsAction::Install {
                url,
                name,
                overwrite,
            } => {
                println!(
                    "{}",
                    veles_core::skills::install(&url, name.as_deref(), overwrite)
                );
                Ok(())
            }
        },
        Some(Command::Version) => {
            println!("veles-agent {}", veles_core::VERSION);
            Ok(())
        }
    }
}

/// The parity pins on the AGGREGATE option surface.
///
/// Every connector pins its own keys against its own source — veles-irc and
/// veles-telegram each walk themselves, in both directions — but nothing
/// pinned the list that puts those surfaces in front of an operator.
/// `blob_surface` and `env_blob` are hand-written `extend` calls, so a third
/// connector could ship a `config_surface()` that `veles config defaults` and
/// `config fill` never show, with every test in that connector green. These
/// walk the workspace for surfaces instead of trusting a list, and one of them
/// walks it for a RENDERED behaviour rather than a key: the route footer has
/// to sit behind a `show_route` knob in every crate that formats it.
#[cfg(test)]
mod surface_parity_tests {
    use super::{blob_surface, env_blob, process_plugin_surfaces_from};
    use std::path::{Path, PathBuf};

    fn crates_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/ above veles-cli")
            .to_path_buf()
    }

    /// Every `crates/<name>/src/**/*.rs`, as (crate dir name, source text).
    fn crate_sources() -> Vec<(String, String)> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(crates_dir()).expect("crates/ exists") {
            let dir = entry.expect("readable").path();
            let src = dir.join("src");
            if !src.is_dir() {
                continue;
            }
            let name = dir
                .file_name()
                .expect("named")
                .to_string_lossy()
                .to_string();
            let mut stack = vec![src];
            while let Some(d) = stack.pop() {
                for entry in std::fs::read_dir(&d).expect("readable") {
                    let p = entry.expect("readable").path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if p.extension().is_some_and(|e| e == "rs") {
                        let text = std::fs::read_to_string(&p).unwrap_or_default();
                        out.push((name.clone(), text));
                    }
                }
            }
        }
        out
    }

    fn this_file() -> &'static str {
        include_str!("main.rs")
    }

    /// The body of `fn <name>` in this file, brace-matched.
    fn fn_body(name: &str) -> String {
        let src = this_file();
        let start = src
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("`fn {name}` is gone; the parity pin needs it"));
        let body = &src[start..];
        let mut depth = 0usize;
        for (i, ch) in body.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return body[..i + 1].to_string();
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced braces in {name}")
    }

    /// The qualified calls to `<name>()` inside `body`, exactly as written:
    /// `veles_irc::config_surface()`,
    /// `veles_core::config_surface::platform_surface()`. Walking the `ident::`
    /// chain left rather than string-matching one shape is what lets a surface
    /// live behind a module path and still count as reached.
    fn qualified_calls(body: &str, name: &str) -> Vec<String> {
        let needle = format!("{name}()");
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(rel) = body[from..].find(&needle) {
            let at = from + rel;
            let mut head = at;
            loop {
                let before = &body[..head];
                let Some(sep) = before.rfind("::") else {
                    break;
                };
                if sep + 2 != head {
                    break; // not `ident::` immediately to our left
                }
                let ident_start = before[..sep]
                    .char_indices()
                    .rev()
                    .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                head = ident_start;
            }
            out.push(body[head..at + needle.len()].to_string());
            from = at + needle.len();
        }
        out
    }

    /// A crate that lists its own keys must reach the aggregate, or its keys
    /// are invisible to `veles config defaults|fill` and `config env-defaults`
    /// — the option exists, the reader reads it, and no surface anywhere shows
    /// it. Walked, so the third connector is caught without editing this test.
    ///
    /// Mutation: delete `out.extend(veles_telegram::config_surface());` from
    /// `blob_surface` and this fails while every test inside veles-telegram
    /// still passes — they pin that crate against itself, not against the CLI.
    #[test]
    fn process_plugin_knobs_reach_config_defaults_through_describe() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fixture");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            "name = \"fixture\"\nkind = \"process\"\nentry = \"plugin\"\n",
        )
        .unwrap();
        let exe = dir.join("plugin");
        std::fs::copy(veles_core::testenv::process_plugin_echo(), &exe).unwrap();
        let knobs = process_plugin_surfaces_from(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(knobs.len(), 3);
        assert!(knobs.iter().any(|k| k.path == "platforms.echo.rooms"
            && k.shape == veles_core::config_surface::Shape::TableArray));
        let documented = knobs
            .iter()
            .find(|k| k.path == "platforms.echo.rooms.fail_start")
            .expect("the real plugin's documented knob");
        assert_eq!(
            documented.shape,
            veles_core::config_surface::Shape::Documented
        );
    }

    #[test]
    fn every_exposed_surface_reaches_the_aggregate() {
        let blob = fn_body("blob_surface");
        let env = fn_body("env_blob");
        let mut found = 0;
        for (name, src) in crate_sources() {
            let krate = name.replace('-', "_");
            // A crate that ships a process-plugin binary (a `veles-plugin-*`
            // [[bin]]) serves its config surface through the plugin's
            // `--describe` manifest, not the linked aggregate (Phase 3): the
            // gateway spawns it, the CLI does not link it. Skip the aggregate
            // assertion for it — but pin that its manifest still declares the
            // `platforms.<name>` config section the knobs describe.
            let cargo_toml = std::fs::read_to_string(crates_dir().join(&name).join("Cargo.toml"))
                .unwrap_or_default();
            let is_process_plugin = cargo_toml.contains("name = \"veles-plugin-");
            for (surface_fn, body, command) in [
                ("config_surface", &blob, "veles config defaults"),
                ("platform_surface", &blob, "veles config defaults"),
                ("env_surface", &env, "veles config env-defaults"),
            ] {
                if !src.contains(&format!("pub fn {surface_fn}(")) {
                    continue;
                }
                if is_process_plugin {
                    // Its knobs reach the user via the plugin manifest's
                    // config surface; pin the section it declares.
                    assert!(
                        src.contains(&format!("platforms.{}", name.trim_start_matches("veles-"))),
                        "{name} is a process plugin but names no `platforms.*` config section"
                    );
                    continue;
                }
                found += 1;
                let reached = qualified_calls(body, surface_fn)
                    .iter()
                    .any(|c| c.starts_with(&format!("{krate}::")));
                assert!(
                    reached,
                    "{name} exposes `{surface_fn}()` but the aggregate never calls it: \
                     its keys are missing from `{command}` and from `config fill`"
                );
            }
        }
        // core's platform_surface + the two connectors' config_surface and
        // env_surface. A walk that found nothing would pass every assertion
        // above, so the walk itself is pinned.
        assert!(
            found >= 5,
            "the walk found {found} surfaces, expected at least 5"
        );
    }

    /// And the other direction, at runtime: nothing the aggregate claims to
    /// extend may be filtered, renamed or dropped on the way in.
    #[test]
    fn the_aggregate_holds_every_part_it_extends() {
        let aggregate: Vec<String> = blob_surface()
            .unwrap()
            .into_iter()
            .map(|k| k.path)
            .collect();
        for part in [
            veles_core::config_surface::platform_surface(),
            veles_irc::config_surface(),
            veles_telegram::config_surface(),
        ] {
            for knob in part {
                assert!(
                    aggregate.contains(&knob.path),
                    "`{}` never reaches `veles config defaults`",
                    knob.path
                );
            }
        }
        let env_aggregate: Vec<String> = env_blob().into_iter().map(|k| k.name).collect();
        for part in [veles_irc::env_surface(), veles_telegram::env_surface()] {
            for var in part {
                assert!(
                    env_aggregate.contains(&var.name),
                    "`{}` never reaches `veles config env-defaults`",
                    var.name
                );
            }
        }
    }

    /// The route footer — `— provider · model` — is a rendering, and a
    /// rendering no knob can turn off is a decision the operator cannot
    /// reverse. Until 2026-09-10 both connectors appended it whenever the
    /// gateway named a provider, which is every reply: there was no way to ask
    /// for a channel without it. Every crate that formats the line must expose
    /// a `show_route` key.
    ///
    /// Walked rather than listed, so a footer added to a third surface — the
    /// desktop transcript, an A2A reply — fails here until that surface grows
    /// the knob.
    ///
    /// Mutation: rename `show_route` on either connector's surface, or drop
    /// the `cfg.show_route &&` / `display.show_route &&` guard and re-point the
    /// knob at nothing, and this fails.
    #[test]
    fn every_route_footer_renderer_is_behind_a_show_route_knob() {
        let known: [(&str, Vec<String>); 2] = [
            (
                "veles-irc",
                veles_irc::config_surface()
                    .into_iter()
                    .map(|k| k.path)
                    .collect(),
            ),
            (
                "veles-telegram",
                veles_telegram::config_surface()
                    .into_iter()
                    .map(|k| k.path)
                    .collect(),
            ),
        ];
        let mut rendered = 0;
        // Built rather than written: this test's own source is part of the
        // walk, and a literal needle here would match itself and demand a
        // `show_route` knob from veles-cli.
        let needle = format!("{} {{}} {} {{}}", '\u{2014}', '\u{b7}');
        for (name, src) in crate_sources() {
            if !src.contains(&needle) {
                continue;
            }
            rendered += 1;
            let Some((_, paths)) = known.iter().find(|(k, _)| *k == name.as_str()) else {
                panic!(
                    "{name} renders the route footer and this test knows no option surface for \
                     it: add that crate's `config_surface()` to `known`, or do not render the \
                     footer without a knob that turns it off"
                );
            };
            assert!(
                paths.iter().any(|p| p.ends_with(".show_route")),
                "{name} renders the route footer but exposes no `show_route` key: nothing an \
                 operator can write turns that line off"
            );
        }
        assert!(
            rendered >= 2,
            "the footer walk found {rendered} renderers — irc and telegram both format it, so \
             this test is no longer looking at the thing it pins"
        );
    }

    /// One knob name on two connectors has to mean one thing — the same
    /// default — unless the divergence is written down here with its reason.
    /// Written down is the operative part: `show_reasoning` differs on purpose
    /// (IRC is a public channel and a reasoning dump eats the flood budget;
    /// telegram edits its message in place), and `tool_progress` differs
    /// because IRC's default is a POSTURE (narrate in a DM, stay quiet in a
    /// shared channel — the #coven flood fix, 2026-09-16) resolved per
    /// destination at send time, while telegram keeps hermes' flat
    /// `"off" | "verbose"` string and defaults off. Those two are decisions.
    /// A third divergence is an accident, and an accident is what this fails
    /// on.
    ///
    /// Mutation: give telegram's `show_route` a default of `false` and this
    /// fails, which is the point — `show_route` must mean the same on both.
    #[test]
    fn a_knob_on_two_connectors_means_the_same_thing() {
        /// (leaf key, irc default, telegram default) — each rendered exactly
        /// as the surface renders it, quotes and all.
        const DELIBERATE: &[(&str, &str, &str)] = &[
            (
                "show_reasoning",
                "false",
                "true",
                // reason lives in the doc comment above: the surfaces carry it
                // to the operator as a note, and this list carries it to the
                // next person who wonders whether it is a bug.
            ),
            // IRC's default is the posture string `ToolProgress::surface_default()`
            // renders (see the doc comment above for why it is not a bool any
            // more); telegram's is hermes' `"off"`. Both mean "do not narrate
            // tool rounds into a public channel by default" — they just spell
            // the decision differently because IRC resolves it per destination.
            (
                "tool_progress",
                "posture (on in DMs, off in channels)",
                "\"off\"",
            ),
        ];
        let leaves = |surface: Vec<veles_core::config_surface::Knob>| {
            surface
                .into_iter()
                .map(|k| {
                    (
                        k.path.rsplit('.').next().unwrap_or(&k.path).to_string(),
                        k.default,
                    )
                })
                .collect::<Vec<(String, Option<String>)>>()
        };
        let irc = leaves(veles_irc::config_surface());
        let telegram = leaves(veles_telegram::config_surface());

        let mut shared = 0;
        for (leaf, default) in &irc {
            let Some((_, other)) = telegram.iter().find(|(l, _)| l == leaf) else {
                continue;
            };
            shared += 1;
            if default == other {
                continue;
            }
            let (a, b) = (
                default.as_deref().unwrap_or("<unset>"),
                other.as_deref().unwrap_or("<unset>"),
            );
            assert!(
                DELIBERATE
                    .iter()
                    .any(|(l, i, t)| *l == leaf.as_str() && *i == a && *t == b),
                "`{leaf}` defaults to {a} on IRC and {b} on telegram: make them agree, or write \
                 the divergence and its reason into DELIBERATE — a knob that means two things \
                 is two knobs wearing one name"
            );
        }
        // enabled, require_mention, show_reasoning, tool_progress, show_route.
        assert!(
            shared >= 5,
            "only {shared} keys are shared between the two connectors — the comparison is no \
             longer comparing the knobs it was written for"
        );
    }
}

#[cfg(test)]
mod argv_tests {
    use super::*;
    use clap::Parser;

    /// The builder lives in veles-core and the parser lives here, which
    /// is how `-q` came to be followed by a flag twice in five weeks
    /// (`39c1ed3` inserted `-m/-p/-r`, `c0c12c6` inserted `--resume`).
    /// `-q` IS `--query` and takes a VALUE, so clap refused the whole
    /// command line with exit 2 and no child ever booted — while every
    /// spawn test in delegate.rs pointed `VELES_BINARY` at a shell
    /// script and asserted on argv CONTENT, which cannot see order.
    ///
    /// This is the only test in the tree that puts the two in one room.
    fn parses(argv: Vec<String>) -> Result<Cli, clap::Error> {
        let full: Vec<String> = std::iter::once("veles".to_string()).chain(argv).collect();
        Cli::try_parse_from(full)
    }

    #[test]
    fn argv_the_real_parser_accepts() {
        // Every shape the three spawn modes can produce, including the
        // one that was broken in production: resumed child + routing.
        let shapes = [
            veles_core::delegate::child_argv("do the thing", None, None, None, None),
            veles_core::delegate::child_argv(
                "do the thing",
                Some("20260101_000000_aaaaaa"),
                None,
                None,
                None,
            ),
            veles_core::delegate::child_argv(
                "do the thing",
                Some("20260101_000000_aaaaaa"),
                Some("m1"),
                Some("p1"),
                Some("high"),
            ),
            veles_core::delegate::child_argv("do the thing", None, Some("m1"), None, None),
        ];
        for argv in shapes {
            let cli = parses(argv.clone())
                .unwrap_or_else(|e| panic!("the real parser refused {argv:?}: {e}"));
            match cli.command {
                Some(Command::Chat {
                    query,
                    quiet,
                    resume,
                    model,
                    ..
                }) => {
                    assert_eq!(
                        query.as_deref(),
                        Some("do the thing"),
                        "-q must land on the PROMPT, not on a flag: {argv:?}"
                    );
                    assert!(quiet, "a child is always -Q: {argv:?}");
                    // …and the flags that were eating the prompt still
                    // arrive, so the fix is an ORDER change and not a
                    // dropped argument.
                    if argv.iter().any(|a| a == "--resume") {
                        assert_eq!(resume.as_deref(), Some("20260101_000000_aaaaaa"));
                    }
                    if argv.iter().any(|a| a == "-m") {
                        assert_eq!(model.as_deref(), Some("m1"));
                    }
                }
                _ => panic!("not a chat command: {argv:?}"),
            }
        }
    }

    /// B4: `veles tool` escalates to the session registry ONLY through
    /// the explicit `--full` flag — the default stays base_registry
    /// (no delegate_task, no cronjob), which is what the two-gateway
    /// check's placement driver needs and what the execute_code
    /// harness must never get by accident.
    ///
    /// `Command` has no `Debug`, so the panics name the ARGV (the same
    /// way `argv_the_real_parser_accepts` does) rather than the parse.
    #[test]
    fn tool_full_is_an_explicit_flag_and_the_default_stays_base() {
        let with_full = vec![
            "tool".to_string(),
            "--name".into(),
            "delegate_task".into(),
            "--args".into(),
            "{}".into(),
            "--full".into(),
        ];
        match parses(with_full.clone())
            .unwrap_or_else(|e| panic!("--full was refused: {e}"))
            .command
        {
            Some(Command::Tool { name, full, .. }) => {
                assert_eq!(name, "delegate_task", "{with_full:?}");
                assert!(full, "--full must set the flag: {with_full:?}");
            }
            _ => panic!("not a tool command: {with_full:?}"),
        }
        let bare = vec![
            "tool".to_string(),
            "--name".into(),
            "read_file".into(),
            "--args".into(),
            "{}".into(),
        ];
        match parses(bare.clone())
            .unwrap_or_else(|e| panic!("bare tool was refused: {e}"))
            .command
        {
            Some(Command::Tool { full, .. }) => {
                assert!(!full, "the default is the base registry: {bare:?}");
            }
            _ => panic!("not a tool command: {bare:?}"),
        }
    }
}
