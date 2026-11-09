//! Everything the library needs from its surroundings, passed in
//! explicitly: file locations, the clock, the live-metrics source, the
//! signal supervisor, and a few environment-derived values. The CLI builds
//! one from the process environment; tests and embedders build their own.

use crate::admit::LiveSnapshot;
use crate::paths::Paths;
use chrono::{DateTime, Utc};
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
    /// The run is over (also called when spawning failed).
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
    /// Temp-directory prefixes recognised when normalising a command into
    /// its history key (the CLI passes `$TMPDIR`).
    pub tmp_roots: Vec<String>,
    /// How long to wait for the ledger lock before giving up.
    pub ledger_lock_timeout: Duration,
}

impl Context {
    /// Real clocks, real machine metrics, no signal handling.
    pub fn new(paths: Paths) -> Self {
        Context {
            paths,
            clock: Arc::new(SystemClock::new()),
            live: Arc::new(SystemLive),
            supervisor: Arc::new(NoSupervisor),
            tmp_roots: Vec::new(),
            ledger_lock_timeout: crate::lock::LEDGER_LOCK_TIMEOUT,
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

    pub fn with_tmp_roots(mut self, roots: Vec<String>) -> Self {
        self.tmp_roots = roots;
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
