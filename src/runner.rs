//! Spawn the gated command, forward signals, and collect rusage via
//! `wait4` (spec `exec` steps 5–6).
//!
//! Nothing here installs a signal handler or reads the process
//! environment: stop signals arrive through [`crate::Supervisor`], which
//! the caller owns. To run several commands in one process, give each run
//! its own supervisor; this module has no process-global state. (A
//! supervisor can only interrupt `wait4` on the thread that receives the
//! signal, so a multi-threaded embedder cancels through the `cancelled`
//! hook of [`crate::exec::run_gated`] and kills by pid instead.)

use crate::error::{Error, Result};
use crate::lineage::Tracked;
use crate::marker;
use crate::reap::Probe;
use crate::Context;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

impl Exit {
    /// Shell-style code: 128+N for a signal death.
    pub fn code(self) -> i32 {
        match self {
            Exit::Code(c) => c,
            Exit::Signal(s) => 128 + s,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Outcome {
    pub exit: Exit,
    pub wall_secs: f64,
    pub cpu_secs: f64,
    pub max_rss_bytes: u64,
    /// A managed command still had descendants after the leader was reaped
    /// (in its process group, or carrying its `CANIRUN_MANAGED_ID`
    /// elsewhere) and they were sent SIGTERM, then SIGKILL, but were not
    /// confirmed gone in time or could not be checked. Always false for a
    /// plain run.
    pub descendants_left: bool,
    /// The recorder of the descendants failed (or recorded more than fit):
    /// whether anything is left cannot be ruled out. [`crate::managed::finish`]
    /// then keeps the entry. Always false for a plain run.
    pub tracker_unknown: bool,
}

/// Run `argv` with inherited stdio in our process group, in `cwd`, with
/// `CANIRUN_LEASE=lease` in its environment. With `measure`, the child's
/// process tree is sampled while it runs and the cores it uses are written
/// to the ledger entry `lease`.
pub fn run(
    ctx: &Context,
    argv: &[String],
    cwd: &Path,
    lease: &str,
    measure: bool,
) -> Result<Outcome> {
    run_with(ctx, argv, cwd, lease, measure, Spawn::default())
}

/// How [`run_with`] spawns.
#[derive(Default)]
pub struct Spawn<'a> {
    /// Put the child in a process group of its own (a managed command, so
    /// the whole tree can be probed and stopped by group). Off for a plain
    /// `exec`, where terminal signals must keep reaching the child.
    pub own_process_group: bool,
    /// Put `CANIRUN_MANAGED_ID=<id>` in the child's environment, and after
    /// the leader exits also stop any process still carrying it, even one
    /// that left the process group.
    pub marker_id: Option<&'a str>,
    /// Called with the child's pid right after the spawn.
    pub on_spawn: Option<&'a dyn Fn(u32)>,
    /// With `own_process_group` and `marker_id`: the descendants of the
    /// child are recorded while it runs (see [`crate::tracker`]) and handed
    /// to this each time the set grows. Those still running are stopped
    /// together with the marked ones.
    pub on_descendants: Option<crate::tracker::Record>,
    /// Replaces [`crate::tracker::Tracker::start`] (tests make it fail).
    #[doc(hidden)]
    pub start_tracker: Option<StartTracker>,
}

/// Starts the recorder of a command's descendants.
pub type StartTracker = fn(u32, crate::tracker::Record) -> crate::tracker::Tracker;

/// Grace between SIGTERM and SIGKILL for descendants that outlive a
/// managed command's leader, and how long to wait for the group to empty
/// after SIGKILL.
const GROUP_TERM_GRACE: Duration = Duration::from_secs(2);
const GROUP_KILL_WAIT: Duration = Duration::from_secs(3);

/// How often the marker scan is repeated while waiting for marked
/// processes to exit.
const MARKER_POLL: Duration = Duration::from_millis(100);
/// How long to wait for marked processes to be gone after SIGKILL; a scan
/// reads every process of the user, so it is slower than a group probe.
const MARKER_KILL_WAIT: Duration = Duration::from_secs(8);
/// Rescans before an unknown answer is believed (see [`settled_scan`]).
const UNKNOWN_RETRIES: usize = 3;

fn group_empty(pgid: libc::pid_t) -> bool {
    crate::process::probe_group(Some(pgid)) == crate::reap::Probe::Gone
}

/// Stop whatever is left of the process group `pgid` once its leader is
/// gone; true if it is confirmed empty.
fn clear_group(ctx: &Context, pgid: libc::pid_t) -> bool {
    if group_empty(pgid) {
        return true;
    }
    for (sig, wait) in [
        (libc::SIGTERM, GROUP_TERM_GRACE),
        (libc::SIGKILL, GROUP_KILL_WAIT),
    ] {
        unsafe {
            libc::killpg(pgid, sig);
        }
        let until = ctx.clock.monotonic() + wait;
        while ctx.clock.monotonic() < until {
            if group_empty(pgid) {
                return true;
            }
            ctx.clock.sleep(Duration::from_millis(20));
        }
        if group_empty(pgid) {
            return true;
        }
    }
    false
}

/// Scan for the processes of managed command `id` (marked, or recorded in
/// `tracked`) until the answer can be
/// believed. A process that exits while the scan reads it can make one scan
/// `Unknown` (only an answer that stays unknown through
/// [`UNKNOWN_RETRIES`] rescans counts) or miss a process that was just
/// forked (so `Gone` must come twice in a row). `Alive` is believed at once.
pub(crate) fn settled_scan(ctx: &Context, id: &str, tracked: &Tracked) -> crate::marker::Scan {
    let (mut gone, mut unknown) = (0, 0);
    loop {
        let scan = ctx.scan_tracked(id, tracked);
        match scan.probe {
            Probe::Alive => return scan,
            Probe::Gone => {
                gone += 1;
                if gone >= 2 {
                    return scan;
                }
            }
            Probe::Unknown => {
                gone = 0;
                unknown += 1;
                if unknown > UNKNOWN_RETRIES {
                    return scan;
                }
            }
        }
        ctx.clock.sleep(MARKER_POLL);
    }
}

/// Stop every process still carrying `CANIRUN_MANAGED_ID=<id>` (and this
/// state directory's scope): SIGTERM, a grace, then SIGKILL, signalling
/// only a pid that is still the process that was seen. True only when
/// scans then find none, twice in a row, and every process could be read.
pub(crate) fn stop_marked(ctx: &Context, id: &str, tracked: &Tracked) -> bool {
    let mut scan = settled_scan(ctx, id, tracked);
    if scan.probe != Probe::Alive {
        return scan.probe == Probe::Gone;
    }
    for (sig, wait) in [
        (libc::SIGTERM, GROUP_TERM_GRACE),
        (libc::SIGKILL, MARKER_KILL_WAIT),
    ] {
        for p in &scan.alive {
            crate::process::signal_if_same(p, sig);
        }
        let until = ctx.clock.monotonic() + wait;
        loop {
            ctx.clock.sleep(MARKER_POLL);
            scan = settled_scan(ctx, id, tracked);
            if scan.probe != Probe::Alive {
                return scan.probe == Probe::Gone;
            }
            if ctx.clock.monotonic() >= until {
                break;
            }
        }
    }
    false
}

/// [`run`] with spawn options.
pub fn run_with(
    ctx: &Context,
    argv: &[String],
    cwd: &Path,
    lease: &str,
    measure: bool,
    spawn: Spawn<'_>,
) -> Result<Outcome> {
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| Error::Spawn("no command given".into()))?;
    ctx.supervisor.running();
    let start = ctx.clock.monotonic();
    let mut cmd = Command::new(prog);
    cmd.args(args).current_dir(cwd).env("CANIRUN_LEASE", lease);
    if spawn.own_process_group {
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    }
    if let Some(id) = spawn.marker_id {
        cmd.env(marker::ENV_VAR, id)
            .env(marker::SCOPE_VAR, marker::scope(ctx.paths.state_dir()));
    }
    let child = cmd
        .spawn()
        .map_err(|e| Error::Spawn(format!("cannot run {prog:?}: {e}{}", spawn_hint(argv, &e))))?;
    let pid = child.id() as libc::pid_t;
    if let Some(f) = spawn.on_spawn {
        f(child.id());
    }
    // Records what the child starts, to find it again if it leaves the
    // group. Stopped and joined once the child has been reaped.
    let tracker = match (
        &spawn.on_descendants,
        spawn.own_process_group,
        spawn.marker_id,
    ) {
        (Some(record), true, Some(_)) => {
            let start = spawn
                .start_tracker
                .unwrap_or(crate::tracker::Tracker::start);
            Some(start(child.id(), record.clone()))
        }
        _ => None,
    };
    // Dropped (stopped and joined) once the child has been reaped.
    let _monitor =
        measure.then(|| crate::usage::Monitor::start(ctx.clone(), lease.to_string(), child.id()));
    // A signal delivered between handler installation and spawn is
    // forwarded now rather than waiting for the next EINTR.
    let early = ctx.supervisor.take_pending();
    // A managed command leads its own group, so the signal goes to all of
    // it; a plain run keeps the terminal's own delivery and signals the
    // child alone.
    let forward = |sig: libc::c_int| unsafe {
        if spawn.own_process_group {
            libc::killpg(pid, sig);
        } else {
            libc::kill(pid, sig);
        }
    };
    if early != 0 {
        forward(early);
    }

    let mut status: libc::c_int = 0;
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        let r = unsafe { libc::wait4(pid, &mut status, 0, &mut ru) };
        if r == pid {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            let sig = ctx.supervisor.take_pending();
            if sig != 0 {
                forward(sig);
            }
            continue;
        }
        return Err(Error::Spawn(format!("wait4 failed: {err}")));
    }
    let wall_secs = (ctx.clock.monotonic().saturating_sub(start)).as_secs_f64();
    // The leader is gone, but a managed command's descendants may not be:
    // the budget is only free once the whole group, and anything marked
    // that left it, is confirmed stopped.
    // No tracker for a managed command is not "nothing recorded": a clear
    // scan cannot be believed.
    let tracked = match tracker {
        Some(t) => t.finish(),
        None if spawn.own_process_group && spawn.marker_id.is_some() => Tracked::unknown(),
        None => Tracked::default(),
    };
    // A recorder that failed left nothing in the sidecar to say so: write
    // the unknown there too, or the reaper would read an empty record as
    // "nothing was running".
    if tracked.truncated {
        if let Some(record) = &spawn.on_descendants {
            record(&tracked);
        }
    }
    let tracker_unknown = tracked.truncated;
    let descendants_left = spawn.own_process_group && {
        let group = clear_group(ctx, pid);
        let marked = spawn
            .marker_id
            .is_none_or(|id| stop_marked(ctx, id, &tracked));
        !(group && marked)
    };

    let exit = if libc::WIFEXITED(status) {
        Exit::Code(libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        Exit::Signal(libc::WTERMSIG(status))
    } else {
        Exit::Code(1)
    };
    let cpu_secs = tv(ru.ru_utime) + tv(ru.ru_stime);
    let max_rss_bytes = normalise_maxrss(ru.ru_maxrss as u64);
    Ok(Outcome {
        exit,
        wall_secs,
        cpu_secs,
        max_rss_bytes,
        descendants_left,
        tracker_unknown,
    })
}

/// `canirun exec -- "npm run x"` looks for a program named `npm run x`.
/// When that is the likely mistake, say how to write it instead.
fn spawn_hint(argv: &[String], e: &std::io::Error) -> String {
    match argv {
        [one] if e.kind() == std::io::ErrorKind::NotFound && one.contains(char::is_whitespace) => {
            format!(
                "\ncanirun: hint: the command after `--` is one quoted argument; \
                 pass it as separate words: `-- {0}`, not `-- \"{0}\"` \
                 (for shell syntax, `-- sh -c '...'`)",
                one.trim()
            )
        }
        _ => String::new(),
    }
}

fn tv(t: libc::timeval) -> f64 {
    t.tv_sec as f64 + t.tv_usec as f64 / 1_000_000.0
}

/// macOS reports `ru_maxrss` in bytes, Linux in kibibytes.
fn normalise_maxrss(raw: u64) -> u64 {
    if cfg!(target_os = "macos") {
        raw
    } else {
        raw.saturating_mul(1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error as IoError, ErrorKind};

    use crate::marker::{ProcRef, Scan};
    use crate::{FakeClock, NoSupervisor, Paths, ProcessScan};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Answers the scripted probes in order, then the last one forever, and
    /// counts the scans.
    struct Scripted {
        left: Mutex<VecDeque<Probe>>,
        last: Mutex<Probe>,
        calls: Mutex<usize>,
    }

    impl ProcessScan for Scripted {
        fn scan(&self, _id: &str, _scope: &str) -> Scan {
            *self.calls.lock().unwrap() += 1;
            let mut left = self.left.lock().unwrap();
            let probe = left.pop_front().unwrap_or(*self.last.lock().unwrap());
            *self.last.lock().unwrap() = probe;
            // A marked process whose start time is unknown is never signalled.
            let alive = if probe == Probe::Alive {
                vec![ProcRef {
                    pid: u32::MAX,
                    start_time: None,
                }]
            } else {
                Vec::new()
            };
            Scan::new(probe, alive)
        }
    }

    /// The probes the scan answers (then the last repeats) and how many
    /// scans one call to `f` made.
    fn scripted<T>(probes: &[Probe], f: impl FnOnce(&Context) -> T) -> (T, usize) {
        let scan = Arc::new(Scripted {
            left: Mutex::new(probes.iter().copied().collect()),
            last: Mutex::new(*probes.last().unwrap()),
            calls: Mutex::new(0),
        });
        let ctx = Context::new(Paths::new("/nonexistent-state", "/nonexistent-config"))
            .with_clock(Arc::new(FakeClock::new(chrono::Utc::now())))
            .with_supervisor(Arc::new(NoSupervisor))
            .with_scan(scan.clone());
        let out = f(&ctx);
        let n = *scan.calls.lock().unwrap();
        (out, n)
    }

    use Probe::{Alive, Gone, Unknown};

    #[test]
    fn gone_is_believed_only_when_it_comes_twice_in_a_row() {
        let (s, n) = scripted(&[Gone, Gone], |c| settled_scan(c, "i", &Tracked::default()));
        assert_eq!((s.probe, n), (Gone, 2));
        let (s, n) = scripted(&[Gone, Alive], |c| {
            settled_scan(c, "i", &Tracked::default())
        });
        assert_eq!((s.probe, n), (Alive, 2));
        // An unknown between two clear scans starts the count again.
        let (s, n) = scripted(&[Gone, Unknown, Gone, Gone], |c| {
            settled_scan(c, "i", &Tracked::default())
        });
        assert_eq!((s.probe, n), (Gone, 4));
        let (s, n) = scripted(&[Alive], |c| settled_scan(c, "i", &Tracked::default()));
        assert_eq!((s.probe, n), (Alive, 1));
    }

    #[test]
    fn unknown_is_rescanned_a_few_times_before_it_is_believed() {
        let (s, n) = scripted(&[Unknown], |c| settled_scan(c, "i", &Tracked::default()));
        assert_eq!((s.probe, n), (Unknown, UNKNOWN_RETRIES + 1));
        let (s, n) = scripted(&[Unknown, Unknown, Unknown, Gone, Gone], |c| {
            settled_scan(c, "i", &Tracked::default())
        });
        assert_eq!((s.probe, n), (Gone, 5));
    }

    #[test]
    fn stop_marked_confirms_gone_twice_and_retries_unknown_after_signalling() {
        assert!(!scripted(&[Unknown], |c| stop_marked(c, "i", &Tracked::default())).0);
        assert!(scripted(&[Gone, Gone], |c| stop_marked(c, "i", &Tracked::default())).0);
        assert!(
            !scripted(&[Gone, Alive, Alive], |c| stop_marked(
                c,
                "i",
                &Tracked::default()
            ))
            .0
        );
        // After the signals an unknown is rescanned like the first one, and
        // a single clear scan is not enough.
        let after = [Alive, Unknown, Unknown, Unknown, Gone, Gone];
        assert!(scripted(&after, |c| stop_marked(c, "i", &Tracked::default())).0);
        let one_clear = [Alive, Gone, Alive];
        assert!(!scripted(&one_clear, |c| stop_marked(c, "i", &Tracked::default())).0);
        let stays_unknown = [Alive, Unknown];
        assert!(!scripted(&stays_unknown, |c| stop_marked(c, "i", &Tracked::default())).0);
    }

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn hint_only_for_one_spaced_argument_not_found() {
        let nf = IoError::from(ErrorKind::NotFound);
        let h = spawn_hint(&argv(&["npm run x"]), &nf);
        assert!(h.contains("`-- npm run x`"), "{h}");
        assert!(h.contains("sh -c"), "{h}");
        assert_eq!(spawn_hint(&argv(&["npm"]), &nf), "");
        assert_eq!(spawn_hint(&argv(&["npm run", "x"]), &nf), "");
        let denied = IoError::from(ErrorKind::PermissionDenied);
        assert_eq!(spawn_hint(&argv(&["npm run x"]), &denied), "");
    }
}
