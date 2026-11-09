use crate::signals::reraise;
use canirun::admit::State;
use canirun::config::Config;
use canirun::display::{label, truncate};
use canirun::error::{Error, Result};
use canirun::exec::{self, Gated, Opts, POLL_INTERVAL};
use canirun::jobs::{self, Job, JobState};
use canirun::ledger::Ledger;
use canirun::runner::Exit;
use canirun::{lock, Context};
use chrono::{Local, Utc};
use std::collections::HashMap;
use std::io::{self, Write};
use std::thread;
use std::time::{Duration, Instant};

pub fn submit(ctx: &Context, cfg: &Config, opts: Opts, detach: bool) -> Result<i32> {
    if opts.argv.is_empty() {
        return Err(Error::Spawn(
            "usage: canirun submit [--weight N] [--front] [--key K] -- <command> [args...]".into(),
        ));
    }
    let job = Job {
        id: exec::new_id(),
        argv: opts.argv,
        cwd: opts.cwd,
        weight: opts.weight,
        front: opts.front,
        key: opts.key,
        submitted_at: ctx.clock.now(),
        submitter_pid: std::process::id(),
        detach,
        orphaned: false,
        finished_at: None,
    };
    jobs::save(ctx, &job)?;
    let mut runner = jobs::spawn_runner(ctx, &std::env::current_exe()?, &job)?;
    // Return only once the runner has a ledger entry, so the next submit
    // (or `jobs`) sees this job in the queue: back-to-back submits keep
    // their order and their estimates count each other.
    let deadline = Instant::now() + Duration::from_secs(STARTING_GRACE_SECS as u64);
    let ledger = loop {
        let ledger = lock::load_reaped(ctx)?;
        let gone =
            jobs::read_exit(ctx, &job.id).is_some() || matches!(runner.try_wait(), Ok(Some(_)));
        if ledger.find(&job.id).is_some() || gone || Instant::now() >= deadline {
            break ledger;
        }
        thread::sleep(REGISTER_POLL);
    };
    let registered = ledger.find(&job.id).is_some();
    let estimate = registered
        .then(|| canirun::eta::estimate(&exec::timed_queue(ctx, &ledger), cfg.limits(), &job.id))
        .flatten();
    if !registered && jobs::read_exit(ctx, &job.id).is_none() {
        // The runner exists and may still start, so the id is printed and
        // the submit succeeds; but `state_of` stops treating a missing
        // entry as "starting" after the same grace, hence the warning.
        eprintln!(
            "canirun: warning: job {} has not joined the queue within \
             {STARTING_GRACE_SECS}s; a later submit may start before it, \
             and `wait` may report it as lost",
            job.id
        );
    }
    println!("{}", job.id);
    if let Some(e) = estimate {
        eprintln!("canirun: job {} {}", job.id, exec::fmt_estimate(&e));
    }
    Ok(0)
}

/// Hidden: the detached runner. Runs the v0.1 path, persists the exit.
pub fn run_job(ctx: &Context, cfg: &Config, id: &str) -> Result<i32> {
    let job = jobs::load(ctx, id)?;
    let opts = Opts {
        weight: job.weight,
        front: job.front,
        key: job.key.clone(),
        argv: job.argv.clone(),
        cwd: job.cwd.clone(),
    };
    if !job.detach {
        spawn_orphan_watchdog(
            ctx.clone(),
            id.to_string(),
            job.submitted_at,
            cfg.jobs_orphan_grace_secs,
        );
    }
    let exit = match crate::cmd::exec::run_gated(ctx, cfg, &opts, id, &|| {
        jobs::read_exit(ctx, id).is_some()
    }) {
        Ok(Gated::Finished(exit)) => exit,
        Ok(Gated::Interrupted(sig)) => {
            jobs::write_exit(ctx, id, 128 + sig);
            reraise(sig);
        }
        Err(e) => {
            eprintln!("canirun: {e}");
            jobs::write_exit(ctx, id, 2);
            return Ok(2);
        }
    };
    jobs::write_exit(ctx, id, exit.code());
    match exit {
        Exit::Code(c) => Ok(c),
        Exit::Signal(s) => reraise(s),
    }
}

/// Once the grace period is over, a job nobody waits on is cancelled:
/// SIGTERM to ourselves, which the wait-phase handler or the run-phase
/// forwarding turns into a clean 143 with the exit file written.
fn spawn_orphan_watchdog(
    ctx: Context,
    id: String,
    submitted_at: chrono::DateTime<Utc>,
    grace_secs: u64,
) {
    thread::spawn(move || loop {
        thread::sleep(std::time::Duration::from_secs(1));
        if jobs::read_exit(&ctx, &id).is_some() {
            return;
        }
        let age = (ctx.clock.now() - submitted_at).num_seconds().max(0) as u64;
        if age >= grace_secs && !jobs::has_waiter(&ctx, &id) {
            jobs::mark_orphaned(&ctx, &id);
            unsafe {
                libc::kill(std::process::id() as libc::pid_t, libc::SIGTERM);
            }
            return;
        }
    });
}

/// How often `submit` checks whether its runner has registered.
const REGISTER_POLL: Duration = Duration::from_millis(10);

/// Seconds after submit during which "no ledger entry yet" means the
/// runner is still starting, not lost.
const STARTING_GRACE_SECS: i64 = 5;

/// Queued / running / done / lost, from the exit file and the ledger.
pub fn state_of(ctx: &Context, id: &str, ledger: &Ledger) -> JobState {
    if let Some(code) = jobs::read_exit(ctx, id) {
        return JobState::Done(code);
    }
    match ledger.find(id) {
        Some(e) if e.state == State::Running => JobState::Running,
        Some(_) => JobState::Queued,
        None => {
            let starting = jobs::load(ctx, id)
                .map(|j| (ctx.clock.now() - j.submitted_at).num_seconds() < STARTING_GRACE_SECS)
                .unwrap_or(false);
            if starting {
                JobState::Queued
            } else {
                JobState::Lost
            }
        }
    }
}

pub fn wait(ctx: &Context, ids: &[String], no_output: bool) -> Result<i32> {
    if ids.is_empty() {
        return Err(Error::Spawn(
            "usage: canirun wait [--no-output] <job-id>...".into(),
        ));
    }
    let mut waiter_locks = Vec::with_capacity(ids.len());
    for id in ids {
        jobs::load(ctx, id)?;
        waiter_locks.push(jobs::take_waiter(ctx, id)?);
    }
    let mut lost_strikes: HashMap<&str, u32> = HashMap::new();
    let mut final_code = 0;
    let mut reported: Vec<&str> = Vec::new();
    while reported.len() < ids.len() {
        let lock::Snapshot { ledger, locked } = lock::snapshot(ctx)?;
        let mut progressed = false;
        for id in ids {
            if reported.contains(&id.as_str()) {
                continue;
            }
            let state = state_of(ctx, id, &ledger);
            let code = match state {
                JobState::Done(c) => Some(c),
                JobState::Lost => {
                    // The runner may be between registration steps; give
                    // it two polls before declaring it lost. A read made
                    // without the lock does not count: the runner may be
                    // waiting for that same lock to register.
                    let n = lost_strikes.entry(id.as_str()).or_default();
                    if locked {
                        *n += 1;
                    }
                    if *n >= 3 {
                        Some(2)
                    } else {
                        None
                    }
                }
                _ => {
                    lost_strikes.insert(id.as_str(), 0);
                    None
                }
            };
            let Some(code) = code else { continue };
            // Report in the order given: only the first unreported id
            // prints now; later ones wait their turn.
            let first_pending = ids.iter().find(|i| !reported.contains(&i.as_str()));
            if first_pending.map(|i| i != id).unwrap_or(false) {
                continue;
            }
            let lost = matches!(state, JobState::Lost);
            report(ctx, id, code, lost, no_output)?;
            if final_code == 0 && code != 0 {
                final_code = code;
            }
            reported.push(id.as_str());
            progressed = true;
        }
        if !progressed {
            thread::sleep(POLL_INTERVAL);
        }
    }
    Ok(final_code)
}

fn report(ctx: &Context, id: &str, code: i32, lost: bool, no_output: bool) -> Result<()> {
    let job = jobs::load(ctx, id)?;
    let argv = canirun::key::argv_string(&job.argv);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "=== canirun job {id} {} — {argv}",
        if lost {
            "lost (runner died before finishing)".to_string()
        } else if job.orphaned {
            format!("exit {code} (orphaned: nobody was waiting)")
        } else {
            format!("exit {code}")
        }
    )?;
    if !no_output {
        if let Ok(log) = std::fs::read(ctx.paths.job_log(id)) {
            out.write_all(&log)?;
            if !log.ends_with(b"\n") {
                writeln!(out)?;
            }
        }
    }
    Ok(())
}

pub fn list(ctx: &Context, cfg: &Config, json: bool) -> Result<i32> {
    let ledger = lock::load_reaped(ctx)?;
    let all = jobs::list(ctx)?;
    if json {
        let rows: Vec<serde_json::Value> = all
            .iter()
            .map(|j| {
                let mut v = serde_json::to_value(j).unwrap_or_default();
                let st = state_of(ctx, &j.id, &ledger);
                v["state"] = serde_json::Value::String(if j.orphaned {
                    format!("orphaned {}", jobs::read_exit(ctx, &j.id).unwrap_or(143))
                } else {
                    st.label()
                });
                v["exit_code"] = match jobs::read_exit(ctx, &j.id) {
                    Some(c) => serde_json::Value::from(c),
                    None => serde_json::Value::Null,
                };
                v
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    if all.is_empty() {
        println!("no jobs");
        return Ok(0);
    }
    let timed = exec::timed_queue(ctx, &ledger);
    for j in &all {
        let state = state_of(ctx, &j.id, &ledger);
        let state_label = if j.orphaned {
            format!("orphaned {}", jobs::read_exit(ctx, &j.id).unwrap_or(143))
        } else {
            state.label()
        };
        let repo = canirun::gitctx::main_worktree(&j.cwd).unwrap_or_default();
        let subdir = if repo.is_empty() {
            String::new()
        } else {
            canirun::gitctx::subdir(&j.cwd)
        };
        let eta = match state {
            JobState::Queued | JobState::Running => {
                canirun::eta::estimate(&timed, cfg.limits(), &j.id)
                    .map(|e| format!("  ({})", exec::fmt_estimate(&e)))
                    .unwrap_or_default()
            }
            _ => String::new(),
        };
        println!(
            "{}  {:<12} {}  {}{eta}",
            j.id,
            state_label,
            j.submitted_at.with_timezone(&Local).format("%m-%d %H:%M"),
            truncate(
                &label(&repo, &subdir, &canirun::key::argv_string(&j.argv)),
                70
            )
        );
    }
    Ok(0)
}

pub fn cancel(ctx: &Context, ids: &[String]) -> Result<i32> {
    if ids.is_empty() {
        return Err(Error::Spawn("usage: canirun cancel <job-id>...".into()));
    }
    let ledger = lock::load_reaped(ctx)?;
    for id in ids {
        jobs::load(ctx, id)?;
        match state_of(ctx, id, &ledger) {
            JobState::Done(c) => println!("{id}: already finished (exit {c})"),
            JobState::Lost => {
                jobs::write_exit(ctx, id, 2);
                println!("{id}: runner already gone, marked lost");
            }
            JobState::Queued | JobState::Running => {
                match ledger.find(id).map(|e| e.pid) {
                    Some(pid) if pid > 0 => {
                        unsafe {
                            libc::kill(pid as libc::pid_t, libc::SIGTERM);
                        }
                        println!("{id}: sent SIGTERM to runner {pid}");
                    }
                    _ => {
                        // Runner not registered yet: the exit file doubles
                        // as the cancellation request it will see on start.
                        jobs::write_exit(ctx, id, 128 + libc::SIGTERM);
                        println!("{id}: cancelled before it started");
                    }
                }
            }
        }
    }
    Ok(0)
}

/// Remove job files finished more than `keep_days` ago. Returns the count.
pub fn gc(ctx: &Context, keep_days: u64) -> Result<usize> {
    let cutoff = ctx.clock.now() - chrono::Duration::days(keep_days as i64);
    let mut n = 0;
    for j in jobs::list(ctx)? {
        if j.finished_at.map(|t| t < cutoff).unwrap_or(false) {
            jobs::remove_files(ctx, &j.id);
            n += 1;
        }
    }
    Ok(n)
}
