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
        set_handler(
            sig,
            on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
}

fn install_run_handlers() {
    // Terminal-originated signals reach the child directly (same process
    // group); a second copy from us would make test runners force-quit.
    set_handler(libc::SIGINT, libc::SIG_IGN);
    set_handler(libc::SIGQUIT, libc::SIG_IGN);
    for sig in [libc::SIGTERM, libc::SIGHUP] {
        set_handler(
            sig,
            on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
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
/// `CANIRUN_LEASE=lease` in its environment. With `measure`, the child's
/// process tree is sampled while it runs and the cores it uses are written
/// to the ledger entry `lease`.
pub fn run(argv: &[String], lease: &str, measure: bool) -> Result<Outcome> {
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
            Error::Spawn(format!("cannot run {prog:?}: {e}{}", spawn_hint(argv, &e)))
        })?;
    let pid = child.id() as libc::pid_t;
    // Dropped (stopped and joined) once the child has been reaped.
    let _monitor = measure.then(|| crate::usage::Monitor::start(lease.to_string(), child.id()));
    // A signal delivered between handler installation and spawn is
    // forwarded now rather than waiting for the next EINTR.
    let early = take_pending();
    if early != 0 {
        unsafe {
            libc::kill(pid, early);
        }
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

/// Terminate ourselves the way the child did, so the calling shell sees
/// the same signal death. Falls back to exiting 128+N.
pub fn reraise(sig: i32) -> ! {
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
    std::process::exit(128 + sig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error as IoError, ErrorKind};

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
