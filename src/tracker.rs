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
    /// `Err` when the thread could not start (or the command could not be
    /// read); `None` once joined.
    handle: Option<std::io::Result<JoinHandle<Tracked>>>,
}

impl Tracker {
    /// Start recording the descendants of `root`, which must be a live
    /// child of this process.
    pub fn start(root: u32, record: Record) -> Tracker {
        let (tx, rx) = mpsc::channel::<()>();
        let Some(start_time) = root_start_with(
            || process::start_time(root),
            || process::has_ended(root),
            ROOT_READ_TRIES,
            || std::thread::sleep(Duration::from_millis(10)),
        ) else {
            return Tracker {
                stop: None,
                handle: Some(Err(std::io::Error::other("cannot read the command"))),
            };
        };
        let root = ProcRef {
            pid: root,
            start_time,
        };
        // Like the usage sampler: never the thread a SIGTERM lands on.
        let old = crate::usage::block_all_signals();
        let handle = std::thread::Builder::new()
            .name("canirun-track".into())
            .spawn(move || track_loop(root, &record, &rx));
        crate::usage::restore_signals(&old);
        Tracker {
            stop: Some(tx),
            handle: Some(handle),
        }
    }

    /// A recorder that could not start, as when the command cannot be read.
    #[doc(hidden)]
    pub fn failed(_root: u32, _record: Record) -> Tracker {
        Tracker {
            stop: None,
            handle: Some(Err(std::io::Error::other("recorder failed to start"))),
        }
    }

    /// Stop after one last look, and return what was recorded.
    pub fn finish(mut self) -> Tracked {
        self.join()
    }

    fn join(&mut self) -> Tracked {
        drop(self.stop.take());
        join_outcome(self.handle.take())
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

/// How many times the command's start time is read before it counts as
/// unreadable: right after the spawn the process table may not show it yet.
const ROOT_READ_TRIES: usize = 20;

/// The command's start time (`None` inside when it has already ended), or
/// `None` when it is still running but could not be read in `tries` looks.
fn root_start_with(
    mut start: impl FnMut() -> Option<u64>,
    mut ended: impl FnMut() -> bool,
    tries: usize,
    mut pause: impl FnMut(),
) -> Option<Option<u64>> {
    for i in 0..tries {
        if i > 0 {
            pause();
        }
        let t = start();
        if !lineage::root_unreadable(t, ended()) {
            return Some(t);
        }
    }
    None
}

/// What the thread recorded. A thread that panicked or never started
/// recorded nothing reliable: that is [`Tracked::unknown`], never "no
/// descendants".
fn join_outcome(handle: Option<std::io::Result<JoinHandle<Tracked>>>) -> Tracked {
    match handle {
        Some(Ok(h)) => h.join().unwrap_or_else(|_| Tracked::unknown()),
        Some(Err(_)) => Tracked::unknown(),
        // Already joined (by `finish`, then again on drop).
        None => Tracked::default(),
    }
}

fn track_loop(root: ProcRef, record: &Record, stop: &mpsc::Receiver<()>) -> Tracked {
    sysinfo::set_open_files_limit(0);
    let mut reader = TableReader::new();
    let mut tracked = Tracked::default();
    let mut missed = Vec::new();
    loop {
        tick(&mut reader, root, &mut tracked, &mut missed, record);
        if !matches!(stop.recv_timeout(INTERVAL), Err(RecvTimeoutError::Timeout)) {
            // One last look: the leader is likely reaped by now, and the
            // recorded ones still seed the walk.
            tick(&mut reader, root, &mut tracked, &mut missed, record);
            return tracked;
        }
    }
}

fn tick(
    reader: &mut TableReader,
    root: ProcRef,
    tracked: &mut Tracked,
    missed: &mut Vec<ProcRef>,
    record: &Record,
) {
    let table = reader.read();
    if lineage::step(&table, root, tracked, missed, MAX_DESCENDANTS) {
        record(tracked);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicked_or_unstarted_thread_is_unknown_not_empty() {
        let panicked = std::thread::spawn(|| -> Tracked { panic!("boom") });
        let t = join_outcome(Some(Ok(panicked)));
        assert_eq!(t, Tracked::unknown());
        assert!(t.truncated && t.descendants.is_empty());
        let t = join_outcome(Some(Err(std::io::Error::other("no thread"))));
        assert_eq!(t, Tracked::unknown());
        let fine = std::thread::spawn(Tracked::default);
        assert_eq!(join_outcome(Some(Ok(fine))), Tracked::default());
        assert_eq!(join_outcome(None), Tracked::default());
    }

    #[test]
    fn the_command_start_is_read_again_before_it_counts_as_unreadable() {
        let mut n = 0;
        let mut pauses = 0;
        let got = root_start_with(
            || {
                n += 1;
                (n == 3).then_some(7)
            },
            || false,
            5,
            || pauses += 1,
        );
        assert_eq!((got, n, pauses), (Some(Some(7)), 3, 2));
        // Ended: nothing to read, and not an error.
        assert_eq!(root_start_with(|| None, || true, 5, || {}), Some(None));
        // Never readable while running.
        let mut looks = 0;
        assert_eq!(
            root_start_with(
                || {
                    looks += 1;
                    None
                },
                || false,
                4,
                || {}
            ),
            None
        );
        assert_eq!(looks, 4);
    }

    #[test]
    fn finish_reports_what_the_thread_returned() {
        let handle = std::thread::spawn(|| Tracked {
            descendants: Vec::new(),
            truncated: true,
        });
        let tracker = Tracker {
            stop: None,
            handle: Some(Ok(handle)),
        };
        assert!(tracker.finish().truncated);
    }

    #[test]
    fn a_command_that_cannot_be_read_while_running_is_unknown() {
        // A pid that is no process and no zombie, as if sysinfo failed.
        let tracker = Tracker::start(0x3fff_fff0, Arc::new(|_| {}));
        assert_eq!(tracker.finish(), Tracked::unknown());
    }

    #[test]
    fn a_child_that_ended_unreaped_is_not_unknown() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        // Let it exit without reaping it.
        while !process::has_ended(child.id()) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let t = Tracker::start(child.id(), Arc::new(|_| {})).finish();
        assert!(!t.truncated, "{t:?}");
        child.wait().unwrap();
    }

    #[test]
    fn a_running_child_is_tracked_and_a_finished_one_is_not_unknown() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let tracker = Tracker::start(child.id(), Arc::new(|_| {}));
        let t = tracker.finish();
        assert!(!t.truncated, "{t:?}");
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
