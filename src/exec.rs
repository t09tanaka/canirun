//! The admission path: plan a command from config and history, queue it in
//! the ledger, wait to be admitted, run it, measure it and record the run.
//!
//! Nothing here exits the process, prints, or reads the environment or the
//! working directory: those come in through [`Context`], [`Opts`] and
//! [`Hooks`], and how a run ended comes back as a [`Gated`] for the caller
//! (the CLI) to turn into an exit code or a re-raised signal.

use crate::admit::{self, Decision, Reason, State};
use crate::config::Config;
use crate::display::{fmt_bytes, fmt_duration};
use crate::error::Result;
use crate::eta;
use crate::gitctx;
use crate::history::{self, Record};
use crate::key::{self, CommandKey};
use crate::ledger::{Entry, Ledger};
use crate::runner::{self, Exit};
use crate::weight::{self, Measurement};
use crate::{lock, Context};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// What to run and how it should be queued.
pub struct Opts {
    pub weight: Option<u32>,
    pub front: bool,
    pub key: Option<String>,
    pub argv: Vec<String>,
    /// Directory the command runs in; also what the history key is derived
    /// from.
    pub cwd: PathBuf,
}

/// The history key for `argv` run from `cwd`, as `exec`, `submit` and
/// `history` all derive it.
pub fn command_key(
    ctx: &Context,
    cwd: &std::path::Path,
    key_override: Option<&str>,
    argv: &[String],
) -> Result<CommandKey> {
    let repo = gitctx::main_worktree(cwd);
    let subdir = if repo.is_some() {
        gitctx::subdir(cwd)
    } else {
        String::new()
    };
    if let Some(k) = key_override {
        return Ok(key::make_override_key(repo.as_deref(), &subdir, k, argv));
    }
    let roots = &ctx.tmp_roots;
    let head = key::temp_script(argv, roots).and_then(read_head);
    let normalized = key::normalize(argv, roots, head.as_deref());
    Ok(key::make_key(repo.as_deref(), &subdir, argv, normalized))
}

/// The first `SCRIPT_HEAD_BYTES` of a file, or `None` when unreadable.
fn read_head(path: &str) -> Option<String> {
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(key::SCRIPT_HEAD_BYTES as u64)
        .read_to_end(&mut buf)
        .ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// True when `id` (the `CANIRUN_LEASE` of the calling environment) names a
/// live running entry, i.e. we are already inside a gated command.
pub fn nested_lease_is_live(ctx: &Context, id: &str) -> Result<bool> {
    let ledger = lock::load_reaped(ctx)?;
    Ok(ledger
        .find(id)
        .map(|e| e.state == State::Running && lock::lease_is_held(ctx, id))
        .unwrap_or(false))
}

/// How a gated run ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gated {
    /// The command ran; this is how it exited (a signal death of the
    /// *child* is `Exit::Signal`, for the caller to mirror).
    Finished(Exit),
    /// Stopped before the command started, by this signal (or SIGTERM for
    /// the `cancelled` hook). The ledger entry is already removed; the
    /// caller dies as the signal says, `128 + signal` being the shell code.
    Interrupted(i32),
}

/// Callbacks into the caller of [`run_gated`].
pub struct Hooks<'a> {
    /// Polled while waiting; true cancels the run (as SIGTERM would).
    pub cancelled: &'a dyn Fn() -> bool,
    /// Receives each "waiting ..." progress line.
    pub progress: &'a dyn Fn(&str),
    /// Whether progress goes to a terminal (changes the line cadence).
    pub progress_tty: bool,
}

impl Hooks<'_> {
    /// Never cancelled, no progress output.
    pub fn quiet() -> Hooks<'static> {
        Hooks {
            cancelled: &|| false,
            progress: &|_| {},
            progress_tty: false,
        }
    }
}

/// A fresh lease / job id.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Everything derived from config and history before an entry is
/// registered.
pub struct Plan {
    pub key: crate::key::CommandKey,
    pub weight: u32,
    pub predicted_rss: u64,
    pub expected_secs: Option<f64>,
}

pub fn plan(ctx: &Context, cfg: &Config, opts: &Opts) -> Result<Plan> {
    let key = command_key(ctx, &opts.cwd, opts.key.as_deref(), &opts.argv)?;
    let recs = history::for_key(ctx.paths.history_file(), &key.hash)?;
    let records: Vec<Measurement> = recs.iter().map(Record::measurement).collect();
    let walls: Vec<f64> = recs.iter().map(|r| r.wall_secs).collect();
    // An explicit `--key` names the history, not the work: classify the
    // command itself.
    let kind_of = if opts.key.is_some() {
        &key.argv
    } else {
        &key.key_argv
    };
    let hint = cfg.hint_for_key(&key.argv, kind_of);
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
pub fn timed_queue(ctx: &Context, ledger: &Ledger) -> Vec<eta::Timed> {
    let now = ctx.clock.now();
    ledger
        .queue_at(now)
        .into_iter()
        .zip(&ledger.entries)
        .map(|(entry, e)| eta::Timed {
            entry,
            elapsed_secs: e
                .started_at
                .map(|s| (now - s).num_milliseconds().max(0) as f64 / 1000.0)
                .unwrap_or(0.0),
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

/// The admission path: register `id` as waiting, wait for admission, run,
/// measure, record history, and return how it ended.
///
/// A stop request while waiting (a signal reported by the context's
/// supervisor, or `hooks.cancelled`) removes the entry and returns
/// [`Gated::Interrupted`]; this function never exits the process.
pub fn run_gated(
    ctx: &Context,
    cfg: &Config,
    opts: &Opts,
    id: &str,
    hooks: &Hooks,
) -> Result<Gated> {
    let Plan {
        key,
        weight: w,
        predicted_rss,
        expected_secs,
    } = plan(ctx, cfg, opts)?;
    let limits = cfg.limits();

    // Register as waiting. The lease is taken before the ledger entry so
    // the entry is never observed without a held lease.
    let _lease = lock::take_lease(ctx, id)?;
    ctx.supervisor.waiting();
    // A busy ledger lock (a saturated machine can stall its holder for
    // seconds) is retried, here and while waiting, rather than failing a
    // command that may have queued for minutes.
    loop {
        if let Some(sig) = stop_requested(ctx, id, hooks) {
            return Ok(Gated::Interrupted(sig));
        }
        let Some(_guard) = lock::try_ledger(ctx)? else {
            continue;
        };
        let mut ledger = Ledger::load(&ctx.paths.ledger_file())?;
        lock::reap_dead(ctx, &mut ledger);
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
            enqueued_at: ctx.clock.now(),
            started_at: None,
            using: None,
            backfilled: false,
        });
        ledger.save(&ctx.paths.ledger_file())?;
        break;
    }

    let enqueued = ctx.clock.monotonic();
    let waited = || ctx.clock.monotonic().saturating_sub(enqueued);
    let mut last_progress: Option<(Duration, Topic)> = None;
    let cadence = Cadence {
        tty: hooks.progress_tty,
        every: Duration::from_secs(cfg.progress_interval_secs.max(1)),
        every_non_tty: Duration::from_secs(cfg.progress_interval_secs_non_tty.max(1)),
    };
    loop {
        if let Some(sig) = stop_requested(ctx, id, hooks) {
            return Ok(Gated::Interrupted(sig));
        }
        // Phase 1 (under the lock, no OS sampling): queue position and
        // budget. Only a budget-admissible head pays for a live snapshot.
        let (first, queue, est, holders) = {
            let Some(_guard) = lock::try_ledger(ctx)? else {
                continue;
            };
            let mut ledger = Ledger::load(&ctx.paths.ledger_file())?;
            if !lock::reap_dead(ctx, &mut ledger).is_empty() {
                ledger.save(&ctx.paths.ledger_file())?;
            }
            let queue = ledger.queue_at(ctx.clock.now());
            let est = eta::estimate(&timed_queue(ctx, &ledger), limits, id);
            let holders = holders(ctx, &ledger);
            (
                admit::admit(
                    &queue,
                    id,
                    predicted_rss,
                    waited().as_secs_f64(),
                    limits,
                    None,
                ),
                queue,
                est,
                holders,
            )
        };
        let decision = match first {
            Decision::Wait(_) => first,
            Decision::Admitted => {
                // Phase 2: sample outside the lock, then re-check and
                // flip to running atomically.
                let snapshot = if queue.iter().any(|e| e.state == State::Running) {
                    ctx.live.snapshot()
                } else {
                    None
                };
                let Some(_guard) = lock::try_ledger(ctx)? else {
                    continue;
                };
                let mut ledger = Ledger::load(&ctx.paths.ledger_file())?;
                let reaped = !lock::reap_dead(ctx, &mut ledger).is_empty();
                let queue = ledger.queue_at(ctx.clock.now());
                let decision = admit::admit(
                    &queue,
                    id,
                    predicted_rss,
                    waited().as_secs_f64(),
                    limits,
                    snapshot,
                );
                if decision == Decision::Admitted {
                    // Started ahead of someone: the starvation cap in
                    // `admit` must be able to tell it from the rest.
                    let backfilled = admit::queue_order(&queue)
                        .first()
                        .is_some_and(|head| head.id != id);
                    if let Some(e) = ledger.find_mut(id) {
                        e.state = State::Running;
                        e.started_at = Some(ctx.clock.now());
                        e.backfilled = backfilled;
                    }
                }
                if reaped || decision == Decision::Admitted {
                    ledger.save(&ctx.paths.ledger_file())?;
                }
                decision
            }
        };
        if let Decision::Wait(reason) = &decision {
            let topic = topic_of(reason);
            let last =
                last_progress.map(|(t, last)| (ctx.clock.monotonic().saturating_sub(t), last));
            if cadence.due(last, topic) {
                let running = queue.iter().filter(|e| e.state == State::Running).count();
                // Phase 2 may have decided on a fresher ledger.
                let used = match reason {
                    Reason::Budget { used, .. } => *used,
                    _ => admit::used_budget(&queue),
                };
                (hooks.progress)(&format!(
                    "canirun: waiting {} ({}) — running {running} ({used}/{} used){}{}{}",
                    fmt_duration(waited()),
                    describe(reason),
                    limits.budget,
                    match reason {
                        Reason::NotHead { ahead } | Reason::Reserved { ahead }
                            if *ahead != usize::MAX =>
                            format!(", ahead of you {ahead}"),
                        _ => String::new(),
                    },
                    est.map(|e| format!(", {}", fmt_estimate(&e)))
                        .unwrap_or_default(),
                    match reason {
                        Reason::Budget { .. } | Reason::Reserved { .. } if !holders.is_empty() =>
                            format!("; held by {holders}"),
                        _ => String::new(),
                    }
                ));
                last_progress = Some((ctx.clock.monotonic(), topic));
            }
        }
        if decision == Decision::Admitted {
            break;
        }
        ctx.clock.sleep(POLL_INTERVAL);
    }

    // A signal that arrived between the last check and admission must not
    // be lost: the child would otherwise start un-cancelled.
    let sig = ctx.supervisor.take_pending();
    if sig != 0 || (hooks.cancelled)() {
        let sig = if sig != 0 { sig } else { libc::SIGTERM };
        remove_entry(ctx, id);
        ctx.supervisor.finished();
        return Ok(Gated::Interrupted(sig));
    }
    // One sample right before the start: a run that begins on a saturated
    // machine is recorded as contended so its parallelism is not learned.
    let contended = ctx
        .live
        .snapshot()
        .map(|l| l.cpu_busy > limits.cpu_threshold)
        .unwrap_or(false);
    let started_at = ctx.clock.now();
    let out = runner::run(ctx, &opts.argv, &opts.cwd, id, true);
    remove_entry(ctx, id);
    let out = out?;
    let _ = history::append(
        ctx.paths.history_file(),
        &Record {
            key: key.hash,
            repo: key.repo,
            subdir: key.subdir,
            key_argv: if key.key_argv == key.argv || opts.key.is_some() {
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
    Ok(Gated::Finished(out.exit))
}

/// The signal that stopped a run while it waited (SIGTERM for a
/// cancellation), after removing its entry; `None` to carry on.
fn stop_requested(ctx: &Context, id: &str, hooks: &Hooks) -> Option<i32> {
    let sig = ctx.supervisor.take_pending();
    let sig = if sig != 0 {
        sig
    } else if (hooks.cancelled)() {
        libc::SIGTERM
    } else {
        return None;
    };
    remove_entry(ctx, id);
    ctx.supervisor.finished();
    Some(sig)
}

fn remove_entry(ctx: &Context, id: &str) {
    if let Ok(_guard) = lock::ledger(ctx) {
        if let Ok(mut ledger) = Ledger::load(&ctx.paths.ledger_file()) {
            ledger.remove(id);
            let _ = ledger.save(&ctx.paths.ledger_file());
        }
    }
    let _ = std::fs::remove_file(ctx.paths.lease_file(id));
}

/// How many running entries a waiting line names.
const HOLDERS_SHOWN: usize = 3;

/// The running entries that hold the budget, biggest first, as
/// `48211 w2 idle 52:10 api: make api, ...` (empty when nothing runs).
fn holders(ctx: &Context, ledger: &Ledger) -> String {
    let now = ctx.clock.now();
    let mut running: Vec<_> = ledger
        .queue_at(now)
        .into_iter()
        .zip(&ledger.entries)
        .filter(|(q, _)| q.state == State::Running)
        .collect();
    running.sort_by_key(|(q, e)| (std::cmp::Reverse(admit::charged(q)), e.started_at));
    let mut parts: Vec<String> = running
        .iter()
        .take(HOLDERS_SHOWN)
        .map(|(_, e)| crate::display::holder(e, now, 40))
        .collect();
    if running.len() > HOLDERS_SHOWN {
        parts.push(format!("+{} more", running.len() - HOLDERS_SHOWN));
    }
    parts.join(", ")
}

/// What a progress line is about: the kind of wait and, behind others in
/// the queue, how many are ahead. Amounts (memory, cpu %) are left out so
/// a fluctuating sample does not count as a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Topic {
    Queue(usize),
    Reserved(usize),
    Budget,
    Memory,
    Cpu,
    Overload,
}

fn topic_of(reason: &Reason) -> Topic {
    match reason {
        Reason::NotHead { ahead } => Topic::Queue(*ahead),
        Reason::Reserved { ahead } => Topic::Reserved(*ahead),
        Reason::Budget { .. } => Topic::Budget,
        Reason::Memory { .. } => Topic::Memory,
        Reason::Cpu { .. } => Topic::Cpu,
        Reason::Overload { .. } => Topic::Overload,
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
        Reason::Reserved { .. } => {
            "queue; the entry ahead has waited long enough, nothing more passes it".into()
        }
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
        Reason::Overload { load, limit } => {
            format!("overloaded: load {load:.0}, limit {limit:.0}; --front waits briefly")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod with_context {
        use super::*;
        use crate::context::{Clock, FakeClock, FixedLive};
        use crate::history;
        use crate::Paths;
        use std::cell::Cell;
        use std::sync::Arc;

        fn ctx_in(dir: &std::path::Path) -> (Context, Arc<FakeClock>) {
            let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
            let ctx = Context::new(Paths::new(dir.join("state"), dir.join("config.toml")))
                .with_clock(clock.clone())
                .with_live(Arc::new(FixedLive(None)));
            (ctx, clock)
        }

        /// Hooks that give up after `n` polls, so a run that wrongly keeps
        /// waiting (under a mutated admission rule, say) fails instead of
        /// hanging.
        fn bounded<'a>(n: u32, polls: &'a Cell<u32>) -> impl Fn() -> bool + 'a {
            move || {
                polls.set(polls.get() + 1);
                polls.get() > n
            }
        }

        fn opts(dir: &std::path::Path, argv: &[&str]) -> Opts {
            Opts {
                weight: None,
                front: false,
                key: None,
                argv: argv.iter().map(|s| s.to_string()).collect(),
                cwd: dir.to_path_buf(),
            }
        }

        #[test]
        fn a_run_uses_only_the_given_directories() {
            let dir = tempfile::tempdir().unwrap();
            let (ctx, _) = ctx_in(dir.path());
            let o = opts(dir.path(), &["sh", "-c", "exit 3"]);
            let polls = Cell::new(0);
            let cancelled = bounded(50, &polls);
            let hooks = Hooks {
                cancelled: &cancelled,
                progress: &|_| {},
                progress_tty: false,
            };
            let got = run_gated(&ctx, &Config::default(), &o, "run1", &hooks).unwrap();
            assert_eq!(got, Gated::Finished(Exit::Code(3)));
            let recs = history::read_all(ctx.paths.history_file()).unwrap();
            assert_eq!(recs.len(), 1);
            assert_eq!(recs[0].exit_code, 3);
            assert!(Ledger::load(&ctx.paths.ledger_file())
                .unwrap()
                .entries
                .is_empty());
            assert!(!ctx.paths.lease_file("run1").exists());
            assert!(ctx
                .paths
                .history_file()
                .starts_with(dir.path().join("state")));
        }

        #[test]
        fn a_cancelled_wait_removes_its_entry_and_advances_the_fake_clock() {
            let dir = tempfile::tempdir().unwrap();
            let (ctx, clock) = ctx_in(dir.path());
            let cfg = Config::default();
            // Someone else holds the whole budget.
            let _held = lock::take_lease(&ctx, "holder").unwrap();
            let mut ledger = Ledger::load(&ctx.paths.ledger_file()).unwrap();
            let seq = ledger.take_seq();
            ledger.entries.push(Entry {
                id: "holder".into(),
                pid: 1,
                state: State::Running,
                front: false,
                weight: cfg.budget,
                predicted_rss: 0,
                expected_secs: None,
                repo: String::new(),
                subdir: String::new(),
                argv: "sleep".into(),
                key: "k".into(),
                seq,
                enqueued_at: clock.now(),
                started_at: Some(clock.now()),
                using: None,
                backfilled: false,
            });
            ledger.save(&ctx.paths.ledger_file()).unwrap();

            let polls = Cell::new(0);
            let cancelled = || {
                polls.set(polls.get() + 1);
                polls.get() > 4
            };
            let lines = std::cell::RefCell::new(Vec::new());
            let o = opts(dir.path(), &["true"]);
            let got = run_gated(
                &ctx,
                &cfg,
                &o,
                "waiter",
                &Hooks {
                    cancelled: &cancelled,
                    progress: &|l| lines.borrow_mut().push(l.to_string()),
                    progress_tty: true,
                },
            )
            .unwrap();
            assert_eq!(got, Gated::Interrupted(libc::SIGTERM));
            let left = Ledger::load(&ctx.paths.ledger_file()).unwrap();
            assert_eq!(left.entries.len(), 1);
            assert_eq!(left.entries[0].id, "holder");
            assert!(!ctx.paths.lease_file("waiter").exists());
            // Time advanced by sleeping, not by waiting.
            assert!(clock.monotonic() >= POLL_INTERVAL * 2);
            assert!(
                lines.borrow()[0].contains("waiting"),
                "{:?}",
                lines.borrow()
            );
        }

        #[test]
        fn concurrent_runs_in_one_process_do_not_share_state() {
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            let run = |dir: std::path::PathBuf| {
                std::thread::spawn(move || {
                    let (ctx, _) = ctx_in(&dir);
                    let o = opts(&dir, &["true"]);
                    let polls = Cell::new(0);
                    let cancelled = bounded(50, &polls);
                    let hooks = Hooks {
                        cancelled: &cancelled,
                        progress: &|_| {},
                        progress_tty: false,
                    };
                    let got = run_gated(&ctx, &Config::default(), &o, "same-id", &hooks);
                    (
                        got.unwrap(),
                        history::read_all(ctx.paths.history_file()).unwrap().len(),
                    )
                })
            };
            let (ha, hb) = (run(a.path().to_path_buf()), run(b.path().to_path_buf()));
            assert_eq!(ha.join().unwrap(), (Gated::Finished(Exit::Code(0)), 1));
            assert_eq!(hb.join().unwrap(), (Gated::Finished(Exit::Code(0)), 1));
        }
    }

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
        // Backfill closing is a change worth a line of its own.
        assert_ne!(
            topic_of(&Reason::Reserved { ahead: 3 }),
            topic_of(&Reason::NotHead { ahead: 3 })
        );
    }
}
