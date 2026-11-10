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
    refresh_verdicts(ctx);
    let Some(_guard) = try_ledger(ctx)? else {
        let mut ledger = Ledger::load(&ctx.paths.ledger_file())?;
        // Without the lock nothing is reaped: a managed entry is kept
        // whatever its lease says (the reaper decides under the lock).
        ledger
            .entries
            .retain(|e| lease_is_held(ctx, &e.id) || ctx.paths.managed_file(&e.id).exists());
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
    try_take_lease(ctx, id)?
        .ok_or_else(|| Error::Ledger(format!("lease {} already held", path.display())))
}

/// [`take_lease`], but `None` when another process holds it.
pub fn try_take_lease(ctx: &Context, id: &str) -> Result<Option<Lock>> {
    let path = ctx.paths.lease_file(id);
    let file = open_lock_file(&path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(Lock { file })),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(Error::Ledger(format!(
            "lease {} cannot be locked: {e}",
            path.display()
        ))),
    }
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

/// One assessment of a ledger entry, kept by the context that made it.
#[derive(Debug, Clone)]
pub struct Cached {
    /// The ledger entry it was made for (a new registration under the same
    /// id has another sequence number).
    pub seq: u64,
    /// When the assessment began.
    pub at: chrono::DateTime<chrono::Utc>,
    pub assessment: crate::managed::Assessment,
}

fn cache_of(ctx: &Context) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Cached>> {
    ctx.reap_cache.lock().unwrap_or_else(|p| p.into_inner())
}

/// Assess, outside the ledger lock, every entry whose owner lease is not
/// held, and remember the answers for [`reap_dead`]. A process scan takes
/// hundreds of milliseconds, which nobody should wait for holding the
/// lock, and an entry that is kept is looked at again only every
/// [`crate::reap::REASSESS_SECS`] (per context), so waiters polling twice a
/// second do not rescan every process each time. Call it before taking the
/// lock; it takes none and writes nothing.
pub fn refresh_verdicts(ctx: &Context) {
    let Ok(ledger) = Ledger::load(&ctx.paths.ledger_file()) else {
        return;
    };
    cache_of(ctx).retain(|id, _| ledger.find(id).is_some());
    for e in &ledger.entries {
        if lease_is_held(ctx, &e.id) {
            cache_of(ctx).remove(&e.id);
            continue;
        }
        let now = ctx.clock.now();
        let prev = cache_of(ctx)
            .get(&e.id)
            .filter(|c| c.seq == e.seq)
            .map(|c| {
                (
                    c.assessment.verdict.is_release(),
                    (now - c.at).num_seconds(),
                )
            });
        if !crate::reap::reassess_due(prev, ctx.reassess_secs) {
            continue;
        }
        let assessment = crate::managed::assess(ctx, &e.id);
        cache_of(ctx).insert(
            e.id.clone(),
            Cached {
                seq: e.seq,
                at: now,
                assessment,
            },
        );
    }
}

/// The assessment to act on for `e` under the lock, when it is a release
/// reached recently, for this very entry and for the sidecar as it is now.
/// A kept answer is never returned: the entry is only released on a fresh
/// assessment that said so.
fn usable_release(ctx: &Context, e: &crate::ledger::Entry) -> Option<crate::managed::Assessment> {
    let c = cache_of(ctx).get(&e.id)?.clone();
    let age = (ctx.clock.now() - c.at).num_seconds();
    (c.seq == e.seq
        && c.assessment.verdict.is_release()
        && crate::reap::release_usable(age)
        && crate::managed::load_sidecar(ctx, &e.id) == c.assessment.sidecar)
        .then_some(c.assessment)
}

/// Remove ledger entries whose owner is gone, and their lease files.
/// Returns the ids removed. Caller holds the ledger lock.
///
/// A plain entry is gone when its lease is not held. A managed entry (one
/// with a sidecar, see [`crate::managed`]) also needs its child process
/// tree to be known stopped, or the machine to have rebooted; until then it
/// stays, charge and all. That proof needs process scans, which are not made
/// here: they are made by [`refresh_verdicts`] before the lock is taken, and
/// a managed entry is released here only on such a recent answer that still
/// matches the entry and its sidecar. Without one, only what needs no scan
/// (a plain entry, a finished or never-launched run, a reboot) is decided.
pub fn reap_dead(ctx: &Context, ledger: &mut Ledger) -> Vec<String> {
    let mut dead: Vec<String> = Vec::new();
    for e in &ledger.entries {
        if lease_is_held(ctx, &e.id) {
            continue;
        }
        let a =
            usable_release(ctx, e).unwrap_or_else(|| crate::managed::assess_unscanned(ctx, &e.id));
        if a.verdict.is_release() {
            crate::managed::settle_released(ctx, &a);
            dead.push(e.id.clone());
        }
    }
    for id in &dead {
        ledger.remove(id);
        cache_of(ctx).remove(id);
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

    /// Run `snapshot_read_only` on its own thread, so the caller can hold
    /// the exclusive ledger lock meanwhile.
    fn snapshot_in_thread(
        ctx: &Context,
    ) -> std::sync::mpsc::Receiver<(ReadOnlySnapshot, Duration)> {
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let start = Instant::now();
            let snap = snapshot_read_only(&ctx).unwrap();
            let _ = tx.send((snap, start.elapsed()));
        });
        rx
    }

    /// Deterministic in the direction that matters: while an exclusive
    /// lock is held the snapshot must not return (the wait is bounded by
    /// `TRY_ROUND`, 1 s, far above the 200 ms checked here), and once the
    /// lock is released it must take the shared lock (`locked == true`)
    /// instead of reading without it.
    #[test]
    fn read_only_snapshot_waits_for_the_exclusive_lock() {
        let (_d, ctx) = state_with(&["a"]);
        let _lease = take_lease(&ctx, "a").unwrap();
        let held = ledger(&ctx).unwrap();
        let rx = snapshot_in_thread(&ctx);
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the snapshot finished while the exclusive lock was held"
        );
        drop(held);
        let (snap, _) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(snap.locked);
        assert_eq!(snap.ledger.entries.len(), 1);
    }

    /// If the exclusive lock stays held past the (shortened) round, the
    /// snapshot falls back to reading without the lock: `locked == false`,
    /// and not before the round has elapsed.
    #[test]
    fn read_only_snapshot_falls_back_unlocked_after_the_round() {
        let (_d, ctx) = state_with(&["a"]);
        let ctx = ctx.with_ledger_lock_timeout(Duration::from_millis(300));
        let _lease = take_lease(&ctx, "a").unwrap();
        let _held = ledger(&ctx).unwrap();
        let rx = snapshot_in_thread(&ctx);
        let (snap, took) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!snap.locked);
        assert!(took >= Duration::from_millis(300), "{took:?}");
        assert_eq!(snap.ledger.entries.len(), 1);
    }
}
