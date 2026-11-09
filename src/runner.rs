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
            let sig = ctx.supervisor.take_pending();
            if sig != 0 {
                unsafe {
                    libc::kill(pid, sig);
                }
            }
            continue;
        }
        return Err(Error::Spawn(format!("wait4 failed: {err}")));
    }
    let wall_secs = (ctx.clock.monotonic().saturating_sub(start)).as_secs_f64();

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
