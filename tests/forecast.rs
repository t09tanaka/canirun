//! `inspect` and `forecast` against a real state directory: they read, and
//! nothing else changes (spec A06).

use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;
use task_traffic_controller::exec::Opts;
use task_traffic_controller::forecast::{self, FutureDemand, Priority, Runtime};
use task_traffic_controller::history::{self, Record};
use task_traffic_controller::ledger::{Entry, Ledger};
use task_traffic_controller::{estimate, inspect, lock, Context, FakeClock, FixedLive, Paths};

fn t0() -> DateTime<Utc> {
    "2030-06-01T12:00:00Z".parse().unwrap()
}

struct Env {
    _dir: tempfile::TempDir,
    ctx: Context,
    clock: Arc<FakeClock>,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("state"), dir.path().join("config.toml"));
    let clock = Arc::new(FakeClock::new(t0()));
    let ctx = Context::new(paths)
        .with_clock(clock.clone())
        .with_live(Arc::new(FixedLive(None)));
    Env {
        _dir: dir,
        ctx,
        clock,
    }
}

fn entry(id: &str, state: task_traffic_controller::admit::State, seq: u64, key: &str) -> Entry {
    Entry {
        id: id.into(),
        pid: 4242,
        state,
        front: false,
        weight: 1,
        predicted_rss: 0,
        expected_secs: Some(30.0),
        repo: String::new(),
        subdir: String::new(),
        argv: format!("fixture-{id}"),
        key: key.into(),
        seq,
        enqueued_at: t0(),
        started_at: (state == task_traffic_controller::admit::State::Running).then(t0),
        using: None,
        backfilled: false,
    }
}

fn record(key: &str, wall: f64) -> Record {
    Record {
        key: key.into(),
        repo: String::new(),
        subdir: String::new(),
        argv: "fixture".into(),
        key_argv: String::new(),
        started_at: t0(),
        wall_secs: wall,
        cpu_secs: wall,
        max_rss_bytes: 1 << 20,
        exit_code: 0,
        weight_used: 1,
        contended: false,
        measurement_id: None,
    }
}

/// A ledger with a running entry, a waiting one and one whose owner is
/// gone, a held lease for the first two, history, and an old job file.
fn populate(e: &Env) -> Vec<lock::Lock> {
    let state = e.ctx.paths.state_dir();
    std::fs::create_dir_all(state.join("jobs")).unwrap();
    std::fs::write(state.join("jobs/j1.json"), b"{}").unwrap();
    let mut l = Ledger::default();
    l.entries.push(entry(
        "run1",
        task_traffic_controller::admit::State::Running,
        1,
        "ka",
    ));
    l.entries.push(entry(
        "wait1",
        task_traffic_controller::admit::State::Waiting,
        2,
        "kb",
    ));
    l.entries.push(entry(
        "gone1",
        task_traffic_controller::admit::State::Running,
        3,
        "kc",
    ));
    l.next_seq = 4;
    l.save(&e.ctx.paths.ledger_file()).unwrap();
    // The ledger lock file exists, as after any earlier run.
    drop(lock::ledger(&e.ctx).unwrap());
    history::append(e.ctx.paths.history_file(), &record("ka", 30.0)).unwrap();
    history::append(e.ctx.paths.history_file(), &record("ka", 31.0)).unwrap();
    vec![
        lock::take_lease(&e.ctx, "run1").unwrap(),
        lock::take_lease(&e.ctx, "wait1").unwrap(),
    ]
}

type Tree = BTreeMap<String, (Vec<u8>, Option<SystemTime>, bool)>;

fn tree(root: &Path) -> Tree {
    fn walk(dir: &Path, root: &Path, out: &mut Tree) {
        for ent in std::fs::read_dir(dir).unwrap().flatten() {
            let p = ent.path();
            let md = ent.metadata().unwrap();
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            if md.is_dir() {
                out.insert(rel, (Vec::new(), None, true));
                walk(&p, root, out);
            } else {
                out.insert(rel, (std::fs::read(&p).unwrap(), md.modified().ok(), false));
            }
        }
    }
    let mut out = Tree::new();
    walk(root, root, &mut out);
    out
}

fn opts(cwd: &Path) -> Opts {
    Opts {
        weight: None,
        front: false,
        key: Some("fixture-estimate-key".into()),
        argv: vec!["fixture-tool".into(), "--flag".into()],
        cwd: cwd.to_path_buf(),
    }
}

#[test]
fn a_hundred_forecasts_change_nothing() {
    let e = env();
    let _leases = populate(&e);
    let state = e.ctx.paths.state_dir().to_path_buf();
    let before = tree(&state);
    assert!(before.contains_key("ledger.lock"));
    let cfg = task_traffic_controller::config::Config::load(e.ctx.paths.config_file()).unwrap();
    let demand = FutureDemand {
        attempt_id: "att".into(),
        plan_digest: "sha256:p".into(),
        expansion_seq: 0,
        node_id: "n".into(),
        weight: 1,
        predicted_rss: 0,
        runtime: Runtime::measured(5.0, 3),
        priority: Priority::Normal,
        ready_in_ms: 0,
        ledger_entry: None,
        shared_id: None,
        terminal: false,
    };
    let mut first = None;
    for _ in 0..100 {
        e.clock.advance(std::time::Duration::from_millis(1));
        let insp = inspect::inspect(&e.ctx).unwrap();
        let plan = estimate::command_plan(&e.ctx, &cfg, &opts(&state)).unwrap();
        let f = forecast::forecast(&e.ctx, &insp, &plan, std::slice::from_ref(&demand)).unwrap();
        first.get_or_insert(f.snapshot_digest.clone());
        assert_eq!(&f.snapshot_digest, first.as_ref().unwrap());
    }
    assert_eq!(tree(&state), before);
}

#[test]
fn inspect_flags_a_missing_lease_and_keeps_the_entry() {
    let e = env();
    let _leases = populate(&e);
    let insp = inspect::inspect(&e.ctx).unwrap();
    let dead: Vec<&str> = insp
        .entries
        .iter()
        .filter(|x| x.suspect_dead)
        .map(|x| x.entry.id.as_str())
        .collect();
    assert_eq!(dead, ["gone1"]);
    assert_eq!(insp.entries.len(), 3);
    assert!(insp.ledger_locked);
    // `status` would have reaped it; inspect leaves the file alone.
    assert_eq!(
        Ledger::load(&e.ctx.paths.ledger_file())
            .unwrap()
            .entries
            .len(),
        3
    );
    let refs: Vec<(&str, usize)> = insp
        .history
        .iter()
        .map(|h| (h.key.as_str(), h.records))
        .collect();
    assert_eq!(refs, [("ka", 2), ("kb", 0), ("kc", 0)]);
}

#[test]
fn digests_follow_their_inputs() {
    let e = env();
    let _leases = populate(&e);
    let a = inspect::inspect(&e.ctx).unwrap();
    let b = inspect::inspect(&e.ctx).unwrap();
    assert_eq!(a.snapshot_digest, b.snapshot_digest);
    assert_eq!(a.policy_digest, b.policy_digest);
    assert!(a.snapshot_digest.starts_with("sha256:"));
    history::append(e.ctx.paths.history_file(), &record("kb", 12.0)).unwrap();
    let c = inspect::inspect(&e.ctx).unwrap();
    assert_ne!(a.snapshot_digest, c.snapshot_digest);
    assert_eq!(a.policy_digest, c.policy_digest);
    // The default budget is the machine's CPU count, so the override must
    // differ from it on any machine (a fixed 3 equals the default on a
    // 3-core CI runner and leaves the effective policy unchanged).
    let budget = task_traffic_controller::config::logical_cpus() + 1;
    std::fs::write(e.ctx.paths.config_file(), format!("budget = {budget}\n")).unwrap();
    let d = inspect::inspect(&e.ctx).unwrap();
    assert_ne!(c.policy_digest, d.policy_digest);
    assert_eq!(d.limits.budget, budget);
}

/// The policy digest follows the effective configuration, not the file:
/// writing the value the default already has changes nothing.
#[test]
fn policy_digest_follows_the_effective_config_not_the_file() {
    let e = env();
    let a = inspect::inspect(&e.ctx).unwrap();
    let same = task_traffic_controller::config::logical_cpus();
    std::fs::write(e.ctx.paths.config_file(), format!("budget = {same}\n")).unwrap();
    let b = inspect::inspect(&e.ctx).unwrap();
    assert_eq!(a.policy_digest, b.policy_digest);
}

/// History is digested by content, not by mtime or size granularity: a
/// record appended within the same instant (same `started_at`) still counts.
#[test]
fn snapshot_digest_follows_history_appended_in_the_same_instant() {
    let e = env();
    let _leases = populate(&e);
    let mut last = inspect::inspect(&e.ctx).unwrap().snapshot_digest;
    for i in 0..5 {
        history::append(
            e.ctx.paths.history_file(),
            &record("kb", 10.0 + f64::from(i)),
        )
        .unwrap();
        let now = inspect::inspect(&e.ctx).unwrap().snapshot_digest;
        assert_ne!(last, now);
        last = now;
    }
}

#[test]
fn inspect_creates_nothing_in_a_fresh_state_dir() {
    let e = env();
    let insp = inspect::inspect(&e.ctx).unwrap();
    assert!(insp.entries.is_empty());
    assert!(!insp.ledger_locked);
    assert!(!e.ctx.paths.state_dir().exists());
}

#[test]
fn a_busy_ledger_lock_does_not_block_a_reader() {
    let e = env();
    let _leases = populate(&e);
    let ctx = e
        .ctx
        .clone()
        .with_ledger_lock_timeout(std::time::Duration::from_millis(100));
    let _writer = lock::ledger(&ctx).unwrap();
    let started = std::time::Instant::now();
    let insp = inspect::inspect(&ctx).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(!insp.ledger_locked);
    assert_eq!(insp.entries.len(), 3);
}

#[test]
fn estimate_uses_recorded_history_and_never_invents_it() {
    let e = env();
    let _leases = populate(&e);
    std::fs::write(e.ctx.paths.config_file(), "budget = 16\n").unwrap();
    let cfg = task_traffic_controller::config::Config::load(e.ctx.paths.config_file()).unwrap();
    let cwd = e.ctx.paths.state_dir().to_path_buf();
    // No history for the command: fallback, no measured finish (spec A09).
    let plan = estimate::command_plan(&e.ctx, &cfg, &opts(&cwd)).unwrap();
    let insp = inspect::inspect(&e.ctx).unwrap();
    let f = forecast::forecast(&e.ctx, &insp, &plan, &[]).unwrap();
    assert_eq!(
        f.target.timing.runtime_source,
        forecast::RuntimeSource::Fallback
    );
    assert_eq!(f.target.timing.finish_in_ms, None);
    // With two recorded runs the same command gets a measured one.
    let key = plan.nodes[0].identity.clone();
    history::append(e.ctx.paths.history_file(), &record(&key, 8.0)).unwrap();
    history::append(e.ctx.paths.history_file(), &record(&key, 9.0)).unwrap();
    let plan = estimate::command_plan(&e.ctx, &cfg, &opts(&cwd)).unwrap();
    assert_eq!(plan.nodes[0].runtime.history_count, 2);
    assert_eq!(plan.nodes[0].runtime.secs, Some(9.0));
    let insp = inspect::inspect(&e.ctx).unwrap();
    let f = forecast::forecast(&e.ctx, &insp, &plan, &[]).unwrap();
    assert_eq!(f.target.timing.execution_ms, Some(9000));
    assert_eq!(f.target.timing.start_in_ms, Some(0));
    assert_eq!(f.target.timing.finish_in_ms, Some(9000));
}
