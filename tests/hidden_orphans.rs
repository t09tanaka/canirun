//! A managed command whose supervisor was killed, and the hidden processes
//! that are around when it is settled: an unrelated orphan of an older
//! session must not hold the entry, a real descendant that left the session
//! must (see the `lineage` module documentation).
//!
//! Every process here is started by the test, ends by itself within a
//! minute, and is only ever signalled through its own `Child` handle or
//! through `process::signal_if_same` (pid and start time checked).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use task_traffic_controller::config::Config;
use task_traffic_controller::exec::Opts;
use task_traffic_controller::managed::{self, CommandSpec, Loaded, Registration, TryAdmit};
use task_traffic_controller::marker::{ProcRef, Scan};
use task_traffic_controller::process;
use task_traffic_controller::reap::Probe;
use task_traffic_controller::receipt::Phase;
use task_traffic_controller::{Context, FixedBoot, FixedLive, Paths, ProcessScan};

const DIR: &str = "TASK_TRAFFIC_CONTROLLER_TEST_HO_DIR";
const KIND: &str = "TASK_TRAFFIC_CONTROLLER_TEST_HO_KIND";
const ROLE: &str = "TASK_TRAFFIC_CONTROLLER_TEST_HO_ROLE";
const ID: &str = "ho-1";

fn ctx(dir: &Path) -> Context {
    Context::new(Paths::new(dir.join("state"), dir.join("config.toml")))
        .with_live(Arc::new(FixedLive(None)))
        .with_boot(Arc::new(FixedBoot(Some("boot-1".into()))))
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

fn go_file(dir: &Path) -> PathBuf {
    dir.join("go")
}

fn pid_file(dir: &Path) -> PathBuf {
    dir.join("orphan.pid")
}

/// The real scan, but of the hidden processes only the ones the test
/// started: whatever else the machine runs meanwhile is not what is tested.
#[derive(Default)]
struct OnlyOurHidden(Mutex<Vec<u32>>);

impl ProcessScan for OnlyOurHidden {
    fn scan(&self, id: &str, scope: &str) -> Scan {
        let mut s = process::scan_system(id, scope);
        let ours = self.0.lock().unwrap();
        s.hidden.retain(|h| ours.contains(&h.proc.pid));
        s
    }
}

/// Stops what the test started, whatever way the test ends.
#[derive(Default)]
struct Started {
    children: Vec<Child>,
    orphans: Vec<ProcRef>,
}

impl Started {
    fn stop_orphans(&mut self) {
        for p in self.orphans.drain(..) {
            // Our own orphan, recognised by pid and start time; never pid
            // 0 or 1, never a number that is not a pid.
            if p.pid > 1 && i32::try_from(p.pid).is_ok() && p.start_time.is_some() {
                process::signal_if_same(&p, libc::SIGKILL);
            }
        }
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        self.stop_orphans();
        for c in &mut self.children {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// The supervisor: registers and runs the managed command, and is killed
/// by the test while the command runs. Does nothing in a normal test run.
#[test]
fn ho_supervisor_entry() {
    let (Some(dir), Ok(kind)) = (std::env::var_os(DIR), std::env::var(KIND)) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let (ctx, cfg) = (ctx(&dir), Config::default());
    let argv: Vec<String> = if kind == "escape" {
        vec![
            std::env::current_exe().unwrap().display().to_string(),
            "--exact".into(),
            "ho_helper_entry".into(),
            "--nocapture".into(),
        ]
    } else {
        vec!["/bin/sleep".into(), "3".into()]
    };
    let spec = CommandSpec {
        command_id: ID.into(),
        spec_digest: "d".into(),
        opts: Opts {
            weight: Some(1),
            key: Some("hidden-orphans".into()),
            argv,
            cwd: dir.clone(),
            ..Opts::default()
        },
        attempt: None,
        services: vec![],
    };
    let Registration::Registered(_, guard) = managed::register(&ctx, &cfg, &spec).unwrap() else {
        panic!("already terminal");
    };
    let mut admitted = false;
    for _ in 0..200 {
        match managed::try_admit(&ctx, cfg.limits(), ID, 0.0).unwrap() {
            TryAdmit::Admitted => {
                admitted = true;
                break;
            }
            TryAdmit::LockBusy | TryAdmit::Wait(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    assert!(admitted, "never admitted");
    let _ = managed::run_command(&ctx, &guard, &spec.opts, cfg.limits());
}

/// Wait (bounded) for the go file.
fn wait_for_go(dir: &Path) -> bool {
    until(30, || go_file(dir).exists())
}

/// The helpers. Does nothing in a normal test run.
///
/// `escape` is the managed command itself: on the go file it starts
/// `/bin/sleep` in a session of its own and leaves, as a daemon would.
/// `bystander` is unrelated to the command: it leads a session of its own,
/// and on the go file has a shell start `/bin/sleep` in the background and
/// exit, which orphans the sleep inside the bystander's session.
#[test]
fn ho_helper_entry() {
    let (Some(dir), Ok(role)) = (std::env::var_os(DIR), std::env::var(ROLE)) else {
        return;
    };
    let dir = PathBuf::from(dir);
    if !wait_for_go(&dir) {
        return;
    }
    if role == "escape" {
        let mut c = Command::new("/bin/sleep");
        c.arg("40")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut c, || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // Not waited for: this process leaves, which orphans it.
        let pid = c.spawn().unwrap().id();
        let tmp = dir.join("orphan.pid.tmp");
        std::fs::write(&tmp, pid.to_string()).unwrap();
        std::fs::rename(&tmp, pid_file(&dir)).unwrap();
    } else {
        let _ = Command::new("/bin/sh")
            .args([
                "-c",
                "/bin/sleep 40 </dev/null >/dev/null 2>&1 & echo $! > \"$0.tmp\" && mv \"$0.tmp\" \"$0\"",
            ])
            .arg(pid_file(&dir))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // Stay: the session keeps its leader.
        std::thread::sleep(Duration::from_secs(50));
    }
}

fn spawn_entry(dir: &Path, entry: &str, envs: &[(&str, &str)], own_session: bool) -> Child {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args(["--exact", entry, "--nocapture", "--test-threads=1"])
        .env(DIR, dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in envs {
        c.env(k, v);
    }
    if own_session {
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut c, || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    c.spawn().unwrap()
}

/// Start the supervisor, wait for its child, and kill the supervisor.
/// Returns the recorded child.
fn run_and_kill_supervisor(
    ctx: &Context,
    dir: &Path,
    kind: &str,
    started: &mut Started,
) -> process::ChildIdentity {
    let envs: &[(&str, &str)] = &[(KIND, kind), (ROLE, kind)];
    started
        .children
        .push(spawn_entry(dir, "ho_supervisor_entry", envs, false));
    let mut child = None;
    let seen = until(30, || match managed::load_sidecar(ctx, ID) {
        Loaded::Ok(sc) if sc.phase == Phase::ChildStarted => {
            child = sc.child.clone();
            true
        }
        _ => false,
    });
    assert!(seen, "the supervisor never started its child");
    let sup = started.children.last_mut().unwrap();
    sup.kill().unwrap();
    sup.wait().unwrap();
    child.expect("a started child is recorded")
}

/// The orphan the helper started, once its pid file is there: checked to be
/// a live process started at or after `not_before`.
fn orphan(dir: &Path, not_before: u64) -> ProcRef {
    let mut found = None;
    let seen = until(30, || {
        let pid = std::fs::read_to_string(pid_file(dir))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        found = pid.and_then(|pid| {
            process::start_time(pid).map(|t| ProcRef {
                pid,
                start_time: Some(t),
            })
        });
        found.is_some()
    });
    assert!(seen, "the helper never started its orphan");
    let p = found.unwrap();
    assert!(p.pid > 1 && p.start_time.unwrap() >= not_before, "{p:?}");
    p
}

/// On macOS the orphan must be what the rule is about: hidden, and with
/// `launchd` as its parent. (Elsewhere every environment can be read, so
/// nothing is hidden.)
fn assert_hidden_orphan(_ctx: &Context, _p: &ProcRef) {
    #[cfg(target_os = "macos")]
    {
        let scope = task_traffic_controller::marker::scope(_ctx.paths.state_dir());
        let seen = until(10, || {
            process::scan_system(ID, &scope)
                .hidden
                .iter()
                .any(|h| h.proc == *_p && h.parent == Some(1))
        });
        assert!(seen, "{_p:?} is not a hidden orphan");
    }
}

#[test]
fn an_unrelated_orphan_of_an_older_session_does_not_hold_the_entry() {
    let dir = tempfile::tempdir().unwrap();
    let ours = Arc::new(OnlyOurHidden::default());
    let ctx = ctx(dir.path()).with_scan(ours.clone());
    let mut started = Started::default();

    // An unrelated session, begun a full second before the command.
    let bystander = spawn_entry(dir.path(), "ho_helper_entry", &[(ROLE, "bystander")], true);
    let bystander_pid = bystander.id();
    started.children.push(bystander);
    let mut leader_start = None;
    assert!(until(10, || {
        leader_start = process::start_time(bystander_pid);
        leader_start.is_some()
    }));
    let leader_start = leader_start.unwrap();
    assert!(
        until(5, || chrono::Utc::now().timestamp() as u64
            > leader_start + 1),
        "the clock did not advance"
    );

    let child = run_and_kill_supervisor(&ctx, dir.path(), "plain", &mut started);
    let child_start = child.start_time.expect("the child's start time");
    assert!(child_start > leader_start);

    // Now, inside the command's window, the bystander orphans a hidden
    // process. It is not the command's: its session is the bystander's.
    std::fs::write(go_file(dir.path()), b"").unwrap();
    let orphan = orphan(dir.path(), child_start);
    started.orphans.push(orphan);
    assert_eq!(
        process::session_evidence(&orphan),
        task_traffic_controller::lineage::SessionEvidence {
            sid: Some(bystander_pid),
            leader_start: Some(leader_start),
        }
    );
    assert_hidden_orphan(&ctx, &orphan);
    ours.0.lock().unwrap().push(orphan.pid);

    // The command ends by itself; with the supervisor dead the window in
    // which a hidden orphan counts never closes.
    let mut last = None;
    let released = until(30, || {
        let r = managed::resolve_checked(&ctx, ID).unwrap();
        let done = r.released;
        last = Some(r);
        done
    });
    assert!(released, "held by an unrelated orphan: {last:?}");
    assert!(managed::hidden_suspects(&ctx, ID).is_empty());
    // It was there the whole time, and it was not touched.
    assert_eq!(process::start_time(orphan.pid), orphan.start_time);
}

#[test]
fn a_descendant_that_left_the_session_after_the_supervisor_died_holds_the_entry() {
    let dir = tempfile::tempdir().unwrap();
    let ours = Arc::new(OnlyOurHidden::default());
    let ctx = ctx(dir.path()).with_scan(ours.clone());
    let mut started = Started::default();

    let child = run_and_kill_supervisor(&ctx, dir.path(), "escape", &mut started);
    let child_start = child.start_time.expect("the child's start time");

    // Nobody tracks the command any more. It starts a process in a session
    // of its own and leaves.
    std::fs::write(go_file(dir.path()), b"").unwrap();
    let orphan = orphan(dir.path(), child_start);
    started.orphans.push(orphan);
    assert_eq!(process::session_evidence(&orphan).sid, Some(orphan.pid));
    assert!(
        until(30, || process::probe_child(&child) == Probe::Gone),
        "the command never ended"
    );
    assert_hidden_orphan(&ctx, &orphan);
    ours.0.lock().unwrap().push(orphan.pid);

    // For the record (`--nocapture`): what the whole machine shows now.
    // By time and parent alone, and what the session rule leaves of it.
    let real = crate::ctx(dir.path());
    let scope = task_traffic_controller::marker::scope(real.paths.state_dir());
    let by_time = process::scan_system(ID, &scope)
        .hidden
        .iter()
        .filter(|h| task_traffic_controller::lineage::orphan_in_window(h, child_start, None, 0))
        .count();
    let left = managed::hidden_suspects(&real, ID);
    eprintln!(
        "ambient: {by_time} hidden orphans in the window, {} left by the session rule: {}",
        left.len(),
        left.iter()
            .map(|h| h.to_string())
            .collect::<Vec<_>>()
            .join(" | ")
    );

    // It runs: the entry is never released, however often it is asked.
    for _ in 0..5 {
        let r = managed::resolve_checked(&ctx, ID).unwrap();
        assert!(!r.released, "released while a descendant runs: {r:?}");
        #[cfg(target_os = "macos")]
        {
            assert!(r.verdict.contains("ProbeUnknown"), "{r:?}");
            assert!(
                r.checked.contains(&format!("pid {}", orphan.pid))
                    && r.checked.contains("leads its own session"),
                "{r:?}"
            );
            let held_by = managed::hidden_suspects(&ctx, ID);
            assert_eq!(held_by.len(), 1, "{held_by:?}");
            assert_eq!(
                (held_by[0].proc, held_by[0].parent, held_by[0].sid),
                (orphan, Some(1), Some(orphan.pid))
            );
            assert_eq!(
                held_by[0].standing,
                task_traffic_controller::lineage::Standing::OwnSession
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Once it is gone the entry is settled.
    started.stop_orphans();
    let mut last = None;
    let released = until(30, || {
        let r = managed::resolve_checked(&ctx, ID).unwrap();
        let done = r.released;
        last = Some(r);
        done
    });
    assert!(
        released,
        "not released after the descendant ended: {last:?}"
    );
}
