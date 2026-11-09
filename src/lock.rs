//! flock helpers: the ledger lock, per-entry lease locks, and the liveness
//! probe that reaps dead entries (spec "Ledger and locking").

use crate::error::{Error, Result};
use crate::ledger::Ledger;
use crate::Context;
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(20);
pub const LEDGER_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `try_ledger` waits before giving up a round, short so a
/// waiting `exec` still answers signals promptly.
const TRY_ROUND: Duration = Duration::from_secs(1);

/// A held exclusive lock, released on drop.
#[derive(Debug)]
pub struct Lock {
    file: File,
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn open_lock_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?)
}

/// Exclusive lock with polling and timeout. Only contention is retried.
pub fn acquire(path: &Path, timeout: Duration) -> Result<Lock> {
    let file = open_lock_file(path)?;
    let start = Instant::now();
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(Lock { file }),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if start.elapsed() >= timeout {
                    return Err(Error::LockTimeout(format!(
                        "timed out after {timeout:?} waiting for {}",
                        path.display()
                    )));
                }
                thread::sleep(POLL);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

pub fn ledger(ctx: &Context) -> Result<Lock> {
    acquire(&ctx.paths.ledger_lock(), ctx.ledger_lock_timeout)
}

/// The ledger lock, or `None` when it stayed busy for a round (at most
/// `TRY_ROUND`), for callers that can skip a round and try again later.
pub fn try_ledger(ctx: &Context) -> Result<Option<Lock>> {
    match acquire(
        &ctx.paths.ledger_lock(),
        ctx.ledger_lock_timeout.min(TRY_ROUND),
    ) {
        Ok(lock) => Ok(Some(lock)),
        Err(Error::LockTimeout(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The ledger with dead entries removed, for readers.
pub fn load_reaped(ctx: &Context) -> Result<Ledger> {
    Ok(snapshot(ctx)?.ledger)
}

/// A ledger read by `snapshot`.
pub struct Snapshot {
    pub ledger: Ledger,
    /// False when the lock stayed busy and the file was read without it,
    /// so an entry still waiting for the lock to register is missing.
    pub locked: bool,
}

/// The ledger with dead entries removed. Under the lock the removal is
/// saved. When the lock stays busy, the file is read without it (saves
/// are atomic renames, so it is never half-written) and dead entries are
/// only left out of the returned copy.
pub fn snapshot(ctx: &Context) -> Result<Snapshot> {
    let Some(_guard) = try_ledger(ctx)? else {
        let mut ledger = Ledger::load(&ctx.paths.ledger_file())?;
        ledger.entries.retain(|e| lease_is_held(ctx, &e.id));
        return Ok(Snapshot {
            ledger,
            locked: false,
        });
    };
    let mut ledger = Ledger::load(&ctx.paths.ledger_file())?;
    if !reap_dead(ctx, &mut ledger).is_empty() {
        ledger.save(&ctx.paths.ledger_file())?;
    }
    Ok(Snapshot {
        ledger,
        locked: true,
    })
}

/// A ledger read without changing anything on disk.
pub struct ReadOnlySnapshot {
    pub ledger: Ledger,
    /// False when the lock file is missing or stayed exclusively held for
    /// a round, so the file was read without it.
    pub locked: bool,
    /// Ids whose lease is not held. They are kept in `ledger`: whether the
    /// owner is gone is the reaper's call, a reader must not count them as
    /// free capacity.
    pub suspect_dead: Vec<String>,
}

/// The ledger as it is, for readers that must not write (forecast). Unlike
/// [`snapshot`] nothing is created, reaped or saved: the lock file is only
/// opened if it already exists, and a shared lock (never an exclusive one)
/// is held while reading.
pub fn snapshot_read_only(ctx: &Context) -> Result<ReadOnlySnapshot> {
    let guard = match OpenOptions::new().read(true).open(ctx.paths.ledger_lock()) {
        Ok(file) => {
            let start = Instant::now();
            loop {
                match FileExt::try_lock_shared(&file) {
                    Ok(()) => break Some(file),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if start.elapsed() >= ctx.ledger_lock_timeout.min(TRY_ROUND) {
                            break None;
                        }
                        thread::sleep(POLL);
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let locked = guard.is_some();
    // Probe the leases while the shared lock is still held: an owner
    // removes its entry under the exclusive lock before dropping the
    // lease, so every entry read here still has its lease held.
    let read = Ledger::load(&ctx.paths.ledger_file()).map(|ledger| {
        let suspect_dead: Vec<String> = ledger
            .entries
            .iter()
            .filter(|e| !lease_is_held(ctx, &e.id))
            .map(|e| e.id.clone())
            .collect();
        (ledger, suspect_dead)
    });
    if let Some(f) = &guard {
        let _ = FileExt::unlock(f);
    }
    let (ledger, suspect_dead) = read?;
    Ok(ReadOnlySnapshot {
        ledger,
        locked,
        suspect_dead,
    })
}

/// Take the lease for `id`. Held by the owning canirun process until it
/// exits; the fd is close-on-exec (Rust std default) so children never
/// inherit it.
pub fn take_lease(ctx: &Context, id: &str) -> Result<Lock> {
    let path = ctx.paths.lease_file(id);
    let file = open_lock_file(&path)?;
    file.try_lock_exclusive()
        .map_err(|e| Error::Ledger(format!("lease {} already held: {e}", path.display())))?;
    Ok(Lock { file })
}

/// True when some process currently holds the lease lock for `id`.
pub fn lease_is_held(ctx: &Context, id: &str) -> bool {
    let path = ctx.paths.lease_file(id);
    let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
        return false;
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            false
        }
        Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
    }
}

/// Take a *shared* lock on `path` (several holders allowed), held until drop.
pub fn take_shared(path: &Path) -> Result<Lock> {
    let file = open_lock_file(path)?;
    file.lock_shared()?;
    Ok(Lock { file })
}

/// True when at least one shared or exclusive holder exists for `path`.
pub fn is_held(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
        return false;
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            false
        }
        Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
    }
}

/// Remove ledger entries whose lease is not held, and their lease files.
/// Returns the ids removed. Caller holds the ledger lock.
pub fn reap_dead(ctx: &Context, ledger: &mut Ledger) -> Vec<String> {
    let dead: Vec<String> = ledger
        .entries
        .iter()
        .filter(|e| !lease_is_held(ctx, &e.id))
        .map(|e| e.id.clone())
        .collect();
    for id in &dead {
        ledger.remove(id);
        let _ = std::fs::remove_file(ctx.paths.lease_file(id));
    }
    dead
}

/// Remove lease files that no ledger entry refers to (left by crashes
/// between lease creation and ledger registration).
pub fn sweep_orphan_leases(ctx: &Context, ledger: &Ledger) {
    let Ok(rd) = std::fs::read_dir(ctx.paths.leases_dir()) else {
        return;
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".lock")) else {
            continue;
        };
        if ledger.find(id).is_none() && !lease_is_held(ctx, id) {
            let _ = std::fs::remove_file(ent.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admit::State;
    use crate::ledger::Entry;
    use crate::paths::Paths;
    use chrono::Utc;

    fn entry(id: &str, seq: u64) -> Entry {
        Entry {
            id: id.into(),
            pid: 1,
            state: State::Running,
            front: false,
            weight: 1,
            predicted_rss: 0,
            expected_secs: None,
            repo: String::new(),
            subdir: String::new(),
            argv: "x".into(),
            key: "k".into(),
            seq,
            enqueued_at: Utc::now(),
            started_at: Some(Utc::now()),
            using: None,
            backfilled: false,
        }
    }

    fn state_with(ids: &[&str]) -> (tempfile::TempDir, Context) {
        let d = tempfile::tempdir().unwrap();
        let ctx = Context::new(Paths::new(d.path().join("state"), d.path().join("c.toml")));
        let mut l = Ledger::default();
        for id in ids {
            let s = l.take_seq();
            l.entries.push(entry(id, s));
        }
        drop(ledger(&ctx).unwrap());
        l.save(&ctx.paths.ledger_file()).unwrap();
        (d, ctx)
    }

    #[test]
    fn read_only_snapshot_judges_leases_under_the_shared_lock() {
        let (_d, ctx) = state_with(&["a", "b"]);
        let lease = take_lease(&ctx, "a").unwrap();
        let snap = snapshot_read_only(&ctx).unwrap();
        assert!(snap.locked);
        assert_eq!(snap.ledger.entries.len(), 2);
        // "a" has its owner, "b" has none; nothing is removed.
        assert_eq!(snap.suspect_dead, vec!["b".to_string()]);
        drop(lease);
        let snap = snapshot_read_only(&ctx).unwrap();
        assert_eq!(snap.suspect_dead.len(), 2);
    }

    #[test]
    fn read_only_snapshot_releases_its_shared_lock() {
        let (_d, ctx) = state_with(&["a"]);
        let _lease = take_lease(&ctx, "a").unwrap();
        snapshot_read_only(&ctx).unwrap();
        // An exclusive lock is available right after.
        let g = acquire(&ctx.paths.ledger_lock(), Duration::from_millis(100));
        assert!(g.is_ok());
    }

    #[test]
    fn read_only_snapshot_without_a_lock_file_creates_nothing() {
        let d = tempfile::tempdir().unwrap();
        let ctx = Context::new(Paths::new(d.path().join("state"), d.path().join("c.toml")));
        let snap = snapshot_read_only(&ctx).unwrap();
        assert!(!snap.locked);
        assert!(snap.ledger.entries.is_empty());
        assert!(!d.path().join("state").exists());
    }
}
