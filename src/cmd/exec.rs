use crate::admit::{self, Decision, Limits, Reason, State};
use crate::config::Config;
use crate::display::{fmt_bytes, fmt_duration};
use crate::error::Result;
use crate::history::{self, Record};
use crate::key::{make_key, make_override_key};
use crate::ledger::{Entry, Ledger};
use crate::runner::{self, Exit};
use crate::weight::{self, Measurement};
use crate::{gitctx, live, lock, paths};
use chrono::Utc;
use std::thread;
use std::time::{Duration, Instant};

pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub struct Opts {
    pub weight: Option<u32>,
    pub front: bool,
    pub key: Option<String>,
    pub argv: Vec<String>,
}

/// True when `CANIRUN_LEASE` names a live running entry.
fn nested_lease_is_live() -> Result<bool> {
    let Some(id) = std::env::var_os("CANIRUN_LEASE") else {
        return Ok(false);
    };
    let id = id.to_string_lossy().into_owned();
    let _guard = lock::ledger()?;
    let ledger = Ledger::load(&paths::ledger_file())?;
    Ok(ledger
        .find(&id)
        .map(|e| e.state == State::Running && lock::lease_is_held(&id))
        .unwrap_or(false))
}

pub fn run(cfg: &Config, opts: Opts) -> Result<i32> {
    if opts.argv.is_empty() {
        return Err(crate::error::Error::Spawn(
            "usage: canirun exec [--weight N] [--front] [--key K] -- <command> [args...]".into(),
        ));
    }
    if nested_lease_is_live()? {
        let lease = std::env::var("CANIRUN_LEASE").unwrap_or_default();
        let out = runner::run(&opts.argv, &lease)?;
        return Ok(finish(out.exit));
    }

    let cwd = std::env::current_dir()?;
    let repo = gitctx::main_worktree(&cwd);
    let key = match &opts.key {
        Some(k) => make_override_key(repo.as_deref(), k, &opts.argv),
        None => make_key(repo.as_deref(), &opts.argv),
    };
    let records: Vec<Measurement> = history::for_key(&paths::history_file(), &key.hash)?
        .iter()
        .map(Record::measurement)
        .collect();
    let hint = cfg.hint_for(&key.argv);
    let (w, _source) =
        weight::derive_weight(opts.weight, hint, &records, cfg.default_weight, cfg.budget);
    let predicted_rss = weight::predicted_rss(&records);
    let limits = Limits {
        budget: cfg.budget,
        memory_reserve: cfg.memory_reserve_bytes(),
        cpu_threshold: cfg.cpu_threshold(),
    };

    // Register as waiting. The lease is taken before the ledger entry so
    // the entry is never observed without a held lease.
    let id = uuid::Uuid::new_v4().simple().to_string();
    let _lease = lock::take_lease(&id)?;
    runner::install_wait_handlers();
    {
        let _guard = lock::ledger()?;
        let mut ledger = Ledger::load(&paths::ledger_file())?;
        lock::reap_dead(&mut ledger);
        let seq = ledger.take_seq();
        ledger.entries.push(Entry {
            id: id.clone(),
            pid: std::process::id(),
            state: State::Waiting,
            front: opts.front,
            weight: w,
            predicted_rss,
            repo: key.repo.clone(),
            argv: key.argv.clone(),
            key: key.hash.clone(),
            seq,
            enqueued_at: Utc::now(),
            started_at: None,
        });
        ledger.save(&paths::ledger_file())?;
    }

    let enqueued = Instant::now();
    let mut last_progress: Option<Instant> = None;
    let progress_every = Duration::from_secs(cfg.progress_interval_secs.max(1));
    loop {
        let sig = runner::take_pending();
        if sig != 0 {
            remove_entry(&id);
            runner::reraise(sig);
        }
        // Phase 1 (under the lock, no OS sampling): queue position and
        // budget. Only a budget-admissible head pays for a live snapshot.
        let (first, queue) = {
            let _guard = lock::ledger()?;
            let mut ledger = Ledger::load(&paths::ledger_file())?;
            if !lock::reap_dead(&mut ledger).is_empty() {
                ledger.save(&paths::ledger_file())?;
            }
            let queue = ledger.queue();
            (
                admit::admit(&queue, &id, predicted_rss, limits, None),
                queue,
            )
        };
        let decision = match first {
            Decision::Wait(_) => first,
            Decision::Admitted => {
                // Phase 2: sample outside the lock, then re-check and
                // flip to running atomically.
                let snapshot = if queue.iter().any(|e| e.state == State::Running) {
                    live::snapshot()
                } else {
                    None
                };
                let _guard = lock::ledger()?;
                let mut ledger = Ledger::load(&paths::ledger_file())?;
                lock::reap_dead(&mut ledger);
                let queue = ledger.queue();
                let decision = admit::admit(&queue, &id, predicted_rss, limits, snapshot);
                if decision == Decision::Admitted {
                    if let Some(e) = ledger.find_mut(&id) {
                        e.state = State::Running;
                        e.started_at = Some(Utc::now());
                    }
                }
                ledger.save(&paths::ledger_file())?;
                decision
            }
        };
        if let Decision::Wait(reason) = &decision {
            let due = last_progress
                .map(|t| t.elapsed() >= progress_every)
                .unwrap_or(true);
            if due {
                let running = queue.iter().filter(|e| e.state == State::Running).count();
                let used = admit::used_budget(&queue);
                eprintln!(
                    "canirun: waiting {} ({}) — running {running} ({used}/{} used){}",
                    fmt_duration(enqueued.elapsed()),
                    describe(reason),
                    limits.budget,
                    match reason {
                        Reason::NotHead { ahead } if *ahead != usize::MAX =>
                            format!(", ahead of you {ahead}"),
                        _ => String::new(),
                    }
                );
                last_progress = Some(Instant::now());
            }
        }
        if decision == Decision::Admitted {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }

    let started_at = Utc::now();
    let out = runner::run(&opts.argv, &id);
    remove_entry(&id);
    let out = out?;
    let _ = history::append(
        &paths::history_file(),
        &Record {
            key: key.hash,
            repo: key.repo,
            argv: key.argv,
            started_at,
            wall_secs: out.wall_secs,
            cpu_secs: out.cpu_secs,
            max_rss_bytes: out.max_rss_bytes,
            exit_code: out.exit.code(),
            weight_used: w,
        },
    );
    Ok(finish(out.exit))
}

fn finish(exit: Exit) -> i32 {
    match exit {
        Exit::Code(c) => c,
        Exit::Signal(s) => runner::reraise(s),
    }
}

fn remove_entry(id: &str) {
    if let Ok(_guard) = lock::ledger() {
        if let Ok(mut ledger) = Ledger::load(&paths::ledger_file()) {
            ledger.remove(id);
            let _ = ledger.save(&paths::ledger_file());
        }
    }
    let _ = std::fs::remove_file(paths::lease_file(id));
}

pub fn describe(reason: &Reason) -> String {
    match reason {
        Reason::NotHead { .. } => "queue".into(),
        Reason::Budget { .. } => "budget".into(),
        Reason::Memory { available, need } => format!(
            "memory {} available, need {}",
            fmt_bytes(*available),
            fmt_bytes(*need)
        ),
        Reason::Cpu { busy, threshold } => format!(
            "cpu {:.0}% busy, threshold {:.0}%",
            busy * 100.0,
            threshold * 100.0
        ),
    }
}
