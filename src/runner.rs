//! Spawn the gated command, forward signals, and collect rusage via
//! `wait4` (spec `exec` steps 5–6).

use crate::error::{Error, Result};
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Instant;

static PENDING: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    PENDING.store(sig, Ordering::SeqCst);
}

/// Returns and clears the last signal delivered to a handler installed
/// here, or 0.
pub fn take_pending() -> i32 {
    PENDING.swap(0, Ordering::SeqCst)
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

/// While waiting in the queue: SIGINT/SIGTERM/SIGHUP set a flag so the
/// loop can remove its entry and exit.
pub fn install_wait_handlers() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        set_handler(sig, on_signal as libc::sighandler_t);
    }
}

fn install_run_handlers() {
    // Terminal-originated signals reach the child directly (same process
    // group); a second copy from us would make test runners force-quit.
    set_handler(libc::SIGINT, libc::SIG_IGN);
    set_handler(libc::SIGQUIT, libc::SIG_IGN);
    for sig in [libc::SIGTERM, libc::SIGHUP] {
        set_handler(sig, on_signal as libc::sighandler_t);
    }
}

fn restore_defaults() {
    for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
        set_handler(sig, libc::SIG_DFL);
    }
}

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
}

/// Run `argv` with inherited stdio in our process group, with
/// `CANIRUN_LEASE=lease` in its environment.
pub fn run(argv: &[String], lease: &str) -> Result<Outcome> {
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| Error::Spawn("no command given".into()))?;
    install_run_handlers();
    let start = Instant::now();
    let child = Command::new(prog)
        .args(args)
        .env("CANIRUN_LEASE", lease)
        .spawn()
        .map_err(|e| {
            restore_defaults();
            Error::Spawn(format!("cannot run {prog:?}: {e}"))
        })?;
    let pid = child.id() as libc::pid_t;

    let mut status: libc::c_int = 0;
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        let r = unsafe { libc::wait4(pid, &mut status, 0, &mut ru) };
        if r == pid {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            let sig = take_pending();
            if sig != 0 {
                unsafe {
                    libc::kill(pid, sig);
                }
            }
            continue;
        }
        restore_defaults();
        return Err(Error::Spawn(format!("wait4 failed: {err}")));
    }
    restore_defaults();
    let wall_secs = start.elapsed().as_secs_f64();

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
    })
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

/// Terminate ourselves the way the child did, so the calling shell sees
/// the same signal death. Falls back to exiting 128+N.
pub fn reraise(sig: i32) -> ! {
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
    std::process::exit(128 + sig)
}
