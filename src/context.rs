//! Everything the library needs from its surroundings, passed in
//! explicitly: file locations, the clock, the live-metrics source, the
//! signal supervisor, and a few environment-derived values. The CLI builds
//! one from the process environment; tests and embedders build their own.

use crate::admit::LiveSnapshot;
use crate::paths::Paths;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Wall time (for timestamps in the ledger and history), monotonic time
/// (for durations) and sleeping.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    /// Time since an arbitrary fixed origin; never goes backwards.
    fn monotonic(&self) -> Duration;
    fn sleep(&self, d: Duration);
}

/// The real clocks.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        SystemClock {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
    fn monotonic(&self) -> Duration {
        self.origin.elapsed()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// A clock that only moves when told to; `sleep` advances it instead of
/// waiting. For tests.
#[derive(Debug)]
pub struct FakeClock {
    state: Mutex<(DateTime<Utc>, Duration)>,
}

impl FakeClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        FakeClock {
            state: Mutex::new((start, Duration::ZERO)),
        }
    }

    pub fn advance(&self, d: Duration) {
        let mut s = self.state.lock().unwrap();
        s.0 += chrono::Duration::from_std(d).unwrap_or_default();
        s.1 += d;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        self.state.lock().unwrap().0
    }
    fn monotonic(&self) -> Duration {
        self.state.lock().unwrap().1
    }
    fn sleep(&self, d: Duration) {
        self.advance(d);
    }
}

/// Where machine metrics come from (memory, CPU, load).
pub trait LiveProvider: Send + Sync {
    fn snapshot(&self) -> Option<LiveSnapshot>;
}

/// This machine, via `sysinfo`. A sample takes a few hundred milliseconds.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemLive;

impl LiveProvider for SystemLive {
    fn snapshot(&self) -> Option<LiveSnapshot> {
        crate::live::snapshot()
    }
}

/// Always the same answer. For tests.
#[derive(Debug, Clone, Copy)]
pub struct FixedLive(pub Option<LiveSnapshot>);

impl LiveProvider for FixedLive {
    fn snapshot(&self) -> Option<LiveSnapshot> {
        self.0
    }
}

/// Where the machine's boot session id comes from. A managed command
/// records it at registration; a different id later means the machine
/// rebooted and every process of the old session is gone.
pub trait BootIdProvider: Send + Sync {
    /// `None` when the OS gives no id; the reaper then never releases on a
    /// reboot and relies on its process checks alone.
    fn boot_id(&self) -> Option<String>;
}

/// This machine's boot id (`kern.bootsessionuuid` on macOS,
/// `/proc/sys/kernel/random/boot_id` on Linux).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemBoot;

impl BootIdProvider for SystemBoot {
    fn boot_id(&self) -> Option<String> {
        crate::process::system_boot_id()
    }
}

/// Always the same answer. For tests.
#[derive(Debug, Clone)]
pub struct FixedBoot(pub Option<String>);

impl BootIdProvider for FixedBoot {
    fn boot_id(&self) -> Option<String> {
        self.0.clone()
    }
}

/// Where the answer to "is anything still running with this managed id and
/// scope in its environment?" comes from (see [`crate::marker`]).
pub trait ProcessScan: Send + Sync {
    fn scan(&self, id: &str, scope: &str) -> crate::marker::Scan;
    /// Which of the recorded descendants are still running.
    fn alive_recorded(&self, recorded: &[crate::marker::ProcRef]) -> Vec<crate::marker::ProcRef> {
        crate::process::alive_among(recorded)
    }
    /// Whether anyone still belongs to the run's session.
    fn probe_session(&self, c: &crate::process::ChildIdentity) -> crate::reap::Probe {
        crate::process::probe_session(c)
    }
}

/// This machine's processes of the current user.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemScan;

impl ProcessScan for SystemScan {
    fn scan(&self, id: &str, scope: &str) -> crate::marker::Scan {
        crate::process::scan_system(id, scope)
    }
}

/// Always the same answer. For tests.
#[derive(Debug, Clone)]
pub struct FixedScan(pub crate::marker::Scan);

impl FixedScan {
    pub fn probe(probe: crate::reap::Probe) -> Self {
        FixedScan(crate::marker::Scan::new(probe, Vec::new()))
    }
}

impl ProcessScan for FixedScan {
    fn scan(&self, _id: &str, _scope: &str) -> crate::marker::Scan {
        self.0.clone()
    }
}

/// Receives the stop signals meant for one gated run and decides how the
/// process reacts to them. A run goes through three phases, and the
/// supervisor can change its signal dispositions at each.
///
/// The CLI's implementation installs process-global signal handlers, which
/// is why it lives in the binary: those handlers cannot be shared by
/// concurrent runs. The library only polls [`Supervisor::take_pending`],
/// so a caller that runs several gated commands in one process gives each
/// its own supervisor (or [`NoSupervisor`] and cancels through the
/// `cancelled` hook) and nothing global is touched.
pub trait Supervisor: Send + Sync {
    /// Entering the queue: from here a stop request should be recorded, not
    /// kill the process, so the ledger entry can be removed first.
    fn waiting(&self) {}
    /// The command is about to start.
    fn running(&self) {}
    /// The caller is done with the run and has recorded how it ended; it
    /// calls this just before it dies or re-raises a signal. The library
    /// never calls it: restoring default dispositions earlier lets a second
    /// stop signal kill the process before the outcome is written down.
    fn finished(&self) {}
    /// The last signal received since the previous call, or 0.
    fn take_pending(&self) -> i32 {
        0
    }
}

/// No signal handling at all.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoSupervisor;

impl Supervisor for NoSupervisor {}

/// Explicit execution context. Cheap to clone.
#[derive(Clone)]
pub struct Context {
    pub paths: Paths,
    pub clock: Arc<dyn Clock>,
    pub live: Arc<dyn LiveProvider>,
    pub supervisor: Arc<dyn Supervisor>,
    pub boot: Arc<dyn BootIdProvider>,
    pub scan: Arc<dyn ProcessScan>,
    /// Temp-directory prefixes recognised when normalising a command into
    /// its history key (the CLI passes `$TMPDIR`).
    pub tmp_roots: Vec<String>,
    /// How long to wait for the ledger lock before giving up.
    pub ledger_lock_timeout: Duration,
    /// This context's last assessment of each ledger entry that has no
    /// lease holder (see [`crate::lock::refresh_verdicts`]). Shared between
    /// clones; never shared between processes, so a process always forms
    /// its own view before it releases anything.
    pub reap_cache: Arc<Mutex<HashMap<String, crate::lock::Cached>>>,
    /// Seconds before a kept entry is assessed again.
    pub reassess_secs: i64,
}

impl Context {
    /// One scan for the processes of managed command `id` admitted by this
    /// context's state directory.
    pub fn scan_marked(&self, id: &str) -> crate::marker::Scan {
        self.scan
            .scan(id, &crate::marker::scope(self.paths.state_dir()))
    }

    /// [`Context::scan_marked`] plus the recorded descendants of the
    /// command: any of them running makes it `Alive`, and dropped ones make
    /// a clear scan `Unknown`.
    pub fn scan_tracked(&self, id: &str, tracked: &crate::lineage::Tracked) -> crate::marker::Scan {
        let scan = self.scan_marked(id);
        let alive = if tracked.descendants.is_empty() {
            Vec::new()
        } else {
            self.scan.alive_recorded(&tracked.descendants)
        };
        crate::lineage::with_recorded(scan, alive, tracked.truncated)
    }

    /// Real clocks, real machine metrics, no signal handling.
    pub fn new(paths: Paths) -> Self {
        Context {
            paths,
            clock: Arc::new(SystemClock::new()),
            live: Arc::new(SystemLive),
            supervisor: Arc::new(NoSupervisor),
            boot: Arc::new(SystemBoot),
            scan: Arc::new(SystemScan),
            tmp_roots: Vec::new(),
            ledger_lock_timeout: crate::lock::LEDGER_LOCK_TIMEOUT,
            reap_cache: Arc::default(),
            reassess_secs: crate::reap::REASSESS_SECS,
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_live(mut self, live: Arc<dyn LiveProvider>) -> Self {
        self.live = live;
        self
    }

    pub fn with_supervisor(mut self, supervisor: Arc<dyn Supervisor>) -> Self {
        self.supervisor = supervisor;
        self
    }

    pub fn with_boot(mut self, boot: Arc<dyn BootIdProvider>) -> Self {
        self.boot = boot;
        self
    }

    pub fn with_scan(mut self, scan: Arc<dyn ProcessScan>) -> Self {
        self.scan = scan;
        self
    }

    pub fn with_tmp_roots(mut self, roots: Vec<String>) -> Self {
        self.tmp_roots = roots;
        self
    }

    pub fn with_reassess_secs(mut self, secs: i64) -> Self {
        self.reassess_secs = secs;
        self
    }

    pub fn with_ledger_lock_timeout(mut self, timeout: Duration) -> Self {
        self.ledger_lock_timeout = timeout;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_clock_moves_only_when_told() {
        let start = Utc::now();
        let c = FakeClock::new(start);
        assert_eq!(c.now(), start);
        assert_eq!(c.monotonic(), Duration::ZERO);
        c.sleep(Duration::from_secs(5));
        assert_eq!(c.monotonic(), Duration::from_secs(5));
        assert_eq!((c.now() - start).num_seconds(), 5);
    }

    #[test]
    fn system_clock_is_monotonic() {
        let c = SystemClock::new();
        let a = c.monotonic();
        assert!(c.monotonic() >= a);
    }
}
