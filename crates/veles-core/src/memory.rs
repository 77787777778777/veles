//! Memory (S1, Letta MemFS-inspired, no-cloud).
//!
//! Memory is a set of paths, each with a history of contents, held in
//! `memory_revisions` in state.db. There are no files: the database is
//! the store, and every surface reads and writes it through this type.
//!
//!   system/core.md  — ALWAYS injected into the system prompt (agent notes)
//!   system/user.md  — ALWAYS injected (who the user is)
//!   reference/…     — signposted: the tree index is in the prompt, a
//!   projects/…        body is loaded on demand via the memory tool
//!
//! ## Versioning: SQLite, not git
//!
//! Every mutation lands as one row in `memory_revisions`, inside a
//! transaction — history, rollback (`veles memory log|rollback|restore`).
//!
//! This WAS a git repo driven by shelling out, and the change is not
//! cosmetic. That design wrote the file and then ran `git add && git
//! commit`: two steps with a gap, and versions went missing in the gap.
//! A write would land on disk while its commit silently did not happen,
//! so memory's history diverged from memory itself. It could not report
//! the problem either — `git commit` exits non-zero for "nothing to
//! commit", indistinguishable from a real failure, so the code ignored
//! the result. It reproduced about one full-suite run in ten and
//! resisted two attempted fixes (see the git log for the measurements).
//!
//! git was only ever used here as a two-function versioned KV store —
//! list revisions, fetch content at a revision — over three files and
//! 404 KB, while SQLite was already open in the same process. An INSERT
//! inside a transaction IS the commit point, so "written but
//! unrecorded" is not a reachable state.
//!
//! ## No more memory.d (2026-09-26)
//!
//! Until 2026-09-26 every revision was also MIRRORED to
//! `<data>/memory.d/*.md` (and the two system files to flat
//! `MEMORY.md`/`USER.md`), and a file that differed from the newest
//! revision was absorbed as a hand edit. That was a second copy of the
//! truth with its own failure mode: a stale mirror read back as an
//! "edit" undid the revision it was stale against (memsync hit exactly
//! that). The mirror is gone. Editing, listing, searching, diffing,
//! export and import are commands (`veles memory …`), the agent's
//! `memory` tool, the TUI picker and the desktop panel — all over this
//! table. [`MemoryStore::retire_disk_tree`] folds a leftover tree in
//! once and removes it.
//!
//! ## Deletion is a revision
//!
//! Deleting a path records a revision with EMPTY content (`source =
//! 'delete'`); a path whose newest content is empty is absent. Nothing
//! is destroyed — `restore` brings it back — and it needs no schema
//! change, so it replicates through memsync like any other revision.
//! The two system files always exist; emptying one is just emptying it.
//!
//! No cloud, no push — this is the laptop-local MemFS.

use crate::config::veles_home;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

pub const DELIM: &str = "\n§\n";

/// The old generated directory index. Derived data — never versioned;
/// a peer or a leftover tree that carries one is ignored.
const TREE_INDEX: &str = "TREE.md";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StoreKind {
    Memory,
    User,
}

impl StoreKind {
    /// The flat file of the pre-hierarchy layout, folded in (once) by
    /// [`MemoryStore::retire_disk_tree`].
    fn legacy_file(self) -> &'static str {
        match self {
            StoreKind::Memory => "MEMORY.md",
            StoreKind::User => "USER.md",
        }
    }
    /// Where this store lives in the hierarchy. Public so the desktop's
    /// history/rollback surface can name the path it is rolling back,
    /// rather than re-deriving it.
    pub fn hier_path(self) -> &'static str {
        match self {
            StoreKind::Memory => "system/core.md",
            StoreKind::User => "system/user.md",
        }
    }
}

/// The two paths that always exist, even empty.
fn always_exists(rel: &str) -> bool {
    rel == StoreKind::Memory.hier_path() || rel == StoreKind::User.hier_path()
}

/// One revision row as `revisions_since` returns it:
/// `(rev_id, path, content, sha256, created_at)`. A named alias because
/// the bare 5-tuple hit clippy's type-complexity ceiling — and because
/// positional field order is exactly the class of thing the
/// usage-ledger bugs keep teaching us not to leave unlabelled.
pub type RevisionRow = (i64, String, String, String, String);

/// One live memory file: its newest revision.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FileRow {
    pub path: String,
    pub chars: usize,
    pub rev: i64,
    pub at: String,
    pub source: String,
    pub sha256: String,
}

/// One entry of the revision log.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LogRow {
    pub rev: i64,
    pub path: String,
    pub source: String,
    pub note: String,
    pub at: String,
    pub chars: usize,
    /// A tombstone: this revision deleted the path.
    pub deleted: bool,
}

/// A write refused because the path moved on since the caller read it
/// — the edit-in-$EDITOR and desktop-save race. Downcast an error to
/// this to tell it from a real failure (the desktop answers 409).
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub path: String,
    /// The sha the caller must re-read against; None = the path is absent.
    pub current_sha: Option<String>,
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.current_sha {
            Some(_) => write!(
                f,
                "{} changed since it was read — reload it and edit again",
                self.path
            ),
            None => write!(f, "{} was deleted since it was read", self.path),
        }
    }
}

impl std::error::Error for Conflict {}

/// What [`MemoryStore::retire_disk_tree`] did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RetireReport {
    /// Paths recorded from disk because the database did not have them.
    pub folded: Vec<String>,
    /// Whether the disk copies were removed.
    pub removed: bool,
    /// Why they were kept, when they were.
    pub kept: Vec<String>,
}

pub struct MemoryStore {
    home: PathBuf,
    limits: crate::config::MemorySection,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

/// This process's memory-write turn — see [`MemoryStore::write_conn`].
static WRITE_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl MemoryStore {
    pub fn new() -> Self {
        Self::at(veles_home())
    }

    /// A store rooted at an EXPLICIT home, independent of the ambient
    /// `VELES_HOME`.
    ///
    /// `new()` reads that env var, which is process-global — so any code
    /// building a MemoryStore (or an Agent, which bootstraps one) lands
    /// in whatever home is currently set. In a parallel test run that is
    /// whichever scratch directory some other test happens to have
    /// configured, and the two stores then trample each other. Tests
    /// that assert on a store's contents should pin it with `at()` so
    /// they cannot be reached that way at all.
    pub fn at(home: PathBuf) -> Self {
        Self {
            home,
            limits: crate::config::load().map(|c| c.memory).unwrap_or_default(),
        }
    }

    fn limit(&self, kind: StoreKind) -> usize {
        (match kind {
            StoreKind::Memory => self.limits.memory_char_limit,
            StoreKind::User => self.limits.user_char_limit,
        }) as usize
    }

    /// Configured char limits (introspection).
    pub fn limits(&self) -> &crate::config::MemorySection {
        &self.limits
    }

    // ── revision store (SQLite, transactional) ──────────────────────
    //
    // Connections are opened per operation rather than held: rusqlite's
    // Connection is !Sync, MemoryStore is used from async contexts, and
    // on a database this size the open costs nothing measurable.

    fn db_path(&self) -> PathBuf {
        self.home.join("state.db")
    }

    /// The database file this store reads and writes — what a message
    /// about "this store" should name.
    pub fn db_file(&self) -> PathBuf {
        self.db_path()
    }

    /// The B1.5 never-fold verdict: may an offer be folded into this
    /// store? TRUE for a file this node owns or would adopt (fresh,
    /// memory-only — the same cases Store::open adopts), FALSE for a
    /// foreign file (another program's DB). A shipped veles REPLICA
    /// carries the marker too, so it is NOT caught here — B5 needs an
    /// owner check. Read-only and side-effect free: it must not adopt
    /// the marker itself, only measure it.
    pub fn is_sync_fold_target(&self) -> bool {
        let Ok(conn) = rusqlite::Connection::open_with_flags(
            self.db_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) else {
            // No file (or unopenable): a fresh store-to-be — adoptable.
            return true;
        };
        let had_messages: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='messages'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(1)
            > 0;
        if !had_messages {
            // Memory-only or pre-store file: adoptable (Store::open's
            // "memory created state.db first" rule).
            return true;
        }
        conn.query_row(
            "SELECT value FROM meta WHERE key='veles_schema_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .map(|v| v == "1")
        .unwrap_or(false)
    }

    fn db(&self) -> Result<rusqlite::Connection> {
        let path = self.db_path();
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        // WAL + a busy timeout: the gateway holds its own connection to
        // this file, and a memory write must wait rather than fail.
        //
        // THE COLD RACE (measured on ext4, 8 threads on a fresh file;
        // tmpfs is too fast to show it, which is why it only ever
        // surfaced in the nix sandbox under `--test-threads=16`). Every
        // memory write opens a FRESH connection, and the first one
        // converts the file to WAL. A connection that runs the
        // conversion holds a read lock afterwards, and its next
        // statement — `BEGIN IMMEDIATE` below — is a lock PROMOTION,
        // which SQLite refuses IMMEDIATELY for deadlock avoidance: the
        // busy handler is not consulted, so busy_timeout cannot save it
        // and the write fails "database is locked" no matter how patient
        // the connection is. Instrumented, the failure is always at the
        // first write's BEGIN IMMEDIATE, after the pragma answered "wal".
        //
        // So the conversion runs on a connection of its OWN, which is
        // dropped before any write is attempted, and the returned
        // connection has no promotion pending: its BEGIN IMMEDIATE waits
        // on the busy handler like any other writer.
        //
        // AND THE FAST PATH HAS TO COME FIRST, because `db()` runs on
        // every write, so anything it does costs per write. Once the file
        // is WAL and the table exists there is nothing left to set up, and
        // one read-only lookup of sqlite_master says so — no write lock, no
        // fsync. The first version of this fix bootstrapped the schema
        // unconditionally inside write_tx, which made every memory write
        // take TWO write transactions: correct, and measured 40-83s for
        // 2048 writes where this takes ~1.3s.
        let conn = Self::open_conn(&path)?;
        if Self::schema_present(&conn)? && Self::is_wal(&conn) {
            return Ok(conn);
        }
        // COLD: the file is new, or not WAL, or has no table. Convert on a
        // connection of its own and drop it, then create the schema on a
        // fresh one.
        drop(conn);
        self.ensure_wal(&path)?;
        let mut last: Option<anyhow::Error> = None;
        // Retry the bootstrap on a FRESH connection if it loses a lock
        // race. A fresh connection cannot have a promotion pending, which
        // is the whole point: retrying on the same one would fail the same
        // way, instantly, every time.
        for attempt in 0..20u32 {
            let mut conn = Self::open_conn(&path)?;
            match Self::bootstrap_schema(&mut conn) {
                Ok(()) => return Ok(conn),
                Err(e) if Self::is_busy_anyhow(&e) && attempt + 1 < 20 => {
                    last = Some(e);
                    drop(conn);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("memory store schema bootstrap failed")))
    }

    fn open_conn(path: &std::path::Path) -> Result<rusqlite::Connection> {
        let conn =
            rusqlite::Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    /// Does `memory_revisions` exist? A read-only lookup of
    /// sqlite_master in autocommit mode, so it takes no write lock and
    /// leaves no read snapshot on the connection that follows it — the
    /// hazard `db()` exists to avoid is a lingering lock making the next
    /// BEGIN IMMEDIATE an unretryable promotion.
    fn schema_present(conn: &rusqlite::Connection) -> Result<bool> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memory_revisions'",
            [],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Is the database already in WAL mode? Asked of the connection, after
    /// `schema_present` has read the file: the query form of the pragma
    /// changes nothing and takes no lock. NOT by reading the header with
    /// `File::open`: closing that descriptor drops every POSIX lock this
    /// process holds on the file, the gateway's long-lived connection's
    /// included, and a process that closes the database next believes it
    /// is the last one and deletes the live -wal and -shm
    /// (`a_memory_write_keeps_the_process_s_lock_on_the_database`). An
    /// error answers false, sending the caller through the conversion
    /// path.
    fn is_wal(conn: &rusqlite::Connection) -> bool {
        conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .is_ok_and(|m| m.eq_ignore_ascii_case("wal"))
    }

    fn is_busy(e: &rusqlite::Error) -> bool {
        matches!(
            e,
            rusqlite::Error::SqliteFailure(err, _)
                if err.code == rusqlite::ErrorCode::DatabaseBusy
        )
    }

    /// The same question through an anyhow wrapper: `bootstrap_schema`
    /// returns [`Result`], so its error has to be unwrapped to reach the
    /// rusqlite one inside.
    fn is_busy_anyhow(e: &anyhow::Error) -> bool {
        e.downcast_ref::<rusqlite::Error>()
            .is_some_and(Self::is_busy)
    }

    /// Convert the file to WAL on a connection that is then DROPPED. The
    /// caller must not reuse this connection to write: see [`Self::db`].
    /// Best-effort — a filesystem that cannot do WAL (some network mounts
    /// answer another mode without erroring) proceeds in rollback mode,
    /// which is correct if slower, rather than failing every write.
    fn ensure_wal(&self, path: &std::path::Path) -> Result<()> {
        for attempt in 0..200u32 {
            let conn = rusqlite::Connection::open(path)
                .with_context(|| format!("open {}", path.display()))?;
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
            match conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0)) {
                Ok(m) if m.eq_ignore_ascii_case("wal") => return Ok(()),
                // Lost the conversion race (still "delete") or the pragma
                // hit BUSY: drop this connection and try a fresh one.
                Ok(_) => {}
                Err(e) if Self::is_busy(&e) => {}
                // Cannot do WAL here: proceed in whatever mode it is.
                Err(_) => return Ok(()),
            }
            if attempt + 1 >= 200 {
                return Ok(()); // best effort; the bootstrap below still runs
            }
            drop(conn);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Ok(())
    }

    /// Memory owns this table, and ensuring it here means MemoryStore
    /// works even if Store::open never ran. Inside an IMMEDIATE
    /// transaction (write_tx), so a race on the DDL waits on the busy
    /// handler instead of failing. On a warm database every statement is
    /// a no-op and this costs nothing.
    fn bootstrap_schema(conn: &mut rusqlite::Connection) -> Result<()> {
        let ddl = Self::write_tx(conn)?;
        ddl.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_revisions (
                 rev_id     INTEGER PRIMARY KEY AUTOINCREMENT,
                 path       TEXT NOT NULL,
                 content    TEXT NOT NULL,
                 sha256     TEXT NOT NULL,
                 source     TEXT NOT NULL,
                 note       TEXT,
                 created_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE INDEX IF NOT EXISTS memory_rev_path
                 ON memory_revisions(path, rev_id DESC);",
        )?;
        ddl.commit()?;
        Ok(())
    }

    fn sha(content: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(content.as_bytes());
        format!("{:x}", h.finalize())
    }

    /// The content hash, exposed for the replication layer: a receiver
    /// must hash incoming content the SAME way the store does, or the
    /// ancestor matching in `memsync` diverges from what `record`
    /// dedupes on. Public but not re-implemented — one hash, one place.
    pub fn content_sha(content: &str) -> String {
        Self::sha(content)
    }

    /// The newest revision of `rel`, if any: `(rev_id, content, sha)`.
    /// A tombstone is returned as what it is — empty content.
    fn latest(conn: &rusqlite::Connection, rel: &str) -> Option<(i64, String, String)> {
        conn.query_row(
            "SELECT rev_id, content, sha256 FROM memory_revisions
             WHERE path = ?1 ORDER BY rev_id DESC LIMIT 1",
            rusqlite::params![rel],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok()
    }

    /// The newest revision of `rel` if the path is LIVE — present and
    /// not deleted (a system file is always live).
    fn live_latest(conn: &rusqlite::Connection, rel: &str) -> Option<(i64, String, String)> {
        Self::latest(conn, rel).filter(|(_, content, _)| always_exists(rel) || !content.is_empty())
    }

    /// A connection to write on, with THIS PROCESS's write turn held.
    ///
    /// SQLite's busy handler POLLS: a writer that finds the lock taken
    /// sleeps (1, 2, 5 … 100 ms) and looks again, and nothing queues it.
    /// Writers that come straight back for the lock keep winning between
    /// a sleeper's looks, so in a burst one can starve past busy_timeout.
    /// Measured in the gate on 2026-09-30, `concurrent_writers_all_land`
    /// on the real disk: three threads failed their FIRST write after
    /// 103 s — all 20 bootstrap retries, each a full 5 s busy_timeout —
    /// and a fourth failed write 4 after exactly 5 s, while the other four
    /// wrote on. A mutex queues where the busy handler polls, so writers
    /// in one process cannot starve each other; busy_timeout is left with
    /// the other PROCESSES (the CLI, delegate children), where contention
    /// is an occasional overlap rather than a burst. The turn is held
    /// until the caller's transaction is done: bind it first, so it drops
    /// after the connection.
    fn write_conn(&self) -> Result<(std::sync::MutexGuard<'static, ()>, rusqlite::Connection)> {
        let turn = WRITE_TURN.lock().unwrap_or_else(|p| p.into_inner());
        Ok((turn, self.db()?))
    }

    /// A write transaction that takes the write lock at BEGIN.
    ///
    /// Every memory write READS first (the tip sha, liveness) and then
    /// inserts. In a deferred transaction on a WAL database that upgrade
    /// fails outright — SQLITE_BUSY_SNAPSHOT, "cannot promote read
    /// transaction", which busy_timeout does not retry — whenever another
    /// connection wrote in between: the gateway and `veles memory edit`,
    /// or two parallel tests on one home. IMMEDIATE waits for the lock
    /// (busy_timeout applies) and then reads a snapshot nobody can move.
    fn write_tx(conn: &mut rusqlite::Connection) -> Result<rusqlite::Transaction<'_>> {
        Ok(conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?)
    }

    /// Insert a revision inside `tx` unless the content is byte-identical
    /// to the path's newest one. Returns whether a revision was written.
    ///
    /// The INSERT is the commit point. Content is compared by hash, not
    /// by size and mtime, which is what the git version got wrong.
    fn insert_if_changed(
        tx: &rusqlite::Transaction,
        rel: &str,
        content: &str,
        source: &str,
        note: &str,
    ) -> Result<bool> {
        let sha = Self::sha(content);
        let unchanged = tx
            .query_row(
                "SELECT sha256 FROM memory_revisions
                 WHERE path = ?1 ORDER BY rev_id DESC LIMIT 1",
                rusqlite::params![rel],
                |r| r.get::<_, String>(0),
            )
            .map(|prev| prev == sha)
            .unwrap_or(false);
        if unchanged {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO memory_revisions (path, content, sha256, source, note)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![rel, content, sha, source, note],
        )?;
        Ok(true)
    }

    /// Record a new revision unless the content is byte-identical to the
    /// current one. Returns whether a revision was written.
    fn record(&self, rel: &str, content: &str, source: &str, note: &str) -> Result<bool> {
        // TREE.md was DERIVED — regenerated from the directory on every
        // write. Versioning it would double every write's history and
        // bury the actual content changes.
        if rel == TREE_INDEX {
            return Ok(false);
        }
        let (_turn, mut conn) = self.write_conn()?;
        let tx = Self::write_tx(&mut conn)?;
        let landed = Self::insert_if_changed(&tx, rel, content, source, note)?;
        tx.commit()?;
        Ok(landed)
    }

    /// Every revision newer than `after_rev_id` (0 = all), oldest first
    /// — the replication order for `memsync`. Each row is
    /// `(rev_id, path, content, sha256, created_at)`.
    ///
    /// Read-only against `memory_revisions`, which this store owns.
    /// Used by the Plan B memory push: a node bundles what changed
    /// since a cursor and a peer applies the bundle in order, so the
    /// two stores converge on the same history rather than trading
    /// whole files.
    pub fn revisions_since(&self, after_rev_id: i64) -> Result<Vec<RevisionRow>> {
        let conn = self.db()?;
        let mut st = conn.prepare(
            "SELECT rev_id, path, content, sha256, created_at
             FROM memory_revisions WHERE rev_id > ?1
             ORDER BY rev_id ASC",
        )?;
        let rows = st
            .query_map(rusqlite::params![after_rev_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The highest rev_id in the store — the cursor a peer reports back
    /// so the next push is incremental. 0 on an empty store.
    pub fn latest_rev_id(&self) -> Result<i64> {
        let conn = self.db()?;
        let v: i64 = conn.query_row(
            "SELECT COALESCE(MAX(rev_id), 0) FROM memory_revisions",
            [],
            |r| r.get(0),
        )?;
        Ok(v)
    }

    /// Apply one revision arriving from a peer, exactly as `write_file`
    /// would — same path rule, same dedupe-by-hash — but tagged
    /// `source = 'sync'` so the replication is visible in `memory log`
    /// rather than masquerading as a local write. Returns whether a new
    /// revision landed (false = the peer's content was already newest).
    /// A peer's deletion arrives as an empty revision and deletes here.
    pub fn record_from_peer(&self, rel: &str, content: &str, note: &str) -> Result<bool> {
        let rel = Self::check_rel(rel)?;
        self.record(rel, content, "sync", note)
    }

    /// Every sha256 recorded for `rel`, oldest first — the path's own
    /// history, which is what "does the receiver already have this
    /// state" is asked against. One indexed query per path, not a scan
    /// of the whole revision table.
    pub fn path_shas(&self, rel: &str) -> Result<Vec<String>> {
        let conn = self.db()?;
        let mut st = conn
            .prepare("SELECT sha256 FROM memory_revisions WHERE path = ?1 ORDER BY rev_id ASC")?;
        let rows = st
            .query_map(rusqlite::params![rel], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The newest content recorded for `rel`, if any (a tombstone is
    /// `Some("")` — the replication layer compares histories, not
    /// liveness).
    pub fn latest_content(&self, rel: &str) -> Option<String> {
        let conn = self.db().ok()?;
        Self::latest(&conn, rel).map(|(_, content, _)| content)
    }

    /// The sha of `rel`'s newest revision AT OR BEFORE `rev_id` — what
    /// the path looked like at a sync cursor. This is the BASE an
    /// incremental offer carries, so a receiver whose tip is that state
    /// can fast-forward even though the bundle omits it.
    pub fn sha_at(&self, rel: &str, rev_id: i64) -> Option<String> {
        let conn = self.db().ok()?;
        conn.query_row(
            "SELECT sha256 FROM memory_revisions
             WHERE path = ?1 AND rev_id <= ?2 ORDER BY rev_id DESC LIMIT 1",
            rusqlite::params![rel, rev_id],
            |r| r.get::<_, String>(0),
        )
        .ok()
    }

    /// Current content of a live `rel`.
    fn current(&self, rel: &str) -> Option<String> {
        let conn = self.db().ok()?;
        Self::live_latest(&conn, rel).map(|(_, content, _)| content)
    }

    /// Every live path with its newest revision, sorted by path, and its
    /// content — the one query `list`, `tree_index`, `search`, `doctor`
    /// and `export_to` share.
    fn live_files(&self) -> Result<Vec<(FileRow, String)>> {
        let conn = self.db()?;
        let mut st = conn.prepare(
            "SELECT r.path, r.content, r.rev_id, r.created_at, r.source, r.sha256
             FROM memory_revisions r
             JOIN (SELECT path, MAX(rev_id) AS top FROM memory_revisions GROUP BY path) t
               ON r.path = t.path AND r.rev_id = t.top
             ORDER BY r.path",
        )?;
        let rows = st
            .query_map([], |r| {
                let path: String = r.get(0)?;
                let content: String = r.get(1)?;
                Ok((
                    FileRow {
                        chars: content.chars().count(),
                        path,
                        rev: r.get(2)?,
                        at: r.get(3)?,
                        source: r.get(4)?,
                        sha256: r.get(5)?,
                    },
                    content,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter(|(f, content)| {
                f.path != TREE_INDEX && (always_exists(&f.path) || !content.is_empty())
            })
            .collect())
    }

    /// Every live memory file, sorted by path.
    pub fn list(&self) -> Result<Vec<FileRow>> {
        Ok(self.live_files()?.into_iter().map(|(f, _)| f).collect())
    }

    /// The live file `rel` with its content, or None when it is absent.
    pub fn file(&self, rel: &str) -> Result<Option<(FileRow, String)>> {
        let rel = Self::check_rel(rel)?;
        Ok(self.live_files()?.into_iter().find(|(f, _)| f.path == rel))
    }

    /// Ensure the store is usable: the table exists, and anything the
    /// file era left on disk is folded in and removed. Idempotent — once
    /// nothing is left on disk this is one `CREATE TABLE IF NOT EXISTS`.
    pub fn ensure_hierarchy(&self) -> Result<()> {
        self.db()?;
        let report = self.retire_disk_tree()?;
        if !report.folded.is_empty() {
            tracing::info!(
                "memory: folded {} file(s) from the old memory.d into the database: {}",
                report.folded.len(),
                report.folded.join(", ")
            );
        }
        if report.removed {
            tracing::info!("memory: removed the old on-disk copies; the database is the store");
        }
        for why in &report.kept {
            tracing::warn!("memory: kept the old on-disk copies: {why}");
        }
        Ok(())
    }

    /// Fold what the file era left under `root` — `memory.d/**/*.md` and
    /// the flat `MEMORY.md`/`USER.md` — into the revision table, WITHOUT
    /// touching the files. Same rules the mirror had while it existed:
    /// a tree file that differs from the newest revision is newer (it was
    /// a hand edit), a flat file only fills a system path that has no
    /// history at all. Empty files are never folded — empty content is a
    /// deletion now, and an empty file is likelier a truncation than an
    /// intent. Returns the paths recorded.
    ///
    /// `restore_from` runs this against a backup that predates the
    /// database; [`Self::retire_disk_tree`] runs it against the home.
    pub fn fold_legacy_files(&self, root: &Path) -> Result<Vec<String>> {
        let mut folded = Vec::new();
        for (rel, file) in Self::legacy_tree_files(&root.join("memory.d"))? {
            let Ok(content) = std::fs::read_to_string(&file) else {
                continue;
            };
            if content.is_empty() || Self::check_rel(&rel).is_err() {
                continue;
            }
            if self.record(&rel, &content, "migration", "folded in from memory.d")? {
                folded.push(rel);
            }
        }
        for kind in [StoreKind::Memory, StoreKind::User] {
            let rel = kind.hier_path();
            let has_history = !self.path_shas(rel)?.is_empty();
            if has_history {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(root.join(kind.legacy_file())) {
                if !content.is_empty()
                    && self.record(rel, &content, "migration", "folded in from the flat file")?
                {
                    folded.push(rel.to_string());
                }
            }
        }
        Ok(folded)
    }

    /// The `.md` files under an old memory.d, as (relative path, file),
    /// sorted — TREE.md excluded, it was derived.
    fn legacy_tree_files(tree: &Path) -> Result<Vec<(String, PathBuf)>> {
        let mut out = Vec::new();
        if !tree.is_dir() {
            return Ok(out);
        }
        let mut stack = vec![tree.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d)?.flatten() {
                let p = e.path();
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() {
                    if e.file_name() != ".git" {
                        stack.push(p);
                    }
                } else if ft.is_file() && p.extension().and_then(|x| x.to_str()) == Some("md") {
                    let rel = p.strip_prefix(tree)?.to_string_lossy().replace('\\', "/");
                    if rel != TREE_INDEX {
                        out.push((rel, p));
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// One-time retirement of the file era: fold `<home>/memory.d` and
    /// the flat `MEMORY.md`/`USER.md` into the database, VERIFY, and only
    /// then remove them.
    ///
    /// Verification is per file, after the fold: every tree file must be
    /// the newest revision of its path (or empty), and every flat file's
    /// content must be somewhere in its path's history. Anything that is
    /// not — or a tree holding things that are not `.md` files, or a
    /// `.git` directory whose history the database never imported —
    /// keeps everything on disk and says why. A no-op once they are gone.
    pub fn retire_disk_tree(&self) -> Result<RetireReport> {
        let tree = self.home.join("memory.d");
        let flats: Vec<(StoreKind, PathBuf)> = [StoreKind::Memory, StoreKind::User]
            .into_iter()
            .map(|k| (k, self.home.join(k.legacy_file())))
            .filter(|(_, p)| p.is_file())
            .collect();
        if !tree.exists() && flats.is_empty() {
            return Ok(RetireReport::default());
        }
        let mut report = RetireReport {
            folded: self.fold_legacy_files(&self.home)?,
            ..Default::default()
        };

        let conn = self.db()?;
        let files = Self::legacy_tree_files(&tree)?;
        for (rel, file) in &files {
            let content = std::fs::read_to_string(file)
                .with_context(|| format!("re-read {}", file.display()))?;
            if content.is_empty() {
                continue;
            }
            let newest = Self::latest(&conn, rel).map(|(_, _, sha)| sha);
            if newest.as_deref() != Some(Self::sha(&content).as_str()) {
                report.kept.push(format!(
                    "memory.d/{rel} is not the newest revision of {rel}"
                ));
            }
        }
        if tree.is_dir() {
            let mut stack = vec![tree.clone()];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d)?.flatten() {
                    let p = e.path();
                    let ft = e.file_type()?;
                    if ft.is_dir() && e.file_name() == ".git" {
                        report.kept.push(format!(
                            "{} holds git history the database never imported — check it, \
                             then remove it by hand",
                            p.display()
                        ));
                    } else if ft.is_dir() {
                        stack.push(p);
                    } else if p.extension().and_then(|x| x.to_str()) != Some("md") {
                        report
                            .kept
                            .push(format!("{} is not a memory file", p.display()));
                    }
                }
            }
        }
        for (kind, file) in &flats {
            let content = std::fs::read_to_string(file)
                .with_context(|| format!("re-read {}", file.display()))?;
            if content.is_empty() {
                continue;
            }
            let history = self.path_shas(kind.hier_path())?;
            if !history.contains(&Self::sha(&content)) {
                report.kept.push(format!(
                    "{} matches no revision of {}",
                    file.display(),
                    kind.hier_path()
                ));
            }
        }
        if !report.kept.is_empty() {
            return Ok(report);
        }
        if tree.exists() {
            std::fs::remove_dir_all(&tree).with_context(|| format!("remove {}", tree.display()))?;
        }
        for (_, file) in &flats {
            std::fs::remove_file(file).with_context(|| format!("remove {}", file.display()))?;
        }
        report.removed = true;
        Ok(report)
    }

    // ── read/write paths ─────────────────────────────────────────────

    /// Raw text of an ALWAYS-IN-CONTEXT store.
    pub fn read(&self, kind: StoreKind) -> String {
        self.current(kind.hier_path()).unwrap_or_default()
    }

    pub fn entries(&self, kind: StoreKind) -> Vec<String> {
        let raw = self.read(kind);
        if raw.trim().is_empty() {
            return Vec::new();
        }
        raw.split(DELIM).map(|s| s.to_string()).collect()
    }

    pub fn write(&self, kind: StoreKind, entries: &[String]) -> Result<()> {
        let rel = kind.hier_path();
        // The revision is the write: if this returns Err nothing was
        // recorded, and there is no second copy to disagree with it.
        self.record(rel, &entries.join(DELIM), "agent", &format!("update {rel}"))?;
        Ok(())
    }

    /// Case-insensitive substring search across every memory file,
    /// grep-style (`path:line: text`). The signpost follow-up for the
    /// memory that is NOT resident: instead of guessing a path from the
    /// tree and reading the file whole, ask which lines mention a thing
    /// and read only the file that has them. Capped so one broad query
    /// cannot flood the transcript.
    pub fn search(&self, query: &str) -> Result<String> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            anyhow::bail!("search needs a non-empty query");
        }
        let files = self.live_files()?;
        const MAX_HITS: usize = 80;
        let mut hits: Vec<String> = Vec::new();
        let mut truncated = false;
        'outer: for (f, content) in &files {
            for (i, line) in content.lines().enumerate() {
                if line.to_lowercase().contains(&q) {
                    if hits.len() >= MAX_HITS {
                        truncated = true;
                        break 'outer;
                    }
                    hits.push(format!("{}:{}: {}", f.path, i + 1, line.trim()));
                }
            }
        }
        if hits.is_empty() {
            return Ok(format!(
                "no matches for {query:?} across {} memory files",
                files.len()
            ));
        }
        let mut out = hits.join("\n");
        if truncated {
            out.push_str(&format!(
                "\n… truncated at {MAX_HITS} hits; narrow the query"
            ));
        }
        Ok(out)
    }

    /// On-demand read of ANY memory file (the signpost follow-up):
    /// `memory action=read path=reference/foo.md`.
    pub fn read_file(&self, rel: &str) -> Result<String> {
        let rel = Self::check_rel(rel)?;
        self.current(rel)
            .ok_or_else(|| anyhow::anyhow!("no such memory file: {rel}"))
    }

    /// The one path rule for anything written into memory, whoever
    /// asks: the agent (`write_file`), a command, the desktop, or a
    /// peer's sync offer (`record_from_peer`). Returns the path with
    /// leading slashes stripped, or refuses a path that could not be a
    /// relative file name.
    ///
    /// Shared rather than copied because the peer path is the one that
    /// arrives from another machine — and because `export_to` turns
    /// these paths back into files, where a `../../.bashrc` would mean
    /// exactly what it says.
    pub fn check_rel(rel: &str) -> Result<&str> {
        let rel = rel.trim_start_matches('/');
        if rel.is_empty()
            || rel.contains("..")
            || rel.starts_with('.')
            || rel.contains('\0')
            || rel.contains('\\')
            || rel.ends_with('/')
            || rel.contains("//")
        {
            anyhow::bail!("suspicious memory path: {rel:?}");
        }
        Ok(rel)
    }

    /// Write ANY memory file (one revision). Empty content is refused:
    /// an empty revision is a deletion, and deleting is `delete_file`.
    pub fn write_file(&self, rel: &str, content: &str) -> Result<()> {
        self.write_as(rel, content, "agent")?;
        Ok(())
    }

    /// `write_file`, attributed: `source` is what `memory log` shows in
    /// brackets (`agent`, `cli`, `desktop`, …). Returns whether a
    /// revision landed (false = identical content).
    pub fn write_as(&self, rel: &str, content: &str, source: &str) -> Result<bool> {
        let rel = Self::check_rel(rel)?;
        if content.is_empty() && !always_exists(rel) {
            anyhow::bail!("refusing to write an empty {rel} — delete it instead");
        }
        self.record(rel, content, source, &format!("update {rel}"))
    }

    /// Write `rel` only if it is still what the caller read: `expected`
    /// is the sha the caller saw, or None for "it did not exist". The
    /// check and the INSERT share one transaction, so an agent write in
    /// between cannot be overwritten — the caller gets a [`Conflict`].
    /// Returns whether a revision landed (false = identical content).
    pub fn replace_if(
        &self,
        rel: &str,
        content: &str,
        expected: Option<&str>,
        source: &str,
    ) -> Result<bool> {
        let rel = Self::check_rel(rel)?;
        if content.is_empty() && !always_exists(rel) {
            anyhow::bail!("refusing to write an empty {rel} — delete it instead");
        }
        let (_turn, mut conn) = self.write_conn()?;
        let tx = Self::write_tx(&mut conn)?;
        let now = Self::live_latest(&tx, rel).map(|(_, _, sha)| sha);
        if now.as_deref() != expected {
            return Err(Conflict {
                path: rel.to_string(),
                current_sha: now,
            }
            .into());
        }
        let landed = Self::insert_if_changed(&tx, rel, content, source, &format!("update {rel}"))?;
        tx.commit()?;
        Ok(landed)
    }

    /// Delete a memory file: an empty revision, so the history keeps it
    /// and `restore` undoes it. The two system files cannot be deleted —
    /// they are the resident stores; empty them instead.
    pub fn delete_file(&self, rel: &str) -> Result<()> {
        let rel = Self::check_rel(rel)?;
        if always_exists(rel) {
            anyhow::bail!("{rel} is a resident store and cannot be deleted — empty it instead");
        }
        let (_turn, mut conn) = self.write_conn()?;
        let tx = Self::write_tx(&mut conn)?;
        if Self::live_latest(&tx, rel).is_none() {
            anyhow::bail!("no such memory file: {rel}");
        }
        Self::insert_if_changed(&tx, rel, "", "delete", "deleted")?;
        tx.commit()?;
        Ok(())
    }

    /// Rename a memory file: the content lands at `to` and `from` is
    /// deleted, in ONE transaction, so there is no moment where it is in
    /// both places or neither. Refuses an existing `to` and the system
    /// files at either end.
    pub fn move_file(&self, from: &str, to: &str) -> Result<()> {
        let from = Self::check_rel(from)?;
        let to = Self::check_rel(to)?;
        if always_exists(from) || always_exists(to) {
            anyhow::bail!("the resident stores cannot be moved");
        }
        if from == to {
            anyhow::bail!("{from} is already there");
        }
        let (_turn, mut conn) = self.write_conn()?;
        let tx = Self::write_tx(&mut conn)?;
        let Some((_, content, _)) = Self::live_latest(&tx, from) else {
            anyhow::bail!("no such memory file: {from}");
        };
        if Self::live_latest(&tx, to).is_some() {
            anyhow::bail!("{to} already exists — delete it first or pick another name");
        }
        Self::insert_if_changed(&tx, to, &content, "move", &format!("moved from {from}"))?;
        Self::insert_if_changed(&tx, from, "", "delete", &format!("moved to {to}"))?;
        tx.commit()?;
        Ok(())
    }

    /// The content of `rel` at revision `rev` (which must be one of
    /// `rel`'s own revisions).
    pub fn content_at(&self, rel: &str, rev: i64) -> Result<String> {
        let rel = Self::check_rel(rel)?;
        let conn = self.db()?;
        conn.query_row(
            "SELECT content FROM memory_revisions WHERE path = ?1 AND rev_id = ?2",
            rusqlite::params![rel, rev],
            |r| r.get::<_, String>(0),
        )
        .map_err(|_| anyhow::anyhow!("r{rev} is not a revision of {rel}"))
    }

    /// Make revision `rev` of `rel` current again — recorded as a new
    /// revision, so it can be undone in turn. Restoring a deleted file
    /// brings it back.
    pub fn restore(&self, rel: &str, rev: i64) -> Result<String> {
        let content = self.content_at(rel, rev)?;
        let rel = Self::check_rel(rel)?;
        self.record(rel, &content, "rollback", &format!("restored r{rev}"))?;
        Ok(content)
    }

    /// A unified diff of `rel` between two of its revisions. `new`
    /// defaults to the newest; `old` to the revision before `new`
    /// (nothing, for the first one — the whole file reads as added).
    pub fn diff(&self, rel: &str, old: Option<i64>, new: Option<i64>) -> Result<String> {
        let rel = Self::check_rel(rel)?;
        let conn = self.db()?;
        let revs: Vec<i64> = {
            let mut st = conn
                .prepare("SELECT rev_id FROM memory_revisions WHERE path = ?1 ORDER BY rev_id")?;
            let v = st
                .query_map(rusqlite::params![rel], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };
        let Some(&newest) = revs.last() else {
            anyhow::bail!("no such memory file: {rel}");
        };
        let new = new.unwrap_or(newest);
        let pos = revs
            .iter()
            .position(|&r| r == new)
            .ok_or_else(|| anyhow::anyhow!("r{new} is not a revision of {rel}"))?;
        let old = match old {
            Some(o) => Some(o),
            None => pos.checked_sub(1).map(|i| revs[i]),
        };
        let before = match old {
            Some(o) => self.content_at(rel, o)?,
            None => String::new(),
        };
        let after = self.content_at(rel, new)?;
        let old_label = old.map_or_else(|| "(nothing)".to_string(), |o| format!("{rel} r{o}"));
        let new_label = format!("{rel} r{new}");
        Ok(similar::TextDiff::from_lines(&before, &after)
            .unified_diff()
            .context_radius(3)
            .header(&old_label, &new_label)
            .to_string())
    }

    /// Write every live memory file under `dir` as a plain tree — a
    /// greppable snapshot, a hand-merge workspace, or a copy for
    /// another machine. `dir` must be empty or absent: an export never
    /// overwrites. Returns the number of files written.
    pub fn export_to(&self, dir: &Path) -> Result<usize> {
        if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
            anyhow::bail!(
                "{} is not empty — export into a new directory",
                dir.display()
            );
        }
        let files = self.live_files()?;
        for (f, content) in &files {
            let rel = Self::check_rel(&f.path)?;
            let p = dir.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&p, content).with_context(|| format!("write {}", p.display()))?;
        }
        Ok(files.len())
    }

    /// Record every `.md` file under `dir` as a revision of the same
    /// relative path (`source` names where they came from). Unchanged
    /// files record nothing; empty files are skipped, never deletions.
    /// Returns the paths that landed.
    pub fn import_from(&self, dir: &Path, source: &str) -> Result<Vec<String>> {
        if !dir.is_dir() {
            anyhow::bail!("{} is not a directory", dir.display());
        }
        let mut landed = Vec::new();
        for (rel, file) in Self::legacy_tree_files(dir)? {
            let rel = Self::check_rel(&rel)?.to_string();
            let content = std::fs::read_to_string(&file)
                .with_context(|| format!("read {}", file.display()))?;
            if content.is_empty() {
                continue;
            }
            if self.record(
                &rel,
                &content,
                source,
                &format!("imported from {}", dir.display()),
            )? {
                landed.push(rel);
            }
        }
        Ok(landed)
    }

    /// Curate an oversized `system/core.md`: keep the head that fits the
    /// resident budget (always at least the first note — semantics must
    /// not be lost to arithmetic) and move the overflow into topic files
    /// under `reference/topics/`, grouped by the slug of each note's
    /// first line. Existing topic files are MERGED, never overwritten,
    /// and every write is a revision, so the tree index names the topic
    /// files and memory_search/memory_read reach everything that left.
    ///
    /// Repeatable by design: new notes accumulate in core.md, and the
    /// next run redistributes the over-budget tail into the same topic
    /// files. Returns a report of what moved. `resid_chars` > 0 wins;
    /// 0 means the configured `core_char_limit`.
    pub fn curate(&self, resid_chars: usize) -> Result<String> {
        self.ensure_hierarchy()?;
        let entries = self.entries(StoreKind::Memory);
        if entries.is_empty() {
            return Ok("nothing to curate: system/core.md is empty".into());
        }
        let budget = if resid_chars > 0 {
            resid_chars
        } else {
            self.limits.core_char_limit.max(1)
        };

        // Longest prefix of notes whose joined size fits the budget.
        let mut used = 0usize;
        let mut cut = 0usize;
        for (i, e) in entries.iter().enumerate() {
            let extra = e.len() + if i == 0 { 0 } else { DELIM.len() };
            if i > 0 && used + extra > budget {
                break;
            }
            used += extra;
            cut = i + 1;
        }
        if cut == entries.len() {
            return Ok(format!(
                "nothing to curate: system/core.md is {} chars in {} note(s) — already within \
                 the {} resident budget",
                used,
                entries.len(),
                budget
            ));
        }

        let resident = &entries[..cut];
        let overflow = &entries[cut..];
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        for note in overflow {
            let slug = topic_slug(note);
            match groups.iter_mut().find(|(s, _)| *s == slug) {
                Some((_, v)) => v.push(note.clone()),
                None => groups.push((slug, vec![note.clone()])),
            }
        }

        // Shrink core.md FIRST — it is the bill itself. Topic files are
        // written after; a failure in between leaves the moved notes in
        // core.md's history, one rollback away.
        self.write(StoreKind::Memory, resident)?;
        let mut report = format!(
            "curated system/core.md: {} → {} chars — {} note(s) stay resident, \
             {} moved\n",
            joined_len(&entries),
            joined_len(resident),
            resident.len(),
            overflow.len()
        );
        for (slug, notes) in &groups {
            let rel = format!("reference/topics/{slug}.md");
            let existing = self.read_file(&rel).ok().unwrap_or_default();
            let merged = if existing.trim().is_empty() {
                notes.join(DELIM)
            } else {
                format!("{existing}{DELIM}{}", notes.join(DELIM))
            };
            self.write_file(&rel, &merged)?;
            report.push_str(&format!("  {rel} +{} note(s)\n", notes.len()));
        }
        report
            .push_str("the tree index lists them — memory_search and memory_read reach what moved");
        Ok(report)
    }

    /// The tree index shown in the prompt: every live file, grouped by
    /// its top directory, with a one-line description. Built from the
    /// database on each call — it used to be TREE.md, regenerated on
    /// every write and read back from disk.
    pub fn tree_index(&self) -> String {
        let Ok(files) = self.live_files() else {
            return String::new();
        };
        let top = |p: &str| p.split_once('/').map_or("", |(d, _)| d).to_string();
        let mut groups: Vec<String> = files.iter().map(|(f, _)| top(&f.path)).collect();
        groups.sort_by_key(|g| (g != "system", g.clone()));
        groups.dedup();
        let mut lines = vec!["# memory tree".to_string()];
        for g in groups {
            lines.push(String::new());
            lines.push(match g.as_str() {
                "system" => "## system/ (always in context)".to_string(),
                "" => "## (top level — read on demand, the memory tool)".to_string(),
                d => format!("## {d}/ (read on demand — the memory tool)"),
            });
            for (f, content) in files.iter().filter(|(f, _)| top(&f.path) == g) {
                lines.push(format!("- {} — {}", f.path, first_line_desc(content)));
            }
        }
        lines.push(String::new());
        lines.push(
            "Files outside system/ are NOT in context. Use the `memory` tool with action=read path=<rel> to load one.".into(),
        );
        lines.join("\n") + "\n"
    }

    /// Whether the hierarchy has been initialized.
    pub fn hierarchy_ready(&self) -> bool {
        self.db()
            .and_then(|c| {
                Ok(
                    c.query_row("SELECT COUNT(*) FROM memory_revisions", [], |r| {
                        r.get::<_, i64>(0)
                    })?,
                )
            })
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// The revision log, newest first — every path, or one.
    pub fn log_rows(&self, path: Option<&str>, n: usize) -> Result<Vec<LogRow>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT rev_id, path, source, note, created_at, content
             FROM memory_revisions WHERE ?1 IS NULL OR path = ?1
             ORDER BY rev_id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![path, n as i64], |r| {
                let path: String = r.get(1)?;
                let content: String = r.get(5)?;
                Ok(LogRow {
                    rev: r.get(0)?,
                    deleted: content.is_empty() && !always_exists(&path),
                    chars: content.chars().count(),
                    path,
                    source: r.get(2)?,
                    note: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    at: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Revision log (veles memory log), newest first, one line each.
    pub fn log(&self, n: usize) -> Result<Vec<String>> {
        Ok(self.log_rows(None, n)?.iter().map(log_line).collect())
    }

    /// Roll a memory file back `n` revisions OF THAT FILE.
    ///
    /// Note this is per-path, unlike the git version's `HEAD~n`, which
    /// counted commits across the whole store — so "roll core.md back
    /// one" could land on a revision that never touched core.md. Here
    /// `n = 1` is always the previous version of that file.
    ///
    /// The rollback is itself recorded as a new revision, so it can be
    /// rolled back in turn and nothing is ever destroyed.
    pub fn rollback(&self, rel: &str, n: usize) -> Result<String> {
        let rel = Self::check_rel(rel)?;
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT rev_id, content FROM memory_revisions
             WHERE path = ?1 ORDER BY rev_id DESC LIMIT 1 OFFSET ?2",
        )?;
        let (rev_id, content): (i64, String) = stmt
            .query_row(rusqlite::params![rel, n as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(|_| {
                let have: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM memory_revisions WHERE path = ?1",
                        rusqlite::params![rel],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                anyhow::anyhow!("cannot go back {n} revision(s) of {rel}: only {have} exist")
            })?;
        drop(stmt);
        drop(conn);
        self.record(rel, &content, "rollback", &format!("restored r{rev_id}"))?;
        Ok(content)
    }
}

/// One log row as a line: `r12  2026-09-26 10:00:00  path  [source] note`.
pub fn log_line(r: &LogRow) -> String {
    format!(
        "r{}  {}  {}  [{}] {}",
        r.rev, r.at, r.path, r.source, r.note
    )
}

/// First meaningful line of a memory file as a one-line description.
fn first_line_desc(raw: &str) -> String {
    // YAML frontmatter description wins.
    if raw.starts_with("---") {
        for line in raw.lines().skip(1) {
            if let Some(v) = line.strip_prefix("description:") {
                return v.trim().trim_matches('"').to_string();
            }
        }
    }
    raw.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("---"))
        .map(String::from)
        .unwrap_or_else(|| "(empty)".into())
}

fn joined_len(entries: &[String]) -> usize {
    if entries.is_empty() {
        0
    } else {
        entries.iter().map(|e| e.len()).sum::<usize>() + DELIM.len() * (entries.len() - 1)
    }
}

/// The topic filename a note belongs to: the slug of its first
/// meaningful line. Two notes about the same thing open with the same
/// line, so the slug IS the grouping — no embedding, no API.
fn topic_slug(note: &str) -> String {
    let first = note
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("general");
    let first = first.trim_start_matches('#').trim();
    let mut slug = String::new();
    for c in first.chars().take(48) {
        let lc = c.to_lowercase().next().unwrap_or(c);
        if lc.is_alphanumeric() {
            slug.push(lc);
        } else if slug.chars().last().map(|p| p != '-').unwrap_or(false) {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "general".to_string()
    } else {
        slug
    }
}

impl MemoryStore {
    fn usage_header(&self, kind: StoreKind) -> String {
        let used = self.read(kind).len();
        let limit = self.limit(kind);
        format!(
            "[{}% — {}/{} chars, {} entries]",
            (used as f64 / limit as f64 * 100.0) as u32,
            used,
            limit,
            self.entries(kind).len()
        )
    }

    /// S8: memory doctor — read-only analysis over the hierarchy.
    /// Reports per-file size, near-duplicate clusters, and a rough token
    /// estimate (chars/4) so the owner can prune. Never mutates.
    /// All user-visible text goes through i18n (l10n plan phase 1).
    pub fn doctor(&self) -> String {
        self.ensure_hierarchy().ok();
        let mut out = std::format!("{}\n", crate::i18n::lookup("doctor_title", &[]));
        // Always-in-context blocks.
        let mem = self.read(StoreKind::Memory);
        let user = self.read(StoreKind::User);
        let block = |label: &str, chars: usize, entries: usize| {
            crate::i18n::lookup(
                "doctor_block",
                &[
                    ("block", label.to_string()),
                    ("chars", chars.to_string()),
                    ("entries", entries.to_string()),
                    ("tokens", (chars / 4).to_string()),
                ],
            )
        };
        out.push_str(&block(
            "MEMORY",
            mem.len(),
            self.entries(StoreKind::Memory).len(),
        ));
        out.push('\n');
        out.push_str(&block(
            "USER",
            user.len(),
            self.entries(StoreKind::User).len(),
        ));
        out.push('\n');

        // What the PROMPT actually carries. Doctor's whole job is
        // telling the user what they are paying for, and since the
        // resident caps ([memory] core_char_limit /
        // user_resident_char_limit) a block's size on disk stopped
        // being its size in every prompt. Reported in CHARS — the same
        // unit the cap is written in, and the same count
        // `prompt::append_block` cuts on.
        let cfg = crate::config::load().unwrap_or_default();
        let mut resident = |chars: usize, cap: usize, path: &str| {
            if cap == 0 || chars <= cap {
                return;
            }
            out.push_str("  ");
            out.push_str(&crate::i18n::lookup(
                "doctor_resident",
                &[
                    ("shown", cap.to_string()),
                    ("total", chars.to_string()),
                    ("tokens", (cap / 4).to_string()),
                    ("path", path.to_string()),
                ],
            ));
            out.push('\n');
        };
        resident(
            mem.chars().count(),
            cfg.memory.core_char_limit,
            StoreKind::Memory.hier_path(),
        );
        resident(
            user.chars().count(),
            cfg.memory.user_resident_char_limit,
            StoreKind::User.hier_path(),
        );

        // Every live file, the system two included — once each. (The
        // disk walk this replaced seeded the system files and then walked
        // system/ again, and counted the generated TREE.md besides.)
        let per_file: Vec<(String, usize)> = self
            .live_files()
            .unwrap_or_default()
            .into_iter()
            .map(|(f, content)| (f.path, content.len()))
            .collect();
        let total_chars: usize = per_file.iter().map(|(_, n)| n).sum();
        out.push_str(&format!(
            "\n  {}\n\n",
            crate::i18n::lookup(
                "doctor_total",
                &[
                    ("chars", total_chars.to_string()),
                    ("files", per_file.len().to_string()),
                    ("tokens", (total_chars / 4).to_string()),
                ],
            )
        ));

        // Near-duplicate clusters: |a-b| / max(|a|,|b|) < 0.2 → overlap.
        let mut bodies: Vec<(&str, usize)> =
            per_file.iter().map(|(p, l)| (p.as_str(), *l)).collect();
        bodies.retain(|(_, l)| *l > 0);
        let mut clusters = Vec::new();
        for i in 0..bodies.len() {
            for j in (i + 1)..bodies.len() {
                let (pa, la) = bodies[i];
                let (pb, lb) = bodies[j];
                // Skip serde-same-file duplicates (doctor over ghost pairs).
                let big = usize::max(la, lb).max(1);
                let ratio = la.abs_diff(lb) as f64 / big as f64;
                if ratio < 0.2 {
                    clusters.push(crate::i18n::lookup(
                        "doctor_dup",
                        &[("a", format!("{pa} ({la})")), ("b", format!("{pb} ({lb})"))],
                    ));
                }
            }
        }
        if clusters.is_empty() {
            out.push_str("  ");
            out.push_str(&crate::i18n::lookup("doctor_no_dups", &[]));
            out.push('\n');
        } else {
            out.push_str("  ");
            out.push_str(&crate::i18n::lookup("doctor_dups_found", &[]));
            out.push('\n');
            for c in &clusters {
                out.push_str(c);
                out.push('\n');
            }
        }
        out
    }

    /// Apply one operation batch (memory add/replace/remove). Returns
    /// the tool response text. Rides write() → hierarchy + commit.
    pub fn apply(&self, kind: StoreKind, ops: &[serde_json::Value]) -> String {
        let mut entries = self.entries(kind);
        let limit = self.limit(kind);
        let mut msgs: Vec<String> = Vec::new();

        for op in ops {
            let action = op["action"].as_str().unwrap_or("");
            match action {
                "add" => {
                    let Some(content) = op["content"].as_str() else {
                        msgs.push("add: missing content".into());
                        continue;
                    };
                    if content.trim().is_empty() {
                        msgs.push("add: empty content".into());
                        continue;
                    }
                    let prospective = joined_len(&entries) + content.len() + DELIM.len();
                    if !entries.is_empty() && prospective > limit {
                        msgs.push(format!(
                            "add: rejected — would exceed budget ({prospective} > {limit}); remove or shorten first"
                        ));
                        continue;
                    }
                    entries.push(content.to_string());
                    msgs.push("added".into());
                }
                "replace" | "remove" => {
                    // `old_string` is Claude Code's Edit spelling of the
                    // same thing: the exact text to find.
                    let Some(old_text) = op["old_text"]
                        .as_str()
                        .or_else(|| op["old_string"].as_str())
                        .filter(|t| !t.is_empty())
                    else {
                        msgs.push(format!("{action}: missing old_text"));
                        continue;
                    };
                    let hits: Vec<usize> = entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.contains(old_text))
                        .map(|(i, _)| i)
                        .collect();
                    match hits.len() {
                        0 => msgs.push(format!("{action}: old_text matches no entry")),
                        n if n > 1 => msgs.push(format!(
                            "{action}: old_text matches {n} entries — be more specific"
                        )),
                        1 => {
                            let idx = hits[0];
                            if action == "remove" {
                                entries.remove(idx);
                                msgs.push("removed".into());
                            } else {
                                // The replacement for old_text — `new_text` /
                                // `new_string` are the Edit spellings.
                                let Some(content) = op["content"]
                                    .as_str()
                                    .or_else(|| op["new_text"].as_str())
                                    .or_else(|| op["new_string"].as_str())
                                else {
                                    msgs.push("replace: missing content".into());
                                    continue;
                                };
                                // Claude Code's Edit is the model (USER,
                                // 2026-09-28): EXACTLY old_text is replaced.
                                // This used to swap the whole entry that
                                // contained it, and nothing said so — an
                                // agent replaced one sentence of a 2 KB
                                // entry and was told "replaced" while the
                                // rest was gone. old_text equal to the whole
                                // entry still rewrites it.
                                let entry = &entries[idx];
                                let whole = entry.trim() == old_text.trim();
                                let updated = if whole {
                                    content.to_string()
                                } else {
                                    let n = entry.matches(old_text).count();
                                    if n > 1 {
                                        msgs.push(format!(
                                            "replace: old_text occurs {n} times in its entry — \
                                             include more of the surrounding text so it names \
                                             one place"
                                        ));
                                        continue;
                                    }
                                    entry.replacen(old_text, content, 1)
                                };
                                if updated.trim().is_empty() {
                                    msgs.push(
                                        "replace: that would leave the entry empty — use remove"
                                            .into(),
                                    );
                                    continue;
                                }
                                let before = entry.chars().count();
                                let mut trial = entries.clone();
                                trial[idx] = updated;
                                if joined_len(&trial) > limit {
                                    msgs.push(format!(
                                        "replace: rejected — would exceed budget ({} > {limit})",
                                        joined_len(&trial)
                                    ));
                                } else {
                                    let after = trial[idx].chars().count();
                                    entries = trial;
                                    msgs.push(format!(
                                        "replaced {} (entry {before} → {after} chars)",
                                        if whole {
                                            "the whole entry"
                                        } else {
                                            "old_text in its entry"
                                        }
                                    ));
                                }
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                other => msgs.push(format!("unknown action: {other}")),
            }
        }

        if let Err(e) = self.write(kind, &entries) {
            return format!("write failed: {e}");
        }
        format!("{}\n{}", msgs.join("; "), self.usage_header(kind))
    }
}

/// The tool implementation registered in the tool registry.
pub struct MemoryTool {
    /// Session id for the ACL principal lookup (direction gating).
    pub session_id: String,
}

#[async_trait]
impl crate::tools::Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }
    fn description(&self) -> &str {
        "Persistent memory across sessions. ALWAYS-IN-CONTEXT stores: 'memory' (agent notes) and 'user' (who the user is) — operations add {content}; replace {old_text, content} swaps just old_text in its entry, like Edit (whole entry as old_text = rewrite); remove {old_text} drops its entry; batch via 'operations'. On-demand memory files (the prompt's memory tree): action=read path=<rel>; action=search query=<text> greps all; action=list shows sizes; action=write_file path=<rel> content=<text>; action=delete_file path=<rel> (restorable); action=move_file path=<rel> to=<rel> (these three need memory-write)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {"type": "string", "enum": ["memory", "user"], "description": "Which always-in-context store"},
                "operations": {
                    "type": "array",
                    "description": "Batch of changes applied atomically",
                    "items": {
                        "type": "object",
                        "properties": {
                            "action": {"type": "string", "enum": ["add", "replace", "remove"]},
                            "content": {"type": "string"},
                            "old_text": {"type": "string"}
                        },
                        "required": ["action"]
                    }
                },
                "action": {"type": "string", "enum": ["add", "replace", "remove", "read", "write_file", "delete_file", "move_file", "list", "tree", "search"], "description": "Single-op shorthand; read/write_file/delete_file/move_file/list/tree/search address the memory files"},
                "path": {"type": "string", "description": "Memory path for read/write_file/delete_file/move_file (e.g. reference/projects/foo.md)"},
                "to": {"type": "string", "description": "The new path, for move_file"},
                "query": {"type": "string", "description": "Substring to grep for with action=search (case-insensitive, across every memory file)"},
                "content": {"type": "string"},
                "old_text": {"type": "string"}
            },
            "required": ["target"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> String {
        // ACL direction check (S15): the dispatch gate already required
        // memory-read for this tool to be reachable; a MUTATING op
        // additionally needs memory-write.
        let mutating = args["operations"].is_array()
            || matches!(
                args["action"].as_str(),
                Some("add")
                    | Some("replace")
                    | Some("remove")
                    | Some("write_file")
                    | Some("delete_file")
                    | Some("move_file")
            );
        if mutating {
            if let Some(who) = crate::acl::session_principal(&self.session_id) {
                if !who.has(crate::acl::Cap::MemoryWrite) {
                    return format!(
                        "error: access denied — memory writes require the `memory-write` \
capability, which {} does not have. Reads would be allowed; mutations are not.",
                        who.label()
                    );
                }
            }
        }
        let store = MemoryStore::new();
        // Hierarchy ops first (S1).
        match args["action"].as_str() {
            Some("read") => {
                let path = args["path"].as_str().unwrap_or_default();
                if path.is_empty() {
                    return "usage: action=read path=<rel>".into();
                }
                return match store.read_file(path) {
                    Ok(c) => c,
                    Err(e) => format!("error: {e:#}"),
                };
            }
            Some("write_file") => {
                let path = args["path"].as_str().unwrap_or_default();
                let content = args["content"].as_str().unwrap_or_default();
                if path.is_empty() {
                    return "usage: action=write_file path=<rel> content=<text>".into();
                }
                return match store.write_file(path, content) {
                    Ok(()) => format!("written: {path} (recorded)"),
                    Err(e) => format!("error: {e:#}"),
                };
            }
            Some("delete_file") => {
                let path = args["path"].as_str().unwrap_or_default();
                if path.is_empty() {
                    return "usage: action=delete_file path=<rel>".into();
                }
                return match store.delete_file(path) {
                    Ok(()) => format!("deleted: {path} (its history keeps it)"),
                    Err(e) => format!("error: {e:#}"),
                };
            }
            Some("move_file") => {
                let path = args["path"].as_str().unwrap_or_default();
                let to = args["to"].as_str().unwrap_or_default();
                if path.is_empty() || to.is_empty() {
                    return "usage: action=move_file path=<rel> to=<rel>".into();
                }
                return match store.move_file(path, to) {
                    Ok(()) => format!("moved: {path} → {to}"),
                    Err(e) => format!("error: {e:#}"),
                };
            }
            Some("list") => {
                return match store.list() {
                    Ok(files) => files
                        .iter()
                        .map(|f| format!("{}  ({} chars, {})", f.path, f.chars, f.at))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Err(e) => format!("error: {e:#}"),
                };
            }
            Some("tree") => {
                return store.tree_index();
            }
            Some("search") => {
                // Either field reads naturally; `content` keeps the
                // schema small, `query` is what the model reaches for.
                let query = args["query"]
                    .as_str()
                    .or_else(|| args["content"].as_str())
                    .unwrap_or_default();
                return match store.search(query) {
                    Ok(s) => s,
                    Err(e) => format!("error: {e:#}"),
                };
            }
            _ => {}
        }
        let kind = match args["target"].as_str() {
            Some("user") => StoreKind::User,
            _ => StoreKind::Memory,
        };
        let ops: Vec<serde_json::Value> = if let Some(arr) = args["operations"].as_array() {
            arr.clone()
        } else {
            vec![serde_json::json!({
                "action": args["action"],
                "content": args["content"],
                "old_text": args["old_text"],
                // Edit-shaped spellings, read by `apply`.
                "old_string": args["old_string"],
                "new_string": args["new_string"],
                "new_text": args["new_text"],
            })]
        };
        store.apply(kind, &ops)
    }
}

// ── tests ─────────────────────────────────────────────────────────────

/// Shared env lock — prompt tests race the memory tests on VELES_HOME
/// otherwise (parallel set_var). Exposed for sibling test modules.
#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch VELES_HOME that CANNOT be shared with another test.
    ///
    /// This used to key the directory on process id + thread id and
    /// clean it up best-effort at both ends (`let _ = remove_dir_all`).
    /// Cargo reuses worker threads, so two tests routinely got the same
    /// path — and if either cleanup failed, the second inherited a git
    /// repo full of the first test's commits. `ensure_hierarchy` then
    /// skipped `git init` (`.git` already existed) and HEAD~1 pointed
    /// into somebody else's history, which is exactly the shape of the
    /// intermittent failure this replaced:
    ///
    ///     fatal: path 'system/core.md' exists on disk, but not in 'HEAD~1'
    ///
    /// `tempfile::tempdir()` is unique per call and removes itself on
    /// drop, so the reuse is impossible rather than merely unlikely.
    fn scratch() -> (
        tempfile::TempDir,
        MemoryStore,
        std::sync::MutexGuard<'static, ()>,
    ) {
        // The guard is RETURNED, not dropped here: it has to outlive the
        // fixture or the test body runs unserialized against a
        // process-global VELES_HOME.
        let g = crate::testenv::lock();
        let dir = tempfile::tempdir().expect("scratch home");
        // Deliberately does NOT set VELES_HOME.
        //
        // Pinning the store by path is not enough on its own: as long
        // as the env var points here, any test that reads it without
        // taking the lock can still write into this directory. Not
        // publishing the path at all makes it unreachable.
        let store = MemoryStore::at(dir.path().to_path_buf());
        (dir, store, g)
    }

    /// `replace` is an Edit: EXACTLY old_text changes. The incident
    /// (2026-09-27, B4 session): an agent passed one sentence of a 2 KB
    /// entry as old_text and a new sentence as content; the whole entry
    /// became that sentence and the tool said "replaced".
    #[test]
    fn replace_swaps_only_old_text_like_an_edit() {
        let (_dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();
        let long = format!(
            "B4 landed in the worktree. {} Verified: lib 1819/1819. Open tails: none.",
            "Shipped placement.rs and remotetask.rs. ".repeat(40)
        );
        s.write(StoreKind::Memory, &[long.clone(), "other".to_string()])
            .unwrap();
        let out = s.apply(
            StoreKind::Memory,
            &[serde_json::json!({
                "action": "replace",
                "old_text": "Verified: lib 1819/1819.",
                "content": "Verified (final): lib 1819/1819, mutcheck 13/13 red."
            })],
        );
        assert!(out.contains("old_text in its entry"), "{out}");
        let e = s.entries(StoreKind::Memory);
        assert!(
            e[0].starts_with("B4 landed in the worktree."),
            "the rest survives: {}",
            e[0]
        );
        assert!(e[0].contains("mutcheck 13/13 red."), "{}", e[0]);
        assert!(e[0].ends_with("Open tails: none."), "{}", e[0]);
        assert!(!e[0].contains("Verified: lib"), "{}", e[0]);
        assert_eq!(e[1], "other");
    }

    /// The whole entry as old_text rewrites it; a duplicated old_text is
    /// refused instead of guessing which place; Edit spellings work, in
    /// a batch too; an empty result is refused (that is remove).
    #[test]
    fn replace_whole_entries_refuses_ambiguity_and_takes_edit_spellings() {
        let (_dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();
        s.write(
            StoreKind::Memory,
            &["alpha beta alpha".to_string(), "gamma".to_string()],
        )
        .unwrap();
        let out = s.apply(
            StoreKind::Memory,
            &[serde_json::json!({"action": "replace", "old_text": "gamma", "content": "delta"})],
        );
        assert!(out.contains("the whole entry"), "{out}");
        let out = s.apply(
            StoreKind::Memory,
            &[serde_json::json!({"action": "replace", "old_text": "alpha", "content": "x"})],
        );
        assert!(out.contains("occurs 2 times"), "{out}");
        assert_eq!(
            s.entries(StoreKind::Memory)[0],
            "alpha beta alpha",
            "unchanged"
        );
        let out = s.apply(
            StoreKind::Memory,
            &[serde_json::json!({
                "action": "replace",
                "old_string": "beta alpha",
                "new_string": "BETA"
            })],
        );
        assert!(out.contains("replaced"), "{out}");
        assert_eq!(s.entries(StoreKind::Memory), vec!["alpha BETA", "delta"]);
        let out = s.apply(
            StoreKind::Memory,
            &[serde_json::json!({"action": "replace", "old_text": "delta", "new_text": " "})],
        );
        assert!(out.contains("use remove"), "{out}");
        assert_eq!(s.entries(StoreKind::Memory)[1], "delta");
    }

    #[test]
    fn writes_commit_and_tree_updates() {
        let (_dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();
        let entries = vec!["first".to_string(), "second".to_string()];
        s.write(StoreKind::Memory, &entries).unwrap();
        // The write is recorded. Note this is ONE revision, not two:
        // the git version opened with an empty "ensure hierarchy"
        // commit, but history should record changes, not ceremony.
        let log = s.log(10).unwrap();
        assert_eq!(log.len(), 1, "one write, one revision: {log:?}");
        assert!(log[0].contains("system/core.md"), "{log:?}");
        // TREE.md mentions system/core.md.
        let tree = s.tree_index();
        assert!(tree.contains("system/core.md"), "{tree}");
        // On-demand file: write + read back.
        s.write_file("reference/projects/impro.md", "# impro\nPWA client")
            .unwrap();
        assert!(s
            .read_file("reference/projects/impro.md")
            .unwrap()
            .contains("PWA"));
        let tree = s.tree_index();
        assert!(tree.contains("reference/projects/impro.md"), "{tree}");
    }

    #[test]
    fn search_finds_lines_across_the_hierarchy() {
        let (_dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();
        s.write(
            StoreKind::Memory,
            &["the gateway listens on 8642".to_string()],
        )
        .unwrap();
        s.write_file(
            "reference/networking.md",
            "# net\nVPS nginx reverse proxy\nport 8642 is the gateway",
        )
        .unwrap();
        let out = s.search("8642").unwrap();
        // Both the resident store and the on-demand file match, with
        // paths and line numbers the agent can act on.
        assert!(out.contains("system/core.md:1:"), "{out}");
        assert!(out.contains("reference/networking.md:3:"), "{out}");
        // Case-insensitive.
        assert!(s.search("GATEWAY").unwrap().contains("8642"));
        // A miss says so rather than returning empty.
        assert!(s.search("zzz-nothing").unwrap().contains("no matches"));
        // Empty query is a usage error, not a match-everything flood.
        assert!(s.search("  ").is_err());
    }

    #[test]
    fn rollback_restores_previous() {
        let (_dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();
        s.write(StoreKind::Memory, &["v1".to_string()]).unwrap();
        s.write(StoreKind::Memory, &["v2".to_string()]).unwrap();
        // rollback needs HEAD~1, so it needs at least two commits.
        // Assert that FIRST, with the log attached: when this failed
        // intermittently the only clue was "invalid object name
        // 'HEAD~1'", which says nothing about which commit went missing.
        let revs = s.log(100).unwrap();
        assert!(
            revs.len() >= 2,
            "expected >=2 revisions before rollback, got {}:\n{revs:#?}",
            revs.len()
        );
        let back = s.rollback("system/core.md", 1).unwrap();
        assert!(back.contains("v1"), "rollback returns the old content");
        assert!(s.read(StoreKind::Memory).contains("v1"));
    }

    /// Every write must produce a revision, even when consecutive
    /// writes are the same length and land microseconds apart.
    ///
    /// This is the bug that killed the git-backed design: git decided
    /// whether to record a change from cached stat data (size + mtime),
    /// so a same-length rewrite within the same tick looked unchanged
    /// and the version was silently lost. There is no equivalent state
    /// here — the INSERT either happens or the transaction fails — but
    /// the test stays, because it is the property that matters and it
    /// is the one that regressed before.
    #[test]
    fn consecutive_same_size_writes_each_record_a_revision() {
        let (_dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();

        for i in 0..40 {
            let a = format!("aaa{i:04}");
            let b = format!("bbb{i:04}");
            s.write(StoreKind::Memory, std::slice::from_ref(&a))
                .unwrap();
            let before = s.log(1000).unwrap().len();
            s.write(StoreKind::Memory, std::slice::from_ref(&b))
                .unwrap();
            let after = s.log(1000).unwrap().len();
            assert_eq!(
                after,
                before + 1,
                "iteration {i}: the second write recorded no revision"
            );
            assert_eq!(s.read(StoreKind::Memory), b, "iteration {i}: read-back");
            // …and the previous version is still reachable.
            let prev = s.rollback("system/core.md", 1).unwrap();
            assert_eq!(
                prev,
                a,
                "iteration {i}: rollback returned {prev:?}, wanted {a:?}\nlog:\n{:#?}",
                s.log(8).unwrap()
            );
            // put it back so the next iteration starts from a known state
            s.write(StoreKind::Memory, std::slice::from_ref(&b))
                .unwrap();
        }
    }

    /// Byte-identical content is not a new revision — the history
    /// should record changes, not writes.
    #[test]
    fn identical_content_records_nothing() {
        let (_dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();
        s.write(StoreKind::Memory, &["same".to_string()]).unwrap();
        let n = s.log(1000).unwrap().len();
        s.write(StoreKind::Memory, &["same".to_string()]).unwrap();
        s.write(StoreKind::Memory, &["same".to_string()]).unwrap();
        assert_eq!(
            s.log(1000).unwrap().len(),
            n,
            "no-op writes are not history"
        );
    }

    /// Rollback is itself a revision, so it can be undone. The git
    /// version wrote a commit too, but counted `HEAD~n` across the whole
    /// store — so "back one" could land on a commit that never touched
    /// the file. Here n is always per-path.
    #[test]
    fn rollback_is_undoable_and_per_path() {
        let (_dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();
        s.write(StoreKind::Memory, &["v1".to_string()]).unwrap();
        // A write to an UNRELATED file between the two versions: under
        // the old HEAD~n semantics this would have shifted the target.
        s.write_file("reference/other.md", "unrelated").unwrap();
        s.write(StoreKind::Memory, &["v2".to_string()]).unwrap();

        assert_eq!(s.rollback("system/core.md", 1).unwrap(), "v1");
        assert_eq!(s.read(StoreKind::Memory), "v1");
        // …and rolling back the rollback returns v2.
        assert_eq!(s.rollback("system/core.md", 1).unwrap(), "v2");
        assert_eq!(s.read(StoreKind::Memory), "v2");
    }

    #[test]
    fn rollback_past_the_beginning_is_an_error_not_a_panic() {
        let (_dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();
        s.write(StoreKind::Memory, &["only".to_string()]).unwrap();
        let err = s.rollback("system/core.md", 99).unwrap_err().to_string();
        assert!(err.contains("only"), "error should say what exists: {err}");
    }

    #[test]
    fn path_traversal_refused() {
        let (_dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();
        assert!(s.read_file("../../etc/passwd").is_err());
        assert!(s.write_file("../evil.md", "x").is_err());
    }

    #[test]
    fn doctor_reports_sizes_and_duplicates() {
        let (_dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();
        s.write(StoreKind::Memory, &["some note".into()]).unwrap();
        s.write_file("reference/a.md", "hello world here").unwrap();
        s.write_file("reference/b.md", "hello world there").unwrap();
        let rep = s.doctor();
        assert!(rep.contains("memory doctor"), "{rep}");
        assert!(rep.contains("MEMORY"), "{rep}");
        assert!(rep.contains("total:"), "{rep}");
        assert!(rep.contains("reference/a.md"), "{rep}");
    }

    /// Doctor's job is telling the user what they are paying for. Since
    /// the resident caps, a block's size ON DISK stopped being its size
    /// in the prompt — so the report has to name both, and the way to
    /// reach the part that is no longer resident.
    #[test]
    fn memory_doctor_reports_the_resident_share() {
        // scratch_home() takes the env lock itself — do NOT also call
        // testenv::lock() here (see its doc comment).
        let (home, _env) = crate::testenv::scratch_home();
        std::fs::write(
            home.path().join("config.toml"),
            "[memory]\ncore_char_limit = 500\n",
        )
        .unwrap();

        let s = MemoryStore::at(home.path().to_path_buf());
        s.ensure_hierarchy()
            .unwrap_or_else(|e| panic!("ensure_hierarchy in {}: {e:#}", home.path().display()));
        let notes: Vec<String> = (0..40)
            .map(|i| format!("note {i}: {}", "x".repeat(200)))
            .collect();
        // .expect with context, not a bare unwrap: this write failed
        // once in a full `cargo test --workspace` run and did not
        // reproduce in 20 further runs. If it happens again, the error
        // and the home path are what identify it — a bare unwrap would
        // only give the line number again.
        s.write(StoreKind::Memory, &notes)
            .unwrap_or_else(|e| panic!("seed write in {}: {e:#}", home.path().display()));
        let total = s.read(StoreKind::Memory).chars().count();

        let rep = s.doctor();
        assert!(
            rep.contains("resident in every prompt"),
            "doctor names the resident share: {rep}"
        );
        assert!(
            rep.contains(&format!("500 of {total} chars")),
            "with both numbers, not just the on-disk one: {rep}"
        );
        assert!(
            rep.contains("memory action=read path=system/core.md"),
            "and the call that reaches the rest: {rep}"
        );

        // USER is empty here, so it is under any cap — doctor must not
        // claim a truncation that will not happen.
        assert!(
            !rep.contains("system/user.md — "),
            "an uncapped block is not reported as capped: {rep}"
        );
    }

    // ── curate (the core.md split, Bill 1) ───────────────────────────

    /// A core.md too big for the resident budget: 30 notes spread over
    /// four topics (the topic is the WHOLE first line, so several notes
    /// share each slug).
    fn oversized_core(s: &MemoryStore) -> Vec<String> {
        let notes: Vec<String> = (0..30)
            .map(|i| {
                format!(
                    "# {}\nbody {i}: {} END-{i}",
                    ["networking", "backups", "networking", "style"][i % 4],
                    "x".repeat(180)
                )
            })
            .collect();
        s.write(StoreKind::Memory, &notes)
            .unwrap_or_else(|e| panic!("seed: {e:#}"));
        notes
    }

    /// The split keeps the budget head in core.md and moves the tail
    /// into topic files the signpost, memory_search and memory_read can
    /// all reach. Nothing may be lost: resident + topics must
    /// reconstruct every original note.
    #[test]
    fn curate_keeps_the_budget_head_and_moves_the_tail_into_topics() {
        let (_dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();
        let notes = oversized_core(&s);

        let budget = 1_000;
        let report = s.curate(budget).unwrap();
        assert!(report.contains("curated system/core.md"), "{report}");
        assert!(report.contains("reference/topics/"), "{report}");

        // The resident head is a strict prefix of the notes.
        let resident = s.read(StoreKind::Memory);
        let resident_chars = resident.chars().count();
        assert!(
            resident_chars <= budget + notes[0].len(),
            "resident {resident_chars} must fit the {budget}-char budget (plus the \
             always-kept first note)"
        );
        for (i, _n) in notes.iter().enumerate() {
            if resident.contains(&format!("END-{i}")) {
                break;
            }
            assert!(
                !resident.contains(&format!("body {i}:")),
                "note {i} left resident — the head must be a prefix, not a grab bag"
            );
        }

        // The signpost names the topic files; search reaches moved text.
        let tree = s.tree_index();
        assert!(
            tree.contains("reference/topics/networking.md"),
            "the tree names the topics: {tree}"
        );
        // Note 12 is deep in the overflow — search must still find it.
        let hit = s.search("END-12").unwrap_or_else(|e| e.to_string());
        assert!(
            !hit.contains("no matches"),
            "a moved note is searchable: {hit}"
        );
        assert!(hit.contains("reference/topics/"), "{hit}");
    }

    /// Every original note survives somewhere: concatenate the resident
    /// core and every topic file and each unique tail marker must be
    /// present exactly once.
    #[test]
    fn curate_loses_nothing() {
        let (_dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();
        let notes = oversized_core(&s);
        s.curate(1_000).unwrap();

        let mut recovered = s.read(StoreKind::Memory);
        for f in s.list().unwrap() {
            if f.path.starts_with("reference/topics/") {
                recovered.push_str(&s.read_file(&f.path).unwrap());
            }
        }
        for (i, _) in notes.iter().enumerate() {
            assert!(
                recovered.contains(&format!("END-{i}")),
                "note {i} must survive the split"
            );
        }
    }

    /// Repeatable, not one-shot: new notes accumulate in core.md, and
    /// the next curate appends into the SAME topic files rather than
    /// overwriting them.
    #[test]
    fn curate_is_repeatable_and_merges_into_existing_topics() {
        let (_dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();
        let _notes = oversized_core(&s);
        s.curate(1_000).unwrap();
        let topics_before = s
            .read_file("reference/topics/networking.md")
            .expect("topic exists");

        // More notes on the same topics arrive — core.md bloats again.
        let entries = s.entries(StoreKind::Memory);
        let mut grown = entries;
        grown.push(format!(
            "# networking\nbody new: fresh net note {} END-NEW-NET",
            "x".repeat(400)
        ));
        s.write(StoreKind::Memory, &grown).unwrap();
        let report = s.curate(1_000).unwrap();

        let topics_after = s
            .read_file("reference/topics/networking.md")
            .expect("topic survives");
        assert!(
            topics_after.contains(topics_before.trim()),
            "existing topic contents are merged, not clobbered"
        );
        assert!(
            topics_after.contains("END-NEW-NET"),
            "and the new note joined the same topic file"
        );
        assert!(
            report.contains("+1"),
            "the report counts the re-split: {report}"
        );
        assert!(
            !s.read(StoreKind::Memory).contains("fresh net note"),
            "the re-curated core is back within budget"
        );
    }

    // ── the database is the store (2026-09-26) ─────────────────────────

    /// Everything under the home except the database itself.
    fn files_beside_the_db(home: &Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(home)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with("state.db"))
            .collect();
        out.sort();
        out
    }

    /// No write path leaves a second copy anywhere: no memory.d, no flat
    /// MEMORY.md/USER.md, no TREE.md. The mirror was the bug.
    #[test]
    fn no_write_leaves_a_file_on_disk() {
        let (dir, s, _g) = scratch();
        s.ensure_hierarchy().unwrap();
        s.write(StoreKind::Memory, &["a note".into()]).unwrap();
        s.write(StoreKind::Memory, &["a note".into(), "another".into()])
            .unwrap();
        s.write(StoreKind::User, &["the user".into()]).unwrap();
        s.write_file("reference/x.md", "x").unwrap();
        s.move_file("reference/x.md", "reference/y.md").unwrap();
        s.delete_file("reference/y.md").unwrap();
        s.rollback("system/core.md", 1).unwrap();
        s.curate(1).unwrap();
        let _ = s.tree_index();
        assert_eq!(files_beside_the_db(dir.path()), Vec::<String>::new());
    }

    /// A leftover memory.d is folded in — a file the database does not
    /// have lands as a `migration` revision — and then removed, with the
    /// flat files, once every copy is verified to be in the history.
    #[test]
    fn a_leftover_tree_is_folded_in_then_removed() {
        let (dir, s, _g) = scratch();
        s.write(StoreKind::Memory, &["db version".into()]).unwrap();
        let tree = dir.path().join("memory.d");
        std::fs::create_dir_all(tree.join("system")).unwrap();
        std::fs::create_dir_all(tree.join("reference/deep")).unwrap();
        std::fs::write(tree.join("system/core.md"), "db version").unwrap();
        std::fs::write(tree.join("reference/deep/new.md"), "written by hand").unwrap();
        std::fs::write(tree.join("TREE.md"), "# derived, ignored").unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "db version").unwrap();
        std::fs::write(dir.path().join("USER.md"), "who they are").unwrap();

        let report = s.retire_disk_tree().unwrap();
        assert_eq!(
            report.folded,
            vec![
                "reference/deep/new.md".to_string(),
                "system/user.md".to_string()
            ],
            "{report:?}"
        );
        assert!(report.removed, "{report:?}");
        assert_eq!(files_beside_the_db(dir.path()), Vec::<String>::new());
        assert_eq!(
            s.read_file("reference/deep/new.md").unwrap(),
            "written by hand"
        );
        assert_eq!(s.read(StoreKind::User), "who they are");
        assert!(s.log(10).unwrap().iter().any(|l| l.contains("[migration]")));
        // And the second run has nothing to do.
        assert_eq!(s.retire_disk_tree().unwrap(), RetireReport::default());
    }

    /// Nothing is removed unless everything is verified. A flat file the
    /// history has never seen, a file that is not memory, or a git
    /// history the database never imported each keep ALL the disk
    /// copies, and the report says which.
    #[test]
    fn an_unverified_leftover_is_kept_and_named() {
        let (dir, s, _g) = scratch();
        s.write(StoreKind::Memory, &["db version".into()]).unwrap();
        // Not in core.md's history (core.md has history, so it is not
        // folded either — the flat file was a mirror, never the truth).
        std::fs::write(dir.path().join("MEMORY.md"), "a stale mirror").unwrap();
        let tree = dir.path().join("memory.d");
        std::fs::create_dir_all(tree.join(".git")).unwrap();
        std::fs::write(tree.join("notes.txt"), "not memory").unwrap();
        std::fs::write(tree.join("kept.md"), "kept").unwrap();

        let report = s.retire_disk_tree().unwrap();
        assert!(!report.removed, "{report:?}");
        let why = report.kept.join("\n");
        assert!(why.contains("MEMORY.md"), "{why}");
        assert!(why.contains(".git"), "{why}");
        assert!(why.contains("notes.txt"), "{why}");
        assert!(dir.path().join("MEMORY.md").exists());
        assert!(tree.join("notes.txt").exists());
        // The .md file was still folded: the data is safe either way.
        assert_eq!(s.read_file("kept.md").unwrap(), "kept");
        assert_eq!(s.read(StoreKind::Memory), "db version");
    }

    /// Deleting is a revision: the path disappears from every view, the
    /// history keeps it, and restore brings it back.
    #[test]
    fn a_deleted_file_is_absent_everywhere_and_restorable() {
        let (_dir, s, _g) = scratch();
        s.write_file("reference/gone.md", "# gone\nfind-me-token")
            .unwrap();
        let rev = s.log_rows(Some("reference/gone.md"), 1).unwrap()[0].rev;
        s.delete_file("reference/gone.md").unwrap();

        assert!(s.read_file("reference/gone.md").is_err());
        assert!(!s
            .list()
            .unwrap()
            .iter()
            .any(|f| f.path == "reference/gone.md"));
        assert!(!s.tree_index().contains("reference/gone.md"));
        assert!(s.search("find-me-token").unwrap().contains("no matches"));
        let log = s.log_rows(Some("reference/gone.md"), 5).unwrap();
        assert!(log[0].deleted && log[0].source == "delete", "{log:?}");
        assert!(s.delete_file("reference/gone.md").is_err(), "already gone");

        assert_eq!(
            s.restore("reference/gone.md", rev).unwrap(),
            "# gone\nfind-me-token"
        );
        assert!(s
            .read_file("reference/gone.md")
            .unwrap()
            .contains("find-me-token"));
        assert!(s.tree_index().contains("reference/gone.md"));
    }

    /// The resident stores always exist, and empty content is never a
    /// write — it is a delete, which has its own verb.
    #[test]
    fn the_resident_stores_cannot_be_deleted_and_empty_is_not_a_write() {
        let (_dir, s, _g) = scratch();
        s.write(StoreKind::Memory, &[]).unwrap();
        assert!(s.list().unwrap().iter().any(|f| f.path == "system/core.md"));
        assert_eq!(s.read_file("system/core.md").unwrap(), "");
        assert!(s.delete_file("system/core.md").is_err());
        assert!(s.write_file("reference/x.md", "").is_err());
        assert!(
            s.read_file("reference/x.md").is_err(),
            "nothing was recorded"
        );
    }

    /// A move is one transaction: the content is at the new path and not
    /// the old one, and a move onto an existing file changes nothing.
    #[test]
    fn a_move_is_atomic_and_refuses_an_existing_target() {
        let (_dir, s, _g) = scratch();
        s.write_file("reference/a.md", "alpha").unwrap();
        s.write_file("reference/b.md", "beta").unwrap();
        let before = s.log(100).unwrap().len();
        assert!(s.move_file("reference/a.md", "reference/b.md").is_err());
        assert_eq!(
            s.log(100).unwrap().len(),
            before,
            "a refused move records nothing"
        );
        assert_eq!(s.read_file("reference/b.md").unwrap(), "beta");

        s.move_file("reference/a.md", "projects/a.md").unwrap();
        assert!(s.read_file("reference/a.md").is_err());
        assert_eq!(s.read_file("projects/a.md").unwrap(), "alpha");
        assert!(s.move_file("system/core.md", "reference/core.md").is_err());
        assert!(s
            .move_file("reference/nothing.md", "reference/c.md")
            .is_err());
    }

    /// The optimistic write behind `veles memory edit` and the desktop:
    /// a stale base is a Conflict, and nothing lands.
    #[test]
    fn replace_if_refuses_a_stale_base() {
        let (_dir, s, _g) = scratch();
        // Creating: "it did not exist" is the base.
        assert!(s.replace_if("reference/e.md", "v1", None, "edit").unwrap());
        let sha1 = MemoryStore::content_sha("v1");
        // Someone (the agent) writes in between.
        s.write_file("reference/e.md", "agent v2").unwrap();
        let err = s
            .replace_if("reference/e.md", "my v2", Some(&sha1), "edit")
            .unwrap_err();
        let c = err
            .downcast_ref::<Conflict>()
            .expect("a Conflict, not a failure");
        assert_eq!(
            c.current_sha.as_deref(),
            Some(MemoryStore::content_sha("agent v2").as_str())
        );
        assert_eq!(s.read_file("reference/e.md").unwrap(), "agent v2");
        // Creating over an existing file is stale too.
        assert!(s.replace_if("reference/e.md", "x", None, "edit").is_err());
        // With the right base it lands.
        let sha2 = MemoryStore::content_sha("agent v2");
        assert!(s
            .replace_if("reference/e.md", "my v3", Some(&sha2), "edit")
            .unwrap());
        assert_eq!(s.read_file("reference/e.md").unwrap(), "my v3");
    }

    #[test]
    fn diff_shows_what_changed_between_revisions() {
        let (_dir, s, _g) = scratch();
        s.write_file("reference/d.md", "one\ntwo\nthree\n").unwrap();
        s.write_file("reference/d.md", "one\n2\nthree\n").unwrap();
        let d = s.diff("reference/d.md", None, None).unwrap();
        assert!(d.contains("-two\n") && d.contains("+2\n"), "{d}");
        let first = s
            .log_rows(Some("reference/d.md"), 10)
            .unwrap()
            .last()
            .unwrap()
            .rev;
        let d0 = s.diff("reference/d.md", None, Some(first)).unwrap();
        assert!(d0.contains("+two\n") && d0.contains("(nothing)"), "{d0}");
        assert!(s.diff("reference/none.md", None, None).is_err());
        assert!(
            s.content_at("reference/none.md", first).is_err(),
            "a revision of another path"
        );
    }

    /// export → import reproduces the live set exactly, and an export
    /// never writes into a directory that already holds something.
    #[test]
    fn export_then_import_round_trips() {
        let (_dir, s, _g) = scratch();
        s.write(StoreKind::Memory, &["m".into()]).unwrap();
        s.write_file("reference/r.md", "r").unwrap();
        s.write_file("projects/p/q.md", "q").unwrap();
        s.write_file("reference/gone.md", "g").unwrap();
        s.delete_file("reference/gone.md").unwrap();

        let out = tempfile::tempdir().unwrap();
        let n = s.export_to(&out.path().join("snap")).unwrap();
        assert_eq!(n, 3, "core, r and q — not the deleted one: {:?}", s.list());
        assert!(!out.path().join("snap/reference/gone.md").exists());
        assert!(
            s.export_to(&out.path().join("snap")).is_err(),
            "never into a non-empty dir"
        );

        let dir2 = tempfile::tempdir().unwrap();
        let other = MemoryStore::at(dir2.path().to_path_buf());
        let landed = other
            .import_from(&out.path().join("snap"), "import")
            .unwrap();
        assert_eq!(landed.len(), 3);
        let content = |st: &MemoryStore| -> Vec<(String, String)> {
            st.list()
                .unwrap()
                .into_iter()
                .map(|f| (f.path.clone(), st.read_file(&f.path).unwrap()))
                .collect()
        };
        assert_eq!(content(&s), content(&other));
        assert!(other
            .import_from(&out.path().join("snap"), "import")
            .unwrap()
            .is_empty());
    }

    /// Concurrent writers — the gateway and a `veles memory` command, or
    /// parallel tests on one home — must all land.
    ///
    /// Three lock hazards have gone red here, none of which busy_timeout
    /// alone covers:
    ///
    ///   * a deferred transaction's read→write upgrade is refused outright
    ///     (SQLITE_BUSY_SNAPSHOT) when another connection wrote in between
    ///     — `write_tx` is IMMEDIATE for that (2026-09-26);
    ///   * the COLD WAL conversion: the connection that converts the file
    ///     is not safe to write on, so `db()` converts on one of its own
    ///     and drops it (2026-09-30, first red in `nix flake check`);
    ///   * STARVATION: the busy handler polls rather than queues, and
    ///     writers that come straight back for the lock win between a
    ///     sleeper's looks. With this test's database on the real disk the
    ///     gate went red twice more on 2026-09-30, the second time with
    ///     the wait in the message: three threads failed their first
    ///     write after 103 s and one failed write 4 after 5.0 s, while
    ///     the other four wrote on. `write_conn`'s in-process turn queues
    ///     them instead, and `a_write_waits_for_this_process_s_turn` pins
    ///     that deterministically — this test only shows the result.
    ///
    /// Back on the scratch home (tmpfs under the gates): with writers in
    /// one process taking turns, the filesystem no longer decides
    /// anything here, and the build filesystem was the SSD.
    #[test]
    fn concurrent_writers_all_land() {
        let (dir, _s, _g) = scratch();
        let home = dir.path().to_path_buf();
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let home = home.clone();
                std::thread::spawn(move || {
                    let s = MemoryStore::at(home);
                    for i in 0..25 {
                        // How long the failing write waited says which
                        // hazard it was: ~0 is a refused promotion, ~5s
                        // is busy_timeout running out.
                        let t0 = std::time::Instant::now();
                        s.write_file(&format!("reference/t{t}/n{i}.md"), &format!("{t}/{i}"))
                            .map_err(|e| {
                                format!("thread {t} write {i} after {:?}: {e:#}", t0.elapsed())
                            })?;
                    }
                    Ok::<(), String>(())
                })
            })
            .collect();
        let errors: Vec<String> = threads
            .into_iter()
            .filter_map(|h| h.join().unwrap().err())
            .collect();
        assert!(errors.is_empty(), "{errors:#?}");
        assert_eq!(MemoryStore::at(home).list().unwrap().len(), 200);
    }

    /// A memory write leaves this process's lock on state.db alone.
    ///
    /// The gateway keeps a connection to state.db for its whole life, and
    /// in WAL mode that connection holds a POSIX read lock on the file
    /// between statements: it is how a connection closing in ANOTHER
    /// process (the `veles` CLI, a sqlite3 shell) knows it is not the last
    /// one, and must not checkpoint and delete the -wal and -shm. POSIX
    /// locks belong to the process, and closing ANY descriptor of the file
    /// drops all of them. `db()` read two header bytes through a plain
    /// `File::open` on every write, so after the first memory write the
    /// gateway held no lock, and the next process to close the database
    /// deleted the live WAL from under it (2026-10-01: the gateway went on
    /// writing to an unlinked WAL no new connection could see, and the
    /// memory tool failed with SQLITE_IOERR_SHORT_READ).
    ///
    /// Asked with an open file description lock, which conflicts with this
    /// process's own POSIX locks, so no second process is needed.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_memory_write_keeps_the_process_s_lock_on_the_database() {
        use std::os::fd::AsRawFd;
        let (dir, s, _g) = scratch();
        s.write_file("reference/warm.md", "warm").unwrap();
        let db = dir.path().join("state.db");
        // The gateway's connection, after a read.
        let gw = rusqlite::Connection::open(&db).unwrap();
        let _: i64 = gw
            .query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))
            .unwrap();
        // Open to the end of the test: closing it would drop the locks too.
        let probe = std::fs::File::open(&db).unwrap();
        let held = || {
            // SQLite's SHARED lock: 510 bytes from PENDING_BYTE + 2.
            // SAFETY: an all-zero flock is a valid value of a plain C struct.
            let mut fl: libc::flock = unsafe { std::mem::zeroed() };
            fl.l_type = libc::F_WRLCK as _;
            fl.l_whence = libc::SEEK_SET as _;
            fl.l_start = 0x4000_0002;
            fl.l_len = 510;
            // SAFETY: F_OFD_GETLK only reads and fills in `fl`.
            let rc = unsafe { libc::fcntl(probe.as_raw_fd(), libc::F_OFD_GETLK, &mut fl) };
            assert_eq!(rc, 0, "F_OFD_GETLK: {}", std::io::Error::last_os_error());
            fl.l_type != libc::F_UNLCK as libc::c_short
        };
        assert!(held(), "premise: a WAL connection keeps its read lock");
        // …and a warm file still takes the fast path, asked without a
        // descriptor of its own.
        let fresh = rusqlite::Connection::open(&db).unwrap();
        assert!(MemoryStore::schema_present(&fresh).unwrap() && MemoryStore::is_wal(&fresh));
        drop(fresh);
        s.write_file("reference/n.md", "n").unwrap();
        assert!(
            held(),
            "a memory write dropped the process's lock on state.db"
        );
        drop(gw);
    }

    /// A memory write waits for this process's write turn instead of
    /// racing SQLite's polling busy handler for the lock — the fix for the
    /// starvation `concurrent_writers_all_land` measured, pinned without
    /// depending on a race.
    #[test]
    fn a_write_waits_for_this_process_s_turn() {
        let (dir, s, _g) = scratch();
        // Warm the file first, so what is timed below is only the turn.
        s.write_file("reference/warm.md", "warm").unwrap();
        let turn = WRITE_TURN.lock().unwrap_or_else(|p| p.into_inner());
        let (done_tx, done) = std::sync::mpsc::channel();
        let home = dir.path().to_path_buf();
        let writer = std::thread::spawn(move || {
            let r = MemoryStore::at(home).write_file("reference/queued.md", "queued");
            let _ = done_tx.send(());
            r
        });
        assert!(
            done.recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "a write does not start while another holds the turn"
        );
        drop(turn);
        done.recv_timeout(std::time::Duration::from_secs(10))
            .expect("…and lands as soon as the turn is free");
        writer.join().unwrap().unwrap();
        assert_eq!(s.read_file("reference/queued.md").unwrap(), "queued");
    }

    /// The agent can prune and reorganize what it wrote: list, delete
    /// and move go through the same store calls as every other surface,
    /// and the two mutating ones need memory-write like write_file.
    #[tokio::test]
    async fn the_tool_lists_deletes_and_moves_files() {
        use crate::tools::Tool;
        let (_home, _env) = crate::testenv::scratch_home();
        let tool = MemoryTool {
            session_id: "memory-tool-files".into(),
        };
        let call = |v: serde_json::Value| tool.call(v);
        let out = call(
            serde_json::json!({"target": "memory", "action": "write_file",
            "path": "reference/a.md", "content": "alpha"}),
        )
        .await;
        assert!(out.starts_with("written"), "{out}");
        let out = call(
            serde_json::json!({"target": "memory", "action": "move_file",
            "path": "reference/a.md", "to": "projects/a.md"}),
        )
        .await;
        assert!(out.starts_with("moved"), "{out}");
        let out = call(serde_json::json!({"target": "memory", "action": "list"})).await;
        assert!(
            out.contains("projects/a.md") && !out.contains("reference/a.md"),
            "{out}"
        );
        let out = call(
            serde_json::json!({"target": "memory", "action": "delete_file",
            "path": "projects/a.md"}),
        )
        .await;
        assert!(out.starts_with("deleted"), "{out}");
        let out = call(serde_json::json!({"target": "memory", "action": "read",
            "path": "projects/a.md"}))
        .await;
        assert!(out.starts_with("error"), "{out}");
        let out = call(
            serde_json::json!({"target": "memory", "action": "delete_file",
            "path": "system/core.md"}),
        )
        .await;
        assert!(out.contains("cannot be deleted"), "{out}");
    }
}
