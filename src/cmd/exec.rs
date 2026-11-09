//! `canirun exec`: CLI adapter over [`canirun::exec`]. Turns how a run
//! ended into an exit code, re-raising signals as the shell expects.

use crate::signals::reraise;
use canirun::config::Config;
use canirun::error::{Error, Result};
use canirun::exec::{self, Gated, Hooks, Opts};
use canirun::runner::{self, Exit};
use canirun::Context;
use std::io::IsTerminal;

pub fn run(ctx: &Context, cfg: &Config, opts: Opts) -> Result<i32> {
    if opts.argv.is_empty() {
        return Err(Error::Spawn(
            "usage: canirun exec [--weight N] [--front] [--key K] -- <command> [args...]".into(),
        ));
    }
    if let Some(lease) = std::env::var_os("CANIRUN_LEASE") {
        let lease = lease.to_string_lossy().into_owned();
        if exec::nested_lease_is_live(ctx, &lease)? {
            let out = runner::run(ctx, &opts.argv, &opts.cwd, &lease, false)?;
            return Ok(finish(out.exit));
        }
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
