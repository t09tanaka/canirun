//! Records the descendants of a running managed command (see
//! [`crate::lineage`]): a background thread reads the process table every
//! [`INTERVAL`] and hands the recorded set to a callback whenever it changes, and
//! again about once a second while it does not (a heartbeat).
//! Nothing here slows down or stops the command.

use crate::lineage::{self, Tracked, MAX_DESCENDANTS};
use crate::marker::ProcRef;
use crate::process::{self, TableReader};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often the process table is read. A descendant that forks and
/// loses its parent within one interval is missed.
pub const INTERVAL: Duration = Duration::from_millis(250);

/// Called with everything recorded so far: when it changed, as a heartbeat,
/// and again every tick while a write fails. The flag says whether to stamp
/// `recorded_at` (false after a look that could not be believed). Returns
/// whether the record was persisted.
pub type Record = Arc<dyn Fn(&Tracked, bool) -> bool + Send + Sync>;

/// What a tracker ends with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub tracked: Tracked,
    /// The record covers the command's whole life: every look was good
    /// (nothing is truncated) and the last one is persisted. Only then may a
    /// reader end the window of hidden orphans at the last record.
    pub covered: bool,
}

impl Finished {
    fn unknown() -> Finished {
        Finished {
            tracked: Tracked::unknown(),
            covered: false,
        }
    }
}

pub struct Tracker {
    /// Told whether the command was reaped (so the last look does not ask
    /// the kernel about a pid that may have been reused).
    stop: Option<Sender<bool>>,
    /// `Err` when the thread could not start (or the command could not be
    /// read); `None` once joined.
    handle: Option<std::io::Result<JoinHandle<Finished>>>,
}

impl Tracker {
    /// Start recording the descendants of `root`, which must be a live
    /// child of this process.
    pub fn start(root: u32, record: Record) -> Tracker {
        let (tx, rx) = mpsc::channel::<bool>();
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
            .name("task-traffic-controller-track".into())
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

    /// A recorder that starts and records nothing, as when no look has
    /// happened yet.
    #[doc(hidden)]
    pub fn idle(_root: u32, _record: Record) -> Tracker {
        Tracker {
            stop: None,
            handle: Some(Ok(std::thread::spawn(|| Finished {
                tracked: Tracked::default(),
                covered: false,
            }))),
        }
    }

    /// Stop after one last look, and return what was recorded. `reaped`:
    /// the caller has reaped the command (`wait4`), so it is gone.
    pub fn finish(mut self, reaped: bool) -> Finished {
        self.join(reaped)
    }

    fn join(&mut self, reaped: bool) -> Finished {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(reaped);
        }
        join_outcome(self.handle.take())
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        let _ = self.join(false);
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
fn join_outcome(handle: Option<std::io::Result<JoinHandle<Finished>>>) -> Finished {
    match handle {
        Some(Ok(h)) => h.join().unwrap_or_else(|_| Finished::unknown()),
        Some(Err(_)) => Finished::unknown(),
        // Already joined (by `finish`, then again on drop).
        None => Finished {
            tracked: Tracked::default(),
            covered: false,
        },
    }
}

/// When the record was last persisted, and whether a write is owed.
#[derive(Default)]
struct Writes {
    last: Option<Instant>,
    pending: bool,
}

fn track_loop(root: ProcRef, record: &Record, stop: &mpsc::Receiver<bool>) -> Finished {
    sysinfo::set_open_files_limit(0);
    let mut reader = TableReader::new();
    let mut tracked = Tracked::default();
    let mut missed = Vec::new();
    let mut writes = Writes::default();
    loop {
        tick(
            &mut reader,
            root,
            false,
            &mut tracked,
            &mut missed,
            record,
            &mut writes,
            Instant::now(),
        );
        match stop.recv_timeout(INTERVAL) {
            Err(RecvTimeoutError::Timeout) => {}
            // One last look: the leader is likely reaped by now, and the
            // recorded ones still seed the walk.
            done => {
                let reaped = done.unwrap_or(false);
                let step = tick(
                    &mut reader,
                    root,
                    reaped,
                    &mut tracked,
                    &mut missed,
                    record,
                    &mut writes,
                    Instant::now(),
                );
                let covered = !step.failed && !tracked.truncated && !writes.pending;
                return Finished { tracked, covered };
            }
        }
    }
}

/// One look, at `now`. The record is handed over when it changed, again
/// every tick while a write fails, and as a heartbeat (at most once per
/// [`lineage::HEARTBEAT`] of successful writes) when it did not, so the
/// sidecar's `recorded_at` says when the tracker last worked. A look that
/// could not be believed is handed over without the stamp. `reaped`: the
/// command was reaped by the caller, so it is gone and the kernel is not
/// asked about its pid.
#[allow(clippy::too_many_arguments)]
fn tick(
    reader: &mut TableReader,
    root: ProcRef,
    reaped: bool,
    tracked: &mut Tracked,
    missed: &mut Vec<ProcRef>,
    record: &Record,
    writes: &mut Writes,
    now: Instant,
) -> lineage::Step {
    let table = reader.read();
    let gone = lineage::root_is_gone(reaped, lineage::contains_root(&table, root), || {
        process::root_gone(root.pid)
    });
    let step = lineage::step(&table, root, gone, tracked, missed, MAX_DESCENDANTS);
    let plan = lineage::plan(
        step,
        writes.pending,
        writes.last.map(|t| now.saturating_duration_since(t)),
    );
    if plan.write {
        let ok = record(tracked, plan.stamp);
        // Only a stamped write is a heartbeat.
        if ok && plan.stamp {
            writes.last = Some(now);
        }
        writes.pending = lineage::still_pending(plan, ok);
    }
    step
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicked_or_unstarted_thread_is_unknown_not_empty() {
        let panicked = std::thread::spawn(|| -> Finished { panic!("boom") });
        let t = join_outcome(Some(Ok(panicked)));
        assert_eq!(t, Finished::unknown());
        assert!(t.tracked.truncated && t.tracked.descendants.is_empty() && !t.covered);
        let t = join_outcome(Some(Err(std::io::Error::other("no thread"))));
        assert_eq!(t, Finished::unknown());
        let ok = Finished {
            tracked: Tracked::default(),
            covered: true,
        };
        let o = ok.clone();
        let fine = std::thread::spawn(move || o);
        assert_eq!(join_outcome(Some(Ok(fine))), ok);
        assert!(!join_outcome(None).covered);
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
        let handle = std::thread::spawn(|| Finished {
            tracked: Tracked {
                descendants: Vec::new(),
                truncated: true,
            },
            covered: false,
        });
        let tracker = Tracker {
            stop: None,
            handle: Some(Ok(handle)),
        };
        assert!(tracker.finish(false).tracked.truncated);
    }

    #[test]
    fn a_command_that_cannot_be_read_while_running_is_unknown() {
        // A pid that is no process and no zombie, as if sysinfo failed.
        let tracker = Tracker::start(0x3fff_fff0, Arc::new(|_, _| true));
        assert_eq!(tracker.finish(false), Finished::unknown());
    }

    #[test]
    fn a_child_that_ended_unreaped_is_not_unknown() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        // Let it exit without reaping it.
        while !process::has_ended(child.id()) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let t = Tracker::start(child.id(), Arc::new(|_, _| true)).finish(false);
        assert!(!t.tracked.truncated && t.covered, "{t:?}");
        child.wait().unwrap();
    }

    #[test]
    fn a_running_child_is_tracked_and_a_finished_one_is_not_unknown() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let tracker = Tracker::start(child.id(), Arc::new(|_, _| true));
        let t = tracker.finish(false);
        assert!(!t.tracked.truncated && t.covered, "{t:?}");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// One tick at `now` on a live child, with a recorder that answers
    /// `ok`; returns what it was handed (`None`: it was not called).
    fn tick_at(pid: u32, writes: &mut Writes, now: Instant, ok: bool) -> Option<(Tracked, bool)> {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let s = seen.clone();
        let record: Record = Arc::new(move |t, stamp| {
            *s.lock().unwrap() = Some((t.clone(), stamp));
            ok
        });
        let root = ProcRef {
            pid,
            start_time: process::start_time(pid),
        };
        tick(
            &mut TableReader::new(),
            root,
            false,
            &mut Tracked::default(),
            &mut Vec::new(),
            &record,
            writes,
            now,
        );
        let got = seen.lock().unwrap().take();
        got
    }

    #[test]
    fn a_quiet_run_is_stamped_once_a_second_and_a_failed_write_is_retried() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut w = Writes::default();
        // The first look writes, stamped; then one per second, not per look.
        let (t, stamp) = tick_at(child.id(), &mut w, t0, true).unwrap();
        assert!(stamp && !t.truncated && t.descendants.is_empty(), "{t:?}");
        assert!(tick_at(child.id(), &mut w, t0 + ms(250), true).is_none());
        assert!(tick_at(child.id(), &mut w, t0 + ms(999), true).is_none());
        assert!(tick_at(child.id(), &mut w, t0 + ms(1000), true).is_some());
        assert!(tick_at(child.id(), &mut w, t0 + ms(1250), true).is_none());
        // A write that fails is owed: retried every tick, and the time of
        // the last write does not move until one succeeds.
        assert!(tick_at(child.id(), &mut w, t0 + ms(2000), false).is_some());
        assert!(w.pending);
        assert!(tick_at(child.id(), &mut w, t0 + ms(2250), false).is_some());
        assert!(w.pending && w.last == Some(t0 + ms(1000)));
        assert!(tick_at(child.id(), &mut w, t0 + ms(2500), true).is_some());
        assert!(!w.pending && w.last == Some(t0 + ms(2500)));
        assert!(tick_at(child.id(), &mut w, t0 + ms(2750), true).is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn a_failed_look_is_handed_over_without_the_stamp() {
        // A command the table does not list although it is not gone: the
        // look fails, so the record is truncated and not stamped.
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let root = ProcRef {
            pid: child.id(),
            start_time: Some(1),
        };
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = seen.clone();
        let record: Record = Arc::new(move |t, stamp| {
            s.lock().unwrap().push((t.truncated, stamp));
            true
        });
        let mut w = Writes::default();
        let mut tracked = Tracked::default();
        let step = tick(
            &mut TableReader::new(),
            root,
            false,
            &mut tracked,
            &mut Vec::new(),
            &record,
            &mut w,
            Instant::now(),
        );
        assert!(step.failed && tracked.truncated);
        assert_eq!(*seen.lock().unwrap(), vec![(true, false)]);
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
