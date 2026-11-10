//! The CLI's [`Supervisor`]: process-global signal handlers, kept out of
//! the library because they cannot be shared by concurrent runs.

use canirun::Supervisor;
use std::sync::atomic::{AtomicI32, Ordering};

static PENDING: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    PENDING.store(sig, Ordering::SeqCst);
}

fn set_handler(sig: libc::c_int, handler: libc::sighandler_t) {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0; // no SA_RESTART: wait4 must return EINTR
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

fn record() -> libc::sighandler_t {
    on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t
}

/// Handlers for the one gated run this process performs.
pub struct ProcessSignals;

impl Supervisor for ProcessSignals {
    /// While waiting in the queue: SIGINT/SIGTERM/SIGHUP set a flag so the
    /// loop can remove its entry and stop.
    fn waiting(&self) {
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            set_handler(sig, record());
        }
    }

    fn running(&self) {
        // Terminal-originated signals reach the child directly (same
        // process group); a second copy from us would make test runners
        // force-quit.
        set_handler(libc::SIGINT, libc::SIG_IGN);
        set_handler(libc::SIGQUIT, libc::SIG_IGN);
        for sig in [libc::SIGTERM, libc::SIGHUP] {
            set_handler(sig, record());
        }
    }

    fn finished(&self) {
        for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
            set_handler(sig, libc::SIG_DFL);
        }
    }

    fn take_pending(&self) -> i32 {
        PENDING.swap(0, Ordering::SeqCst)
    }
}

/// The run is recorded: put the handled signals back to their defaults so
/// a SIGINT is not swallowed on the way to exit.
pub fn restore() {
    ProcessSignals.finished();
}

/// Terminate ourselves the way the child did, so the calling shell sees
/// the same signal death. Falls back to exiting 128+N.
pub fn reraise(sig: i32) -> ! {
    // Only now, with the outcome recorded, do the handled signals go back
    // to their defaults: earlier, a second SIGTERM would kill the process
    // before the exit file was written.
    ProcessSignals.finished();
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
    std::process::exit(128 + sig)
}
