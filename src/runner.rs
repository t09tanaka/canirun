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
    /// A managed command's process group still had members after the
    /// leader was reaped and they were sent SIGTERM, then SIGKILL, and the
    /// group did not empty in time. Always false for a plain run.
    pub group_left: bool,
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
    /// Called with the child's pid right after the spawn.
    pub on_spawn: Option<&'a dyn Fn(u32)>,
}

/// Grace between SIGTERM and SIGKILL for descendants that outlive a
/// managed command's leader, and how long to wait for the group to empty
/// after SIGKILL.
const GROUP_TERM_GRACE: Duration = Duration::from_secs(2);
const GROUP_KILL_WAIT: Duration = Duration::from_secs(3);

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
    let child = cmd
        .spawn()
        .map_err(|e| Error::Spawn(format!("cannot run {prog:?}: {e}{}", spawn_hint(argv, &e))))?;
    let pid = child.id() as libc::pid_t;
    if let Some(f) = spawn.on_spawn {
        f(child.id());
    }
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
    // the budget is only free once the whole group is confirmed stopped.
    let group_left = spawn.own_process_group && !clear_group(ctx, pid);

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
        group_left,
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
