//! `task-traffic-controller exec`: CLI adapter over [`task_traffic_controller::exec`]. Turns how a run
//! ended into an exit code, re-raising signals as the shell expects.

use crate::signals::reraise;
use std::io::IsTerminal;
use task_traffic_controller::config::Config;
use task_traffic_controller::error::{Error, Result};
use task_traffic_controller::exec::{self, Gated, Hooks, Opts};
use task_traffic_controller::runner::{self, Exit};
use task_traffic_controller::Context;

pub fn run(ctx: &Context, cfg: &Config, opts: Opts) -> Result<i32> {
    if opts.argv.is_empty() {
        return Err(Error::Spawn(
            "usage: task-traffic-controller exec [--weight N] [--front] [--key K] -- <command> [args...]".into(),
        ));
    }
    let var = |name: &str| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned());
    if let Some(lease) = exec::nested_run_id(
        ctx,
        var("TASK_TRAFFIC_CONTROLLER_LEASE").as_deref(),
        var(task_traffic_controller::marker::ENV_VAR).as_deref(),
        var(task_traffic_controller::marker::SCOPE_VAR).as_deref(),
    )? {
        let out = runner::run(ctx, &opts.argv, &opts.cwd, &lease, false)?;
        return Ok(finish(out.exit));
    }
    let id = exec::new_id();
    match run_gated(ctx, cfg, &opts, &id, &|| false)? {
        Gated::Finished(exit) => Ok(finish(exit)),
        Gated::Interrupted(sig) => reraise(sig),
    }
}

/// [`exec::run_gated`] with progress lines on stderr.
pub fn run_gated(
    ctx: &Context,
    cfg: &Config,
    opts: &Opts,
    id: &str,
    cancelled: &dyn Fn() -> bool,
) -> Result<Gated> {
    exec::run_gated(
        ctx,
        cfg,
        opts,
        id,
        &Hooks {
            cancelled,
            progress: &|line| eprintln!("{line}"),
            progress_tty: std::io::stderr().is_terminal(),
        },
    )
}

pub fn finish(exit: Exit) -> i32 {
    match exit {
        Exit::Code(c) => {
            crate::signals::restore();
            c
        }
        Exit::Signal(s) => reraise(s),
    }
}
