use crate::admit::State;
use crate::cmd::exec::{self, Opts, POLL_INTERVAL};
use crate::cmd::status::label;
use crate::config::Config;
use crate::display::truncate;
use crate::error::{Error, Result};
use crate::jobs::{self, Job, JobState};
use crate::ledger::Ledger;
use crate::runner::Exit;
use crate::{lock, paths, runner};
use chrono::{Local, Utc};
use std::collections::HashMap;
use std::io::{self, Write};
use std::thread;

pub fn submit(cfg: &Config, opts: Opts, detach: bool) -> Result<i32> {
    if opts.argv.is_empty() {
        return Err(Error::Spawn(
            "usage: canirun submit [--weight N] [--front] [--key K] -- <command> [args...]".into(),
        ));
    }
    let job = Job {
        id: exec::new_id(),
        argv: opts.argv,
        cwd: std::env::current_dir()?,
        weight: opts.weight,
        front: opts.front,
        key: opts.key,
        submitted_at: Utc::now(),
        submitter_pid: std::process::id(),
        detach,
        orphaned: false,
        finished_at: None,
    };
    jobs::save(&job)?;
    // Estimate before the runner registers: simulate with a synthetic
    // waiting entry appended to the current queue.
    let opts = Opts {
        weight: job.weight,
        front: job.front,
        key: job.key.clone(),
        argv: job.argv.clone(),
    };
    let estimate = match (exec::plan(cfg, &opts), load_ledger_reaped()) {
        (Ok(plan), Ok(ledger)) => {
            let mut items = exec::timed_queue(&ledger);
            items.push(crate::eta::Timed {
                entry: crate::admit::QueueEntry {
                    id: job.id.clone(),
                    state: State::Waiting,
                    front: job.front,
                    weight: plan.weight,
                    seq: ledger.next_seq,
                },
                elapsed_secs: 0.0,
                expected_secs: plan.expected_secs,
            });
            crate::eta::estimate(&items, cfg.budget, &job.id)
        }
        _ => None,
    };
    jobs::spawn_runner(&job)?;
    println!("{}", job.id);
    if let Some(e) = estimate {
        eprintln!("canirun: job {} {}", job.id, exec::fmt_estimate(&e));
    }
    Ok(0)
}

/// Hidden: the detached runner. Runs the v0.1 path, persists the exit.
pub fn run_job(cfg: &Config, id: &str) -> Result<i32> {
    let job = jobs::load(id)?;
    let opts = Opts {
        weight: job.weight,
        front: job.front,
        key: job.key.clone(),
        argv: job.argv.clone(),
    };
    if !job.detach {
        spawn_orphan_watchdog(id.to_string(), job.submitted_at, cfg.jobs_orphan_grace_secs);
    }
    let exit = match exec::run_gated(cfg, &opts, id, &|code| jobs::write_exit(id, code), &|| {
        jobs::read_exit(id).is_some()
    }) {
        Ok(exit) => exit,
        Err(e) => {
            eprintln!("canirun: {e}");
            jobs::write_exit(id, 2);
            return Ok(2);
        }
    };
    jobs::write_exit(id, exit.code());
    match exit {
        Exit::Code(c) => Ok(c),
        Exit::Signal(s) => runner::reraise(s),
    }
}

/// Once the grace period is over, a job nobody waits on is cancelled:
/// SIGTERM to ourselves, which the wait-phase handler or the run-phase
/// forwarding turns into a clean 143 with the exit file written.
fn spawn_orphan_watchdog(id: String, submitted_at: chrono::DateTime<Utc>, grace_secs: u64) {
    thread::spawn(move || loop {
        thread::sleep(std::time::Duration::from_secs(1));
        if jobs::read_exit(&id).is_some() {
            return;
        }
        let age = (Utc::now() - submitted_at).num_seconds().max(0) as u64;
        if age >= grace_secs && !jobs::has_waiter(&id) {
            jobs::mark_orphaned(&id);
            unsafe {
                libc::kill(std::process::id() as libc::pid_t, libc::SIGTERM);
            }
            return;
        }
    });
}

/// Seconds after submit during which "no ledger entry yet" means the
/// runner is still starting, not lost.
const STARTING_GRACE_SECS: i64 = 5;

/// Queued / running / done / lost, from the exit file and the ledger.
pub fn state_of(id: &str, ledger: &Ledger) -> JobState {
    if let Some(code) = jobs::read_exit(id) {
        return JobState::Done(code);
    }
    match ledger.find(id) {
        Some(e) if e.state == State::Running => JobState::Running,
        Some(_) => JobState::Queued,
        None => {
            let starting = jobs::load(id)
                .map(|j| (Utc::now() - j.submitted_at).num_seconds() < STARTING_GRACE_SECS)
                .unwrap_or(false);
            if starting {
                JobState::Queued
            } else {
                JobState::Lost
            }
        }
    }
}

fn load_ledger_reaped() -> Result<Ledger> {
    let _guard = lock::ledger()?;
    let mut ledger = Ledger::load(&paths::ledger_file())?;
    if !lock::reap_dead(&mut ledger).is_empty() {
        ledger.save(&paths::ledger_file())?;
    }
    Ok(ledger)
}

pub fn wait(ids: &[String], no_output: bool) -> Result<i32> {
    if ids.is_empty() {
        return Err(Error::Spawn(
            "usage: canirun wait [--no-output] <job-id>...".into(),
        ));
    }
    let mut waiter_locks = Vec::with_capacity(ids.len());
    for id in ids {
        jobs::load(id)?;
        waiter_locks.push(jobs::take_waiter(id)?);
    }
    let mut lost_strikes: HashMap<&str, u32> = HashMap::new();
    let mut final_code = 0;
    let mut reported: Vec<&str> = Vec::new();
    while reported.len() < ids.len() {
        let ledger = load_ledger_reaped()?;
        let mut progressed = false;
        for id in ids {
            if reported.contains(&id.as_str()) {
                continue;
            }
            let state = state_of(id, &ledger);
            let code = match state {
                JobState::Done(c) => Some(c),
                JobState::Lost => {
                    // The runner may be between registration steps; give
                    // it two polls before declaring it lost.
                    let n = lost_strikes.entry(id.as_str()).or_default();
                    *n += 1;
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
            report(id, code, lost, no_output)?;
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

fn report(id: &str, code: i32, lost: bool, no_output: bool) -> Result<()> {
    let job = jobs::load(id)?;
    let argv = crate::key::argv_string(&job.argv);
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
        if let Ok(log) = std::fs::read(paths::job_log(id)) {
            out.write_all(&log)?;
            if !log.ends_with(b"\n") {
                writeln!(out)?;
            }
        }
    }
    Ok(())
}

pub fn list(cfg: &Config, json: bool) -> Result<i32> {
    let ledger = load_ledger_reaped()?;
    let all = jobs::list()?;
    if json {
        let rows: Vec<serde_json::Value> = all
            .iter()
            .map(|j| {
                let mut v = serde_json::to_value(j).unwrap_or_default();
                let st = state_of(&j.id, &ledger);
                v["state"] = serde_json::Value::String(if j.orphaned {
                    format!("orphaned {}", jobs::read_exit(&j.id).unwrap_or(143))
                } else {
                    st.label()
                });
                v["exit_code"] = match jobs::read_exit(&j.id) {
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
    let timed = exec::timed_queue(&ledger);
    for j in &all {
        let state = state_of(&j.id, &ledger);
        let state_label = if j.orphaned {
            format!("orphaned {}", jobs::read_exit(&j.id).unwrap_or(143))
        } else {
            state.label()
        };
        let repo = crate::gitctx::main_worktree(&j.cwd).unwrap_or_default();
        let eta = match state {
            JobState::Queued | JobState::Running => crate::eta::estimate(&timed, cfg.budget, &j.id)
                .map(|e| format!("  ({})", exec::fmt_estimate(&e)))
                .unwrap_or_default(),
            _ => String::new(),
        };
        println!(
            "{}  {:<12} {}  {}{eta}",
            j.id,
            state_label,
            j.submitted_at.with_timezone(&Local).format("%m-%d %H:%M"),
            truncate(&label(&repo, &crate::key::argv_string(&j.argv)), 70)
        );
    }
    Ok(0)
}

pub fn cancel(ids: &[String]) -> Result<i32> {
    if ids.is_empty() {
        return Err(Error::Spawn("usage: canirun cancel <job-id>...".into()));
    }
    let ledger = load_ledger_reaped()?;
    for id in ids {
        jobs::load(id)?;
        match state_of(id, &ledger) {
            JobState::Done(c) => println!("{id}: already finished (exit {c})"),
            JobState::Lost => {
                jobs::write_exit(id, 2);
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
                        jobs::write_exit(id, 128 + libc::SIGTERM);
                        println!("{id}: cancelled before it started");
                    }
                }
            }
        }
    }
    Ok(0)
}

/// Remove job files finished more than `keep_days` ago. Returns the count.
pub fn gc(keep_days: u64) -> Result<usize> {
    let cutoff = Utc::now() - chrono::Duration::days(keep_days as i64);
    let mut n = 0;
    for j in jobs::list()? {
        if j.finished_at.map(|t| t < cutoff).unwrap_or(false) {
            jobs::remove_files(&j.id);
            n += 1;
        }
    }
    Ok(n)
}
