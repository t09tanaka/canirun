//! flock helpers: the ledger lock, per-entry lease locks, and the liveness
//! probe that reaps dead entries (spec "Ledger and locking").

use crate::error::{Error, Result};
use crate::ledger::Ledger;
use crate::paths;
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(20);
pub const LEDGER_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

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
                    return Err(Error::Ledger(format!(
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

pub fn ledger() -> Result<Lock> {
    acquire(&paths::ledger_lock(), LEDGER_LOCK_TIMEOUT)
}

/// Take the lease for `id`. Held by the owning canirun process until it
/// exits; the fd is close-on-exec (Rust std default) so children never
/// inherit it.
pub fn take_lease(id: &str) -> Result<Lock> {
    let path = paths::lease_file(id);
    let file = open_lock_file(&path)?;
    file.try_lock_exclusive()
        .map_err(|e| Error::Ledger(format!("lease {} already held: {e}", path.display())))?;
    Ok(Lock { file })
}

/// True when some process currently holds the lease lock for `id`.
pub fn lease_is_held(id: &str) -> bool {
    let path = paths::lease_file(id);
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

/// Remove ledger entries whose lease is not held, and their lease files.
/// Returns the ids removed. Caller holds the ledger lock.
pub fn reap_dead(ledger: &mut Ledger) -> Vec<String> {
    let dead: Vec<String> = ledger
        .entries
        .iter()
        .filter(|e| !lease_is_held(&e.id))
        .map(|e| e.id.clone())
        .collect();
    for id in &dead {
        ledger.remove(id);
        let _ = std::fs::remove_file(paths::lease_file(id));
    }
    dead
}

/// Remove lease files that no ledger entry refers to (left by crashes
/// between lease creation and ledger registration).
pub fn sweep_orphan_leases(ledger: &Ledger) {
    let Ok(rd) = std::fs::read_dir(paths::leases_dir()) else {
        return;
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".lock")) else {
            continue;
        };
        if ledger.find(id).is_none() && !lease_is_held(id) {
            let _ = std::fs::remove_file(ent.path());
        }
    }
}
