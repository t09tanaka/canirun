//! Managed commands: registration, receipts, the reaper's rules and the
//! measurement idempotence. Every test runs in its own temp state dir with
//! an explicit `Context`; nothing touches the real user state.

use canirun::admit::State;
use canirun::config::Config;
use canirun::exec::{self, Gated, Hooks, Opts};
use canirun::history::{self, Record};
use canirun::ledger::Ledger;
use canirun::managed::{self, CommandSpec, Loaded, Measured, Registration, TerminalKind, TryAdmit};
use canirun::marker::{HiddenProc, ProcRef, Scan};
use canirun::reap::{Keep, Probe, Verdict};
use canirun::receipt::Phase;
use canirun::{lock, Context, FakeClock, FixedBoot, FixedLive, FixedScan, Paths, Supervisor};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
        Err(canirun::error::Error::AlreadyOwned(_))
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
        Err(canirun::error::Error::Conflict(_))
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
fn orphan(ctx: &Context, id: &str, phase: Phase, child: Option<canirun::process::ChildIdentity>) {
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
        Err(canirun::error::Error::Conflict(_))
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
    let ghost = canirun::process::ChildIdentity {
        pid: std::process::id(), // alive, would be "kept" on the same boot
        start_time: canirun::process::start_time(std::process::id()),
        pgid: Some(unsafe { libc::getpgid(0) }),
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

#[test]
fn an_older_binary_rewriting_the_ledger_leaves_the_sidecar_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let (_, _g) =
        registered(managed::register(&ctx, &cfg, &spec(dir.path(), "c", "d", &["true"])).unwrap());
    // The ledger is plain v1: no key an older binary would not know.
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(ctx.paths.ledger_file()).unwrap()).unwrap();
    assert_eq!(raw["version"], 1);
    let known = [
        "id",
        "pid",
        "state",
        "front",
        "weight",
        "predicted_rss",
        "expected_secs",
        "repo",
        "subdir",
        "argv",
        "key",
        "seq",
        "enqueued_at",
        "started_at",
        "using",
        "backfilled",
    ];
    for k in raw["entries"][0].as_object().unwrap().keys() {
        assert!(known.contains(&k.as_str()), "new ledger key {k}");
    }
    // v0.4.2 saving: parse, add an entry it wrote itself, serialise.
    let mut l = ledger(&ctx);
    let mut old = l.entries[0].clone();
    old.id = "plain".into();
    old.seq = l.take_seq();
    l.entries.push(old);
    l.save(&ctx.paths.ledger_file()).unwrap();
    assert_eq!(sidecar(&ctx, "c").spec_digest, "d");
    assert!(ledger(&ctx).find("c").is_some());
    // The plain entry (no sidecar, no lease) is reaped as ever.
    assert!(reaped(&ctx).find("plain").is_none());
    assert!(reaped(&ctx).find("c").is_some());
}

#[test]
fn inspect_reports_a_quarantined_entry_and_still_counts_it() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
    let (_, g) =
        registered(managed::register(&ctx, &cfg, &spec(dir.path(), "c", "d", &["true"])).unwrap());
    orphan(&ctx, "c", Phase::LaunchIntent, None);
    drop(g);
    let insp = canirun::inspect::inspect_with(&ctx, &cfg).unwrap();
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
    let plan =
        canirun::estimate::command_plan(&ctx, &cfg, &opts(dir.path(), &["fixture-tool"])).unwrap();
    let insp = canirun::inspect::inspect_with(&ctx, &cfg).unwrap();
    assert!(insp.entries[0].suspect_dead);
    let f = canirun::forecast::forecast(&ctx, &insp, &plan, &[]).unwrap();
    // The only slot is held by the quarantined entry: no start is promised.
    assert_eq!(f.target.timing.start_in_ms, None);
    assert!(ledger(&ctx).find("q").is_some());
}

/// Runs only when re-executed by the SIGKILL test below, as the supervisor.
#[test]
fn supervisor_entry() {
    let Some(dir) = std::env::var_os("CANIRUN_TEST_SUPERVISOR_DIR") else {
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
    let (ctx, _cfg) = (ctx(dir.path()), Config::default());
    let mut sup = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "supervisor_entry",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("CANIRUN_TEST_SUPERVISOR_DIR", dir.path())
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
    let state = dir.path().join("state/canirun");
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
        let o = Command::new(env!("CARGO_BIN_EXE_canirun"))
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
    assert_eq!(insp["managed"][0]["hint"], "canirun resolve c");
    let text = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_canirun"))
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
    assert!(shown.contains("canirun resolve c"), "{shown}");
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
) -> (canirun::managed::Ran, managed::OwnerGuard) {
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
    let (ctx, cfg) = (ctx(dir.path()), Config::default());
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
    assert_eq!(v[0].hint, "canirun resolve l");
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
        .contains("canirun resolve"));
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
    let script = format!("printf %s \"$CANIRUN_MANAGED_ID\" > {}", out.display());
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
    let Ok(role) = std::env::var("CANIRUN_TEST_ROLE") else {
        return;
    };
    let pidfile = std::env::var_os("CANIRUN_TEST_PIDFILE");
    if role == "escape" {
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.args(["--exact", "helper_entry", "--nocapture"])
            .env("CANIRUN_TEST_ROLE", "sleep")
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
        if std::env::var_os("CANIRUN_TEST_IGNORE_TERM").is_some() {
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
    let mut v = vec!["env".to_string(), format!("CANIRUN_TEST_ROLE={role}")];
    v.push(format!("CANIRUN_TEST_PIDFILE={}", pidfile.display()));
    if ignore_term {
        v.push("CANIRUN_TEST_IGNORE_TERM=1".into());
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
        .env("CANIRUN_TEST_ROLE", "sleep")
        .env(canirun::marker::ENV_VAR, "shared-id")
        .env(
            canirun::marker::SCOPE_VAR,
            canirun::marker::scope(other.path()),
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
fn a_reaper_keeps_the_entry_while_an_escaped_descendant_runs() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path());
    let cfg = Config::default();
    let s = spec(dir.path(), "esc-held", "d", &["true"]);
    let (ran, guard) = run_to_end(&ctx, &cfg, &s);
    // A stand-in survivor outside the group, marked like the real thing.
    let mut survivor = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "helper_entry", "--nocapture"])
        .env("CANIRUN_TEST_ROLE", "sleep")
        .env(canirun::marker::ENV_VAR, "esc-held")
        .env(
            canirun::marker::SCOPE_VAR,
            canirun::marker::scope(ctx.paths.state_dir()),
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

#[test]
fn gc_keeps_only_the_newest_applied_markers() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path());
    let m = ctx.paths.measurements_dir();
    std::fs::create_dir_all(&m).unwrap();
    let base = std::time::SystemTime::now() - Duration::from_secs(1000);
    for i in 0..5u64 {
        let f = std::fs::File::create(m.join(format!("m{i}.applied"))).unwrap();
        f.set_modified(base + Duration::from_secs(i * 10)).unwrap();
    }
    std::fs::write(m.join("p.pending"), b"").unwrap();
    assert_eq!(managed::gc_measurements(&ctx, 3).unwrap(), 2);
    let mut left: Vec<String> = std::fs::read_dir(&m)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(
        left,
        ["m2.applied", "m3.applied", "m4.applied", "p.pending"]
    );
    assert_eq!(managed::gc_measurements(&ctx, 3).unwrap(), 0);
    assert_eq!(managed::gc_measurements(&ctx, 0).unwrap(), 3);
    let none = ctx_with_boot(tempfile::tempdir().unwrap().path(), None);
    assert_eq!(managed::gc_measurements(&none, 3).unwrap(), 0);
}

// ---- descendants recorded while the command runs, and `resolve --force` ----

/// Runs only when re-executed by the test below, as the supervisor of a
/// command whose child starts a descendant in a session of its own (so it
/// leaves the process group) and writes its pid to a file. On a Mac with
/// Apple's perl and sleep their environment is hidden from other processes,
/// so only the recorded descendants can find it there.
#[test]
fn setsid_supervisor_entry() {
    let Some(dir) = std::env::var_os("CANIRUN_TEST_SETSID_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let (ctx, cfg) = (ctx(&dir), Config::default());
    // The descendant drops the marker variables before it execs, so only the
    // recorded descendants (not the environment marker) can find it.
    let script = "use POSIX qw(setsid); my $p = fork(); if (!$p) { setsid(); \
        open(F, '>', $ARGV[0] . '.tmp'); print F $$; close F; \
        rename($ARGV[0] . '.tmp', $ARGV[0]); \
        delete $ENV{CANIRUN_MANAGED_ID}; delete $ENV{CANIRUN_MANAGED_SCOPE}; \
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

#[test]
fn a_descendant_that_left_the_group_is_recorded_and_keeps_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path());
    let sup = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "setsid_supervisor_entry",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("CANIRUN_TEST_SETSID_DIR", dir.path())
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
        "not released after the descendant stopped"
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
    sc.child = Some(canirun::process::ChildIdentity {
        pid: 0x3fff_fff0,
        start_time: Some(1),
        pgid: Some(0x3fff_fff0),
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
        start_time: canirun::process::start_time(live.id()),
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
fn failing_tracker(root: u32, record: canirun::tracker::Record) -> canirun::tracker::Tracker {
    canirun::tracker::Tracker::failed(root, record)
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

fn idle_tracker(root: u32, record: canirun::tracker::Record) -> canirun::tracker::Tracker {
    canirun::tracker::Tracker::idle(root, record)
}

#[test]
fn a_clean_run_closes_the_window_and_one_not_tracked_to_the_exit_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(chrono::Utc::now()));
    let now = canirun::Clock::now(&*clock);
    let start = now.timestamp() as u64;
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
    // The supervisor dies; everything is gone.
    drop(guard);
    drop(guard2);
    // Inside the window (the record plus 2 s): it may be the command's.
    assert!(reaped(&hidden(start + 2)).find("nr").is_some(), "inside");
    // After it: an unrelated process, so the entry is settled.
    assert!(reaped(&hidden(start + 3)).find("nr").is_none(), "after");
    // The one nobody followed to its exit is held by the same late process.
    assert!(reaped(&hidden(start + 3)).find("nr2").is_some(), "open");
}

/// A clock whose `now` moves 10 s on every read, so a stamp taken later is
/// always later.
struct Ticking(std::sync::Mutex<chrono::DateTime<chrono::Utc>>);

impl canirun::Clock for Ticking {
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
    let start = canirun::Clock::now(&*clock).timestamp() as u64;
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
}
