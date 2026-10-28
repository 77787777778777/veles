//! Brain backup & restore — the safety net for cutover.
//!
//! The "brain" spans TWO roots since the XDG migration (2026-09-04):
//! configuration in the config dir (`config.toml`, `.env`, `commands/`,
//! `plugins/`, `skills/`) and state in the data dir (`state.db` — which
//! holds memory too — `notes/`, `checkpoints/`, `plans/`, `certs/`, the
//! gateway state files). A backup that copies
//! only one root loses half the brain — and for months `backup_to`
//! took a single `home` and reported `✓ brain backed up` regardless
//! of what was actually there.
//!
//! `backup_to` snapshots both roots into a timestamped directory:
//!   - state.db via `VACUUM INTO` — a consistent snapshot even while a
//!     gateway is running and writing (WAL mode; busy timeout).
//!   - every entry in [`BACKUP_SET`], each to the root it names.
//!   - MANIFEST.txt records version + timestamp + per-root counts.
//!
//! `restore_from` refuses while a gateway owns the pid file, moves the
//! current brain aside (`.pre-restore`), then copies each backup entry
//! back to the root the set names — a restore can no longer scatter
//! config into the data dir. A backup from the file era (a `memory.d/`,
//! `MEMORY.md`, `USER.md` beside its state.db) has that memory folded
//! into the restored database, and the copies removed.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

pub struct BackupReport {
    pub dest: PathBuf,
    pub db_bytes: u64,
    pub files: usize,
}

pub struct RestoreReport {
    pub src: PathBuf,
    pub moved_aside: Option<PathBuf>,
    pub files: usize,
    /// Memory paths folded in from a file-era backup's memory.d / flat
    /// files (see [`crate::memory::MemoryStore::retire_disk_tree`]).
    pub memory_folded: Vec<String>,
}

/// Which root an entry is backed up from and restored to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Root {
    Config,
    Data,
}

/// Everything a backup carries, as (root, name) pairs.
///
/// The `name` is a file or directory relative to that root. Text that
/// names a file is a claim — the `backup_set_matches_reality` test
/// walks the live dirs and holds this list to it (a `60786dd`-style
/// walking test, because `--help` and this table are two more copies
/// of the same claim that can drift).
///
/// Deliberately NOT included: the `config.toml.bak*` / `.before-*`
/// / `.env.bak*` litter (26 files on the dev box at the time of
/// writing) — a backup of the brain wants its state, not its edit
/// history; the git-tracked config and the timestamped backups
/// themselves are the history. `backups/` is excluded for the same
/// reason (a backup containing backups grows quadratically), and so
/// are `cache/` (regenerable) and `logs/` / `gateway.log` (diagnostics,
/// not brain).
pub const BACKUP_SET: &[(&str, Root)] = &[
    // — the config root —
    ("config.toml", Root::Config),
    (".env", Root::Config),
    // The node identity (id + name; minted on first boot, and the value
    // that tells THIS node from a copy). It is deliberately kept out of
    // config.toml — a config you can copy between machines must not
    // carry it — but a RESTORE that drops it mints a fresh identity, so
    // the node silently becomes a different node. Same reasoning that
    // puts `certs` in the set.
    ("node.toml", Root::Config),
    ("commands", Root::Config),
    ("plugins", Root::Config),
    ("skills", Root::Config),
    // The vault: the shared secrets, encrypted, and this node's
    // identity for it (`identity.txt` + `signing.key`, the node's own
    // key as one of the members). A restore without it re-enters every
    // key by hand; a restore without the identity makes the node a
    // stranger to its own vault. That case is detected now (pitfall
    // 76): `Vault::open` refuses to mint new keys while vault.json still
    // names the node, `veles doctor` / `veles vault status` report the
    // lost or mismatched identity, and the way back without this backup
    // is `veles vault reidentify` + a member's `veles vault admit
    // <node> --fingerprint <new>`, which re-encrypts every value for the
    // new key. vault.json is encrypted to the members' keys, but this
    // node's key is in the same backup, so the backup opens the vault:
    // keep it as private as the home it came from (it carries .env in
    // plain text already).
    ("vault", Root::Config),
    // — the data root —
    ("state.db", Root::Data), // handled specially: VACUUM INTO — memory is in it
    ("notes", Root::Data),
    ("checkpoints", Root::Data),
    ("scripts", Root::Data),
    // Apps the agent built for the person (apps.rs) and what they stored:
    // their own work, gone for good without a copy.
    ("apps", Root::Data),
    ("app-data", Root::Data),
    ("plans", Root::Data),
    ("certs", Root::Data),
    ("gateway-state.json", Root::Data),
    ("gateway-shutdown.log", Root::Data),
    ("desktop_state.json", Root::Data),
    // Other nodes' stores as this node holds them (storesync:
    // `replicas/<node-id>.db`). A sender moves its cursor past what this
    // node reports applying, so a restore without them loses those rows
    // for good; for a phone that is lost, they are its history.
    // Snapshotted like state.db: they are live databases.
    ("replicas", Root::Data),
    // Memory offers peers sent (`[cluster] memsync_auto`). Only the ones
    // still owed a decision are carried — a path both sides changed waits
    // here, local content kept, for `veles memory apply --inbox`. The
    // sender's cursor has already moved past it, so unlike storesync's
    // inbox nothing heals a restore that drops it. `inbox/applied/` is
    // history and stays out.
    ("memsync", Root::Data),
    // Config offers peers sent (configsync's inbox). Only the ones
    // still owed a decision are carried — a security-section change
    // waits here, unapplied, for the owner's `/configsync apply`. An
    // applied offer is history; one still held when a restore drops it
    // is healed: the sender's per-peer cursor is already past it, but
    // the next config change there re-pushes what the receiver lacks,
    // and the held change was never live meanwhile.
    ("configsync", Root::Data),
    // What process plugins keep across a restart (`plugin-state/<name>/
    // <instance>`, the `state` dir the host hands each one): a cursor, a
    // ledger, a login. Their `plugin-cache` lives under the cache root,
    // which stays out. Files are copied as they are, so a plugin that
    // keeps a live database here gets a copy taken mid-write.
    ("plugin-state", Root::Data),
    // The same for in-process modules (`module-state/<name>/`): slopdrop's
    // ledger, withheld posts and safety marks live here since R5. The
    // installed module code (`modules/`) stays out — it is a build
    // artifact keyed to one core build, and the installer puts it back.
    ("module-state", Root::Data),
];

fn copy_dir(src: &Path, dest: &Path, count: &mut usize) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to, count)?;
        } else {
            fs::copy(&from, &to).with_context(|| format!("copy {}", from.display()))?;
            *count += 1;
        }
    }
    Ok(())
}

/// A consistent copy of a database that may be live: `VACUUM INTO`
/// reads one snapshot, its write-ahead log included. Returns its size.
fn snapshot_db(src: &Path, dest: &Path) -> Result<u64> {
    let conn =
        rusqlite::Connection::open(src).with_context(|| format!("open {}", src.display()))?;
    conn.busy_timeout(Duration::from_secs(10))?;
    let escaped = dest.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{escaped}';"))
        .with_context(|| format!("VACUUM INTO {}", dest.display()))?;
    Ok(fs::metadata(dest).map(|m| m.len()).unwrap_or(0))
}

/// Where an entry of [`BACKUP_SET`] lives under `config`/`data`.
fn entry_path(config: &Path, data: &Path, name: &str, root: Root) -> PathBuf {
    match root {
        Root::Config => config.join(name),
        Root::Data => data.join(name),
    }
}

/// Snapshot the brain — both roots — into `dest` (created if missing).
pub fn backup_to(config: &Path, data: &Path, dest: &Path) -> Result<BackupReport> {
    fs::create_dir_all(dest)?;

    // state.db first: consistent snapshot of a possibly-live database.
    // The schema and sessions are the hard-to-rebuild half of the brain.
    let db_src = data.join("state.db");
    let mut db_bytes = 0u64;
    if db_src.exists() {
        db_bytes = snapshot_db(&db_src, &dest.join("state.db"))?;
    }

    // Then the rest of the set, each entry from the root it names.
    let mut files = 0usize;
    for (name, root) in BACKUP_SET {
        if *name == "state.db" {
            if db_src.exists() {
                files += 1;
            }
            continue;
        }
        let p = entry_path(config, data, name, *root);
        if *name == "replicas" {
            // Each database as a snapshot; their -wal/-shm are folded in.
            if let Ok(rd) = fs::read_dir(&p) {
                for e in rd.flatten() {
                    let from = e.path();
                    if from.extension().is_some_and(|x| x == "db") {
                        fs::create_dir_all(dest.join(name))?;
                        snapshot_db(&from, &dest.join(name).join(e.file_name()))?;
                        files += 1;
                    }
                }
            }
            continue;
        }
        if *name == "memsync" {
            // The pending offers only (see BACKUP_SET).
            let to = dest.join(name).join("inbox");
            for from in crate::memsync::inbox_pending(&p.join("inbox"))? {
                fs::create_dir_all(&to)?;
                fs::copy(&from, to.join(from.file_name().unwrap_or_default()))?;
                files += 1;
            }
            continue;
        }
        if *name == "configsync" {
            // The held offers only (see BACKUP_SET).
            let to = dest.join(name).join("inbox");
            for from in crate::configsync::inbox_pending(&p.join("inbox"))? {
                fs::create_dir_all(&to)?;
                fs::copy(&from, to.join(from.file_name().unwrap_or_default()))?;
                files += 1;
            }
            continue;
        }
        if p.is_dir() {
            copy_dir(&p, &dest.join(name), &mut files)?;
        } else if p.exists() {
            fs::copy(&p, dest.join(name))?;
            files += 1;
        }
    }

    // The manifest says WHICH root each entry came from, because the
    // restore needs it and a human diffing the directory deserves it.
    let mut manifest = format!(
        "veles backup · v{}\ncreated: {}\nconfig root: {}\ndata root: {}\ncontents: state.db ({} bytes), {} files\nrestore: veles restore {}\n",
        env!("CARGO_PKG_VERSION"),
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        config.display(),
        data.display(),
        db_bytes,
        files,
        dest.display()
    );
    for (name, root) in BACKUP_SET {
        let label = match root {
            Root::Config => "config",
            Root::Data => "data",
        };
        manifest.push_str(&format!("{label}\t{name}\n"));
    }
    fs::write(dest.join("MANIFEST.txt"), manifest)?;
    files += 1;

    Ok(BackupReport {
        dest: dest.to_path_buf(),
        db_bytes,
        files,
    })
}

/// True when something answers on the desktop API port (127.0.0.1:8642,
/// or $VELES_DESKTOP_PORT). Cheap TCP probe — a live gateway always
/// listens there.
fn desktop_api_alive() -> bool {
    use std::io::{Read, Write};
    use std::net::{IpAddr, SocketAddr, TcpStream};
    use std::time::Duration;
    let port: u16 = std::env::var("VELES_DESKTOP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8642);
    let addr = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), port);
    match TcpStream::connect_timeout(&addr, Duration::from_millis(400)) {
        Ok(mut s) => {
            let _ = s.set_read_timeout(Some(Duration::from_millis(400)));
            let _ = s.write_all(b"GET /api/health HTTP/1.0\r\n\r\n");
            let mut buf = [0u8; 32];
            matches!(s.read(&mut buf), Ok(n) if n > 0 && buf[..n].starts_with(b"HTTP/"))
        }
        Err(_) => false,
    }
}

/// The restore guard, reusable: true when a gateway owns the pid file
/// OR answers on the desktop API port. The importer uses the same gate.
pub fn gateway_alive() -> bool {
    if let Some(pid) = crate::lifecycle::read_pid() {
        if crate::lifecycle::is_running(pid) {
            return true;
        }
    }
    desktop_api_alive()
}

/// Restore a backup into both roots. Refuses while a gateway is running.
///
/// The backup's MANIFEST names the root each entry belongs to; the
/// entries are restored THERE, so a round trip cannot scatter config
/// into the data dir (which is what the single-root restore did —
/// `config.toml` landed next to `state.db` and the next boot read the
/// defaults instead of the brain).
pub fn restore_from(config: &Path, data: &Path, src: &Path) -> Result<RestoreReport> {
    if !src.join("MANIFEST.txt").exists() && !src.join("state.db").exists() {
        anyhow::bail!(
            "{} does not look like a veles backup (no MANIFEST.txt / state.db)",
            src.display()
        );
    }
    // Refuse while a gateway owns the pid file OR the desktop API port
    // answers (legacy gateways started before the pid-file era don't
    // leave a pid file — the :8642 probe catches them).
    if let Some(pid) = crate::lifecycle::read_pid() {
        if crate::lifecycle::is_running(pid) {
            anyhow::bail!("gateway is running (pid {pid}) — stop it first (`veles gateway stop`)");
        }
    }
    if desktop_api_alive() {
        anyhow::bail!(
            "a gateway is answering on 127.0.0.1:8642 — stop it first (`veles gateway stop`)"
        );
    }

    // Where each backed-up name goes: the manifest's word if present,
    // BACKUP_SET's otherwise (pre-manifest backups restore by the
    // current table).
    let mut table: std::collections::HashMap<String, Root> = BACKUP_SET
        .iter()
        .map(|(n, r)| (n.to_string(), *r))
        .collect();
    if let Ok(manifest) = fs::read_to_string(src.join("MANIFEST.txt")) {
        for line in manifest.lines() {
            // The per-entry rows are "<root>\t<name>".
            let mut parts = line.splitn(2, '\t');
            let (Some(label), Some(name)) = (parts.next(), parts.next()) else {
                continue;
            };
            let root = match label {
                "config" => Some(Root::Config),
                "data" => Some(Root::Data),
                _ => None,
            };
            if let Some(r) = root {
                table.insert(name.to_string(), r);
            }
        }
    }

    // Move the current brain aside (non-destructive restore).
    let aside = data.join(format!(
        "pre-restore-{}",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    ));
    fs::create_dir_all(&aside)?;
    let mut moved: Option<PathBuf> = None;
    let mut files = 0usize;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "MANIFEST.txt" {
            continue;
        }
        let Some(root) = table.get(name.as_ref()).copied() else {
            // Not in the set: an entry this backup predates or a file
            // the set no longer carries. Restore it to the data root —
            // the single-root era destination — rather than dropping
            // data on the floor.
            let to_home = data.join(name.as_ref());
            if to_home.exists() {
                fs::rename(&to_home, aside.join(name.as_ref()))?;
                moved = Some(aside.clone());
            }
            if entry.path().is_dir() {
                copy_dir(&entry.path(), &to_home, &mut files)?;
            } else {
                fs::copy(entry.path(), &to_home)?;
                files += 1;
            }
            continue;
        };
        let to = entry_path(config, data, &name, root);
        if let Some(parent) = to.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if to.exists() {
            let away = aside.join(name.as_ref());
            let _ = fs::create_dir_all(away.parent().unwrap_or(&aside));
            fs::rename(&to, &away)?;
            moved = Some(aside.clone());
        }
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &to, &mut files)?;
        } else {
            fs::copy(entry.path(), &to)?;
            files += 1;
        }
    }

    // A restored state.db must NOT inherit the old database's WAL —
    // stale -wal/-shm from the moved-aside live DB would corrupt the
    // restored one. SQLite rebuilds them on first write.
    let db_dest = data.join("state.db");
    if db_dest.exists() {
        for suffix in ["-wal", "-shm"] {
            let stale = data.join(format!("state.db{suffix}"));
            if stale.exists() {
                let _ = fs::remove_file(&stale);
            }
        }
    }

    // A backup from the file era carried memory as files; they were
    // restored to the data root above like any other entry. Fold them
    // into the restored database now rather than at the next start, so
    // the restore reports what it did — and so nothing reads the stale
    // copies in between.
    let retired = crate::memory::MemoryStore::at(data.to_path_buf()).retire_disk_tree()?;
    for why in &retired.kept {
        tracing::warn!("restore: kept the backup's memory files: {why}");
    }

    Ok(RestoreReport {
        src: src.to_path_buf(),
        moved_aside: moved,
        files,
        memory_folded: retired.folded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a two-root fake brain. The old test built a single home
    /// and therefore could never see the split — which is exactly how
    /// the single-root backup shipped.
    fn fake_brain(dir: &Path) -> (PathBuf, PathBuf) {
        let config = dir.join("config-root");
        let data = dir.join("data-root");
        std::fs::create_dir_all(config.join("commands")).unwrap();
        std::fs::create_dir_all(data.join("notes")).unwrap();

        std::fs::write(config.join("config.toml"), "# brain config").unwrap();
        std::fs::write(config.join(".env"), "SECRET=1").unwrap();
        std::fs::write(config.join("commands/ping.md"), "ping").unwrap();
        std::fs::write(config.join("config.toml.bak-nim"), "litter").unwrap();
        // The node identity. Present in the fixture because its ABSENCE
        // let a real gap through: the set-completeness assert caught that
        // BACKUP_SET lacked `node.toml` on the live box, but this
        // round-trip never would have, because the fixture did not carry
        // the file. A restore that drops it mints a new identity.
        std::fs::write(config.join("node.toml"), "id = \"test\"\nname = \"box\"\n").unwrap();

        let conn = rusqlite::Connection::open(data.join("state.db")).unwrap();
        conn.execute_batch("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('hello');")
            .unwrap();
        drop(conn);
        crate::memory::MemoryStore::at(data.clone())
            .write_file("reference/topic.md", "the memory")
            .unwrap();
        std::fs::write(data.join("notes/peer.md"), "dossier").unwrap();
        (config, data)
    }

    #[test]
    fn backup_restore_round_trip() {
        // The restore guard refuses while a gateway is live, and it
        // checks TWO things: the desktop port, and VELES_HOME's pid file.
        // Pointing only the port at an unused one left `read_pid()`
        // reading the developer's REAL gateway.pid — so this
        // test passed on a machine with the gateway stopped and failed
        // on one with it running ("gateway is running (pid …) — stop it
        // first"). Both are redirected now.
        //
        // Both env vars are process-global, so take the shared lock —
        // guard(), which puts VELES_HOME back on drop. The bare lock()
        // left it on this test's deleted dir, and
        // backup_set_matches_reality, next in name order, then found
        // no roots and skipped: it walked the real box only in runs
        // where it happened to go first.
        let _g = crate::testenv::guard();
        let dir = std::env::temp_dir().join(format!("veles-backup-test-{}", std::process::id()));
        std::env::set_var("VELES_DESKTOP_PORT", "48642");
        std::env::set_var("VELES_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);
        let dest = dir.join("backup");
        let (config, data) = fake_brain(&dir);

        let report = backup_to(&config, &data, &dest).unwrap();
        assert!(dest.join("state.db").exists());
        assert!(dest.join("MANIFEST.txt").exists());
        assert!(dest.join(".env").exists());
        assert!(dest.join("config.toml").exists());
        assert!(dest.join("commands/ping.md").exists());
        // The identity survives the round trip. Without it a restore
        // produces a node that is not this node.
        assert!(
            dest.join("node.toml").exists(),
            "the backup dropped node.toml — a restore would mint a new node identity"
        );
        assert!(dest.join("notes/peer.md").exists());
        // Memory travels inside state.db; there are no memory files.
        assert!(!dest.join("memory.d").exists() && !dest.join("MEMORY.md").exists());
        // The litter is deliberately excluded.
        assert!(!dest.join("config.toml.bak-nim").exists());
        assert!(report.files >= 5);

        // Mutate the live brain, then restore and verify the backup won
        // — and that each file landed in the root it came from.
        std::fs::write(data.join("notes/peer.md"), "CORRUPTED").unwrap();
        std::fs::write(config.join("config.toml"), "# WRONG").unwrap();
        std::fs::remove_file(config.join("config.toml")).unwrap();
        let conn = rusqlite::Connection::open(data.join("state.db")).unwrap();
        conn.execute_batch("INSERT INTO t VALUES ('post-backup');")
            .unwrap();
        drop(conn);

        let restored = restore_from(&config, &data, &dest).unwrap();
        assert!(restored.moved_aside.is_some());
        assert_eq!(
            std::fs::read_to_string(config.join("config.toml")).unwrap(),
            "# brain config",
            "config.toml must return to the CONFIG root"
        );
        assert_eq!(
            std::fs::read_to_string(data.join("notes/peer.md")).unwrap(),
            "dossier"
        );
        assert_eq!(
            crate::memory::MemoryStore::at(data.clone())
                .read_file("reference/topic.md")
                .unwrap(),
            "the memory",
            "memory comes back with the database"
        );
        // Nothing scattered: the data root must not have grown a copy
        // of config.toml (the single-root restore's signature).
        assert!(!data.join("config.toml").exists());
        assert!(!config.join("state.db").exists());
        let conn = rusqlite::Connection::open(data.join("state.db")).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "post-backup write must be gone after restore");
        drop(conn);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A replica is a live database like state.db: the backup takes a
    /// snapshot of it, write-ahead log included, and the restore puts it
    /// back. A plain copy of the .db alone left out the rows still in
    /// its -wal (and copied the -wal beside it, half-written).
    #[test]
    fn replicas_are_snapshotted_and_restored() {
        let _g = crate::testenv::guard();
        let dir = std::env::temp_dir().join(format!("veles-backup-rep-{}", std::process::id()));
        std::env::set_var("VELES_DESKTOP_PORT", "48642");
        std::env::set_var("VELES_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);
        let dest = dir.join("backup");
        let (config, data) = fake_brain(&dir);
        std::fs::create_dir_all(data.join("replicas")).unwrap();
        let rep = data.join("replicas/node-a.db");
        let live = rusqlite::Connection::open(&rep).unwrap();
        live.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE r(v TEXT); INSERT INTO r VALUES ('from the phone');",
        )
        .unwrap();
        assert!(
            data.join("replicas/node-a.db-wal").exists(),
            "the row must be in the WAL for this test to mean anything"
        );

        backup_to(&config, &data, &dest).unwrap();
        drop(live);
        let names: Vec<String> = std::fs::read_dir(dest.join("replicas"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["node-a.db".to_string()], "only the snapshot");

        std::fs::remove_dir_all(data.join("replicas")).unwrap();
        restore_from(&config, &data, &dest).unwrap();
        let back = rusqlite::Connection::open(data.join("replicas/node-a.db")).unwrap();
        let v: String = back.query_row("SELECT v FROM r", [], |r| r.get(0)).unwrap();
        assert_eq!(v, "from the phone");
        drop(back);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A memory offer held as a conflict is owed a decision, and its
    /// sender will not send it again: a backup carries it and a restore
    /// brings it back. Offers already applied are history and stay out.
    #[test]
    fn a_held_memory_offer_survives_a_restore_and_applied_ones_stay_out() {
        let _g = crate::testenv::guard();
        let dir = std::env::temp_dir().join(format!("veles-backup-ms-{}", std::process::id()));
        std::env::set_var("VELES_DESKTOP_PORT", "48642");
        std::env::set_var("VELES_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);
        let dest = dir.join("backup");
        let (config, data) = fake_brain(&dir);
        let inbox = data.join("memsync/inbox");
        std::fs::create_dir_all(inbox.join("applied")).unwrap();
        let held = "00000000000000000001-task-held.json";
        std::fs::write(inbox.join(held), "{\"held\":true}").unwrap();
        std::fs::write(
            inbox.join("applied/00000000000000000000-task-old.json"),
            "{}",
        )
        .unwrap();

        backup_to(&config, &data, &dest).unwrap();
        let carried: Vec<String> = std::fs::read_dir(dest.join("memsync/inbox"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            carried,
            vec![held.to_string()],
            "the held offer, nothing applied"
        );

        std::fs::remove_dir_all(data.join("memsync")).unwrap();
        restore_from(&config, &data, &dest).unwrap();
        assert_eq!(
            std::fs::read_to_string(inbox.join(held)).unwrap(),
            "{\"held\":true}"
        );
        let pending = crate::memsync::inbox_pending(&inbox).unwrap();
        assert_eq!(pending.len(), 1, "restored as a pending offer: {pending:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A backup from before the database held memory carries it as
    /// files. Restoring one must land that memory in the restored
    /// database — not leave it as files nothing reads any more.
    #[test]
    fn restoring_a_file_era_backup_folds_its_memory_in() {
        // guard(): the lock AND VELES_HOME put back on drop.
        let _g = crate::testenv::guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("VELES_DESKTOP_PORT", "48643");
        std::env::set_var("VELES_HOME", dir.path());
        let config = dir.path().join("config-root");
        let data = dir.path().join("data-root");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let src = dir.path().join("old-backup");
        std::fs::create_dir_all(src.join("memory.d/reference")).unwrap();
        let conn = rusqlite::Connection::open(src.join("state.db")).unwrap();
        conn.execute_batch("CREATE TABLE t (v TEXT);").unwrap();
        drop(conn);
        std::fs::write(src.join("memory.d/reference/old.md"), "from the file era").unwrap();
        std::fs::write(src.join("MEMORY.md"), "old notes").unwrap();

        let report = restore_from(&config, &data, &src).unwrap();
        assert_eq!(
            report.memory_folded,
            vec!["reference/old.md".to_string(), "system/core.md".to_string()]
        );
        let mem = crate::memory::MemoryStore::at(data.clone());
        assert_eq!(
            mem.read_file("reference/old.md").unwrap(),
            "from the file era"
        );
        assert_eq!(mem.read(crate::memory::StoreKind::Memory), "old notes");
        assert!(!data.join("memory.d").exists() && !data.join("MEMORY.md").exists());
    }

    /// Text that names a file is a claim. `--help` says "sessions,
    /// memory, notes, checkpoints, plugins" and BACKUP_SET says the
    /// same thing in code — this walk holds the table to the REAL
    /// directory contents so a new brain file can be added silently
    /// next to a set that still doesn't know it. Directories the set
    /// deliberately excludes are named here with their reason.
    ///
    /// Walks the DEVELOPER'S real roots (the dirs `config_dir()` /
    /// `data_dir()` answer with the thread override cleared) — the
    /// point is the claim matching THIS box's brain, not a fixture's.
    #[test]
    fn backup_set_matches_reality() {
        let _g = crate::testenv::lock();
        // Clear the thread-scoped dir override (other tests' scratch
        // dirs) so config_dir()/data_dir() answer for the real box.
        // The guard restores it on drop.
        let _clear = crate::config::set_dirs(None);
        let excluded = [
            "backups", // a backup containing backups grows quadratically
            "cache",   // regenerable by definition
            "logs",    // diagnostics, not brain
            "gateway.log",
            "gui-gateway.log",
            "media",    // large binaries, reproducible from sessions
            "sessions", // export target, mirrored in state.db
            "sandbox",  // per-sandbox scratch
            "desktop",  // desktop app scratch
            "modelbench",
            "notesbench",
            "slopdrop", // the pre-R5 slopdrop tree; the module moves it
            // to module-state/slopdrop on its first start
            // Installed code, not brain: a module is keyed to one core
            // build and a process plugin to one image; the installer
            // (`push-plugin.sh`, `install-module-dev.sh`) puts them back,
            // and a restored `.so` from another build would only be
            // refused by the key.
            "modules",
            "process-plugins",
            "user-envs", // per-user env scratch
            // One unix socket per running veles process (runsock.rs,
            // session mail): meaningless once the process is gone.
            "run",
            // Pool invites (pool.rs): ten minutes long, single use, and
            // their WireGuard queue belongs to the hub's root helper.
            "pool",
            // The guest browser's hand-over file (browsertools.rs
            // challenge_window): a session's logins, deleted as soon as
            // read — never something to keep a copy of.
            "browser",
            "ghostwire",
            "a2a_conversations", // JSONL mirror of store rows
            "a2a_audit.jsonl",
            "profiles",
            ".veles-env-incarnation", // regenerable id, minted on first use
            "MANIFEST.txt",           // the BACKUP format's own file, not brain: a
            // pre-fix restore scattered one into the data
            // dir, and one is written afresh at every backup
            // File-era memory copies: memory is in state.db, and
            // `MemoryStore::retire_disk_tree` folds these in and removes
            // them the next time the store opens.
            "memory.d",
            "MEMORY.md",
            "USER.md",
            // Store offers peers sent (storesync's inbox). Applied ones
            // are history; one still pending when a restore drops it is
            // healed: the next offer leaves a gap, and a gap asks the
            // sender for rows from the replica's applied cursor on.
            "storesync",
            // When each held sync decision may be asked next. A restore
            // without it asks each one again, once.
            "syncask.json",
            // The Bluesky hint lane's resume position (v1's timestamp,
            // v2's seq). A restore without it starts the lane live, and
            // the notification poll reconciles the gap.
            "atproto-jetstream.cursor",
            "atproto-jetstream-v2.cursor",
        ];
        let roots = [crate::config::config_dir(), crate::config::data_dir()];
        // No box to walk = no assertion to make. The hermetic build
        // (`nix flake check`) runs with an empty HOME, so neither root
        // exists there and the walk has nothing to read; the dev box
        // has both. Say so loudly rather than passing in silence: the
        // first cut `continue`d past a missing root, which is how this
        // ran vacuously for a whole round.
        if !roots.iter().any(|r| r.is_dir()) {
            eprintln!(
                "backup_set_matches_reality: skipped — no veles roots on \
                 this box (hermetic build?). Looked in: {roots:?}"
            );
            return;
        }
        for root in roots.iter().filter(|r| r.is_dir()) {
            let entries = fs::read_dir(root)
                .unwrap_or_else(|e| panic!("read_dir {} failed: {e}", root.display()));
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                // Timestamped litter and pid/log tails are excluded by
                // policy, not by enumeration.
                if name.starts_with("config.toml.bak")
                    || name.starts_with("config.toml.before")
                    || name.starts_with(".env.bak")
                    || name.starts_with("memory.d.pre-")
                    || name.starts_with("slopdrop.pre-module")
                    || name.starts_with("pre-restore-")
                    || name.ends_with(".pid")
                    || name.ends_with(".log")
                    // A log's backup is the log's kind of litter (the
                    // M5 cleanup kept `gateway-shutdown.log.bak-…` beside
                    // the log on purpose, 2026-10-10).
                    || name.contains(".log.bak")
                    || name.ends_with("-shm")
                    || name.ends_with("-wal")
                    // The Windows logon task's definition: `veles gateway
                    // install` writes it again from the install paths.
                    || name == "gateway-task.xml"
                {
                    continue;
                }
                if excluded.contains(&name.as_str()) {
                    continue;
                }
                let known = BACKUP_SET.iter().any(|(n, _)| *n == name);
                assert!(
                    known || name == "MOVED-TO-XDG",
                    "{name} exists in {} but BACKUP_SET does not carry it — \
                     either add it or add it to the exclusion list with a reason",
                    root.display()
                );
            }
        }
    }
}
