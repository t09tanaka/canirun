use crate::admit::{self, Decision, Limits, QueueEntry, Reason, State};
use crate::config::Config;
use crate::display::{fmt_bytes, fmt_duration};
use crate::error::Result;
use crate::eta;
use crate::history::{self, Record};
use crate::ledger::{Entry, Ledger};
use crate::runner::{self, Exit};
use crate::weight::{self, Measurement};
use crate::{live, lock, paths};
use chrono::Utc;
use std::io::IsTerminal;
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
        let out = runner::run(&opts.argv, &lease, false)?;
        return Ok(finish(out.exit));
    }
    let id = new_id();
    let exit = run_gated(cfg, &opts, &id, &|_| {}, &|| false)?;
    Ok(finish(exit))
}

/// A fresh lease / job id.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The v0.1 admission path: register `id` as waiting, wait for admission,
/// run, measure, record history, and return how the child exited.
///
/// `before_exit` is called with the shell-style exit code right before this
/// process dies from a signal received while waiting (so a job runner can
/// persist the outcome first). `cancelled` is polled while waiting; when it
/// turns true the entry is removed and this process dies as if by SIGTERM.
/// Signal deaths of the *child* are returned as `Exit::Signal` for the
/// caller to mirror.
/// Everything derived from config and history before an entry is
/// registered.
pub struct Plan {
    pub key: crate::key::CommandKey,
    pub weight: u32,
    pub predicted_rss: u64,
    pub expected_secs: Option<f64>,
}

pub fn plan(cfg: &Config, opts: &Opts) -> Result<Plan> {
    let key = crate::cmd::command_key(opts.key.as_deref(), &opts.argv)?;
    let recs = history::for_key(&paths::history_file(), &key.hash)?;
    let records: Vec<Measurement> = recs.iter().map(Record::measurement).collect();
    let walls: Vec<f64> = recs.iter().map(|r| r.wall_secs).collect();
    let hint = cfg.hint_for(&key.argv);
    let (weight, _source) =
        weight::derive_weight(opts.weight, hint, &records, cfg.default_weight, cfg.budget);
    Ok(Plan {
        key,
        weight,
        predicted_rss: weight::predicted_rss(&records),
        expected_secs: eta::expected_wall(&walls),
    })
}

/// Ledger entries with elapsed time and learned wall time, for estimates.
pub fn timed_queue(ledger: &Ledger) -> Vec<eta::Timed> {
    let now = Utc::now();
    ledger
        .entries
        .iter()
        .map(|e| eta::Timed {
            entry: QueueEntry {
                id: e.id.clone(),
                state: e.state,
                front: e.front,
                weight: e.weight,
                seq: e.seq,
                using: e.using,
            },
            elapsed_secs: e
                .started_at
                .map(|s| (now - s).num_milliseconds().max(0) as f64 / 1000.0)
                .unwrap_or(0.0),
            expected_secs: e.expected_secs,
        })
        .collect()
}

/// "start in ~0:10, done by ~1:30" (with a trailing `?` when rough).
pub fn fmt_estimate(e: &eta::Estimate) -> String {
    let q = if e.rough { "?" } else { "" };
    if e.start_in <= 0.0 {
        format!(
            "done in ~{}{q}",
            fmt_duration(Duration::from_secs_f64(e.finish_in))
        )
    } else {
        format!(
            "start in ~{}, done in ~{}{q}",
            fmt_duration(Duration::from_secs_f64(e.start_in)),
            fmt_duration(Duration::from_secs_f64(e.finish_in))
        )
    }
}

pub fn run_gated(
    cfg: &Config,
    opts: &Opts,
    id: &str,
    before_exit: &dyn Fn(i32),
    cancelled: &dyn Fn() -> bool,
) -> Result<Exit> {
    let Plan {
        key,
        weight: w,
        predicted_rss,
        expected_secs,
    } = plan(cfg, opts)?;
    let limits = Limits {
        budget: cfg.budget,
        memory_reserve: cfg.memory_reserve_bytes(),
        cpu_threshold: cfg.cpu_threshold(),
        cpu_guard_max_wait_secs: cfg.cpu_guard_max_wait_secs as f64,
    };

    // Register as waiting. The lease is taken before the ledger entry so
    // the entry is never observed without a held lease.
    let _lease = lock::take_lease(id)?;
    runner::install_wait_handlers();
    {
        let _guard = lock::ledger()?;
        let mut ledger = Ledger::load(&paths::ledger_file())?;
        lock::reap_dead(&mut ledger);
        let seq = ledger.take_seq();
        ledger.entries.push(Entry {
            id: id.to_string(),
            pid: std::process::id(),
            state: State::Waiting,
            front: opts.front,
            weight: w,
            predicted_rss,
            expected_secs,
            repo: key.repo.clone(),
            subdir: key.subdir.clone(),
            argv: key.argv.clone(),
            key: key.hash.clone(),
            seq,
            enqueued_at: Utc::now(),
            started_at: None,
            using: None,
        });
        ledger.save(&paths::ledger_file())?;
    }

    let enqueued = Instant::now();
    let mut last_progress: Option<(Instant, Topic)> = None;
    let cadence = Cadence {
        tty: std::io::stderr().is_terminal(),
        every: Duration::from_secs(cfg.progress_interval_secs.max(1)),
        every_non_tty: Duration::from_secs(cfg.progress_interval_secs_non_tty.max(1)),
    };
    loop {
        let sig = runner::take_pending();
        if sig != 0 {
            remove_entry(id);
            before_exit(128 + sig);
            runner::reraise(sig);
        }
        if cancelled() {
            remove_entry(id);
            before_exit(128 + libc::SIGTERM);
            runner::reraise(libc::SIGTERM);
        }
        // Phase 1 (under the lock, no OS sampling): queue position and
        // budget. Only a budget-admissible head pays for a live snapshot.
        let (first, queue, est) = {
            let _guard = lock::ledger()?;
            let mut ledger = Ledger::load(&paths::ledger_file())?;
            if !lock::reap_dead(&mut ledger).is_empty() {
                ledger.save(&paths::ledger_file())?;
            }
            let queue = ledger.queue();
            let est = eta::estimate(&timed_queue(&ledger), cfg.budget, id);
            (
                admit::admit(
                    &queue,
                    id,
                    predicted_rss,
                    enqueued.elapsed().as_secs_f64(),
                    limits,
                    None,
                ),
                queue,
                est,
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
                let decision = admit::admit(
                    &queue,
                    id,
                    predicted_rss,
                    enqueued.elapsed().as_secs_f64(),
                    limits,
                    snapshot,
                );
                if decision == Decision::Admitted {
                    if let Some(e) = ledger.find_mut(id) {
                        e.state = State::Running;
                        e.started_at = Some(Utc::now());
                    }
                }
                ledger.save(&paths::ledger_file())?;
                decision
            }
        };
        if let Decision::Wait(reason) = &decision {
            let topic = topic_of(reason);
            let last = last_progress.map(|(t, last)| (t.elapsed(), last));
            if cadence.due(last, topic) {
                let running = queue.iter().filter(|e| e.state == State::Running).count();
                let used = admit::used_budget(&queue);
                eprintln!(
                    "canirun: waiting {} ({}) — running {running} ({used}/{} used){}{}",
                    fmt_duration(enqueued.elapsed()),
                    describe(reason),
                    limits.budget,
                    match reason {
                        Reason::NotHead { ahead } if *ahead != usize::MAX =>
                            format!(", ahead of you {ahead}"),
                        _ => String::new(),
                    },
                    est.map(|e| format!(", {}", fmt_estimate(&e)))
                        .unwrap_or_default()
                );
                last_progress = Some((Instant::now(), topic));
            }
        }
        if decision == Decision::Admitted {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }

    // A signal that arrived between the last check and admission must not
    // be lost: the child would otherwise start un-cancelled.
    let sig = runner::take_pending();
    if sig != 0 || cancelled() {
        let sig = if sig != 0 { sig } else { libc::SIGTERM };
        remove_entry(id);
        before_exit(128 + sig);
        runner::reraise(sig);
    }
    // One sample right before the start: a run that begins on a saturated
    // machine is recorded as contended so its parallelism is not learned.
    let contended = live::snapshot()
        .map(|l| l.cpu_busy > limits.cpu_threshold)
        .unwrap_or(false);
    let started_at = Utc::now();
    let out = runner::run(&opts.argv, id, true);
    remove_entry(id);
    let out = out?;
    let _ = history::append(
        &paths::history_file(),
        &Record {
            key: key.hash,
            repo: key.repo,
            subdir: key.subdir,
            key_argv: if key.key_argv == key.argv {
                String::new()
            } else {
                key.key_argv
            },
            argv: key.argv,
            started_at,
            wall_secs: out.wall_secs,
            cpu_secs: out.cpu_secs,
            max_rss_bytes: out.max_rss_bytes,
            exit_code: out.exit.code(),
            weight_used: w,
            contended,
        },
    );
    Ok(out.exit)
}

pub fn finish(exit: Exit) -> i32 {
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

/// What a progress line is about: the kind of wait and, behind others in
/// the queue, how many are ahead. Amounts (memory, cpu %) are left out so
/// a fluctuating sample does not count as a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Topic {
    Queue(usize),
    Budget,
    Memory,
    Cpu,
}

fn topic_of(reason: &Reason) -> Topic {
    match reason {
        Reason::NotHead { ahead } => Topic::Queue(*ahead),
        Reason::Budget { .. } => Topic::Budget,
        Reason::Memory { .. } => Topic::Memory,
        Reason::Cpu { .. } => Topic::Cpu,
    }
}

/// When to print the next progress line. On a terminal: every `every`.
/// Otherwise (a log file, an agent's tool result) the lines pile up, so
/// only when the topic changes, and every `every_non_tty` as a heartbeat.
#[derive(Debug, Clone, Copy)]
struct Cadence {
    tty: bool,
    every: Duration,
    every_non_tty: Duration,
}

impl Cadence {
    /// `last` is the time since the previous line and what it was about.
    fn due(&self, last: Option<(Duration, Topic)>, now: Topic) -> bool {
        let Some((since, topic)) = last else {
            return true;
        };
        if self.tty {
            since >= self.every
        } else {
            topic != now || since >= self.every_non_tty
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    const TTY: Cadence = Cadence {
        tty: true,
        every: Duration::from_secs(5),
        every_non_tty: Duration::from_secs(60),
    };
    const PIPE: Cadence = Cadence { tty: false, ..TTY };

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn first_line_is_always_due() {
        assert!(TTY.due(None, Topic::Budget));
        assert!(PIPE.due(None, Topic::Budget));
    }

    #[test]
    fn terminal_prints_on_the_interval_only() {
        assert!(!TTY.due(Some((secs(4), Topic::Budget)), Topic::Cpu));
        assert!(TTY.due(Some((secs(5), Topic::Budget)), Topic::Budget));
    }

    #[test]
    fn non_terminal_prints_on_change_or_heartbeat() {
        assert!(!PIPE.due(Some((secs(5), Topic::Budget)), Topic::Budget));
        assert!(!PIPE.due(Some((secs(59), Topic::Queue(2))), Topic::Queue(2)));
        assert!(PIPE.due(Some((secs(1), Topic::Budget)), Topic::Memory));
        assert!(PIPE.due(Some((secs(1), Topic::Queue(2))), Topic::Queue(1)));
        assert!(PIPE.due(Some((secs(60), Topic::Budget)), Topic::Budget));
    }

    #[test]
    fn topic_ignores_amounts() {
        let a = Reason::Cpu {
            busy: 0.9,
            threshold: 0.85,
        };
        let b = Reason::Cpu {
            busy: 0.95,
            threshold: 0.85,
        };
        assert_eq!(topic_of(&a), topic_of(&b));
        let m = |available| Reason::Memory {
            available,
            need: 10,
        };
        assert_eq!(topic_of(&m(1)), topic_of(&m(2)));
        assert_eq!(
            topic_of(&Reason::Budget { used: 1, budget: 2 }),
            Topic::Budget
        );
        assert_eq!(topic_of(&Reason::NotHead { ahead: 3 }), Topic::Queue(3));
    }
}
