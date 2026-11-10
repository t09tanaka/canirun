//! Managed commands: registration, receipts, the reaper's rules and the
//! measurement idempotence. Every test runs in its own temp state dir with
//! an explicit `Context`; nothing touches the real user state.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use task_traffic_controller::admit::State;
use task_traffic_controller::config::Config;
use task_traffic_controller::exec::{self, Gated, Hooks, Opts};
use task_traffic_controller::history::{self, Record};
use task_traffic_controller::ledger::Ledger;
use task_traffic_controller::managed::{
    self, CommandSpec, Loaded, Measured, Registration, TerminalKind, TryAdmit,
};
use task_traffic_controller::marker::{HiddenProc, ProcRef, Scan};
use task_traffic_controller::reap::{Keep, Probe, Verdict};
use task_traffic_controller::receipt::Phase;
use task_traffic_controller::{
    lock, Context, FakeClock, FixedBoot, FixedLive, FixedScan, Paths, Supervisor,
};

fn ctx_with_boot(dir: &Path, boot: Option<&str>) -> Context {
    Context::new(Paths::new(dir.join("state"), dir.join("config.toml")))
        .with_live(Arc::new(FixedLive(None)))
        .with_boot(Arc::new(FixedBoot(boot.map(String::from))))
}

fn ctx(dir: &Path) -> Context {
    ctx_with_boot(dir, Some("boot-1"))
}

fn opts(dir: &Path, argv: &[&str]) -> Opts {
    Opts {
        weight: Some(1),
        front: false,
        key: Some("managed-test".into()),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cwd: dir.to_path_buf(),
        ..Opts::default()
    }
}

fn spec(dir: &Path, id: &str, digest: &str, argv: &[&str]) -> CommandSpec {
    CommandSpec {
        command_id: id.into(),
        spec_digest: digest.into(),
        opts: opts(dir, argv),
        attempt: Some(serde_json::json!({"attempt_id": "a-1", "node_id": "test"})),
        services: vec![],
    }
}

fn registered(r: Registration) -> (managed::RegisteredReceipt, managed::OwnerGuard) {
    match r {
        Registration::Registered(rc, g) => (rc, g),
        Registration::AlreadyTerminal(_) => panic!("unexpected AlreadyTerminal"),
    }
}

fn ledger(ctx: &Context) -> Ledger {
    Ledger::load(&ctx.paths.ledger_file()).unwrap()
}

fn sidecar(ctx: &Context, id: &str) -> managed::Sidecar {
    match managed::load_sidecar(ctx, id) {
        Loaded::Ok(s) => *s,
        other => panic!("sidecar {id}: {other:?}"),
    }
}

/// Poll `f` until true, at most `secs`.
fn until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    false
}

fn admit_now(ctx: &Context, cfg: &Config, id: &str) {
    for _ in 0..200 {
        match managed::try_admit(ctx, cfg.limits(), id, 0.0).unwrap() {
            TryAdmit::Admitted => return,
            TryAdmit::LockBusy | TryAdmit::Wait(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    panic!("never admitted");
}

#[test]
fn register_is_idempotent_and_the_entry_exists_before_the_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let s = spec(dir.path(), "cmd-a", "digest-1", &["true"]);
    let (r1, g1) = registered(managed::register(&ctx, &cfg, &s).unwrap());
    assert!(r1.newly_registered);
    assert_eq!(r1.phase, Phase::Registered);
    assert_eq!(r1.boot_id.as_deref(), Some("boot-1"));
    // The receipt is back: the entry and the sidecar are already there.
    let l = ledger(&ctx);
    let e = l.find("cmd-a").expect("entry before the receipt");
    assert_eq!((e.seq, e.state), (r1.seq, State::Waiting));
    let sc = sidecar(&ctx, "cmd-a");
    assert_eq!(sc.spec_digest, "digest-1");
    assert_eq!(sc.attempt.as_ref().unwrap()["node_id"], "test");

    // The owner is alive: a second registration cannot take the lease.
    assert!(matches!(
        managed::register(&ctx, &cfg, &s),
        Err(task_traffic_controller::error::Error::AlreadyOwned(_))
    ));
    drop(g1);
    // A new supervisor re-registers the same id and digest: same entry.
    let (r2, g2) = registered(managed::register(&ctx, &cfg, &s).unwrap());
    assert!(!r2.newly_registered);
    assert_eq!(r2.seq, r1.seq);
    assert_eq!(ledger(&ctx).entries.len(), 1);

    drop(g2);
    // A different digest for the same id is a conflict, and changes nothing.
    let other = spec(dir.path(), "cmd-a", "digest-2", &["true"]);
    assert!(matches!(
        managed::register(&ctx, &cfg, &other),
        Err(task_traffic_controller::error::Error::Conflict(_))
    ));
    assert_eq!(sidecar(&ctx, "cmd-a").spec_digest, "digest-1");
}

#[test]
fn ids_that_are_not_file_names_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    for bad in ["", "../x", "a/b", ".hidden", "sp ace"] {
        let s = spec(dir.path(), bad, "d", &["true"]);
        assert!(managed::register(&ctx, &cfg, &s).is_err(), "{bad:?}");
    }
    let empty_digest = spec(dir.path(), "ok", "", &["true"]);
    assert!(managed::register(&ctx, &cfg, &empty_digest).is_err());
    assert!(!ctx.paths.managed_dir().exists());
}

#[test]
fn two_commands_from_one_process_do_not_collide() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let (_, ga) = registered(
        managed::register(&ctx, &cfg, &spec(dir.path(), "one", "d1", &["true"])).unwrap(),
    );
    let (_, gb) = registered(
        managed::register(&ctx, &cfg, &spec(dir.path(), "two", "d2", &["true"])).unwrap(),
    );
    let l = ledger(&ctx);
    assert_eq!(l.entries.len(), 2);
    // Same pid (ours), told apart by command id.
    assert_eq!(l.find("one").unwrap().pid, l.find("two").unwrap().pid);
    managed::finish(&ctx, ga, TerminalKind::Cancelled).unwrap();
    let l = ledger(&ctx);
    assert!(l.find("one").is_none());
    assert!(l.find("two").is_some());
    assert!(lock::lease_is_held(&ctx, "two"));
    drop(gb);
}

#[test]
fn a_full_run_records_terminal_before_releasing_and_measures_once() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let s = spec(dir.path(), "run-1", "d", &["sh", "-c", "exit 3"]);
    let (_, guard) = registered(managed::register(&ctx, &cfg, &s).unwrap());
    admit_now(&ctx, &cfg, "run-1");
    assert_eq!(ledger(&ctx).find("run-1").unwrap().state, State::Running);
    let ran = managed::run_command(&ctx, &guard, &s.opts, cfg.limits()).unwrap();
    assert_eq!(ran.outcome.exit.code(), 3);
    // Ran, but not yet released: the child's identity is recorded and the
    // entry still holds its charge.
    let sc = sidecar(&ctx, "run-1");
    assert_eq!(sc.phase, Phase::ChildStarted);
    let child = sc.child.unwrap();
    assert_eq!(child.pgid, Some(child.pid as i32), "own process group");
    assert!(sc.launch_intent_at.is_some());
    assert!(ledger(&ctx).find("run-1").is_some());

    managed::finish(&ctx, guard, ran.outcome.exit.into()).unwrap();
    let sc = sidecar(&ctx, "run-1");
    assert_eq!(sc.phase, Phase::Terminal);
    assert_eq!(sc.terminal.unwrap().kind, TerminalKind::Exited { code: 3 });
    assert!(ledger(&ctx).find("run-1").is_none());
    assert!(!ctx.paths.lease_file("run-1").exists());

    // The id is single use: registering it again creates nothing.
    match managed::register(&ctx, &cfg, &s).unwrap() {
        Registration::AlreadyTerminal(sc) => assert_eq!(sc.phase, Phase::Terminal),
        _ => panic!("expected AlreadyTerminal"),
    }
    assert!(ledger(&ctx).entries.is_empty());
    assert!(!ctx.paths.lease_file("run-1").exists());

    // Measurement: applied once per measurement id.
    let rec = Record {
        key: "k".into(),
        repo: String::new(),
        subdir: String::new(),
        argv: "sh -c exit 3".into(),
        key_argv: String::new(),
        started_at: ran.started_at,
        wall_secs: ran.outcome.wall_secs,
        cpu_secs: ran.outcome.cpu_secs,
        max_rss_bytes: ran.outcome.max_rss_bytes,
        exit_code: 3,
        weight_used: 1,
        contended: ran.contended,
        measurement_id: None,
    };
    assert_eq!(
        managed::record_measurement(&ctx, "m-1", &rec).unwrap(),
        Measured::Applied
    );
    assert_eq!(
        managed::record_measurement(&ctx, "m-1", &rec).unwrap(),
        Measured::Duplicate
    );
    assert_eq!(
        history::read_all(ctx.paths.history_file()).unwrap().len(),
        1
    );
    assert_eq!(
        managed::record_measurement(&ctx, "m-2", &rec).unwrap(),
        Measured::Applied
    );
    assert_eq!(
        history::read_all(ctx.paths.history_file()).unwrap().len(),
        2
    );
    assert!(managed::record_measurement(&ctx, "../m", &rec).is_err());
}

#[test]
fn a_measurement_interrupted_after_the_append_is_not_applied_twice() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path());
    let rec = Record {
        key: "k".into(),
        repo: String::new(),
        subdir: String::new(),
        argv: "x".into(),
        key_argv: String::new(),
        started_at: chrono::Utc::now(),
        wall_secs: 1.5,
        cpu_secs: 1.0,
        max_rss_bytes: 10,
        exit_code: 0,
        weight_used: 1,
        contended: false,
        measurement_id: None,
    };
    // A crash after the append, before the "applied" marker.
    std::fs::create_dir_all(ctx.paths.measurements_dir()).unwrap();
    std::fs::write(ctx.paths.measurements_dir().join("m.pending"), b"").unwrap();
    let with_id = Record {
        measurement_id: Some("m".into()),
        ..rec.clone()
    };
    history::append(ctx.paths.history_file(), &with_id).unwrap();
    assert_eq!(
        managed::record_measurement(&ctx, "m", &rec).unwrap(),
        Measured::Duplicate
    );
    assert_eq!(
        history::read_all(ctx.paths.history_file()).unwrap().len(),
        1
    );
    // A crash before the append: the retry appends.
    std::fs::write(ctx.paths.measurements_dir().join("n.pending"), b"").unwrap();
    let rec2 = Record {
        wall_secs: 2.5,
        ..rec
    };
    assert_eq!(
        managed::record_measurement(&ctx, "n", &rec2).unwrap(),
        Measured::Applied
    );
    assert_eq!(
        history::read_all(ctx.paths.history_file()).unwrap().len(),
        2
    );
}

#[test]
fn an_identical_record_under_another_id_or_none_is_not_this_measurement() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path());
    let rec = Record {
        key: "k".into(),
        repo: String::new(),
        subdir: String::new(),
        argv: "x".into(),
        key_argv: String::new(),
        started_at: chrono::Utc::now(),
        wall_secs: 1.5,
        cpu_secs: 1.0,
        max_rss_bytes: 10,
        exit_code: 0,
        weight_used: 1,
        contended: false,
        measurement_id: None,
    };
    // Two identical records already there: one from another measurement,
    // one from history written before measurement ids existed.
    history::append(
        ctx.paths.history_file(),
        &Record {
            measurement_id: Some("other".into()),
            ..rec.clone()
        },
    )
    .unwrap();
    history::append(ctx.paths.history_file(), &rec).unwrap();
    // A crashed attempt of "m" that never reached the append.
    std::fs::create_dir_all(ctx.paths.measurements_dir()).unwrap();
    std::fs::write(ctx.paths.measurements_dir().join("m.pending"), b"").unwrap();
    assert_eq!(
        managed::record_measurement(&ctx, "m", &rec).unwrap(),
        Measured::Applied
    );
    let all = history::read_all(ctx.paths.history_file()).unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[2].measurement_id.as_deref(), Some("m"));
    assert_eq!(
        managed::record_measurement(&ctx, "m", &rec).unwrap(),
        Measured::Duplicate
    );
    assert_eq!(
        history::read_all(ctx.paths.history_file()).unwrap().len(),
        3
    );
}

/// Pretend the supervisor died: drop its lease, keep everything else.
fn orphan(
    ctx: &Context,
    id: &str,
    phase: Phase,
    child: Option<task_traffic_controller::process::ChildIdentity>,
) {
    let mut sc = sidecar(ctx, id);
    sc.phase = phase;
    sc.child = child;
    managed::write_sidecar(ctx, &sc).unwrap();
}

fn reaped(ctx: &Context) -> Ledger {
    lock::snapshot(ctx).unwrap().ledger
}

#[test]
fn a_dead_owner_with_no_launch_is_released_and_marked_abandoned() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let (_, g) =
        registered(managed::register(&ctx, &cfg, &spec(dir.path(), "c", "d", &["true"])).unwrap());
    assert!(reaped(&ctx).find("c").is_some(), "owner alive");
    drop(g);
    assert!(reaped(&ctx).find("c").is_none());
    let sc = sidecar(&ctx, "c");
    assert_eq!(sc.terminal.unwrap().kind, TerminalKind::Abandoned);
}

#[test]
fn a_launch_intent_without_a_child_identity_is_never_freed_until_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let (_, g) =
        registered(managed::register(&ctx, &cfg, &spec(dir.path(), "c", "d", &["true"])).unwrap());
    orphan(&ctx, "c", Phase::LaunchIntent, None);
    drop(g);
    assert!(reaped(&ctx).find("c").is_some());
    let views = managed::views(&ctx, &ledger(&ctx).entries);
    assert_eq!(views.len(), 1);
    assert!(views[0].quarantined);
    assert!(views[0].reason.contains("LaunchUnknown"), "{:?}", views[0]);
    // An investigator settles it by hand.
    managed::resolve(
        &ctx,
        "c",
        TerminalKind::Interrupted,
        "checked: nothing running",
    )
    .unwrap();
    assert!(reaped(&ctx).find("c").is_none());
    assert_eq!(sidecar(&ctx, "c").phase, Phase::Terminal);
}

#[test]
fn an_unreadable_sidecar_is_not_free_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let (_, g) =
        registered(managed::register(&ctx, &cfg, &spec(dir.path(), "c", "d", &["true"])).unwrap());
    std::fs::write(ctx.paths.managed_file("c"), b"{ not json").unwrap();
    drop(g);
    assert!(reaped(&ctx).find("c").is_some());
    // Even with the lock busy (the fallback read), and even after a reboot.
    let rebooted = ctx_with_boot(dir.path(), Some("boot-2"));
    assert!(reaped(&rebooted).find("c").is_some());
    let v = managed::views(&ctx, &ledger(&ctx).entries);
    assert!(v[0].quarantined && v[0].phase.is_none());
    // Re-registering over a broken sidecar is refused, not papered over.
    assert!(matches!(
        managed::register(&ctx, &cfg, &spec(dir.path(), "c", "d", &["true"])),
        Err(task_traffic_controller::error::Error::Conflict(_))
    ));
    managed::resolve(&ctx, "c", TerminalKind::Interrupted, "unreadable").unwrap();
    assert!(reaped(&ctx).find("c").is_none());
}

#[test]
fn a_changed_boot_id_releases_what_the_old_boot_left() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx1, cfg) = (ctx(dir.path()), Config::default());
    let (_, g) =
        registered(managed::register(&ctx1, &cfg, &spec(dir.path(), "c", "d", &["true"])).unwrap());
    let ghost = task_traffic_controller::process::ChildIdentity {
        pid: std::process::id(), // alive, would be "kept" on the same boot
        start_time: task_traffic_controller::process::start_time(std::process::id()),
        pgid: Some(unsafe { libc::getpgid(0) }),
        sid: None,
    };
    orphan(&ctx1, "c", Phase::ChildStarted, Some(ghost));
    drop(g);
    assert!(
        reaped(&ctx1).find("c").is_some(),
        "same boot: the tree looks alive"
    );
    // No boot id available: the shortcut is off, still kept.
    assert!(reaped(&ctx_with_boot(dir.path(), None)).find("c").is_some());
    // Rebooted: nothing of the old session exists.
    let ctx2 = ctx_with_boot(dir.path(), Some("boot-2"));
    assert!(reaped(&ctx2).find("c").is_none());
    assert_eq!(
        sidecar(&ctx2, "c").terminal.unwrap().kind,
        TerminalKind::Interrupted
    );
}

/// The ledger types of the last release before managed entries existed,
/// copied as they were: what an older binary parses the ledger with.
mod old_ledger {
    use chrono::{DateTime, Utc};
    use serde::Deserialize;
    use task_traffic_controller::admit::State;

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    pub struct Entry {
        pub id: String,
        pub pid: u32,
        pub state: State,
        pub front: bool,
        pub weight: u32,
        pub predicted_rss: u64,
        #[serde(default)]
        pub expected_secs: Option<f64>,
        pub repo: String,
        #[serde(default)]
        pub subdir: String,
        pub argv: String,
        pub key: String,
        pub seq: u64,
        pub enqueued_at: DateTime<Utc>,
        #[serde(default)]
        pub started_at: Option<DateTime<Utc>>,
        #[serde(default)]
        pub using: Option<f64>,
        #[serde(default)]
        pub backfilled: bool,
    }

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    pub struct Ledger {
        pub version: u32,
        pub next_seq: u64,
        pub entries: Vec<Entry>,
    }
}

#[test]
fn an_older_binary_refuses_a_ledger_that_holds_a_managed_entry() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let old = |c: &Context| {
        serde_json::from_slice::<old_ledger::Ledger>(&std::fs::read(c.paths.ledger_file()).unwrap())
    };
    let (_, g) =
        registered(managed::register(&ctx, &cfg, &spec(dir.path(), "c", "d", &["true"])).unwrap());
    // It cannot load it, so it neither reaps the entry nor rewrites the file.
    assert!(old(&ctx).is_err());
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(ctx.paths.ledger_file()).unwrap()).unwrap();
    assert_eq!(raw["version"], 2);
    assert!(raw.get("entries").is_none());
    assert_eq!(raw["entries_v2"][0]["id"], "c");
    // This binary reads it, the sidecar is untouched, and the entry is kept
    // while its owner lives and released once it is gone.
    assert!(reaped(&ctx).find("c").is_some());
    assert_eq!(sidecar(&ctx, "c").spec_digest, "d");
    drop(g);
    assert!(reaped(&ctx).find("c").is_none());
    // Without managed entries the file is the old shape again.
    assert!(old(&ctx).is_ok());
}

#[test]
fn a_plain_entry_keeps_the_old_ledger_readable() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path());
    let mut l = Ledger::default();
    let s = l.take_seq();
    l.entries.push(task_traffic_controller::ledger::Entry {
        id: "plain".into(),
        pid: std::process::id(),
        state: State::Waiting,
        front: false,
        weight: 1,
        predicted_rss: 0,
        expected_secs: None,
        repo: String::new(),
        subdir: String::new(),
        argv: "x".into(),
        key: "k".into(),
        seq: s,
        enqueued_at: chrono::Utc::now(),
        started_at: None,
        using: None,
        backfilled: false,
    });
    l.save(&ctx.paths.ledger_file()).unwrap();
    let old: old_ledger::Ledger =
        serde_json::from_slice(&std::fs::read(ctx.paths.ledger_file()).unwrap()).unwrap();
    assert_eq!(old.version, 1);
    assert_eq!(old.entries.len(), 1);
}

#[test]
fn inspect_reports_a_quarantined_entry_and_still_counts_it() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let (_, g) =
        registered(managed::register(&ctx, &cfg, &spec(dir.path(), "c", "d", &["true"])).unwrap());
    orphan(&ctx, "c", Phase::LaunchIntent, None);
    drop(g);
    let insp = task_traffic_controller::inspect::inspect_with(&ctx, &cfg).unwrap();
    assert_eq!(insp.entries.len(), 1);
    assert!(insp.entries[0].suspect_dead);
    // Reading changed nothing.
    assert!(ledger(&ctx).find("c").is_some());
}

#[test]
fn a_quarantined_entry_is_demand_not_free_capacity_in_a_forecast() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        budget: 1,
        ..Config::default()
    };
    let ctx = ctx(dir.path());
    let (_, g) =
        registered(managed::register(&ctx, &cfg, &spec(dir.path(), "q", "d", &["true"])).unwrap());
    admit_now(&ctx, &cfg, "q");
    orphan(&ctx, "q", Phase::LaunchIntent, None);
    drop(g);
    let plan = task_traffic_controller::estimate::command_plan(
        &ctx,
        &cfg,
        &opts(dir.path(), &["fixture-tool"]),
    )
    .unwrap();
    let insp = task_traffic_controller::inspect::inspect_with(&ctx, &cfg).unwrap();
    assert!(insp.entries[0].suspect_dead);
    let f = task_traffic_controller::forecast::forecast(&ctx, &insp, &plan, &[]).unwrap();
    // The only slot is held by the quarantined entry: no start is promised.
    assert_eq!(f.target.timing.start_in_ms, None);
    assert!(ledger(&ctx).find("q").is_some());
}

/// Runs only when re-executed by the SIGKILL test below, as the supervisor.
#[test]
fn supervisor_entry() {
    let Some(dir) = std::env::var_os("TASK_TRAFFIC_CONTROLLER_TEST_SUPERVISOR_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let (ctx, cfg) = (ctx(&dir), Config::default());
    let s = spec(&dir, "sup-1", "d", &["sh", "-c", "sleep 60 & wait"]);
    let (_, guard) = registered(managed::register(&ctx, &cfg, &s).unwrap());
    admit_now(&ctx, &cfg, "sup-1");
    let _ = managed::run_command(&ctx, &guard, &s.opts, cfg.limits());
}

#[test]
fn a_sigkilled_supervisor_keeps_its_budget_until_its_tree_is_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, _cfg) = (
        ctx(dir.path()).with_scan(Arc::new(NoAmbientHidden)),
        Config::default(),
    );
    let mut sup = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "supervisor_entry",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("TASK_TRAFFIC_CONTROLLER_TEST_SUPERVISOR_DIR", dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = until(
        20,
        || matches!(managed::load_sidecar(&ctx, "sup-1"), Loaded::Ok(sc) if sc.phase == Phase::ChildStarted),
    );
    assert!(started, "the supervisor never started its child");
    let pgid = sidecar(&ctx, "sup-1").child.unwrap().pgid.unwrap();

    unsafe { libc::kill(sup.id() as libc::pid_t, libc::SIGKILL) };
    sup.wait().unwrap();
    assert!(
        !lock::lease_is_held(&ctx, "sup-1"),
        "the lease died with the supervisor"
    );

    // The tree lives on: the budget stays charged, over and over.
    for _ in 0..3 {
        let l = reaped(&ctx);
        let e = l.find("sup-1").expect("released while descendants run");
        assert_eq!(e.state, State::Running);
        std::thread::sleep(Duration::from_millis(100));
    }
    let v = managed::views(&ctx, &ledger(&ctx).entries);
    assert!(v[0].quarantined, "{:?}", v[0]);
    assert!(v[0].reason.contains("DescendantsAlive"), "{:?}", v[0]);

    // Stop the tree: now it is released and recorded as interrupted.
    unsafe { libc::killpg(pgid, libc::SIGKILL) };
    assert!(
        until(20, || reaped(&ctx).find("sup-1").is_none()),
        "not released after the tree stopped"
    );
    assert_eq!(
        sidecar(&ctx, "sup-1").terminal.unwrap().kind,
        TerminalKind::Interrupted
    );
}

/// Counts what the library tells its supervisor.
#[derive(Default)]
struct Recording {
    waiting: AtomicU32,
    finished: AtomicU32,
    pending: AtomicU32,
}

impl Supervisor for Recording {
    fn waiting(&self) {
        self.waiting.fetch_add(1, Ordering::SeqCst);
    }
    fn finished(&self) {
        self.finished.fetch_add(1, Ordering::SeqCst);
    }
    fn take_pending(&self) -> i32 {
        if self.pending.swap(0, Ordering::SeqCst) == 1 {
            libc::SIGTERM
        } else {
            0
        }
    }
}

#[test]
fn an_interrupted_wait_leaves_the_signal_handlers_to_the_caller() {
    for via_signal in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(Recording::default());
        rec.pending.store(u32::from(via_signal), Ordering::SeqCst);
        let ctx = ctx(dir.path()).with_supervisor(rec.clone());
        let o = opts(dir.path(), &["true"]);
        let cancelled = || !via_signal;
        let got = exec::run_gated(
            &ctx,
            &Config::default(),
            &o,
            "w",
            &Hooks {
                cancelled: &cancelled,
                progress: &|_| {},
                progress_tty: false,
            },
        )
        .unwrap();
        assert_eq!(got, Gated::Interrupted(libc::SIGTERM));
        assert_eq!(rec.waiting.load(Ordering::SeqCst), 1);
        // Restoring the defaults here would let a second SIGTERM kill the
        // process before the caller wrote the outcome down.
        assert_eq!(rec.finished.load(Ordering::SeqCst), 0);
        assert!(ledger(&ctx).entries.is_empty());
    }
}

#[test]
fn the_cli_shows_quarantine_in_inspect_and_leaves_status_json_alone() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state/task-traffic-controller");
    let cfg = Config::default();
    // The library and the CLI share one state dir; real boot id on both.
    let lib = Context::new(Paths::new(&state, dir.path().join("none.toml")))
        .with_live(Arc::new(FixedLive(None)));
    let (_, g) =
        registered(managed::register(&lib, &cfg, &spec(dir.path(), "c", "d", &["true"])).unwrap());
    orphan(&lib, "c", Phase::LaunchIntent, None);
    // This checks the LaunchUnknown branch: a launch intent that has not
    // aged past the grace keeps the entry without any scan. The intent is
    // placed an hour ahead (not "now") so its age stays negative however
    // slow this test is; a "now" intent would age past the grace on a slow
    // machine and the real scan of this machine (which sees hidden
    // orphans that depend on what else is running) would decide instead.
    let mut sc = sidecar(&lib, "c");
    sc.launch_intent_at = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    managed::write_sidecar(&lib, &sc).unwrap();
    drop(g);
    let run = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_task-traffic-controller"))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", dir.path())
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success());
        serde_json::from_slice::<serde_json::Value>(&o.stdout).unwrap()
    };
    let insp = run(&["inspect", "--json"]);
    assert_eq!(insp["managed"][0]["command_id"], "c");
    assert_eq!(insp["managed"][0]["quarantined"], true);
    assert_eq!(insp["entries"][0]["suspect_dead"], true);
    let status = run(&["status", "--json"]);
    assert_eq!(status["entries"][0]["id"], "c", "still charged");
    assert!(status.get("managed").is_none());
    assert!(status["entries"][0].get("managed").is_none());
    assert_eq!(
        insp["managed"][0]["hint"],
        "task-traffic-controller resolve c"
    );
    let text = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_task-traffic-controller"))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", dir.path())
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .args(args)
            .output()
            .unwrap();
        (
            o.status.code().unwrap(),
            String::from_utf8(o.stdout).unwrap(),
        )
    };
    let (_, shown) = text(&["status"]);
    assert!(shown.contains("held, owner gone (1)"), "{shown}");
    assert!(
        shown.contains("task-traffic-controller resolve c"),
        "{shown}"
    );
    // Nothing runs, so `resolve` releases the launch intent and says why.
    let (code, said) = text(&["resolve", "c"]);
    assert_eq!(code, 0, "{said}");
    assert!(
        said.contains("checked:") && said.contains("released"),
        "{said}"
    );
    let (code, said) = text(&["resolve", "c"]);
    // No such entry is neither "released" (0) nor "kept" (1) nor an error (2).
    assert_eq!(code, 3, "{said}");
    assert!(said.contains("no such entry"), "{said}");
    assert!(!said.contains("still charged"), "{said}");
    let (code, said) = text(&["resolve", "c", "--json"]);
    assert_eq!(code, 3, "{said}");
    let v: serde_json::Value = serde_json::from_str(&said).unwrap();
    assert_eq!(v["found"], false);
    assert_eq!(v["verdict"], "NoEntry");
    assert!(ledger(&lib).find("c").is_none());
    assert_eq!(
        sidecar(&lib, "c").terminal.unwrap().kind,
        TerminalKind::Interrupted
    );
}

fn run_to_end(
    ctx: &Context,
    cfg: &Config,
    s: &CommandSpec,
) -> (task_traffic_controller::managed::Ran, managed::OwnerGuard) {
    let (_, guard) = registered(managed::register(ctx, cfg, s).unwrap());
    admit_now(ctx, cfg, &s.command_id);
    let ran = managed::run_command(ctx, &guard, &s.opts, cfg.limits()).unwrap();
    (ran, guard)
}

#[test]
fn a_clean_exit_stops_the_leftover_group_before_the_budget_is_released() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let s = spec(dir.path(), "bg", "d", &["sh", "-c", "sleep 20 & exit 0"]);
    let (ran, guard) = run_to_end(&ctx, &cfg, &s);
    assert_eq!(ran.outcome.exit.code(), 0);
    assert!(!ran.outcome.descendants_left);
    let pgid = sidecar(&ctx, "bg").child.unwrap().pgid.unwrap();
    assert_eq!(
        unsafe { libc::killpg(pgid, 0) },
        -1,
        "the background sleep was left running"
    );
    managed::finish(&ctx, guard, ran.outcome.exit.into()).unwrap();
    assert!(ledger(&ctx).find("bg").is_none());
}

#[test]
fn finish_keeps_the_entry_while_the_group_has_members() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (
        ctx(dir.path()).with_scan(Arc::new(NoAmbientHidden)),
        Config::default(),
    );
    let s = spec(dir.path(), "live", "d", &["true"]);
    let (ran, guard) = run_to_end(&ctx, &cfg, &s);
    // A stand-in survivor in a group of its own, recorded as the child's.
    let mut survivor = {
        let mut c = Command::new("sleep");
        c.arg("30");
        std::os::unix::process::CommandExt::process_group(&mut c, 0);
        c.spawn().unwrap()
    };
    let mut sc = sidecar(&ctx, "live");
    sc.child.as_mut().unwrap().pgid = Some(survivor.id() as i32);
    managed::write_sidecar(&ctx, &sc).unwrap();

    let err = managed::finish(&ctx, guard, ran.outcome.exit.into()).unwrap_err();
    assert!(err.to_string().contains("still has members"), "{err}");
    assert!(ledger(&ctx).find("live").is_some(), "released too early");
    assert_eq!(sidecar(&ctx, "live").phase, Phase::ChildStarted);

    survivor.kill().unwrap();
    survivor.wait().unwrap();
    assert!(until(20, || reaped(&ctx).find("live").is_none()));
    assert_eq!(
        sidecar(&ctx, "live").terminal.unwrap().kind,
        TerminalKind::Interrupted
    );
}

#[test]
fn a_launch_intent_is_never_spawned_again() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let marker = dir.path().join("spawned");
    let s = spec(
        dir.path(),
        "again",
        "d",
        &["touch", marker.to_str().unwrap()],
    );
    let (_, g) = registered(managed::register(&ctx, &cfg, &s).unwrap());
    orphan(&ctx, "again", Phase::LaunchIntent, None);
    drop(g);
    // The supervisor comes back and adopts the entry.
    let (rc, guard) = registered(managed::register(&ctx, &cfg, &s).unwrap());
    assert_eq!(rc.phase, Phase::LaunchIntent);
    admit_now(&ctx, &cfg, "again");
    let err = managed::run_command(&ctx, &guard, &s.opts, cfg.limits()).unwrap_err();
    assert!(err.to_string().contains("resolve"), "{err}");
    assert!(!marker.exists(), "the command was started a second time");
    assert_eq!(sidecar(&ctx, "again").phase, Phase::LaunchIntent);
}

#[test]
fn finishing_a_terminal_sidecar_keeps_what_was_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let s = spec(dir.path(), "t", "d", &["true"]);
    let (ran, guard) = run_to_end(&ctx, &cfg, &s);
    let mut sc = sidecar(&ctx, "t");
    sc.phase = Phase::Terminal;
    sc.terminal = Some(managed::TerminalRecord {
        kind: TerminalKind::Exited { code: 0 },
        at: chrono::Utc::now(),
        detail: None,
    });
    managed::write_sidecar(&ctx, &sc).unwrap();
    let _ = ran;
    managed::finish(&ctx, guard, TerminalKind::Cancelled).unwrap();
    assert_eq!(
        sidecar(&ctx, "t").terminal.unwrap().kind,
        TerminalKind::Exited { code: 0 }
    );
    assert!(ledger(&ctx).find("t").is_none());
}

fn ctx_with_scan(dir: &Path, clock: Arc<FakeClock>, scan: Probe) -> Context {
    ctx(dir)
        .with_clock(clock)
        .with_scan(Arc::new(FixedScan::probe(scan)))
}

/// A registered entry whose supervisor died at its launch intent, `age`
/// seconds ago (never, when `None`).
fn dead_at_launch(dir: &Path, clock: &Arc<FakeClock>, id: &str, intent: bool) {
    let c = ctx_with_scan(dir, clock.clone(), Probe::Gone);
    let (_, g) = registered(
        managed::register(&c, &Config::default(), &spec(dir, id, "d", &["true"])).unwrap(),
    );
    let mut sc = sidecar(&c, id);
    sc.phase = Phase::LaunchIntent;
    sc.launch_intent_at = intent.then(|| c.clock.now());
    managed::write_sidecar(&c, &sc).unwrap();
    drop(g);
}

#[test]
fn a_launch_intent_is_released_only_after_the_grace_with_a_clear_scan() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    dead_at_launch(dir.path(), &clock, "l", true);
    let with = |scan| ctx_with_scan(dir.path(), clock.clone(), scan);
    assert!(reaped(&with(Probe::Gone)).find("l").is_some(), "too recent");
    clock.advance(Duration::from_secs(59));
    assert!(reaped(&with(Probe::Gone)).find("l").is_some(), "59s");
    clock.advance(Duration::from_secs(2));
    // Old enough, but the scan cannot say or finds a marked process.
    assert!(reaped(&with(Probe::Unknown)).find("l").is_some());
    assert!(reaped(&with(Probe::Alive)).find("l").is_some());
    let v = managed::views(&with(Probe::Unknown), &ledger(&with(Probe::Gone)).entries);
    assert!(
        v[0].quarantined && v[0].reason.contains("ProbeUnknown"),
        "{v:?}"
    );
    assert_eq!(v[0].hint, "task-traffic-controller resolve l");
    // Clear: nothing is running, so it is settled and recorded.
    assert!(reaped(&with(Probe::Gone)).find("l").is_none());
    let sc = sidecar(&with(Probe::Gone), "l");
    assert_eq!(sc.phase, Phase::Terminal);
    let t = sc.terminal.unwrap();
    assert_eq!(t.kind, TerminalKind::Interrupted);
    assert!(t.detail.unwrap().contains("LaunchSettled"));
}

#[test]
fn time_alone_never_releases_a_launch_intent_without_a_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    dead_at_launch(dir.path(), &clock, "n", false);
    clock.advance(Duration::from_secs(86_400));
    let c = ctx_with_scan(dir.path(), clock, Probe::Gone);
    assert!(reaped(&c).find("n").is_some());
}

#[test]
fn resolve_checked_releases_only_what_the_checks_allow() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    dead_at_launch(dir.path(), &clock, "r", false);
    let with = |scan| ctx_with_scan(dir.path(), clock.clone(), scan);
    let unknown = managed::resolve_checked(&with(Probe::Unknown), "r").unwrap();
    assert!(!unknown.released && unknown.verdict.contains("ProbeUnknown"));
    let alive = managed::resolve_checked(&with(Probe::Alive), "r").unwrap();
    assert!(!alive.released && alive.verdict.contains("DescendantsAlive"));
    assert!(ledger(&with(Probe::Gone)).find("r").is_some());
    // No timestamp and a clear scan: a person may settle it at once.
    let done = managed::resolve_checked(&with(Probe::Gone), "r").unwrap();
    assert!(done.released, "{done:?}");
    assert_eq!(done.phase, Some(Phase::LaunchIntent));
    assert!(ledger(&with(Probe::Gone)).find("r").is_none());
    let sc = sidecar(&with(Probe::Gone), "r");
    assert_eq!(
        sc.terminal.as_ref().unwrap().kind,
        TerminalKind::Interrupted
    );
    assert!(sc
        .terminal
        .unwrap()
        .detail
        .unwrap()
        .contains("task-traffic-controller resolve"));
    let again = managed::resolve_checked(&with(Probe::Gone), "r").unwrap();
    assert!(!again.released && !again.found && again.verdict == "NoEntry");
}

#[test]
fn resolve_checked_judges_an_unreadable_sidecar_by_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    let c = ctx_with_scan(dir.path(), clock.clone(), Probe::Gone);
    let (_, g) = registered(
        managed::register(
            &c,
            &Config::default(),
            &spec(dir.path(), "u", "d", &["true"]),
        )
        .unwrap(),
    );
    std::fs::write(c.paths.managed_file("u"), b"{ not json").unwrap();
    // A live owner is never touched.
    let held = managed::resolve_checked(&c, "u").unwrap();
    assert!(!held.released && held.lease_held);
    drop(g);
    let alive = ctx_with_scan(dir.path(), clock, Probe::Alive);
    assert!(!managed::resolve_checked(&alive, "u").unwrap().released);
    assert!(ledger(&c).find("u").is_some());
    let done = managed::resolve_checked(&c, "u").unwrap();
    assert!(done.released && done.phase.is_none());
    assert_eq!(sidecar(&c, "u").phase, Phase::Terminal);
    assert!(ledger(&c).find("u").is_none());
}

#[test]
fn resolve_checked_keeps_the_recorded_outcome_of_a_terminal_entry() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let s = spec(dir.path(), "term", "d", &["true"]);
    let (_, g) = registered(managed::register(&ctx, &cfg, &s).unwrap());
    let mut sc = sidecar(&ctx, "term");
    sc.phase = Phase::Terminal;
    sc.terminal = Some(managed::TerminalRecord {
        kind: TerminalKind::Exited { code: 7 },
        at: chrono::Utc::now(),
        detail: None,
    });
    managed::write_sidecar(&ctx, &sc).unwrap();
    drop(g);
    assert!(managed::resolve_checked(&ctx, "term").unwrap().released);
    assert_eq!(
        sidecar(&ctx, "term").terminal.unwrap().kind,
        TerminalKind::Exited { code: 7 }
    );
    assert!(ledger(&ctx).find("term").is_none());
}

#[test]
fn a_managed_command_carries_its_id_in_its_environment() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let out = dir.path().join("id.txt");
    let script = format!(
        "printf %s \"$TASK_TRAFFIC_CONTROLLER_MANAGED_ID\" > {}",
        out.display()
    );
    let s = spec(dir.path(), "env-id", "d", &["sh", "-c", &script]);
    let (ran, guard) = run_to_end(&ctx, &cfg, &s);
    assert!(!ran.outcome.descendants_left);
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "env-id");
    managed::finish(&ctx, guard, ran.outcome.exit.into()).unwrap();
}

/// Runs only when re-executed as a helper by the tests below (the test
/// executable is used because, on macOS, Apple's own binaries do not show
/// their environment, so a `sleep` could not be found by its marker).
/// `escape` starts a `sleep` role in a session of its own and exits at once;
/// `sleep` blocks, optionally ignoring SIGTERM, and records its pid.
// The escaped child is meant to be orphaned, not waited for.
#[allow(clippy::zombie_processes)]
#[test]
fn helper_entry() {
    let Ok(role) = std::env::var("TASK_TRAFFIC_CONTROLLER_TEST_ROLE") else {
        return;
    };
    let pidfile = std::env::var_os("TASK_TRAFFIC_CONTROLLER_TEST_PIDFILE");
    if role == "escape" {
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.args(["--exact", "helper_entry", "--nocapture"])
            .env("TASK_TRAFFIC_CONTROLLER_TEST_ROLE", "sleep")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut c, || {
                libc::setsid();
                Ok(())
            });
        }
        c.spawn().unwrap();
        // Leave only once the sleeper has ignored SIGTERM and written its
        // pid, so the leader's exit is what starts the stop and the SIGKILL
        // path is certain to be needed.
        if let Some(f) = pidfile {
            let end = Instant::now() + Duration::from_secs(10);
            while !Path::new(&f).exists() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    } else {
        if std::env::var_os("TASK_TRAFFIC_CONTROLLER_TEST_IGNORE_TERM").is_some() {
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
        }
        if let Some(f) = pidfile {
            std::fs::write(f, std::process::id().to_string()).unwrap();
        }
        std::thread::sleep(Duration::from_secs(30));
    }
}

fn helper_command(role: &str, pidfile: &Path, ignore_term: bool) -> Vec<String> {
    let mut v = vec![
        "env".to_string(),
        format!("TASK_TRAFFIC_CONTROLLER_TEST_ROLE={role}"),
    ];
    v.push(format!(
        "TASK_TRAFFIC_CONTROLLER_TEST_PIDFILE={}",
        pidfile.display()
    ));
    if ignore_term {
        v.push("TASK_TRAFFIC_CONTROLLER_TEST_IGNORE_TERM=1".into());
    }
    v.push(std::env::current_exe().unwrap().display().to_string());
    v.extend(["--exact", "helper_entry", "--nocapture"].map(String::from));
    v
}

/// Is `pid` a process that still runs? `kill(pid, 0)` succeeds for a zombie
/// too, and in a container without an init nobody reaps an orphan, so a
/// zombie counts as dead.
fn pid_alive(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    !is_zombie(pid)
}

#[cfg(target_os = "linux")]
fn is_zombie(pid: i32) -> bool {
    // "<pid> (<comm>) <state> ...": the comm may contain anything, so the
    // state is what follows the last ')'.
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            let rest = s.get(s.rfind(')')? + 1..)?;
            rest.trim_start().chars().next()
        })
        .is_some_and(|c| c == 'Z' || c == 'X')
}

#[cfg(target_os = "macos")]
fn is_zombie(pid: i32) -> bool {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    n == size && info.pbi_status == libc::SZOMB
}

fn escaped_case(id: &str, ignore_term: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let pidfile = dir.path().join("pid");
    let argv = helper_command("escape", &pidfile, ignore_term);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let s = spec(dir.path(), id, "d", &argv);
    let (ran, guard) = run_to_end(&ctx, &cfg, &s);
    assert!(until(5, || pidfile.exists()));
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(ran.outcome.exit.code(), 0);
    assert!(
        !ran.outcome.descendants_left,
        "the escaped sleeper survived"
    );
    assert!(
        until(5, || !pid_alive(pid)),
        "the sleeper outside the group was left running"
    );
    managed::finish(&ctx, guard, ran.outcome.exit.into()).unwrap();
    assert!(ledger(&ctx).find(id).is_none());
}

#[test]
fn a_descendant_that_left_the_process_group_is_stopped_before_release() {
    escaped_case("escaped-term", false);
}

#[test]
fn a_descendant_that_ignores_sigterm_is_killed() {
    escaped_case("escaped-kill", true);
}

#[test]
fn a_process_with_the_same_id_from_another_state_dir_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    // Same id, but admitted by another state directory: not ours to stop
    // and not a reason to hold the budget.
    let mut stranger = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "helper_entry", "--nocapture"])
        .env("TASK_TRAFFIC_CONTROLLER_TEST_ROLE", "sleep")
        .env(task_traffic_controller::marker::ENV_VAR, "shared-id")
        .env(
            task_traffic_controller::marker::SCOPE_VAR,
            task_traffic_controller::marker::scope(other.path()),
        )
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let s = spec(dir.path(), "shared-id", "d", &["true"]);
    let (ran, guard) = run_to_end(&ctx, &cfg, &s);
    assert!(!ran.outcome.descendants_left);
    managed::finish(&ctx, guard, ran.outcome.exit.into()).unwrap();
    assert!(ledger(&ctx).find("shared-id").is_none());
    assert!(stranger.try_wait().unwrap().is_none(), "it was signalled");
    stranger.kill().unwrap();
    stranger.wait().unwrap();
}

#[test]
fn force_keeps_a_group_with_no_leader_even_if_no_member_is_marked() {
    use std::os::unix::process::CommandExt;
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    // The original group: its leader is gone but a member (one that carries
    // no marker, as after `env -i`) lives on. Nobody leads the group, so it
    // is not a reused number and must not be freed.
    let mut leader = Command::new("sh")
        .args(["-c", "sleep 60 & exit 0"])
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = leader.id() as i32;
    leader.wait().unwrap();
    let gone = ctx_with_fixed(dir.path(), clock.clone(), Scan::new(Probe::Gone, vec![]));
    dead_child(dir.path(), &clock, "o1", move |sc| {
        sc.child.as_mut().unwrap().pgid = Some(pgid);
    });
    let r = managed::resolve_checked_with(&gone, "o1", true).unwrap();
    assert!(!r.released && !r.forced, "{r:?}");
    assert!(ledger(&gone).find("o1").is_some());
    unsafe { libc::killpg(pgid, libc::SIGKILL) };
}

#[test]
fn a_reaper_keeps_the_entry_while_an_escaped_descendant_runs() {
    let dir = tempfile::tempdir().unwrap();
    // Unrelated hidden orphans on a busy machine would hold the entry as
    // `ProbeUnknown` and make the final release flaky (see `NoAmbientHidden`).
    let ctx = ctx(dir.path()).with_scan(Arc::new(NoAmbientHidden));
    let cfg = Config::default();
    let s = spec(dir.path(), "esc-held", "d", &["true"]);
    let (ran, guard) = run_to_end(&ctx, &cfg, &s);
    // A stand-in survivor outside the group, marked like the real thing.
    let mut survivor = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "helper_entry", "--nocapture"])
        .env("TASK_TRAFFIC_CONTROLLER_TEST_ROLE", "sleep")
        .env(task_traffic_controller::marker::ENV_VAR, "esc-held")
        .env(
            task_traffic_controller::marker::SCOPE_VAR,
            task_traffic_controller::marker::scope(ctx.paths.state_dir()),
        )
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    drop(guard);
    assert!(
        reaped(&ctx).find("esc-held").is_some(),
        "released too early"
    );
    let v = managed::views(&ctx, &ledger(&ctx).entries);
    assert!(v[0].reason.contains("DescendantsAlive"), "{v:?}");
    let r = managed::resolve_checked(&ctx, "esc-held").unwrap();
    assert!(!r.released);
    let _ = ran;
    survivor.kill().unwrap();
    survivor.wait().unwrap();
    assert!(until(20, || reaped(&ctx).find("esc-held").is_none()));
}

fn record_for(key: &str, mid: Option<&str>) -> Record {
    Record {
        key: key.into(),
        repo: String::new(),
        subdir: String::new(),
        argv: "x".into(),
        key_argv: String::new(),
        started_at: chrono::Utc::now(),
        wall_secs: 1.0,
        cpu_secs: 1.0,
        max_rss_bytes: 1,
        exit_code: 0,
        weight_used: 1,
        contended: false,
        measurement_id: mid.map(String::from),
    }
}

#[test]
fn gc_keeps_a_marker_exactly_as_long_as_its_record() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path());
    // Three measurements of one key and two of another, history_keep 2.
    for (key, id) in [
        ("a", "m0"),
        ("a", "m1"),
        ("a", "m2"),
        ("b", "m3"),
        ("b", "m4"),
    ] {
        assert_eq!(
            managed::record_measurement(&ctx, id, &record_for(key, None)).unwrap(),
            Measured::Applied
        );
    }
    std::fs::write(ctx.paths.measurements_dir().join("p.pending"), b"").unwrap();
    // Per key: only m0 (the oldest of "a") loses its record.
    assert_eq!(managed::trim_history(&ctx, 2).unwrap(), 1);
    assert_eq!(managed::gc_measurements(&ctx).unwrap(), 1);
    let mut left: Vec<String> = std::fs::read_dir(ctx.paths.measurements_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            "m1.applied",
            "m2.applied",
            "m3.applied",
            "m4.applied",
            "p.pending"
        ]
    );
    // Every marker left has its record, so none is dropped (the old
    // behaviour kept the newest N markers overall and could keep one whose
    // record was gone, or drop one whose record was not).
    assert_eq!(managed::gc_measurements(&ctx).unwrap(), 0);
    assert_eq!(
        managed::record_measurement(&ctx, "m4", &record_for("b", None)).unwrap(),
        Measured::Duplicate
    );
    let none = ctx_with_boot(tempfile::tempdir().unwrap().path(), None);
    assert_eq!(managed::gc_measurements(&none).unwrap(), 0);
}

#[test]
fn trimming_the_history_waits_for_the_history_lock() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path()).with_ledger_lock_timeout(Duration::from_millis(200));
    managed::record_measurement(&ctx, "m0", &record_for("a", None)).unwrap();
    managed::record_measurement(&ctx, "m1", &record_for("a", None)).unwrap();
    // A recorder is in the middle of its append: the trim must not run.
    let held = lock::acquire(&ctx.paths.history_lock(), Duration::from_secs(1)).unwrap();
    let err = managed::trim_history(&ctx, 1).unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert_eq!(
        history::read_all(ctx.paths.history_file()).unwrap().len(),
        2
    );
    drop(held);
    assert_eq!(managed::trim_history(&ctx, 1).unwrap(), 1);
    // The marker of the dropped record goes with the next gc, not before.
    assert_eq!(managed::gc_measurements(&ctx).unwrap(), 1);
}

// ---- descendants recorded while the command runs, and `resolve --force` ----

/// Runs only when re-executed by the test below, as the supervisor of a
/// command whose child starts a descendant in a session of its own (so it
/// leaves the process group) and writes its pid to a file. On a Mac with
/// Apple's perl and sleep their environment is hidden from other processes,
/// so only the recorded descendants can find it there.
#[test]
fn setsid_supervisor_entry() {
    let Some(dir) = std::env::var_os("TASK_TRAFFIC_CONTROLLER_TEST_SETSID_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let (ctx, cfg) = (ctx(&dir), Config::default());
    // The descendant drops the marker variables before it execs, so only the
    // recorded descendants (not the environment marker) can find it.
    let script = "use POSIX qw(setsid); my $p = fork(); if (!$p) { setsid(); \
        open(F, '>', $ARGV[0] . '.tmp'); print F $$; close F; \
        rename($ARGV[0] . '.tmp', $ARGV[0]); \
        delete $ENV{TASK_TRAFFIC_CONTROLLER_MANAGED_ID}; delete $ENV{TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE}; \
        exec('sleep', '60'); } sleep 60;";
    let pidfile = dir.join("sleeper.pid");
    let s = spec(
        &dir,
        "sup-2",
        "d",
        &["perl", "-e", script, pidfile.to_str().unwrap()],
    );
    let (_, guard) = registered(managed::register(&ctx, &cfg, &s).unwrap());
    admit_now(&ctx, &cfg, "sup-2");
    let _ = managed::run_command(&ctx, &guard, &s.opts, cfg.limits());
}

/// Stops the supervisor and the sleeper even when an assert fails.
struct Cleanup {
    sup: Option<std::process::Child>,
    sleeper: Option<u32>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(mut s) = self.sup.take() {
            // The command (perl) leads its own group and is a child of the
            // supervisor: stop that group while the parent link still shows.
            if let Ok(o) = Command::new("pgrep")
                .args(["-P", &s.id().to_string()])
                .output()
            {
                for pid in String::from_utf8_lossy(&o.stdout).split_whitespace() {
                    if let Ok(pid) = pid.parse::<libc::pid_t>() {
                        unsafe { libc::killpg(pid, libc::SIGKILL) };
                        unsafe { libc::kill(pid, libc::SIGKILL) };
                    }
                }
            }
            let _ = s.kill();
            let _ = s.wait();
        }
        if let Some(p) = self.sleeper {
            unsafe { libc::kill(p as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// The real process scan, minus the hidden processes it reports. The
/// supervisor in this test is killed, so the window in which a hidden
/// orphan counts as a possible descendant stays open, and any unrelated
/// process the OS starts meanwhile and keeps running (Spotlight's
/// `mdworker_shared` appears while a build writes files) would hold the
/// entry as `ProbeUnknown`. That rule has its own tests with fixed scans;
/// here only the recorded descendant decides.
struct NoAmbientHidden;

impl task_traffic_controller::ProcessScan for NoAmbientHidden {
    fn scan(&self, id: &str, scope: &str) -> Scan {
        let mut s = task_traffic_controller::process::scan_system(id, scope);
        s.hidden.clear();
        s
    }
}

#[test]
fn a_descendant_that_left_the_group_is_recorded_and_keeps_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path()).with_scan(Arc::new(NoAmbientHidden));
    let sup = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "setsid_supervisor_entry",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("TASK_TRAFFIC_CONTROLLER_TEST_SETSID_DIR", dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut guard = Cleanup {
        sup: Some(sup),
        sleeper: None,
    };
    let pidfile = dir.path().join("sleeper.pid");
    let sleeper = || -> Option<u32> { std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok() };
    assert!(
        until(20, || sleeper().is_some()),
        "the descendant never started"
    );
    let sleeper = sleeper().unwrap();
    guard.sleeper = Some(sleeper);
    // Recorded within a few sampling intervals.
    let recorded = until(20, || match managed::load_sidecar(&ctx, "sup-2") {
        Loaded::Ok(sc) => sc.descendants.iter().any(|p| p.pid == sleeper),
        _ => false,
    });
    assert!(recorded, "the descendant was not recorded");
    let pgid = sidecar(&ctx, "sup-2").child.unwrap().pgid.unwrap();

    // The supervisor dies, then the leader: only the descendant is left,
    // orphaned and outside the group.
    let mut sup = guard.sup.take().unwrap();
    unsafe { libc::kill(sup.id() as libc::pid_t, libc::SIGKILL) };
    sup.wait().unwrap();
    unsafe { libc::killpg(pgid, libc::SIGKILL) };
    assert!(until(20, || unsafe { libc::killpg(pgid, 0) } == -1));
    for _ in 0..3 {
        assert!(
            reaped(&ctx).find("sup-2").is_some(),
            "released while the descendant runs"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let v = managed::views(&ctx, &ledger(&ctx).entries);
    assert!(v[0].reason.contains("DescendantsAlive"), "{:?}", v[0]);

    unsafe { libc::kill(sleeper as libc::pid_t, libc::SIGKILL) };
    assert!(
        until(20, || reaped(&ctx).find("sup-2").is_none()),
        "not released after the descendant stopped: {:?}",
        managed::views(&ctx, &ledger(&ctx).entries)
    );
}

fn ctx_with_fixed(dir: &Path, clock: Arc<FakeClock>, scan: Scan) -> Context {
    ctx(dir)
        .with_clock(clock)
        .with_scan(Arc::new(FixedScan(scan)))
}

/// A registered entry whose supervisor died with the child gone (no
/// process, no group), `edit`ed afterwards.
fn dead_child(
    dir: &Path,
    clock: &Arc<FakeClock>,
    id: &str,
    edit: impl FnOnce(&mut managed::Sidecar),
) {
    let c = ctx_with_fixed(dir, clock.clone(), Scan::new(Probe::Alive, vec![]));
    let (_, g) = registered(
        managed::register(&c, &Config::default(), &spec(dir, id, "d", &["true"])).unwrap(),
    );
    let mut sc = sidecar(&c, id);
    sc.phase = Phase::ChildStarted;
    sc.child = Some(task_traffic_controller::process::ChildIdentity {
        pid: 0x3fff_fff0,
        start_time: Some(1),
        pgid: Some(0x3fff_fff0),
        sid: None,
    });
    edit(&mut sc);
    managed::write_sidecar(&c, &sc).unwrap();
    drop(g);
}

#[test]
fn dropped_descendants_make_a_clear_scan_unknown_and_only_force_releases_it() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    dead_child(dir.path(), &clock, "tr", |sc| {
        sc.descendants_truncated = true
    });
    let c = ctx_with_fixed(dir.path(), clock.clone(), Scan::new(Probe::Gone, vec![]));
    assert!(reaped(&c).find("tr").is_some(), "freed on a clear scan");
    let kept = managed::resolve_checked(&c, "tr").unwrap();
    assert!(!kept.released && !kept.forced);
    assert!(kept.verdict.contains("ProbeUnknown"), "{kept:?}");
    let forced = managed::resolve_checked_with(&c, "tr", true).unwrap();
    assert!(forced.released && forced.forced, "{forced:?}");
    assert!(forced.checked.starts_with("forced: "), "{forced:?}");
    assert!(ledger(&c).find("tr").is_none());
    let sc = sidecar(&c, "tr");
    assert_eq!(sc.phase, Phase::Terminal);
    let t = sc.terminal.unwrap();
    assert_eq!(t.kind, TerminalKind::Interrupted);
    assert!(t.detail.unwrap().starts_with("forced: "));
}

#[test]
fn a_hidden_orphan_started_since_the_child_keeps_a_started_entry() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    // The child identity written by `dead_child` started at second 1.
    let hidden = |parent: u32, start: u64| {
        let mut s = Scan::new(Probe::Gone, vec![]);
        s.hidden.push(HiddenProc {
            proc: ProcRef {
                pid: 4242,
                start_time: Some(start),
            },
            parent: Some(parent),
        });
        ctx_with_fixed(dir.path(), clock.clone(), s)
    };
    dead_child(dir.path(), &clock, "hc1", |_| {});
    dead_child(dir.path(), &clock, "hc2", |_| {});
    dead_child(dir.path(), &clock, "hc3", |_| {});
    dead_child(dir.path(), &clock, "hc4", |sc| {
        sc.child.as_mut().unwrap().start_time = Some(100)
    });
    // An orphan that began after the child: it may be a descendant.
    let orphan = hidden(1, 50);
    assert!(reaped(&orphan).find("hc1").is_some());
    let kept = managed::resolve_checked(&orphan, "hc1").unwrap();
    assert!(
        !kept.released && kept.verdict.contains("ProbeUnknown"),
        "{kept:?}"
    );
    let forced = managed::resolve_checked_with(&orphan, "hc1", true).unwrap();
    assert!(forced.released && forced.forced, "{forced:?}");
    // Somebody else's child: not it.
    let other = hidden(31337, 50);
    assert!(reaped(&other).find("hc2").is_none());
    // The same orphan with a clear scan and nothing hidden is freed.
    let clear = ctx_with_fixed(dir.path(), clock.clone(), Scan::new(Probe::Gone, vec![]));
    assert!(reaped(&clear).find("hc3").is_none());
    // An orphan older than the child: not it either.
    let older = hidden(1, 99);
    assert!(reaped(&older).find("hc4").is_none());
}

#[test]
fn force_never_releases_what_is_known_to_be_running() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    // A marked process, a live recorded descendant, even with the record
    // truncated.
    let mut live = Command::new("sleep").arg("60").spawn().unwrap();
    let rec = ProcRef {
        pid: live.id(),
        start_time: task_traffic_controller::process::start_time(live.id()),
    };
    dead_child(dir.path(), &clock, "m1", |_| {});
    dead_child(dir.path(), &clock, "m2", |sc| {
        sc.descendants = vec![rec];
        sc.descendants_truncated = true;
    });
    let gone = ctx_with_fixed(dir.path(), clock.clone(), Scan::new(Probe::Gone, vec![]));
    let marked = ctx_with_fixed(
        dir.path(),
        clock.clone(),
        Scan::new(
            Probe::Alive,
            vec![ProcRef {
                pid: 1,
                start_time: Some(1),
            }],
        ),
    );
    let r = managed::resolve_checked_with(&marked, "m1", true).unwrap();
    assert!(
        !r.released && !r.forced && r.verdict.contains("DescendantsAlive"),
        "{r:?}"
    );
    assert!(r.checked.contains("--force"), "{r:?}");
    let r = managed::resolve_checked_with(&gone, "m2", true).unwrap();
    assert!(
        !r.released && !r.forced && r.verdict.contains("DescendantsAlive"),
        "{r:?}"
    );
    // Reused pid (start time differs): not the recorded process.
    let mut sc = sidecar(&gone, "m2");
    sc.descendants[0].start_time = rec.start_time.map(|t| t + 1000);
    managed::write_sidecar(&gone, &sc).unwrap();
    let r = managed::resolve_checked_with(&gone, "m2", false).unwrap();
    assert!(!r.released && r.verdict.contains("ProbeUnknown"), "{r:?}");
    // The recorded one is gone: a clear, complete record releases.
    live.kill().unwrap();
    live.wait().unwrap();
    let mut sc = sidecar(&gone, "m2");
    sc.descendants[0].start_time = rec.start_time;
    sc.descendants_truncated = false;
    managed::write_sidecar(&gone, &sc).unwrap();
    let r = managed::resolve_checked_with(&gone, "m2", false).unwrap();
    assert!(r.released && !r.forced, "{r:?}");
}

#[test]
fn force_leaves_a_held_lease_and_a_missing_entry_alone() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    let c = ctx_with_fixed(dir.path(), clock, Scan::new(Probe::Unknown, vec![]));
    let (_, guard) = registered(
        managed::register(
            &c,
            &Config::default(),
            &spec(dir.path(), "h", "d", &["true"]),
        )
        .unwrap(),
    );
    let r = managed::resolve_checked_with(&c, "h", true).unwrap();
    assert!(r.lease_held && !r.released && !r.forced, "{r:?}");
    let n = managed::resolve_checked_with(&c, "nope", true).unwrap();
    assert!(!n.found && !n.released && !n.forced);
    drop(guard);
}

#[test]
fn a_hidden_orphan_started_since_the_intent_keeps_a_launch_intent() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    dead_at_launch(dir.path(), &clock, "hid", true);
    clock.advance(Duration::from_secs(120));
    let intent = sidecar(&ctx(dir.path()), "hid")
        .launch_intent_at
        .unwrap()
        .timestamp() as u64;
    let hidden = |parent: u32, start: u64| {
        let mut s = Scan::new(Probe::Gone, vec![]);
        s.hidden.push(HiddenProc {
            proc: ProcRef {
                pid: 4242,
                start_time: Some(start),
            },
            parent: Some(parent),
        });
        ctx_with_fixed(dir.path(), clock.clone(), s)
    };
    // Started before the intent, or somebody else's child: not it.
    let before = hidden(1, intent - 5);
    let other = hidden(31337, intent + 5);
    let orphan = hidden(1, intent + 5);
    let v = managed::views(&orphan, &ledger(&orphan).entries);
    assert!(
        v[0].quarantined && v[0].reason.contains("ProbeUnknown"),
        "{v:?}"
    );
    let r = managed::resolve_checked(&orphan, "hid").unwrap();
    assert!(!r.released, "{r:?}");
    let forced = managed::resolve_checked_with(&orphan, "hid", true).unwrap();
    assert!(forced.released && forced.forced, "{forced:?}");
    // A second entry, with the unrelated hidden processes: settled.
    dead_at_launch(dir.path(), &clock, "hid2", true);
    clock.advance(Duration::from_secs(120));
    assert!(reaped(&before).find("hid2").is_none());
    dead_at_launch(dir.path(), &clock, "hid3", true);
    clock.advance(Duration::from_secs(120));
    assert!(reaped(&other).find("hid3").is_none());
}

/// A recorder that cannot start, as when the command cannot be read.
fn failing_tracker(
    root: u32,
    record: task_traffic_controller::tracker::Record,
) -> task_traffic_controller::tracker::Tracker {
    task_traffic_controller::tracker::Tracker::failed(root, record)
}

#[test]
fn a_failed_recorder_is_persisted_so_finish_and_the_reaper_both_keep_the_entry() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let s = spec(dir.path(), "tf", "d", &["sh", "-c", "sleep 20 & exit 0"]);
    let (_, guard) = registered(managed::register(&ctx, &cfg, &s).unwrap());
    admit_now(&ctx, &cfg, "tf");
    let ran = managed::run_command_with_tracker(
        &ctx,
        &guard,
        &s.opts,
        cfg.limits(),
        Some(failing_tracker),
    )
    .unwrap();
    assert!(ran.outcome.tracker_unknown);
    // The unknown reached the sidecar, with the time it was written.
    let sc = sidecar(&ctx, "tf");
    assert!(
        sc.descendants_truncated && sc.recorded_at.is_some(),
        "{sc:?}"
    );
    // The in-memory guard refuses on its own...
    let err = managed::finish(&ctx, guard, ran.outcome.exit.into()).unwrap_err();
    assert!(err.to_string().contains("could not be recorded"), "{err}");
    assert!(
        ledger(&ctx).find("tf").is_some(),
        "the entry was released although the recorder failed"
    );
    assert_eq!(sidecar(&ctx, "tf").phase, Phase::ChildStarted);
    // ...and so does the reaper, reading only the sidecar: everything is
    // gone, but nothing was recorded to prove it.
    let a = managed::assess(&ctx, "tf");
    assert_eq!(a.verdict, Verdict::Keep(Keep::ProbeUnknown), "{a:?}");
    let kept = managed::resolve_checked(&ctx, "tf").unwrap();
    assert!(!kept.released, "{kept:?}");
    let forced = managed::resolve_checked_with(&ctx, "tf", true).unwrap();
    assert!(forced.released && forced.forced, "{forced:?}");
}

#[test]
fn a_recorder_that_failed_after_recording_keeps_what_it_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let s = spec(dir.path(), "tk", "d", &["true"]);
    let (_, guard) = registered(managed::register(&ctx, &cfg, &s).unwrap());
    admit_now(&ctx, &cfg, "tk");
    let kept = ProcRef {
        pid: 0x3fff_ffe0,
        start_time: Some(5),
    };
    let mut sc = sidecar(&ctx, "tk");
    sc.descendants = vec![kept];
    managed::write_sidecar(&ctx, &sc).unwrap();
    let ran = managed::run_command_with_tracker(
        &ctx,
        &guard,
        &s.opts,
        cfg.limits(),
        Some(failing_tracker),
    )
    .unwrap();
    assert!(ran.outcome.tracker_unknown);
    let sc = sidecar(&ctx, "tk");
    assert_eq!(sc.descendants, vec![kept]);
    assert!(sc.descendants_truncated);
}

#[test]
fn a_hidden_orphan_counts_only_inside_the_window_of_the_last_record() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    let at = |secs: i64| chrono::DateTime::from_timestamp(secs, 0).unwrap();
    let hidden = |start: u64| {
        let mut s = Scan::new(Probe::Gone, vec![]);
        s.hidden.push(HiddenProc {
            proc: ProcRef {
                pid: 4242,
                start_time: Some(start),
            },
            parent: Some(1),
        });
        ctx_with_fixed(dir.path(), clock.clone(), s)
    };
    // The child began at second 1 (see `dead_child`); the last record was
    // written at second 60.
    for id in ["w-in", "w-late"] {
        dead_child(dir.path(), &clock, id, |sc| {
            sc.recorded_at = Some(at(60));
            sc.tracked_to_exit = true;
        });
    }
    // The end is the last record plus the 2 s slack, inclusive.
    assert!(reaped(&hidden(62)).find("w-in").is_some(), "the slack");
    // Started after the last record plus the slack: an unrelated process.
    let after = reaped(&hidden(63));
    assert!(after.find("w-in").is_none(), "after");
    assert!(after.find("w-late").is_none(), "after");
    // No child start time: the launch intent is the lower bound.
    let no_start = |id: &str| {
        dead_child(dir.path(), &clock, id, |sc| {
            sc.child.as_mut().unwrap().start_time = None;
            sc.launch_intent_at = Some(at(40));
            sc.recorded_at = Some(at(60));
            sc.tracked_to_exit = true;
        })
    };
    no_start("w-before");
    assert!(reaped(&hidden(39)).find("w-before").is_none(), "before");
    no_start("w-intent");
    assert!(reaped(&hidden(40)).find("w-intent").is_some(), "intent");
}

#[test]
fn a_supervisor_that_died_mid_run_leaves_the_window_open_so_a_late_hidden_fork_holds() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    let at = |secs: i64| chrono::DateTime::from_timestamp(secs, 0).unwrap();
    let hidden = |start: u64| {
        let mut s = Scan::new(Probe::Gone, vec![]);
        s.hidden.push(HiddenProc {
            proc: ProcRef {
                pid: 4242,
                start_time: Some(start),
            },
            parent: Some(1),
        });
        ctx_with_fixed(dir.path(), clock.clone(), s)
    };
    // The last heartbeat was at second 60 and nobody saw the child exit: it
    // may have forked a hidden process long after.
    dead_child(dir.path(), &clock, "sd", |sc| sc.recorded_at = Some(at(60)));
    let late = hidden(5_000);
    assert!(reaped(&late).find("sd").is_some(), "held");
    let kept = managed::resolve_checked(&late, "sd").unwrap();
    assert!(
        !kept.released && kept.verdict.contains("ProbeUnknown"),
        "{kept:?}"
    );
    let forced = managed::resolve_checked_with(&late, "sd", true).unwrap();
    assert!(forced.released && forced.forced, "{forced:?}");
    // Still not before the child began.
    dead_child(dir.path(), &clock, "sd2", |sc| {
        sc.recorded_at = Some(at(60))
    });
    assert!(reaped(&hidden(0)).find("sd2").is_none(), "before the child");
}

fn idle_tracker(
    root: u32,
    record: task_traffic_controller::tracker::Record,
) -> task_traffic_controller::tracker::Tracker {
    task_traffic_controller::tracker::Tracker::idle(root, record)
}

#[test]
fn a_clean_run_closes_the_window_and_one_not_tracked_to_the_exit_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    let now = task_traffic_controller::Clock::now(&*clock);
    let hidden = |start: u64| {
        let mut s = Scan::new(Probe::Gone, vec![]);
        s.hidden.push(HiddenProc {
            proc: ProcRef {
                pid: 4242,
                start_time: Some(start),
            },
            parent: Some(1),
        });
        ctx_with_fixed(dir.path(), clock.clone(), s)
    };
    let cfg = Config::default();
    let c = hidden(0);
    // The real tracker followed the command to its exit and took the last
    // look.
    let s = spec(dir.path(), "nr", "d", &["true"]);
    let (_, guard) = registered(managed::register(&c, &cfg, &s).unwrap());
    admit_now(&c, &cfg, "nr");
    let ran = managed::run_command_with_tracker(&c, &guard, &s.opts, cfg.limits(), None).unwrap();
    assert!(!ran.outcome.tracker_unknown);
    let sc = sidecar(&c, "nr");
    assert!(sc.tracked_to_exit, "{sc:?}");
    // Stamped after the stop (which sleeps on the fake clock), not at `now`.
    assert!(sc.recorded_at > Some(now), "{sc:?}");
    assert!(sc.descendants.is_empty() && !sc.descendants_truncated);
    // A tracker that never looked, as if it died: the spawn still stamped
    // the sidecar, but nothing says the whole life was seen.
    let s2 = spec(dir.path(), "nr2", "d", &["true"]);
    let (_, guard2) = registered(managed::register(&c, &cfg, &s2).unwrap());
    admit_now(&c, &cfg, "nr2");
    let ran =
        managed::run_command_with_tracker(&c, &guard2, &s2.opts, cfg.limits(), Some(idle_tracker))
            .unwrap();
    assert!(!ran.outcome.tracker_unknown);
    let sc = sidecar(&c, "nr2");
    assert!(!sc.tracked_to_exit && sc.recorded_at.is_some(), "{sc:?}");
    // The window is relative to the stamp taken after the stop, whatever the
    // fake clock advanced meanwhile.
    let stamp = sidecar(&c, "nr").recorded_at.unwrap().timestamp() as u64;
    // The supervisor dies; everything is gone.
    drop(guard);
    drop(guard2);
    // Inside the window (the record plus 2 s): it may be the command's.
    assert!(reaped(&hidden(stamp + 2)).find("nr").is_some(), "inside");
    // After it: an unrelated process, so the entry is settled.
    assert!(reaped(&hidden(stamp + 3)).find("nr").is_none(), "after");
    // The one nobody followed to its exit is held by the same late process.
    assert!(reaped(&hidden(stamp + 3)).find("nr2").is_some(), "open");
}

/// A clock whose `now` moves 10 s on every read, so a stamp taken later is
/// always later.
struct Ticking(std::sync::Mutex<chrono::DateTime<chrono::Utc>>);

impl task_traffic_controller::Clock for Ticking {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        let mut t = self.0.lock().unwrap();
        *t += chrono::Duration::seconds(10);
        *t
    }
    fn monotonic(&self) -> Duration {
        Duration::ZERO
    }
    fn sleep(&self, _: Duration) {}
}

#[test]
fn the_window_closes_at_the_time_after_the_stop_not_at_the_spawn_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(Ticking(std::sync::Mutex::new(chrono::Utc::now())));
    let c = ctx(dir.path()).with_clock(clock);
    let cfg = Config::default();
    let s = spec(dir.path(), "tk", "d", &["true"]);
    let (_, guard) = registered(managed::register(&c, &cfg, &s).unwrap());
    admit_now(&c, &cfg, "tk");
    let ran = managed::run_command(&c, &guard, &s.opts, cfg.limits()).unwrap();
    let sc = sidecar(&c, "tk");
    assert!(sc.tracked_to_exit, "{sc:?}");
    let at = ran.outcome.tracked_to_exit_at.expect("closed");
    assert_eq!(sc.recorded_at, Some(at), "{sc:?}");
    // Later than the spawn and the run's start: it is the post-stop time.
    assert!(at > ran.started_at, "{at} vs {}", ran.started_at);
}

#[test]
fn a_run_that_left_descendants_does_not_close_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    let start = task_traffic_controller::Clock::now(&*clock).timestamp() as u64;
    let hidden = |start: u64, probe: Probe, alive: Vec<ProcRef>| {
        let mut s = Scan::new(probe, alive);
        s.hidden.push(HiddenProc {
            proc: ProcRef {
                pid: 4242,
                start_time: Some(start),
            },
            parent: Some(1),
        });
        ctx_with_fixed(dir.path(), clock.clone(), s)
    };
    let cfg = Config::default();
    // The marked scan keeps answering Alive: the stop never confirms.
    let stuck = ProcRef {
        pid: 0x3fff_fff0,
        start_time: Some(1),
    };
    let c = hidden(0, Probe::Alive, vec![stuck]);
    let s = spec(dir.path(), "dl", "d", &["true"]);
    let (_, guard) = registered(managed::register(&c, &cfg, &s).unwrap());
    admit_now(&c, &cfg, "dl");
    let ran = managed::run_command(&c, &guard, &s.opts, cfg.limits()).unwrap();
    assert!(ran.outcome.descendants_left);
    assert!(ran.outcome.tracked_to_exit_at.is_none());
    let sc = sidecar(&c, "dl");
    assert!(!sc.tracked_to_exit, "{sc:?}");
    drop(guard);
    // The supervisor died holding it; later a hidden process appears long
    // after the run: with the window open it may be the command's.
    let late = hidden(start + 3600, Probe::Gone, vec![]);
    assert!(reaped(&late).find("dl").is_some(), "held");
    // And it is held for the open window, not for some other reason.
    let kept = managed::resolve_checked(&late, "dl").unwrap();
    assert!(
        !kept.released && kept.verdict.contains("ProbeUnknown"),
        "{kept:?}"
    );
}

/// A ChildStarted entry whose child (this test process) is alive and whose
/// supervisor is gone, as of `ctx`'s boot.
fn orphan_with_live_child(ctx: &Context, id: &str) {
    let ghost = task_traffic_controller::process::ChildIdentity {
        pid: std::process::id(),
        start_time: task_traffic_controller::process::start_time(std::process::id()),
        pgid: Some(unsafe { libc::getpgid(0) }),
        sid: None,
    };
    orphan(ctx, id, Phase::ChildStarted, Some(ghost));
}

/// A started run whose supervisor died, then the machine rebooted; `settler`
/// is who notices first. Returns what a later registration of the id gets.
fn after_a_reboot(dir: &Path, id: &str, by_reaper: bool) -> (Box<managed::Sidecar>, usize) {
    let (ctx1, cfg) = (ctx(dir), Config::default());
    let s = spec(dir, id, "d", &["true"]);
    let (_, g) = registered(managed::register(&ctx1, &cfg, &s).unwrap());
    orphan_with_live_child(&ctx1, id);
    drop(g);
    let ctx2 = ctx_with_boot(dir, Some("boot-2"));
    if by_reaper {
        assert!(reaped(&ctx2).find(id).is_none());
    }
    // Registering again never runs it again: the outcome is the recorded one.
    match managed::register(&ctx2, &cfg, &s).unwrap() {
        Registration::AlreadyTerminal(sc) => (sc, ledger(&ctx2).entries.len()),
        Registration::Registered(..) => panic!("the run was registered afresh"),
    }
}

#[test]
fn the_outcome_after_a_reboot_is_the_same_whether_the_reaper_or_register_settles_first() {
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (by_register, n1) = after_a_reboot(d1.path(), "c", false);
    let (by_reaper, n2) = after_a_reboot(d2.path(), "c", true);
    assert_eq!((n1, n2), (0, 0));
    for sc in [&by_register, &by_reaper] {
        assert_eq!(sc.phase, Phase::Terminal);
        let t = sc.terminal.as_ref().unwrap();
        assert_eq!(t.kind, TerminalKind::Interrupted);
        assert!(t.detail.as_ref().unwrap().contains("BootChanged"), "{t:?}");
    }
    let detail = |sc: &managed::Sidecar| sc.terminal.as_ref().unwrap().detail.clone();
    assert_eq!(detail(&by_register), detail(&by_reaper));
}

#[test]
fn adopting_an_unlaunched_registration_after_a_reboot_refreshes_its_boot_id() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx1, cfg) = (ctx(dir.path()), Config::default());
    let s = spec(dir.path(), "c", "d", &["true"]);
    let (first, g) = registered(managed::register(&ctx1, &cfg, &s).unwrap());
    drop(g);
    let ctx2 = ctx_with_boot(dir.path(), Some("boot-2"));
    let (rc, _g) = registered(managed::register(&ctx2, &cfg, &s).unwrap());
    assert!(!rc.newly_registered);
    assert_eq!(rc.seq, first.seq);
    assert_eq!(rc.boot_id.as_deref(), Some("boot-2"));
    assert_eq!(sidecar(&ctx2, "c").boot_id.as_deref(), Some("boot-2"));
}

/// A live-metrics source that copies one file the moment it is asked: the
/// run asks once after the launch intent and before the spawn.
struct CopyOnSample {
    from: PathBuf,
    to: PathBuf,
}

impl task_traffic_controller::LiveProvider for CopyOnSample {
    fn snapshot(&self) -> Option<task_traffic_controller::admit::LiveSnapshot> {
        std::fs::copy(&self.from, &self.to).unwrap();
        None
    }
}

#[test]
fn the_current_boot_is_stamped_at_the_launch_and_at_the_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::default();
    let s = spec(dir.path(), "c", "d", &["true"]);
    let (_, guard) = registered(managed::register(&ctx(dir.path()), &cfg, &s).unwrap());
    // Registered on boot-1, launched on boot-2 (no re-registration).
    let ctx2 = ctx_with_boot(dir.path(), Some("boot-2"));
    admit_now(&ctx2, &cfg, "c");
    // The stamp is written with the intent, before anything is spawned: a
    // supervisor that dies right there must not leave boot-1 behind. The
    // run takes its one live sample after the intent and before the spawn,
    // so a source that copies the sidecar then sees it in between.
    let from = ctx2.paths.managed_file("c");
    let ctx2 = ctx2.with_live(Arc::new(CopyOnSample {
        from,
        to: dir.path().join("at-intent.json"),
    }));
    managed::run_command(&ctx2, &guard, &s.opts, cfg.limits()).unwrap();
    let at_intent: managed::Sidecar =
        serde_json::from_slice(&std::fs::read(dir.path().join("at-intent.json")).unwrap()).unwrap();
    assert_eq!(at_intent.phase, Phase::LaunchIntent, "{at_intent:?}");
    assert_eq!(
        at_intent.boot_id.as_deref(),
        Some("boot-2"),
        "{at_intent:?}"
    );
    let sc = sidecar(&ctx2, "c");
    assert_eq!(sc.phase, Phase::ChildStarted);
    assert_eq!(sc.boot_id.as_deref(), Some("boot-2"));
}

#[test]
fn resolve_settles_a_stranded_sidecar_with_no_ledger_entry() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let s = spec(dir.path(), "c", "d", &["true"]);
    let (_, g) = registered(managed::register(&ctx, &cfg, &s).unwrap());
    orphan(&ctx, "c", Phase::LaunchIntent, None);
    drop(g);
    // What an older binary's reaper did: the entry is gone, the sidecar is not.
    let mut l = ledger(&ctx);
    l.remove("c");
    l.save(&ctx.paths.ledger_file()).unwrap();
    assert!(managed::register(&ctx, &cfg, &s).is_err());
    let alive = ctx_with_scan(
        dir.path(),
        Arc::new(FakeClock::new(chrono::Utc::now())),
        Probe::Alive,
    );
    let kept = managed::resolve_checked(&alive, "c").unwrap();
    assert!(kept.found && !kept.released, "{kept:?}");
    let gone = ctx_with_scan(
        dir.path(),
        Arc::new(FakeClock::new(chrono::Utc::now())),
        Probe::Gone,
    );
    let done = managed::resolve_checked(&gone, "c").unwrap();
    assert!(done.found && done.released, "{done:?}");
    assert_eq!(sidecar(&gone, "c").phase, Phase::Terminal);
    // The id can be registered again; it reports the outcome that was
    // recorded, as for any settled id.
    assert!(matches!(
        managed::register(&gone, &cfg, &s).unwrap(),
        Registration::AlreadyTerminal(_)
    ));
    // A terminal sidecar with no entry is not stranded.
    let n = managed::resolve_checked(&gone, "c").unwrap();
    assert!(!n.found && n.verdict == "NoEntry");
}

#[test]
fn a_started_managed_run_of_this_state_dir_is_nested_without_its_lease() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let (_, g) =
        registered(managed::register(&ctx, &cfg, &spec(dir.path(), "c", "d", &["true"])).unwrap());
    // After the registration: the scope names the directory as it resolves
    // once it exists.
    let scope = task_traffic_controller::marker::scope(ctx.paths.state_dir());
    let nested = |lease: Option<&str>, id: Option<&str>, sc: Option<&str>| {
        exec::nested_run_id(&ctx, lease, id, sc).unwrap()
    };
    // Registered only: nothing started, so a caller is not inside it.
    assert_eq!(nested(None, Some("c"), Some(&scope)), None);
    orphan_with_live_child(&ctx, "c");
    drop(g); // the supervisor and its lease are gone
    assert_eq!(nested(None, Some("c"), Some(&scope)).as_deref(), Some("c"));
    assert_eq!(
        nested(Some("stale"), Some("c"), Some(&scope)).as_deref(),
        Some("c")
    );
    // Another state dir's id, a missing half, or another id: gated as usual.
    assert_eq!(nested(None, Some("c"), Some("0123456789abcdef")), None);
    assert_eq!(nested(None, Some("c"), None), None);
    assert_eq!(nested(None, None, Some(&scope)), None);
    assert_eq!(nested(None, Some("other"), Some(&scope)), None);
    // Settled by hand (no entry any more): not nested.
    managed::resolve(&ctx, "c", TerminalKind::Interrupted, "test").unwrap();
    assert_eq!(nested(None, Some("c"), Some(&scope)), None);
}

/// Run `task-traffic-controller exec -- true` against `lib`'s state directory with only the
/// given managed variables set.
fn cli_true(dir: &Path, lib: &Context, vars: Option<(&str, &str)>) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_task-traffic-controller"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"));
    if let Some((id, scope)) = vars {
        cmd.env("TASK_TRAFFIC_CONTROLLER_MANAGED_ID", id)
            .env("TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE", scope);
    }
    let out = cmd.args(["exec", "--", "true"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = lib;
    out
}

fn cli_state(dir: &Path) -> (Context, String) {
    let state = dir.join("state/task-traffic-controller");
    let lib = Context::new(Paths::new(&state, dir.join("none.toml")))
        .with_live(Arc::new(FixedLive(None)));
    // A roomy budget and no machine guards, so a gated run starts at once.
    let conf = dir.join("config/task-traffic-controller/config.toml");
    std::fs::create_dir_all(conf.parent().unwrap()).unwrap();
    std::fs::write(
        &conf,
        "budget = 64\ncpu_threshold_percent = 100\nmemory_reserve_mib = 0\n",
    )
    .unwrap();
    let cfg = Config::default();
    let (_, g) =
        registered(managed::register(&lib, &cfg, &spec(dir, "c", "d", &["true"])).unwrap());
    admit_now(&lib, &cfg, "c");
    drop(g);
    let scope = task_traffic_controller::marker::scope(lib.paths.state_dir());
    (lib, scope)
}

#[test]
fn the_cli_runs_a_command_ungated_inside_an_orphaned_managed_tree() {
    let dir = tempfile::tempdir().unwrap();
    let (lib, scope) = cli_state(dir.path());
    // The recorded child is this test process, the parent of the binary.
    orphan_with_live_child(&lib, "c");
    cli_true(dir.path(), &lib, Some(("c", &scope)));
    // Ungated: no entry of its own and no history record.
    assert_eq!(ledger(&lib).entries.len(), 1);
    assert!(!lib.paths.state_dir().join("history.jsonl").exists());
    // Control: the same command without the variables is gated, and leaves
    // a history record.
    cli_true(dir.path(), &lib, None);
    assert!(lib.paths.state_dir().join("history.jsonl").exists());
}

#[test]
fn a_process_that_only_inherited_the_managed_variables_is_still_gated() {
    use std::os::unix::process::CommandExt;
    let dir = tempfile::tempdir().unwrap();
    let (lib, scope) = cli_state(dir.path());
    // The run's recorded child is somebody else's process in its own group:
    // this test (and the binary it starts) is neither in that group nor
    // below it, as a daemon that inherited the environment would be.
    let mut other = Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    let ghost = task_traffic_controller::process::ChildIdentity {
        pid: other.id(),
        start_time: task_traffic_controller::process::start_time(other.id()),
        pgid: Some(other.id() as i32),
        sid: None,
    };
    orphan(&lib, "c", Phase::ChildStarted, Some(ghost));
    cli_true(dir.path(), &lib, Some(("c", &scope)));
    assert!(
        lib.paths.state_dir().join("history.jsonl").exists(),
        "the outsider skipped the gate"
    );
    other.kill().unwrap();
    other.wait().unwrap();
}

#[test]
fn a_settled_stranded_sidecar_does_not_block_its_id() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    let ctx = ctx_with_fixed(dir.path(), clock, Scan::new(Probe::Gone, vec![]));
    let cfg = Config::default();
    std::fs::create_dir_all(ctx.paths.managed_dir()).unwrap();
    // Nothing parseable, and nothing parseable that names a digest.
    std::fs::write(ctx.paths.managed_file("bad"), b"{").unwrap();
    // Parseable JSON that is not a sidecar, but still names its digest.
    std::fs::write(
        ctx.paths.managed_file("keep"),
        br#"{"spec_digest":"d","phase":7}"#,
    )
    .unwrap();
    for id in ["bad", "keep"] {
        let r = managed::resolve_checked(&ctx, id).unwrap();
        assert!(r.released, "{r:?}");
    }
    // The id answers with its finished outcome, not a conflict.
    let again = |id: &str, digest: &str| {
        managed::register(&ctx, &cfg, &spec(dir.path(), id, digest, &["true"]))
    };
    assert!(matches!(
        again("bad", "d").unwrap(),
        Registration::AlreadyTerminal(_)
    ));
    // The digest that could be read is kept: another one is still a conflict.
    assert!(matches!(
        again("keep", "d").unwrap(),
        Registration::AlreadyTerminal(_)
    ));
    let err = again("keep", "other").unwrap_err();
    assert!(err.to_string().contains("digest"), "{err}");
}

// ---- the reaper's scans stay out of the ledger lock ----

/// A scan that takes `delay` and counts how often it ran.
struct SlowScan {
    delay: Duration,
    probe: std::sync::Mutex<Probe>,
    calls: AtomicU32,
}

impl SlowScan {
    fn new(delay: Duration, probe: Probe) -> Arc<Self> {
        Arc::new(SlowScan {
            delay,
            probe: std::sync::Mutex::new(probe),
            calls: AtomicU32::new(0),
        })
    }
}

impl task_traffic_controller::ProcessScan for SlowScan {
    fn scan(&self, _id: &str, _scope: &str) -> Scan {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        Scan::new(*self.probe.lock().unwrap(), vec![])
    }
}

fn ctx_slow(dir: &Path, clock: Arc<FakeClock>, scan: Arc<SlowScan>) -> Context {
    ctx(dir).with_clock(clock).with_scan(scan)
}

#[test]
fn the_ledger_lock_is_not_held_while_the_reaper_scans() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    dead_child(dir.path(), &clock, "slow", |_| {});
    let scan = SlowScan::new(Duration::from_millis(250), Probe::Gone);
    let c = ctx_slow(dir.path(), clock.clone(), scan.clone());
    let worker = {
        let c = c.clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            lock::snapshot(&c).unwrap();
            t.elapsed()
        })
    };
    std::thread::sleep(Duration::from_millis(60));
    let t = Instant::now();
    drop(lock::ledger(&c).unwrap());
    let waited = t.elapsed();
    let scanned = worker.join().unwrap();
    assert!(
        scanned >= Duration::from_millis(400),
        "the scan did not run long enough to prove anything: {scanned:?}"
    );
    assert!(
        waited < Duration::from_millis(100),
        "the lock was held during the scan: {waited:?}"
    );
    // The answer was kept: the reap under the lock needs no scan of its own.
    let before = scan.calls.load(Ordering::SeqCst);
    let t = Instant::now();
    assert!(reaped(&c).find("slow").is_none());
    assert!(
        t.elapsed() < Duration::from_millis(200),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(scan.calls.load(Ordering::SeqCst), before);
}

#[test]
fn a_kept_entry_is_assessed_again_only_every_few_seconds_and_a_stale_keep_never_releases() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    dead_child(dir.path(), &clock, "held", |_| {});
    let scan = SlowScan::new(Duration::ZERO, Probe::Alive);
    let c = ctx_slow(dir.path(), clock.clone(), scan.clone());
    assert!(reaped(&c).find("held").is_some());
    let first = scan.calls.load(Ordering::SeqCst);
    assert!(first >= 1);
    // Polling at once, and just under the interval: no new scan.
    assert!(reaped(&c).find("held").is_some());
    clock.advance(Duration::from_secs(4));
    assert!(reaped(&c).find("held").is_some());
    assert_eq!(scan.calls.load(Ordering::SeqCst), first);
    // The process is gone now, but the cached answer is a Keep: it cannot
    // become a release without a fresh look.
    *scan.probe.lock().unwrap() = Probe::Gone;
    assert!(reaped(&c).find("held").is_some());
    assert_eq!(scan.calls.load(Ordering::SeqCst), first);
    clock.advance(Duration::from_secs(1));
    assert!(reaped(&c).find("held").is_none());
    assert!(scan.calls.load(Ordering::SeqCst) > first);
}

#[test]
fn a_release_answer_is_not_acted_on_once_the_sidecar_has_changed_or_it_is_old() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    dead_child(dir.path(), &clock, "chg", |_| {});
    let scan = SlowScan::new(Duration::ZERO, Probe::Gone);
    let c = ctx_slow(dir.path(), clock.clone(), scan.clone());
    lock::refresh_verdicts(&c);
    // The sidecar moved on after the assessment (a descendant was
    // recorded): the answer no longer describes the entry.
    let mut sc = sidecar(&c, "chg");
    sc.recorded_at = Some(chrono::Utc::now());
    managed::write_sidecar(&c, &sc).unwrap();
    *scan.probe.lock().unwrap() = Probe::Alive;
    assert!(reaped(&c).find("chg").is_some());
}

// ---- gc of managed receipts ----

fn terminal_sidecar(
    ctx: &Context,
    id: &str,
    ended: chrono::DateTime<chrono::Utc>,
    in_ledger: bool,
) -> Option<managed::OwnerGuard> {
    let (_, g) = registered(
        managed::register(
            ctx,
            &Config::default(),
            &spec(Path::new("/"), id, "d", &["true"]),
        )
        .unwrap(),
    );
    let mut sc = sidecar(ctx, id);
    sc.phase = Phase::Terminal;
    sc.terminal = Some(managed::TerminalRecord {
        kind: TerminalKind::Interrupted,
        at: ended,
        detail: None,
    });
    managed::write_sidecar(ctx, &sc).unwrap();
    if in_ledger {
        // The owner is still there, so nobody reaps the entry meanwhile.
        return Some(g);
    }
    drop(g);
    let mut l = ledger(ctx);
    l.remove(id);
    l.save(&ctx.paths.ledger_file()).unwrap();
    None
}

#[test]
fn gc_removes_old_terminal_receipts_and_stray_temp_files_only() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path());
    let now = chrono::Utc::now();
    let days = |n| chrono::Duration::days(n);
    terminal_sidecar(&ctx, "old", now - days(10), false);
    terminal_sidecar(&ctx, "recent", now - days(1), false);
    let _owner = terminal_sidecar(&ctx, "in-ledger", now - days(10), true);
    // Not terminal, however old: never removed.
    let (_, g) = registered(
        managed::register(
            &ctx,
            &Config::default(),
            &spec(dir.path(), "live", "d", &["true"]),
        )
        .unwrap(),
    );
    let mut sc = sidecar(&ctx, "live");
    sc.registered_at = now - days(30);
    managed::write_sidecar(&ctx, &sc).unwrap();
    let mut l = ledger(&ctx);
    l.remove("live");
    l.save(&ctx.paths.ledger_file()).unwrap();
    drop(g);
    let m = ctx.paths.managed_dir();
    std::fs::write(m.join("corrupt.json"), b"{").unwrap();
    for (name, age) in [(".tmpOLD", 10), (".tmpNEW", 0)] {
        let f = std::fs::File::create(m.join(name)).unwrap();
        f.set_modified(std::time::SystemTime::now() - Duration::from_secs(age * 86400 + 5))
            .unwrap();
    }
    assert_eq!(managed::gc_sidecars(&ctx, 7).unwrap(), 2);
    let mut left: Vec<String> = std::fs::read_dir(&m)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            ".tmpNEW",
            "corrupt.json",
            "in-ledger.json",
            "live.json",
            "recent.json"
        ]
    );
    assert_eq!(managed::gc_sidecars(&ctx, 7).unwrap(), 0);
    // Nothing to read at all is nothing to remove.
    let none = ctx_with_boot(tempfile::tempdir().unwrap().path(), None);
    assert_eq!(managed::gc_sidecars(&none, 7).unwrap(), 0);
}

// ---- --force and a reused process group ----

#[test]
fn force_frees_a_group_held_only_by_strangers_and_never_one_that_is_the_runs() {
    use std::os::unix::process::CommandExt;
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    // An unrelated process that leads a group of its own: the recorded
    // group number was reused by it.
    let mut stranger = Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = stranger.id() as i32;
    let stranger_ref = ProcRef {
        pid: stranger.id(),
        start_time: task_traffic_controller::process::start_time(stranger.id()),
    };
    let set_group = move |sc: &mut managed::Sidecar| {
        sc.child.as_mut().unwrap().pgid = Some(pgid);
    };
    let gone = ctx_with_fixed(dir.path(), clock.clone(), Scan::new(Probe::Gone, vec![]));
    let marked = ctx_with_fixed(
        dir.path(),
        clock.clone(),
        Scan::new(Probe::Alive, vec![stranger_ref]),
    );
    dead_child(dir.path(), &clock, "g1", set_group);
    dead_child(dir.path(), &clock, "g2", set_group);
    dead_child(dir.path(), &clock, "g3", move |sc| {
        set_group(sc);
        sc.descendants = vec![stranger_ref];
    });
    // Without --force the group keeps the entry.
    let r = managed::resolve_checked_with(&gone, "g1", false).unwrap();
    assert!(
        !r.released && r.verdict.contains("DescendantsAlive"),
        "{r:?}"
    );
    // A marked process: this run's, so never forced.
    let r = managed::resolve_checked_with(&marked, "g2", true).unwrap();
    assert!(!r.released && !r.forced, "{r:?}");
    // A recorded descendant that is alive: never forced.
    let r = managed::resolve_checked_with(&gone, "g3", true).unwrap();
    assert!(!r.released && !r.forced, "{r:?}");
    // Nothing marked, nothing recorded, the child gone: forced.
    let r = managed::resolve_checked_with(&gone, "g1", true).unwrap();
    assert!(r.released && r.forced, "{r:?}");
    assert!(r.checked.contains("different process group"), "{r:?}");
    assert!(ledger(&gone).find("g1").is_none());
    assert_eq!(sidecar(&gone, "g1").phase, Phase::Terminal);
    stranger.kill().unwrap();
    stranger.wait().unwrap();
}

// ---- the session a managed command leads ----

/// Runs only when re-executed by the tests below, as the supervisor. Its
/// command forks a helper that, two seconds later (after the supervisor and
/// the command are dead and nothing records it any more), starts a process
/// with an empty environment (`env -i`) that first moves itself to a new
/// process group (`SID_ESCAPE=group`) or a new session (`SID_ESCAPE=session`).
#[test]
fn session_supervisor_entry() {
    let Some(dir) = std::env::var_os("TASK_TRAFFIC_CONTROLLER_TEST_SID_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let (ctx, cfg) = (ctx(&dir), Config::default());
    // The helper is forked before the "forked" file appears, and the escaped
    // process writes its pid only after it has left the group or session.
    let script = "( sleep 2; perl -e 'if ($ENV{TASK_TRAFFIC_CONTROLLER_TEST_SID_ESCAPE} eq \"session\") { \
         require POSIX; POSIX::setsid(); } else { setpgrp(0, 0); } \
         open F, \">$ENV{TASK_TRAFFIC_CONTROLLER_TEST_SID_PIDFILE}\"; print F $$; close F; \
         exec q(env), q(-i), q(sleep), q(60)' & ) & : > \"$TASK_TRAFFIC_CONTROLLER_TEST_SID_FORKED\"; wait";
    let s = spec(&dir, "sid-1", "d", &["sh", "-c", script]);
    let (_, guard) = registered(managed::register(&ctx, &cfg, &s).unwrap());
    admit_now(&ctx, &cfg, "sid-1");
    let _ = managed::run_command(&ctx, &guard, &s.opts, cfg.limits());
}

/// Cleans up after an escape test, even when it fails: kills the supervisor
/// (while it is still ours), and the escaped process named by the pidfile
/// (waiting for the helper to write it once the helper was forked).
struct Reap {
    sup: Option<std::process::Child>,
    forked: PathBuf,
    pidfile: PathBuf,
}

impl Reap {
    fn pid(&self) -> Option<i32> {
        std::fs::read_to_string(&self.pidfile)
            .ok()?
            .trim()
            .parse()
            .ok()
    }
}

impl Drop for Reap {
    fn drop(&mut self) {
        if let Some(mut sup) = self.sup.take() {
            let _ = sup.kill();
            let _ = sup.wait();
        }
        if self.forked.exists() {
            until(6, || self.pid().is_some());
        }
        if let Some(pid) = self.pid().filter(|p| *p > 0) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// The command name of `pid`, if it runs.
fn comm_of(pid: i32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// The state once the supervisor and the command are dead and the escaped
/// `env -i sleep` (a process of the given kind of escape) runs.
fn orphan_after_escape(escape: &str) -> (tempfile::TempDir, Context, Reap) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path());
    let pidfile = dir.path().join("escaped.pid");
    let forked = dir.path().join("forked");
    let sup = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "session_supervisor_entry",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("TASK_TRAFFIC_CONTROLLER_TEST_SID_DIR", dir.path())
        .env("TASK_TRAFFIC_CONTROLLER_TEST_SID_PIDFILE", &pidfile)
        .env("TASK_TRAFFIC_CONTROLLER_TEST_SID_FORKED", &forked)
        .env("TASK_TRAFFIC_CONTROLLER_TEST_SID_ESCAPE", escape)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut guard = Reap {
        sup: Some(sup),
        forked: forked.clone(),
        pidfile: pidfile.clone(),
    };
    assert!(
        until(
            20,
            || matches!(managed::load_sidecar(&ctx, "sid-1"), Loaded::Ok(sc) if sc.phase == Phase::ChildStarted)
        ),
        "the supervisor never started its child"
    );
    let child = sidecar(&ctx, "sid-1").child.unwrap();
    // The child leads its own session: its id is the child's pid.
    assert_eq!(child.sid, Some(child.pid as i32));
    assert_eq!(child.pgid, child.sid);
    // Only once the command has forked its helper (it writes the file after
    // the fork) may the command and the supervisor be killed.
    assert!(until(20, || forked.exists()), "the helper was never forked");
    let mut sup = guard.sup.take().unwrap();
    unsafe { libc::kill(sup.id() as libc::pid_t, libc::SIGKILL) };
    sup.wait().unwrap();
    unsafe { libc::kill(child.pid as libc::pid_t, libc::SIGKILL) };
    assert!(
        until(20, || guard.pid().is_some()),
        "the escaped process never started"
    );
    let pid = guard.pid().unwrap();
    // Once it runs as `sleep` it has exec'd, with its environment cleared.
    assert!(
        until(20, || pid_alive(pid)
            && comm_of(pid).is_some_and(|c| c.ends_with("sleep"))),
        "the escaped process never became `sleep`"
    );
    (dir, ctx, guard)
}

#[test]
fn an_env_i_process_that_left_the_group_keeps_the_run_held_through_the_reaper_and_force() {
    let (_dir, ctx, escaped) = orphan_after_escape("group");
    // No marker, not recorded, not in the group: only the session shows it.
    for _ in 0..3 {
        assert!(reaped(&ctx).find("sid-1").is_some(), "freed while it runs");
        std::thread::sleep(Duration::from_millis(100));
    }
    let v = managed::views(&ctx, &ledger(&ctx).entries);
    assert!(v[0].reason.contains("DescendantsAlive"), "{:?}", v[0]);
    let r = managed::resolve_checked_with(&ctx, "sid-1", true).unwrap();
    assert!(!r.released && !r.forced, "{r:?}");
    assert!(ledger(&ctx).find("sid-1").is_some());
    // Once it is gone nothing marks, groups or records the run any more.
    // (A Mac may still report "could not be checked" for unrelated hidden
    // processes; `--force` is the way out, and it must now succeed.)
    drop(escaped);
    assert!(
        until(20, || {
            managed::resolve_checked_with(&ctx, "sid-1", true).is_ok_and(|r| r.released)
        }),
        "not released after the process stopped: {:?}",
        managed::views(&ctx, &ledger(&ctx).entries)
    );
}

#[test]
fn a_process_that_left_with_setsid_is_not_a_member() {
    // The documented limitation: a daemon that starts its own session is
    // outside the run's. With an empty environment nothing else shows it
    // either, so `--force` releases the entry (the reaper alone does on a
    // system that can read every environment).
    let (_dir, ctx, _escaped) = orphan_after_escape("session");
    let r = managed::resolve_checked_with(&ctx, "sid-1", true).unwrap();
    assert!(r.released, "{r:?}");
    assert!(ledger(&ctx).find("sid-1").is_none());
}

/// A child identity for a run that is gone, whose session number is `sid`.
fn gone_run_with_session(dir: &Path, id: &str, sid: i32, start: u64) -> Context {
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    dead_child(dir, &clock, id, |sc| {
        let c = sc.child.as_mut().unwrap();
        c.sid = Some(sid);
        c.start_time = Some(start);
    });
    ctx_with_fixed(dir, clock, Scan::new(Probe::Gone, vec![]))
}

#[test]
fn a_reused_session_number_is_not_a_false_match() {
    use std::os::unix::process::CommandExt;
    let dir = tempfile::tempdir().unwrap();
    // Somebody else leads a session with that number now.
    let mut other = unsafe {
        Command::new("sleep")
            .arg("60")
            .pre_exec(|| {
                libc::setsid();
                Ok(())
            })
            .spawn()
            .unwrap()
    };
    let sid = other.id() as i32;
    let c = gone_run_with_session(dir.path(), "reuse", sid, 1);
    let v = managed::resolve_checked(&c, "reuse").unwrap();
    assert!(v.released, "a different session held the run: {v:?}");

    // The same leader, recorded with its true start time, is a member.
    let dir2 = tempfile::tempdir().unwrap();
    let start = task_traffic_controller::process::start_time(other.id()).unwrap();
    let c = gone_run_with_session(dir2.path(), "same", sid, start);
    let held = managed::resolve_checked_with(&c, "same", true).unwrap();
    assert!(!held.released && !held.forced, "{held:?}");
    other.kill().unwrap();
    other.wait().unwrap();
}

#[test]
fn a_sidecar_without_a_session_keeps_the_old_rules() {
    let json = r#"{"pid": 7, "start_time": 3, "pgid": 7}"#;
    let c: task_traffic_controller::process::ChildIdentity = serde_json::from_str(json).unwrap();
    assert_eq!(c.sid, None);
    assert_eq!(
        task_traffic_controller::process::probe_session(&c),
        Probe::Gone
    );
    assert!(!serde_json::to_string(&c).unwrap().contains("sid"));
}

/// A process scan whose session answer is scripted and which counts how many
/// times the session was asked about. Nothing else is found.
struct SessionAnswer {
    probe: Probe,
    asked: AtomicU32,
}

impl SessionAnswer {
    fn new(probe: Probe) -> Arc<Self> {
        Arc::new(SessionAnswer {
            probe,
            asked: AtomicU32::new(0),
        })
    }
}

impl task_traffic_controller::ProcessScan for SessionAnswer {
    fn scan(&self, _id: &str, _scope: &str) -> Scan {
        Scan::new(Probe::Gone, vec![])
    }
    fn probe_session(&self, _c: &task_traffic_controller::process::ChildIdentity) -> Probe {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.probe
    }
}

#[test]
fn finish_holds_the_entry_unless_the_session_is_confirmed_gone() {
    // `Unknown` is what a failing getsid (EPERM on some process) gives.
    for answer in [Probe::Unknown, Probe::Alive] {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, cfg) = (ctx(dir.path()), Config::default());
        let s = spec(dir.path(), "held", "d", &["true"]);
        let (ran, guard) = run_to_end(&ctx, &cfg, &s);
        let scripted = SessionAnswer::new(answer);
        let held_ctx = ctx.clone().with_scan(scripted.clone());
        let err = managed::finish(&held_ctx, guard, ran.outcome.exit.into()).unwrap_err();
        assert!(err.to_string().contains("session"), "{answer:?}: {err}");
        assert!(ledger(&ctx).find("held").is_some(), "{answer:?}: released");
        assert_eq!(sidecar(&ctx, "held").phase, Phase::ChildStarted);
        assert!(scripted.asked.load(Ordering::SeqCst) > 0);
    }
    // Confirmed gone: released.
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let s = spec(dir.path(), "free", "d", &["true"]);
    let (ran, guard) = run_to_end(&ctx, &cfg, &s);
    let gone_ctx = ctx.clone().with_scan(SessionAnswer::new(Probe::Gone));
    managed::finish(&gone_ctx, guard, ran.outcome.exit.into()).unwrap();
    assert!(ledger(&ctx).find("free").is_none());
}

#[test]
fn the_session_is_looked_at_only_when_scanning() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    dead_child(dir.path(), &clock, "sc", |_| {});
    let counting = SessionAnswer::new(Probe::Gone);
    let c = ctx(dir.path())
        .with_clock(clock)
        .with_scan(counting.clone());
    // Under the ledger lock (no scan): the session is not asked about.
    {
        let _lock = lock::try_ledger(&c).unwrap().expect("ledger lock");
        let mut l = ledger(&c);
        lock::reap_dead(&c, &mut l);
        assert_eq!(counting.asked.load(Ordering::SeqCst), 0);
    }
    // A full assessment (scan) does ask.
    let a = managed::assess(&c, "sc");
    assert!(counting.asked.load(Ordering::SeqCst) > 0, "{a:?}");
}

#[test]
fn the_session_is_recorded_even_for_a_child_that_exits_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    for i in 0..5 {
        let id = format!("quick-{i}");
        let s = spec(dir.path(), &id, "d", &["true"]);
        let (_, guard) = registered(managed::register(&ctx, &cfg, &s).unwrap());
        admit_now(&ctx, &cfg, &id);
        let ran = managed::run_command(&ctx, &guard, &s.opts, cfg.limits()).unwrap();
        let child = sidecar(&ctx, &id).child.unwrap();
        assert_eq!(child.sid, Some(child.pid as i32), "{id}");
        assert_eq!(child.pgid, Some(child.pid as i32), "{id}");
        managed::finish(&ctx, guard, ran.outcome.exit.into()).unwrap();
    }
}

/// Registers, admits and runs `opts.argv` as a managed command and returns
/// its exit code.
fn run_managed_opts(dir: &Path, id: &str, opts: Opts) -> i32 {
    let (ctx, cfg) = (ctx(dir), Config::default());
    let mut s = spec(dir, id, "d", &["true"]);
    s.opts = opts;
    let (_, guard) = registered(managed::register(&ctx, &cfg, &s).unwrap());
    admit_now(&ctx, &cfg, id);
    let ran = managed::run_command(&ctx, &guard, &s.opts, cfg.limits()).unwrap();
    let code = ran.outcome.exit.code();
    managed::finish(&ctx, guard, ran.outcome.exit.into()).unwrap();
    code
}

#[test]
fn a_given_env_replaces_ours_and_keeps_the_reserved_variables() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("out.log");
    let mut o = opts(
        dir.path(),
        &[
            "/bin/sh",
            "-c",
            "echo \"home=${HOME-unset} x=$X lease=$TASK_TRAFFIC_CONTROLLER_LEASE \
             id=$TASK_TRAFFIC_CONTROLLER_MANAGED_ID scope=${TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE:+set}\"; \
             echo to-stderr >&2",
        ],
    );
    // Forged reserved variables must lose to the real ones.
    o.env = Some(
        [
            ("X", "1"),
            ("TASK_TRAFFIC_CONTROLLER_LEASE", "forged"),
            ("TASK_TRAFFIC_CONTROLLER_MANAGED_ID", "forged"),
            ("TASK_TRAFFIC_CONTROLLER_MANAGED_SCOPE", "forged"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .into(),
    );
    o.stdout_log = Some(log.clone());
    assert_eq!(run_managed_opts(dir.path(), "env-1", o), 0);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("x=1"), "{text}");
    // Our own HOME is set; the child must not see it.
    assert!(std::env::var_os("HOME").is_some());
    assert!(text.contains("home=unset"), "parent env leaked: {text}");
    assert!(text.contains("lease=env-1"), "{text}");
    assert!(text.contains("id=env-1"), "{text}");
    assert!(text.contains("scope=set"), "{text}");
    assert!(!text.contains("forged"), "{text}");
    assert!(text.contains("to-stderr"), "stderr goes to the log: {text}");
}

#[test]
fn stdout_log_is_appended_to_and_stdin_is_null() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("a.log");
    std::fs::write(&log, "before\n").unwrap();
    let mk = |script: &str| {
        let mut o = opts(dir.path(), &["/bin/sh", "-c", script]);
        o.stdout_log = Some(log.clone());
        o
    };
    // `cat` returns at once only when stdin is at end of file.
    assert_eq!(run_managed_opts(dir.path(), "l-1", mk("echo one; cat")), 0);
    assert_eq!(run_managed_opts(dir.path(), "l-2", mk("echo two")), 0);
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "before\none\ntwo\n");
}

#[test]
fn without_env_the_child_inherits_ours() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts(dir.path(), &["/bin/sh", "-c", "test -n \"$PATH\""]);
    assert!(o.env.is_none() && o.stdout_log.is_none());
    assert_eq!(run_managed_opts(dir.path(), "inh-1", o), 0);
}

#[test]
fn env_and_log_do_not_change_the_history_key() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path());
    let plain = opts(dir.path(), &["tool", "--flag"]);
    let mut other = plain.clone();
    other.env = Some([("X".to_string(), "1".to_string())].into());
    other.stdout_log = Some(dir.path().join("x.log"));
    let key = |o: &Opts| exec::plan(&ctx, &Config::default(), o).unwrap().key;
    assert_eq!(key(&plain).hash, key(&other).hash);
}
