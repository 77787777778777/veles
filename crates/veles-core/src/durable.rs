//! The session store answers a write when SQLite has it in the write-ahead
//! log, and a worker completes it: the sync to the disk, and the fold of
//! the log into the database file.
//!
//! Why. Every commit used to be an fsync (`synchronous = FULL`), made
//! while the committer held both SQLite's write lock and the process-wide
//! store mutex. On the owner's laptop one small fsync takes 35 to 100 ms
//! with the disk idle and seconds with it busy, so under load every
//! surface queued behind whoever was committing: desktop sessions stalled,
//! the gateway's heartbeat and gossip failed, and a `veles chat -q` beside
//! the gateway died with "database is locked" when its five-second wait
//! ran out (2026-11-14, `docs/issues/open-items/release-0.2.6.md`; 100
//! commits measured at 3.5 to 6.7 s with the fsync and 0.1 to 0.3 s
//! without).
//!
//! What changes and what does not:
//!
//! - A commit on a store opened with async durability ([`enable`], which
//!   the `veles` binary calls) writes its frames and returns. Nothing
//!   waits for the disk while it holds a lock.
//! - The worker ([`Syncer`]) is told of every commit by SQLite's WAL hook
//!   and syncs the log soon after, several commits to one fsync. What a
//!   power cut or a hard reset can take is what was written in the last
//!   moments before it, never the database's consistency: SQLite replays
//!   the log up to its last whole commit.
//! - **Nothing leaves the node before it is on the disk.** A peer that has
//!   applied row 900 of this node's log must never see this node come back
//!   from a crash at row 880 and hand out 881 again. So the node-to-node
//!   client and server call [`barrier_async`] before a request is sent and
//!   before a reply is returned: it waits, holding no lock, until
//!   everything committed before the call is synced. That covers what
//!   other processes committed to the same file: the sync is of the whole
//!   log. It holds in a process that keeps the fsync on every commit too
//!   (`VELES_STORE_SYNC=full`), because a chat beside it may not.
//! - A sync that fails is never forgotten. After an fsync error the kernel
//!   may drop the pages it could not write and report success next time;
//!   from then on no barrier of this process passes.
//! - A process that never calls [`enable`] (every test, a library user)
//!   has no worker and no barrier: each commit is synced as it is made.
//!   The replicas and memory's own connection keep that either way.
//!
//! The worker also takes over the checkpoint. Registering a WAL hook
//! replaces SQLite's automatic one, which ran inside the committer's call
//! with the store mutex held. The worker folds the log on its own
//! connection; the hook folds it in place only if the log has grown far
//! past that, which means the worker is not keeping up.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

/// What a pass does to the disk. The real one is [`WalDisk`]; the tests
/// count and gate.
pub trait Disk: Send + 'static {
    /// Make every commit the write-ahead log holds now durable.
    fn sync(&mut self) -> std::io::Result<()>;
    /// Fold the log into the database file, as far as readers allow.
    fn checkpoint(&mut self) -> anyhow::Result<()>;
    /// A number that is different whenever any connection, in any
    /// process, has committed since it was last asked. `None` when that
    /// cannot be told, and then every pass syncs.
    fn version(&mut self) -> Option<i64> {
        None
    }
}

/// The worker's timing.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The least time between two syncs nobody is waiting for. Commits
    /// inside it share the next one. A barrier does not wait for it.
    pub lag: Duration,
    /// The log is folded once it holds this many frames.
    pub checkpoint_frames: u32,
    /// The least time between two folds.
    pub checkpoint_every: Duration,
    /// The hook folds the log itself at this many frames.
    pub hard_frames: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            // At most one sync a second that nobody waits for: what a
            // power cut can take is about that second. More often only
            // adds to the load on a disk that is already slow; under
            // load each sync took 0.1 to 3 s (2026-11-14).
            lag: Duration::from_secs(1),
            // 32 MB of log. SQLite's own automatic checkpoint folds at
            // 1000 frames; this is deliberately rarer. The first commit
            // after a complete fold restarts the log, and SQLite syncs
            // the new log's header inside that commit, under the write
            // lock, whatever `synchronous` says short of OFF: the one
            // sync left on the committer's path. With six chats writing
            // and a fold every thousand frames that was a hold of up to
            // 4.5 s every few seconds (2026-11-14).
            checkpoint_frames: 8000,
            checkpoint_every: Duration::from_secs(2),
            hard_frames: 32_000,
        }
    }
}

#[derive(Default)]
struct State {
    /// Passes begun and finished. They run one at a time, so `finished`
    /// is `begun` or one less.
    begun: u64,
    finished: u64,
    /// The highest pass a barrier is waiting for.
    wanted: u64,
    /// A commit by this process since the last pass began.
    dirty: bool,
    /// The log's length in frames at the last commit this process made.
    frames: u32,
    /// Why nothing this worker says can be trusted any more: a sync
    /// failed, or its thread died. Never cleared.
    broken: Option<String>,
    /// Stores holding this worker. At zero it syncs what is unsynced and
    /// stops.
    holders: usize,
    /// Barriers waiting now.
    waiting: usize,
    stop: bool,
    exited: bool,
}

pub(crate) struct Inner {
    state: Mutex<State>,
    /// The worker waits here for work.
    wake: Condvar,
    /// Blocking barriers wait here for a pass to finish.
    done: Condvar,
    /// Async barriers wait here: the number of passes finished.
    progress: tokio::sync::watch::Sender<u64>,
    opts: Options,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A commit was made. Called from SQLite's WAL hook, on the
    /// committer's thread, with the store mutex held: it takes this lock
    /// for a few instructions and never waits on the disk.
    pub(crate) fn note_commit(&self, frames: u32) {
        let mut st = self.lock();
        st.dirty = true;
        st.frames = frames;
        drop(st);
        self.wake.notify_one();
    }

    /// The frame count at which the hook folds the log itself.
    pub(crate) fn hard_frames(&self) -> u32 {
        self.opts.hard_frames
    }

    /// Whether the worker has stopped.
    #[cfg(test)]
    pub(crate) fn has_exited(&self) -> bool {
        self.lock().exited
    }

    /// Ask for a pass that begins from now on. A pass in flight may have
    /// begun before the caller's commit, so it does not count. `Ok(None)`:
    /// the worker has stopped and cannot be asked.
    fn ask(&self) -> Result<Option<u64>, String> {
        let mut st = self.lock();
        if let Some(why) = &st.broken {
            return Err(why.clone());
        }
        if st.exited {
            return Ok(None);
        }
        let want = st.begun + 1;
        if st.wanted < want {
            st.wanted = want;
        }
        drop(st);
        self.wake.notify_one();
        Ok(Some(want))
    }

    /// Where a barrier that asked for pass `want` stands: `Some(Ok(true))`
    /// it has finished, `Some(Ok(false))` the worker stopped first,
    /// `Some(Err)` nothing can be vouched for, `None` not yet.
    fn outcome(st: &State, want: u64) -> Option<Result<bool, String>> {
        if let Some(why) = &st.broken {
            return Some(Err(why.clone()));
        }
        if st.finished >= want {
            return Some(Ok(true));
        }
        if st.exited {
            return Some(Ok(false));
        }
        None
    }

    /// Wait for a sync that begins after this call. `Ok(false)`: the
    /// worker stopped before it could run one.
    fn barrier(&self) -> Result<bool, String> {
        let Some(want) = self.ask()? else {
            return Ok(false);
        };
        let mut st = self.lock();
        st.waiting += 1;
        let out = loop {
            if let Some(out) = Self::outcome(&st, want) {
                break out;
            }
            st = self.done.wait(st).unwrap_or_else(|p| p.into_inner());
        };
        st.waiting -= 1;
        out
    }

    /// [`Inner::barrier`] without a thread to wait on.
    async fn barrier_async(&self) -> Result<bool, String> {
        let mut seen = self.progress.subscribe();
        let Some(want) = self.ask()? else {
            return Ok(false);
        };
        loop {
            // Scoped: the guard is gone before the await.
            let out = {
                let st = self.lock();
                Self::outcome(&st, want)
            };
            if let Some(out) = out {
                return out;
            }
            if seen.changed().await.is_err() {
                return Ok(false);
            }
        }
    }

    /// Wait for this process's own commits to reach the disk, and for
    /// nothing else: a process that wrote nothing returns at once.
    fn flush(&self) -> Result<(), String> {
        {
            let st = self.lock();
            if let Some(why) = &st.broken {
                return Err(why.clone());
            }
            if st.exited || (!st.dirty && st.begun == st.finished) {
                return Ok(());
            }
        }
        self.barrier().map(|_| ())
    }
}

/// Marks the worker gone however its thread ends, so that a barrier never
/// waits for a worker that panicked, and never passes on its word.
struct Gone(Arc<Inner>);

impl Drop for Gone {
    fn drop(&mut self) {
        let mut st = self.0.lock();
        st.exited = true;
        if std::thread::panicking() && st.broken.is_none() {
            st.broken = Some("its sync worker died".into());
        }
        let finished = st.finished;
        drop(st);
        self.0.done.notify_all();
        self.0.progress.send_replace(finished);
    }
}

fn run(inner: Arc<Inner>, mut disk: Box<dyn Disk>) {
    let _gone = Gone(inner.clone());
    let mut last_start: Option<Instant> = None;
    let mut last_fold: Option<Instant> = None;
    // The store's version when the last sync that succeeded began.
    let mut synced_version: Option<i64> = None;
    loop {
        let (pass, was_dirty, fold, stopping) = {
            let mut st = inner.lock();
            loop {
                if st.wanted > st.begun {
                    break;
                }
                if st.stop {
                    if st.dirty {
                        break;
                    }
                    // Nothing was written since the last sync began: the
                    // last store closed with everything on the disk.
                    return;
                }
                if st.dirty {
                    let due = last_start.map(|t| t + inner.opts.lag);
                    let now = Instant::now();
                    match due {
                        Some(due) if now < due => {
                            st = inner
                                .wake
                                .wait_timeout(st, due - now)
                                .unwrap_or_else(|p| p.into_inner())
                                .0;
                        }
                        _ => break,
                    }
                } else {
                    st = inner.wake.wait(st).unwrap_or_else(|p| p.into_inner());
                }
            }
            st.begun += 1;
            let was_dirty = std::mem::take(&mut st.dirty);
            let fold = st.frames >= inner.opts.checkpoint_frames
                && last_fold.is_none_or(|t| t.elapsed() >= inner.opts.checkpoint_every);
            if fold {
                // The next commit reports the log's real length again.
                st.frames = 0;
            }
            (st.begun, was_dirty, fold, st.stop)
        };
        last_start = Some(Instant::now());
        // A barrier with nothing new behind it costs no sync: if no
        // connection anywhere has committed since the last sync began,
        // that sync covered everything there is. A commit of our own
        // always syncs, whatever the version says.
        let version = disk.version();
        let unchanged = !was_dirty && version.is_some() && version == synced_version;
        let synced = if unchanged { Ok(()) } else { disk.sync() };
        match &synced {
            Ok(()) => synced_version = version,
            Err(e) => {
                synced_version = None;
                tracing::warn!(
                    "store: the write-ahead log did not sync ({e}); nothing more leaves this \
                     node until it is restarted"
                );
            }
        }
        let fold = fold && synced.is_ok();
        {
            let mut st = inner.lock();
            st.finished = pass;
            if let Err(e) = &synced {
                // Never cleared. After a failed fsync the kernel may mark
                // the pages it could not write clean, and the next fsync
                // then succeeds without them.
                st.broken
                    .get_or_insert_with(|| format!("a sync of its write-ahead log failed: {e}"));
            }
        }
        // Whoever waited for this sync goes on now: the fold that may
        // follow writes the database file and syncs it, which can take
        // seconds and is nobody's barrier.
        inner.done.notify_all();
        inner.progress.send_replace(pass);
        if fold {
            last_fold = Some(Instant::now());
            if let Err(e) = disk.checkpoint() {
                tracing::warn!("store: the write-ahead log was not folded: {e:#}");
            }
        }
        if stopping {
            return;
        }
    }
}

/// A store's hold on its worker. The last one to drop stops it, after one
/// more sync if anything was written since the last, and waits for it to
/// have stopped: the store's connection closes after this, and the
/// worker must not have the log open when SQLite removes it.
pub struct Syncer {
    inner: Arc<Inner>,
}

impl Syncer {
    /// Start a worker on `disk`.
    pub fn spawn(disk: Box<dyn Disk>, opts: Options) -> std::io::Result<Self> {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                holders: 1,
                ..State::default()
            }),
            wake: Condvar::new(),
            done: Condvar::new(),
            progress: tokio::sync::watch::channel(0).0,
            opts,
        });
        let theirs = inner.clone();
        std::thread::Builder::new()
            .name("store-sync".into())
            .spawn(move || run(theirs, disk))?;
        Ok(Self { inner })
    }

    /// Another hold on the same worker, or None if it is stopping.
    fn another(inner: &Arc<Inner>) -> Option<Self> {
        let mut st = inner.lock();
        if st.stop || st.exited {
            return None;
        }
        st.holders += 1;
        drop(st);
        Some(Self {
            inner: inner.clone(),
        })
    }

    pub(crate) fn inner(&self) -> &Arc<Inner> {
        &self.inner
    }

    /// Wait for a sync that begins after this call.
    pub fn barrier(&self) -> anyhow::Result<()> {
        self.inner.barrier().map(|_| ()).map_err(not_durable)
    }

    /// A commit was made (what the WAL hook says).
    pub fn note_commit(&self, frames: u32) {
        self.inner.note_commit(frames);
    }

    /// Passes finished so far.
    pub fn passes(&self) -> u64 {
        self.inner.lock().finished
    }

    /// Barriers waiting now.
    pub fn waiting(&self) -> usize {
        self.inner.lock().waiting
    }
}

impl Clone for Syncer {
    fn clone(&self) -> Self {
        self.inner.lock().holders += 1;
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Drop for Syncer {
    fn drop(&mut self) {
        let mut st = self.inner.lock();
        st.holders = st.holders.saturating_sub(1);
        if st.holders > 0 {
            return;
        }
        st.stop = true;
        self.inner.wake.notify_one();
        while !st.exited {
            st = self.inner.done.wait(st).unwrap_or_else(|p| p.into_inner());
        }
    }
}

fn not_durable(e: String) -> anyhow::Error {
    anyhow::anyhow!("the session store is not known to be on the disk: {e}")
}

// ── the real disk ────────────────────────────────────────────────────

/// A store file's write-ahead log, synced with the file system's own call
/// and folded on a connection of the worker's own.
pub struct WalDisk {
    db: PathBuf,
    wal: PathBuf,
    conn: Option<rusqlite::Connection>,
}

impl WalDisk {
    pub fn new(db: &Path) -> Self {
        let mut wal = db.as_os_str().to_owned();
        wal.push("-wal");
        Self {
            db: db.to_path_buf(),
            wal: PathBuf::from(wal),
            conn: None,
        }
    }

    /// The worker's own connection, opened when first needed. Never
    /// creating: a store file that was moved away must not come back
    /// empty because a worker looked at it.
    fn conn(&mut self) -> rusqlite::Result<&rusqlite::Connection> {
        if self.conn.is_none() {
            self.conn = Some(rusqlite::Connection::open_with_flags(
                &self.db,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?);
        }
        Ok(self.conn.as_ref().expect("just opened"))
    }
}

impl Disk for WalDisk {
    /// fsync the log, which is what a `synchronous = FULL` commit does
    /// after it has written its frames. The file is opened here and not
    /// through SQLite. That is safe because SQLite takes its locks on the
    /// database file and on the `-shm` file and none on the log (its unix
    /// VFS opens the log without locking), so closing this descriptor
    /// drops nobody's lock; closing one on either of the other two would
    /// (`Store::open_replica_ro`). It is enough because an fsync covers
    /// the file whichever descriptor wrote it, in whichever process.
    fn sync(&mut self) -> std::io::Result<()> {
        let mut open = std::fs::OpenOptions::new();
        // FlushFileBuffers wants a handle opened for writing; fsync does
        // not care.
        if cfg!(windows) {
            open.write(true);
        } else {
            open.read(true);
        }
        match open.open(&self.wal) {
            Ok(f) => f.sync_data(),
            // No log: nothing has been written since the last connection
            // closed, and that close folded and synced everything.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn checkpoint(&mut self) -> anyhow::Result<()> {
        // PASSIVE never takes the write lock and never waits for a
        // reader: it folds what it can and leaves the rest for next time.
        self.conn()?
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// SQLite's `data_version`: on one connection it changes whenever
    /// another connection, in this process or any other, has committed.
    fn version(&mut self) -> Option<i64> {
        let got = self
            .conn()
            .and_then(|c| c.query_row("PRAGMA data_version", [], |r| r.get(0)));
        if got.is_err() {
            // Ask a fresh connection next time; until one answers, every
            // pass syncs.
            self.conn = None;
        }
        got.ok()
    }
}

// ── the process's workers ────────────────────────────────────────────

/// Whether this process takes part at all: its stores get a worker and
/// its barriers sync. The `veles` binary does; a test does not.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Whether its stores also answer a commit before it is synced.
static RELAXED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct Registry {
    workers: Vec<(PathBuf, Weak<Inner>)>,
    /// Every store file this process has had a worker for. A barrier
    /// syncs each of them, with the worker if one is alive and by itself
    /// if not.
    known: Vec<PathBuf>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    workers: Vec::new(),
    known: Vec::new(),
});

/// The variable that keeps the fsync on every commit, in the environment
/// or in the node's `.env`: `VELES_STORE_SYNC=full`.
pub const SYNC_ENV: &str = "VELES_STORE_SYNC";

/// Give the stores this process opens from now on a sync worker, and
/// answer their commits before they are synced unless
/// `VELES_STORE_SYNC=full` (the environment, else the node's `.env`) says
/// to keep the fsync on every commit. The `veles` binary calls this
/// first thing; a library user and the tests that do not call it get
/// neither.
pub fn enable() {
    ACTIVE.store(true, Ordering::SeqCst);
    RELAXED.store(
        wants_async(
            std::env::var(SYNC_ENV)
                .ok()
                .or_else(|| crate::authn::dotenv_value(SYNC_ENV))
                .as_deref(),
        ),
        Ordering::SeqCst,
    );
}

/// What `VELES_STORE_SYNC` asks for: anything but `full` is the default.
fn wants_async(env: Option<&str>) -> bool {
    !env.is_some_and(|v| v.trim().eq_ignore_ascii_case("full"))
}

/// Whether stores opened now answer a commit before it is synced.
pub fn relaxes() -> bool {
    ACTIVE.load(Ordering::SeqCst) && RELAXED.load(Ordering::SeqCst)
}

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|p| p.into_inner())
}

/// The worker for the store at `db`, started if this is the first store
/// on that file. None in a process that did not call [`enable`], or when
/// the worker's thread could not start: the store then keeps its fsync,
/// and a barrier syncs the file by itself.
pub fn attach(db: &Path) -> Option<Syncer> {
    if !ACTIVE.load(Ordering::SeqCst) {
        return None;
    }
    attach_with(db, || Box::new(WalDisk::new(db)), Options::default())
}

/// [`attach`] with the disk and the timing given.
pub fn attach_with(
    db: &Path,
    disk: impl FnOnce() -> Box<dyn Disk>,
    opts: Options,
) -> Option<Syncer> {
    let key = std::fs::canonicalize(db).unwrap_or_else(|_| db.to_path_buf());
    let mut reg = registry();
    if !reg.known.contains(&key) {
        reg.known.push(key.clone());
    }
    reg.workers.retain(|(_, w)| w.strong_count() > 0);
    for (path, weak) in reg.workers.iter() {
        if *path == key {
            if let Some(s) = weak.upgrade().as_ref().and_then(Syncer::another) {
                return Some(s);
            }
        }
    }
    match Syncer::spawn(disk(), opts) {
        Ok(s) => {
            reg.workers.push((key, Arc::downgrade(&s.inner)));
            Some(s)
        }
        Err(e) => {
            tracing::warn!(
                "store: no sync worker for {} ({e}); it keeps the fsync on every commit",
                db.display()
            );
            None
        }
    }
}

/// Each store file this process knows, with its worker if one is alive
/// and has not stopped.
fn stores() -> Vec<(PathBuf, Vec<Arc<Inner>>)> {
    let reg = registry();
    reg.known
        .iter()
        .map(|path| {
            let live = reg
                .workers
                .iter()
                .filter(|(p, _)| p == path)
                .filter_map(|(_, w)| w.upgrade())
                // A worker that has stopped is still reachable for the
                // moment its thread takes to let go of its state: it can
                // sync nothing, so it does not count as alive. (The first
                // public CI run of 0.2.6 failed on a test that looked
                // here in that moment.)
                .filter(|w| !w.lock().exited)
                .collect();
            (path.clone(), live)
        })
        .collect()
}

/// Sync a store file's log from the caller's thread: the barrier of a
/// file that has no worker alive.
fn sync_alone(path: &Path) -> anyhow::Result<()> {
    WalDisk::new(path)
        .sync()
        .map_err(|e| not_durable(format!("{}: {e}", path.display())))
}

/// Wait until everything committed to this process's stores before the
/// call is on the disk. Holds no lock while it waits. For each store file
/// the process has opened: a sync by its worker that begins after the
/// call, or, with no worker alive, a sync from here. In a process that
/// never called [`enable`] there is nothing to wait for (every commit was
/// synced when it was made) and this returns at once.
///
/// The async form, [`barrier_async`], is what node-to-node code calls.
pub fn barrier() -> anyhow::Result<()> {
    for (path, workers) in stores() {
        let mut synced = false;
        for w in workers {
            synced |= w.barrier().map_err(not_durable)?;
        }
        if !synced {
            sync_alone(&path)?;
        }
    }
    Ok(())
}

/// [`barrier`] for async code. No thread waits: the worker tells the
/// task when its pass is done.
pub async fn barrier_async() -> anyhow::Result<()> {
    for (path, workers) in stores() {
        let mut synced = false;
        for w in workers {
            synced |= w.barrier_async().await.map_err(not_durable)?;
        }
        if !synced {
            crate::rt::spawn_blocking(move || sync_alone(&path))
                .await
                .map_err(|e| anyhow::anyhow!("the store sync was interrupted: {e}"))??;
        }
    }
    Ok(())
}

/// [`barrier_async`], given up after `patience`. For a request that has
/// not left yet: nothing has been said to anyone, so failing is safe, and
/// a disk that does not answer must not hold every message to every peer
/// for ever.
pub async fn barrier_within(patience: Duration) -> anyhow::Result<()> {
    within(patience, barrier_async()).await
}

async fn within<T>(
    patience: Duration,
    wait: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    match tokio::time::timeout(patience, wait).await {
        Ok(done) => done,
        Err(_) => Err(not_durable(format!(
            "the sync did not finish within {} s",
            patience.as_secs()
        ))),
    }
}

/// Wait for this process's own commits to reach the disk. A process that
/// wrote nothing does not touch the disk.
pub fn flush() -> anyhow::Result<()> {
    for (_, workers) in stores() {
        for w in workers {
            w.flush().map_err(not_durable)?;
        }
    }
    Ok(())
}

/// Flushes when it is dropped: the last thing a command does before the
/// process ends, on every way out of `main` that unwinds.
pub struct FlushOnExit;

impl Drop for FlushOnExit {
    fn drop(&mut self) {
        if let Err(e) = flush() {
            tracing::warn!("{e:#}");
        }
    }
}

/// `std::process::exit` for a command that may have written to the
/// store: it skips every destructor, [`FlushOnExit`] included, so the
/// flush is made here.
pub fn exit(code: i32) -> ! {
    if let Err(e) = flush() {
        tracing::warn!("{e:#}");
    }
    std::process::exit(code)
}

/// A disk for tests, here and in the crates that test against a worker
/// (`veles_core::testenv` is the precedent for test support that ships).
#[doc(hidden)]
pub mod testing {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc;

    type Gate = Arc<Mutex<Option<mpsc::Receiver<()>>>>;

    /// A disk that counts, and can be made to wait or to fail.
    #[derive(Clone, Default)]
    pub struct Counting {
        pub syncs: Arc<AtomicU64>,
        pub folds: Arc<AtomicU64>,
        /// Folds that have returned.
        pub folded: Arc<AtomicU64>,
        pub fail: Arc<AtomicBool>,
        /// What `version` answers, when set.
        pub version: Arc<Mutex<Option<i64>>>,
        /// When set, a sync waits for one message before it returns.
        pub gate: Gate,
        /// When set, a fold waits for one message before it returns.
        pub fold_gate: Gate,
        /// Called as each sync begins.
        #[allow(clippy::type_complexity)]
        pub on_sync: Arc<Mutex<Option<Box<dyn FnMut() + Send>>>>,
    }

    impl Counting {
        pub fn gated() -> (Self, mpsc::Sender<()>) {
            let (tx, rx) = mpsc::channel();
            let me = Self::default();
            *me.gate.lock().unwrap() = Some(rx);
            (me, tx)
        }
        pub fn syncs(&self) -> u64 {
            self.syncs.load(Ordering::SeqCst)
        }
        pub fn folds(&self) -> u64 {
            self.folds.load(Ordering::SeqCst)
        }
    }

    impl Disk for Counting {
        fn sync(&mut self) -> std::io::Result<()> {
            if let Some(f) = self.on_sync.lock().unwrap().as_mut() {
                f();
            }
            self.syncs.fetch_add(1, Ordering::SeqCst);
            if let Some(rx) = self.gate.lock().unwrap().as_ref() {
                let _ = rx.recv();
            }
            if self.fail.load(Ordering::SeqCst) {
                return Err(std::io::Error::other("the disk said no"));
            }
            Ok(())
        }
        fn checkpoint(&mut self) -> anyhow::Result<()> {
            self.folds.fetch_add(1, Ordering::SeqCst);
            if let Some(rx) = self.fold_gate.lock().unwrap().as_ref() {
                let _ = rx.recv();
            }
            self.folded.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn version(&mut self) -> Option<i64> {
            *self.version.lock().unwrap()
        }
    }

    pub fn fast() -> Options {
        Options {
            lag: Duration::from_millis(20),
            checkpoint_frames: 50,
            checkpoint_every: Duration::ZERO,
            hard_frames: 400,
        }
    }

    /// Poll `cond` for up to ten seconds: long enough for a thread to be
    /// scheduled on a machine that is doing everything else as well.
    pub fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("never happened: {what}");
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn finished(h: &std::thread::JoinHandle<anyhow::Result<()>>) -> bool {
        h.is_finished()
    }

    /// The property the replication rests on. A sync that was already
    /// running when the barrier was called may have begun before the
    /// caller's commit, so it does not count: the barrier returns after
    /// the next one.
    #[test]
    fn a_barrier_waits_for_a_sync_that_began_after_it() {
        let (disk, release) = Counting::gated();
        let s = Syncer::spawn(Box::new(disk.clone()), fast()).unwrap();
        // Rebound after the worker, so dropped before it: a test that
        // fails with a sync held ends instead of waiting for the worker.
        let release = release;
        s.note_commit(1);
        eventually("the first sync begins", || disk.syncs() == 1);

        let theirs = s.clone();
        let waiter = std::thread::spawn(move || theirs.barrier());
        eventually("the barrier waits", || s.waiting() == 1);
        assert!(!finished(&waiter), "nothing has finished yet");

        release.send(()).unwrap();
        eventually("the second sync begins", || disk.syncs() == 2);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !finished(&waiter),
            "the sync that was in flight when the barrier was called is not the caller's"
        );

        release.send(()).unwrap();
        waiter.join().unwrap().unwrap();
        assert_eq!(disk.syncs(), 2);
        // The gate goes before the worker's last holder: a worker that
        // synced once more on its way out would wait on it for ever.
        drop(release);
    }

    #[test]
    fn barriers_that_wait_together_share_one_sync() {
        let (disk, release) = Counting::gated();
        let s = Syncer::spawn(Box::new(disk.clone()), fast()).unwrap();
        // Rebound after the worker, so dropped before it: a test that
        // fails with a sync held ends instead of waiting for the worker.
        let release = release;
        s.note_commit(1);
        eventually("the first sync begins", || disk.syncs() == 1);
        let waiters: Vec<_> = (0..5)
            .map(|_| {
                let theirs = s.clone();
                std::thread::spawn(move || theirs.barrier())
            })
            .collect();
        eventually("all five are waiting", || s.waiting() == 5);
        release.send(()).unwrap();
        release.send(()).unwrap();
        for w in waiters {
            w.join().unwrap().unwrap();
        }
        assert_eq!(disk.syncs(), 2, "five barriers, one sync between them");
        drop(release);
    }

    /// After an fsync error the kernel may mark the pages it could not
    /// write clean, and the next fsync then reports success without
    /// them. So one failure is the end of this worker's word: a later
    /// sync that "works" vouches for nothing.
    #[test]
    fn a_sync_that_fails_is_never_forgotten() {
        let disk = Counting::default();
        let s = Syncer::spawn(Box::new(disk.clone()), fast()).unwrap();
        disk.fail.store(true, Ordering::SeqCst);
        let err = s.barrier().unwrap_err().to_string();
        assert!(err.contains("not known to be on the disk"), "{err}");
        assert!(err.contains("the disk said no"), "{err}");

        // A later sync that "works", and the failure still stands.
        disk.fail.store(false, Ordering::SeqCst);
        s.note_commit(1);
        eventually("the next sync runs", || s.passes() >= 2);
        let again = s.barrier().unwrap_err().to_string();
        assert!(again.contains("the disk said no"), "{again}");
        assert!(s.inner.flush().is_err(), "a flush does not pass either");
    }

    #[tokio::test]
    async fn the_async_barrier_waits_for_the_same_sync_and_fails_the_same_way() {
        let (disk, release) = Counting::gated();
        let s = Syncer::spawn(Box::new(disk.clone()), fast()).unwrap();
        // Rebound after the worker, so dropped before it: a test that
        // fails with a sync held ends instead of waiting for the worker.
        let release = release;
        let inner = s.inner.clone();
        let mut waiting = std::pin::pin!(inner.barrier_async());
        let early = tokio::time::timeout(Duration::from_millis(100), &mut waiting).await;
        assert!(early.is_err(), "it returned while the sync was running");
        assert_eq!(disk.syncs(), 1);
        release.send(()).unwrap();
        assert_eq!(waiting.await, Ok(true));

        disk.fail.store(true, Ordering::SeqCst);
        drop(release);
        let err = inner.barrier_async().await.unwrap_err();
        assert!(err.contains("the disk said no"), "{err}");
    }

    /// A disk that does not answer must not hold a request for ever:
    /// nothing has been said to the peer yet, so giving up is safe.
    #[tokio::test(start_paused = true)]
    async fn a_sync_that_never_ends_fails_the_wait_and_says_why() {
        let (disk, held) = Counting::gated();
        let s = Syncer::spawn(Box::new(disk), fast()).unwrap();
        let held = held;
        let inner = s.inner.clone();
        let waited = within(Duration::from_secs(120), async {
            inner.barrier_async().await.map_err(not_durable)
        })
        .await;
        let err = waited.unwrap_err().to_string();
        assert!(err.contains("did not finish within 120 s"), "{err}");
        // Let the worker go before its last holder waits for it.
        drop(held);
    }

    #[test]
    fn a_commit_is_synced_without_anyone_asking() {
        let disk = Counting::default();
        let s = Syncer::spawn(Box::new(disk.clone()), fast()).unwrap();
        assert_eq!(disk.syncs(), 0, "an idle worker leaves the disk alone");
        s.note_commit(3);
        eventually("the commit is synced", || s.passes() == 1);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(disk.syncs(), 1, "and it is synced once");
    }

    #[test]
    fn commits_in_a_burst_share_syncs() {
        let disk = Counting::default();
        let opts = Options {
            lag: Duration::from_millis(100),
            ..fast()
        };
        let s = Syncer::spawn(Box::new(disk.clone()), opts).unwrap();
        let began = Instant::now();
        for _ in 0..200 {
            s.note_commit(1);
            std::thread::sleep(Duration::from_millis(1));
        }
        s.barrier().unwrap();
        // One sync per lag at most, however long the machine took over
        // the two hundred commits, and two more for the first and the
        // barrier's.
        let most = began.elapsed().as_millis() as u64 / 100 + 3;
        assert!(
            disk.syncs() <= most,
            "200 commits in {:?} cost {} syncs",
            began.elapsed(),
            disk.syncs()
        );
    }

    /// Most messages between nodes follow nothing new. If no connection
    /// anywhere has committed since the last sync began, that sync
    /// covered everything there is.
    #[test]
    fn a_barrier_with_nothing_new_behind_it_costs_no_sync() {
        let disk = Counting::default();
        *disk.version.lock().unwrap() = Some(7);
        let s = Syncer::spawn(Box::new(disk.clone()), fast()).unwrap();
        s.barrier().unwrap();
        assert_eq!(
            disk.syncs(),
            1,
            "the first barrier knows of no earlier sync"
        );
        s.barrier().unwrap();
        s.barrier().unwrap();
        assert_eq!(disk.syncs(), 1, "nothing was committed in between");

        // Another connection, perhaps another process, commits.
        *disk.version.lock().unwrap() = Some(8);
        s.barrier().unwrap();
        assert_eq!(disk.syncs(), 2);

        // A commit of this process's own syncs whatever the version says.
        s.note_commit(1);
        s.barrier().unwrap();
        assert!(disk.syncs() >= 3, "{}", disk.syncs());
    }

    #[test]
    fn a_disk_that_cannot_tell_its_version_syncs_for_every_barrier() {
        let disk = Counting::default();
        let s = Syncer::spawn(Box::new(disk.clone()), fast()).unwrap();
        s.barrier().unwrap();
        s.barrier().unwrap();
        assert_eq!(disk.syncs(), 2);
    }

    #[test]
    fn a_long_log_is_folded_and_a_short_one_is_not() {
        let disk = Counting::default();
        let s = Syncer::spawn(Box::new(disk.clone()), fast()).unwrap();
        // A fold follows its sync after the barrier is released, so "no
        // fold" is read after one more pass: the worker has one thread,
        // and the next pass begins when the last one's fold is over.
        let settled = || {
            s.barrier().unwrap();
            s.barrier().unwrap();
            disk.folds()
        };
        s.note_commit(49);
        assert_eq!(settled(), 0);
        s.note_commit(50);
        assert_eq!(settled(), 1);
        // Folded: until a commit reports a long log again there is
        // nothing to fold.
        assert_eq!(settled(), 1);
    }

    /// The fold writes the database file and syncs it: seconds on a busy
    /// disk. A barrier is waiting for its pass's sync and nothing else.
    /// (One that arrives while a fold is running does wait for it: the
    /// worker has one thread, and the next sync begins after the fold.)
    #[test]
    fn a_barrier_does_not_wait_for_the_fold_of_its_own_pass() {
        let (disk, release) = Counting::gated();
        let (hold, held) = std::sync::mpsc::channel();
        *disk.fold_gate.lock().unwrap() = Some(held);
        let s = Syncer::spawn(Box::new(disk.clone()), fast()).unwrap();
        let (release, hold) = (release, hold);
        // A first pass, held in its sync, so that the barrier's pass is
        // the next one and the long log is seen when that one begins.
        s.note_commit(1);
        eventually("the first sync begins", || disk.syncs() == 1);
        let theirs = s.clone();
        let waiter = std::thread::spawn(move || theirs.barrier());
        eventually("the barrier waits", || s.waiting() == 1);
        s.note_commit(50);
        release.send(()).unwrap();
        release.send(()).unwrap();
        eventually("the barrier returns with the fold held", || {
            waiter.is_finished()
        });
        waiter.join().unwrap().unwrap();
        eventually("the fold begins", || disk.folds() == 1);
        assert_eq!(
            disk.folded.load(Ordering::SeqCst),
            0,
            "the barrier returned while the fold was still running"
        );
        drop(hold);
        eventually("the fold ends", || disk.folded.load(Ordering::SeqCst) == 1);
    }

    #[test]
    fn a_log_that_did_not_sync_is_not_folded() {
        let disk = Counting::default();
        let s = Syncer::spawn(Box::new(disk.clone()), fast()).unwrap();
        disk.fail.store(true, Ordering::SeqCst);
        s.note_commit(500);
        assert!(s.barrier().is_err());
        // One more pass, so that a fold of the failed one would be over.
        s.note_commit(500);
        eventually("the next pass runs", || s.passes() >= 2);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(disk.folds(), 0, "a pass whose sync failed folds nothing");
    }

    #[test]
    fn the_last_holder_leaving_syncs_what_is_unsynced_and_stops_the_worker() {
        let disk = Counting::default();
        let opts = Options {
            lag: Duration::from_secs(60),
            ..fast()
        };
        let s = Syncer::spawn(Box::new(disk.clone()), opts).unwrap();
        let other = s.clone();
        let inner = s.inner.clone();
        s.note_commit(1);
        eventually("the first commit is synced", || disk.syncs() == 1);
        // A second commit, inside the lag: nothing syncs it but the stop.
        s.note_commit(2);
        drop(s);
        std::thread::sleep(Duration::from_millis(40));
        assert!(!inner.lock().exited, "one holder is left");
        assert_eq!(disk.syncs(), 1);
        // The drop returns when the worker has stopped: whatever closes
        // after it (the store's connection) finds the log let go of.
        drop(other);
        assert!(inner.lock().exited, "the last holder waited for the worker");
        assert_eq!(disk.syncs(), 2, "one last sync on the way out");
        // Nobody is left to ask.
        assert_eq!(inner.barrier(), Ok(false));
        assert!(Syncer::another(&inner).is_none());
    }

    #[test]
    fn a_worker_with_nothing_unsynced_stops_without_touching_the_disk() {
        let disk = Counting::default();
        let s = Syncer::spawn(Box::new(disk.clone()), fast()).unwrap();
        let inner = s.inner.clone();
        s.note_commit(1);
        s.barrier().unwrap();
        let before = disk.syncs();
        drop(s);
        assert!(inner.lock().exited);
        assert_eq!(disk.syncs(), before);
    }

    /// A worker whose thread died must not leave barriers waiting for
    /// ever, and must not let them pass either: nothing is syncing.
    #[test]
    fn a_worker_that_dies_fails_whoever_waits_for_it() {
        struct Dies;
        impl Disk for Dies {
            fn sync(&mut self) -> std::io::Result<()> {
                panic!("the worker's thread dies here (expected in this test)");
            }
            fn checkpoint(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
        }
        let s = Syncer::spawn(Box::new(Dies), fast()).unwrap();
        let err = s.barrier().unwrap_err().to_string();
        assert!(err.contains("worker died"), "{err}");
        assert!(s.barrier().is_err(), "and it stays failed");
    }

    #[test]
    fn a_process_that_wrote_nothing_does_not_touch_the_disk_when_it_flushes() {
        let disk = Counting::default();
        let s = Syncer::spawn(Box::new(disk.clone()), fast()).unwrap();
        s.inner.flush().unwrap();
        assert_eq!(disk.syncs(), 0);
        s.note_commit(1);
        s.inner.flush().unwrap();
        assert!(disk.syncs() >= 1);
        assert!(!s.inner.lock().dirty);
    }

    #[test]
    fn stores_on_one_file_share_a_worker_and_other_files_have_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.db");
        let b = dir.path().join("b.db");
        std::fs::write(&a, b"").unwrap();
        std::fs::write(&b, b"").unwrap();
        let disk_a = Counting::default();
        let d = disk_a.clone();
        let one = attach_with(&a, move || Box::new(d), fast()).unwrap();
        let two = attach_with(&a, || panic!("a second worker for one file"), fast()).unwrap();
        let three = attach_with(&b, || Box::new(Counting::default()), fast()).unwrap();
        assert!(Arc::ptr_eq(&one.inner, &two.inner));
        assert!(!Arc::ptr_eq(&one.inner, &three.inner));
        two.barrier().unwrap();
        assert!(disk_a.syncs() >= 1);

        // A file whose stores have all closed gets a new worker.
        let old = one.inner.clone();
        drop(one);
        drop(two);
        assert!(old.lock().exited);
        let d = disk_a.clone();
        let again = attach_with(&a, move || Box::new(d), fast()).unwrap();
        assert!(!Arc::ptr_eq(&old, &again.inner));
    }

    /// A file this process once had a store on is still synced by its
    /// barrier when no store is open on it any more: another process may
    /// have committed to it since, and this one may be about to send
    /// what it read there.
    #[test]
    fn a_barrier_syncs_a_known_file_that_has_no_worker_alive() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE t(a);")
            .unwrap();
        // A worker whose thread takes a moment to let go of its state
        // after it has stopped, as any thread may on a busy machine.
        struct Lingers(Counting);
        impl Disk for Lingers {
            fn sync(&mut self) -> std::io::Result<()> {
                self.0.sync()
            }
            fn checkpoint(&mut self) -> anyhow::Result<()> {
                self.0.checkpoint()
            }
        }
        impl Drop for Lingers {
            fn drop(&mut self) {
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        let worker = attach_with(&db, || Box::new(Lingers(Counting::default())), fast()).unwrap();
        drop(worker);
        let key = std::fs::canonicalize(&db).unwrap();
        let mine = |all: Vec<(PathBuf, Vec<Arc<Inner>>)>| {
            all.into_iter()
                .find(|(p, _)| *p == key)
                .map(|(_, w)| w.len())
        };
        assert_eq!(mine(stores()), Some(0), "known, and nobody alive for it");
        // Its log is synced from the caller's thread, with the real call.
        sync_alone(&key).unwrap();
    }

    /// A log that is there and cannot be opened is an error, not a pass.
    #[cfg(unix)]
    #[test]
    fn a_log_that_cannot_be_opened_fails_the_sync() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let wal = dir.path().join("state.db-wal");
        std::fs::write(&wal, b"x").unwrap();
        std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&wal).is_ok() {
            return; // root reads anything: nothing to see here
        }
        let err = sync_alone(&db).unwrap_err().to_string();
        assert!(err.contains("not known to be on the disk"), "{err}");
    }

    #[test]
    fn the_environment_can_keep_the_sync_on_every_commit() {
        assert!(wants_async(None));
        assert!(wants_async(Some("")));
        assert!(wants_async(Some("async")));
        assert!(!wants_async(Some("full")));
        assert!(!wants_async(Some(" FULL\n")));
    }

    /// Nothing in the test suite turns this on for the process, so a
    /// store opened by a test has no worker and its barrier is nothing.
    #[test]
    fn a_process_that_did_not_ask_gets_no_worker() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!relaxes());
        assert!(attach(&dir.path().join("state.db")).is_none());
    }

    #[test]
    fn the_real_disk_syncs_a_log_and_shrugs_at_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let mut disk = WalDisk::new(&db);
        disk.sync().expect("no log is nothing to sync");
        assert_eq!(disk.version(), None, "no store file, no version");
        assert!(!db.exists(), "and looking did not create one");

        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;
             PRAGMA wal_autocheckpoint = 0;
             CREATE TABLE t(a); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2);",
        )
        .unwrap();
        assert!(dir.path().join("state.db-wal").exists());
        disk.sync().expect("the log syncs");

        // The version moves when another connection commits, and only then.
        let v1 = disk.version().expect("a version");
        assert_eq!(disk.version(), Some(v1));
        conn.execute_batch("INSERT INTO t VALUES (3);").unwrap();
        let v2 = disk.version().expect("a version");
        assert_ne!(v1, v2);

        // The fold moves the log's frames into the database file: a
        // second look finds every frame folded.
        disk.checkpoint().unwrap();
        let (busy, log, folded): (i64, i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(busy, 0);
        assert!(log > 0);
        assert_eq!(log, folded);
        disk.sync().unwrap();
        conn.execute_batch("INSERT INTO t VALUES (4);").unwrap();
    }
}
