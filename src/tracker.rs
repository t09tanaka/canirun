//! Records the descendants of a running managed command (see
//! [`crate::lineage`]): a background thread reads the process table every
//! [`INTERVAL`] and hands the recorded set to a callback whenever it grows.
//! Nothing here slows down or stops the command.

use crate::lineage::{self, Tracked, MAX_DESCENDANTS};
use crate::marker::ProcRef;
use crate::process::{self, TableReader};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// How often the process table is read. A descendant that forks and
/// loses its parent within one interval is missed.
pub const INTERVAL: Duration = Duration::from_millis(250);

/// Called with everything recorded so far, only when it changed.
pub type Record = Arc<dyn Fn(&Tracked) + Send + Sync>;

pub struct Tracker {
    stop: Option<Sender<()>>,
    handle: Option<JoinHandle<Tracked>>,
}

impl Tracker {
    /// Start recording the descendants of `root`, which must be a live
    /// child of this process.
    pub fn start(root: u32, record: Record) -> Tracker {
        let (tx, rx) = mpsc::channel::<()>();
        let root = ProcRef {
            pid: root,
            start_time: process::start_time(root),
        };
        // Like the usage sampler: never the thread a SIGTERM lands on.
        let old = crate::usage::block_all_signals();
        let handle = std::thread::Builder::new()
            .name("canirun-track".into())
            .spawn(move || track_loop(root, &record, &rx))
            .ok();
        crate::usage::restore_signals(&old);
        Tracker {
            stop: Some(tx),
            handle,
        }
    }

    /// Stop after one last look, and return what was recorded.
    pub fn finish(mut self) -> Tracked {
        self.join()
    }

    fn join(&mut self) -> Tracked {
        drop(self.stop.take());
        self.handle
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

fn track_loop(root: ProcRef, record: &Record, stop: &mpsc::Receiver<()>) -> Tracked {
    sysinfo::set_open_files_limit(0);
    let mut reader = TableReader::new();
    let mut tracked = Tracked::default();
    loop {
        tick(&mut reader, root, &mut tracked, record);
        if !matches!(stop.recv_timeout(INTERVAL), Err(RecvTimeoutError::Timeout)) {
            // One last look: the leader is likely reaped by now, and the
            // recorded ones still seed the walk.
            tick(&mut reader, root, &mut tracked, record);
            return tracked;
        }
    }
}

fn tick(reader: &mut TableReader, root: ProcRef, tracked: &mut Tracked, record: &Record) {
    let table = reader.read();
    // What died is no longer worth recording: the cap counts what runs.
    tracked.descendants = lineage::alive_in(&table, &tracked.descendants);
    let found = lineage::descendants_of(&table, root, &tracked.descendants);
    let m = lineage::merge(
        &tracked.descendants,
        tracked.truncated,
        &found,
        MAX_DESCENDANTS,
    );
    if !m.added.is_empty() || m.truncated != tracked.truncated {
        tracked.descendants.extend(m.added);
        tracked.truncated = m.truncated;
        record(tracked);
    }
}
