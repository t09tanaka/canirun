//! Measured CPU use of a running command's process tree (spec "Measured
//! usage").
//!
//! The running `canirun` samples its own child's process tree from a
//! background thread and writes the cores it uses to its ledger entry, so
//! admission (and `status`) can charge a command that uses more than it
//! declared. Nothing here slows down or stops the command.

use crate::ledger::Ledger;
use crate::{lock, paths};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

/// How often the tree is sampled; each sample is the average over this
/// interval.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// What is reported is the peak of this many recent samples, so a
/// momentary dip (a worker between test files) does not free the budget.
pub const PEAK_WINDOW: usize = 3;

/// The peak of the last [`PEAK_WINDOW`] samples.
#[derive(Debug, Default)]
pub struct RecentPeak {
    samples: VecDeque<f64>,
}

impl RecentPeak {
    /// Add a sample and return the current peak.
    pub fn push(&mut self, cores: f64) -> f64 {
        if self.samples.len() == PEAK_WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(cores);
        self.samples.iter().copied().fold(0.0, f64::max)
    }
}

/// One process as far as tree accounting cares.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub parent: Option<u32>,
    /// Percent of one core (may exceed 100 for a multi-threaded process).
    pub cpu_percent: f32,
}

/// Cores used by `root` and all of its descendants.
pub fn tree_cores(procs: &[Proc], root: u32) -> f64 {
    let mut children: HashMap<u32, Vec<&Proc>> = HashMap::new();
    let mut by_pid: HashMap<u32, &Proc> = HashMap::new();
    for p in procs {
        by_pid.insert(p.pid, p);
        if let Some(parent) = p.parent {
            if parent != p.pid {
                children.entry(parent).or_default().push(p);
            }
        }
    }
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    let mut total = 0.0;
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(p) = by_pid.get(&pid) {
            total += f64::from(p.cpu_percent.max(0.0)) / 100.0;
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().map(|k| k.pid));
        }
    }
    total
}

/// Samples a process tree until dropped.
pub struct Monitor {
    stop: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn start(id: String, root: u32) -> Monitor {
        let (tx, rx) = mpsc::channel::<()>();
        // The sampler must never be the thread a SIGTERM / SIGHUP lands
        // on: the main thread relies on it interrupting `wait4`. Block
        // everything while spawning so the new thread inherits the mask.
        let old = block_all_signals();
        let handle = std::thread::Builder::new()
            .name("canirun-usage".into())
            .spawn(move || sample_loop(&id, root, &rx))
            .ok();
        restore_signals(&old);
        Monitor {
            stop: Some(tx),
            handle,
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn block_all_signals() -> libc::sigset_t {
    unsafe {
        let mut all: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut all);
        libc::pthread_sigmask(libc::SIG_BLOCK, &all, &mut old);
        old
    }
}

fn restore_signals(old: &libc::sigset_t) {
    unsafe {
        libc::pthread_sigmask(libc::SIG_SETMASK, old, std::ptr::null_mut());
    }
}

fn sample_loop(id: &str, root: u32, stop: &mpsc::Receiver<()>) {
    // Do not keep /proc files open between samples (Linux).
    sysinfo::set_open_files_limit(0);
    let kind = ProcessRefreshKind::nothing().with_cpu();
    let mut sys = System::new();
    let mut peak = RecentPeak::default();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    loop {
        match stop.recv_timeout(SAMPLE_INTERVAL) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => return,
        }
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        let procs: Vec<Proc> = sys
            .processes()
            .iter()
            .map(|(pid, p)| Proc {
                pid: pid.as_u32(),
                parent: p.parent().map(|q| q.as_u32()),
                cpu_percent: p.cpu_usage(),
            })
            .collect();
        record(id, peak.push(tree_cores(&procs, root)));
    }
}

fn record(id: &str, cores: f64) {
    let Ok(_guard) = lock::ledger() else {
        return;
    };
    let path = paths::ledger_file();
    let Ok(mut ledger) = Ledger::load(&path) else {
        return;
    };
    if let Some(e) = ledger.find_mut(id) {
        e.using = Some((cores * 10.0).round() / 10.0);
        let _ = ledger.save(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, parent: Option<u32>, cpu: f32) -> Proc {
        Proc {
            pid,
            parent,
            cpu_percent: cpu,
        }
    }

    #[test]
    fn sums_the_root_and_every_descendant_only() {
        let procs = [
            p(1, None, 50.0),
            p(10, Some(1), 100.0),
            p(11, Some(10), 250.0),
            p(12, Some(11), 50.0),
            p(20, Some(1), 400.0),
        ];
        assert!((tree_cores(&procs, 10) - 4.0).abs() < 1e-9);
        assert!((tree_cores(&procs, 12) - 0.5).abs() < 1e-9);
        assert_eq!(tree_cores(&procs, 99), 0.0);
    }

    #[test]
    fn peak_covers_the_last_few_samples() {
        let mut p = RecentPeak::default();
        assert_eq!(p.push(4.0), 4.0);
        assert_eq!(p.push(0.5), 4.0);
        assert_eq!(p.push(1.0), 4.0);
        assert_eq!(p.push(2.0), 2.0);
        assert_eq!(p.push(0.0), 2.0);
    }

    #[test]
    fn cycles_and_negative_samples_are_harmless() {
        let procs = [
            p(1, Some(1), 100.0),
            p(2, Some(3), 100.0),
            p(3, Some(2), -5.0),
        ];
        assert!((tree_cores(&procs, 1) - 1.0).abs() < 1e-9);
        assert!((tree_cores(&procs, 2) - 1.0).abs() < 1e-9);
    }
}
