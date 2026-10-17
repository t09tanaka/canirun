//! End-to-end tests for `submit` / `wait` / `jobs` / `cancel` and estimates.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct TestEnv {
    dir: TempDir,
}

impl TestEnv {
    fn new(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config/canirun/config.toml");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, config).unwrap();
        TestEnv { dir }
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state/canirun")
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_canirun"));
        c.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.dir.path())
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .current_dir(self.dir.path())
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    fn submit(&self, extra: &[&str], script: &str) -> (String, String) {
        let out = self
            .cmd()
            .arg("submit")
            .args(extra)
            .args(["--", "sh", "-c", script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn jobs_json(&self) -> Vec<serde_json::Value> {
        let out = self.run(&["jobs", "--json"]);
        assert!(out.status.success());
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn state_of(&self, id: &str) -> String {
        self.jobs_json()
            .iter()
            .find(|j| j["id"] == id)
            .map(|j| j["state"].as_str().unwrap().to_string())
            .unwrap_or_else(|| "missing".into())
    }
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn submit_returns_immediately_and_wait_collects_output_and_code() {
    let env = TestEnv::new("budget = 4\n");
    let t = Instant::now();
    let (id, _) = env.submit(&[], "echo hello; sleep 1; echo bye >&2; exit 5");
    assert!(t.elapsed() < Duration::from_secs(5), "submit blocked");
    assert_eq!(id.len(), 32);
    let out = env.run(&["wait", &id]);
    assert_eq!(out.status.code(), Some(5));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(&format!("=== canirun job {id} exit 5")),
        "{text}"
    );
    assert!(text.contains("hello\n") && text.contains("bye\n"), "{text}");
    assert_eq!(env.state_of(&id), "done 5");
    let out = env.run(&["wait", "--no-output", &id]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.starts_with("=== canirun job"), "{text}");
}

#[test]
fn wait_reports_in_given_order_and_first_failure_wins() {
    let env = TestEnv::new("budget = 4\n");
    let (a, _) = env.submit(&[], "sleep 0.5; echo A; exit 3");
    let (b, _) = env.submit(&[], "echo B; exit 0");
    let (c, _) = env.submit(&[], "echo C; exit 9");
    let out = env.run(&["wait", &b, &a, &c]);
    assert_eq!(out.status.code(), Some(3));
    let text = String::from_utf8_lossy(&out.stdout);
    let pa = text.find("B\n").unwrap();
    let pb = text.find("A\n").unwrap();
    let pc = text.find("C\n").unwrap();
    assert!(pa < pb && pb < pc, "{text}");
}

#[test]
fn jobs_are_gated_by_the_budget() {
    let env = TestEnv::new("budget = 1\n");
    let (a, _) = env.submit(&[], "sleep 1");
    let (b, _) = env.submit(&[], "true");
    assert!(wait_until(Duration::from_secs(5), || env.state_of(&a) == "running"));
    assert_eq!(env.state_of(&b), "queued");
    assert!(wait_until(Duration::from_secs(5), || {
        String::from_utf8_lossy(&env.run(&["status"]).stdout).contains("waiting (1)")
    }));
    assert!(env.run(&["wait", &a, &b]).status.success());
    assert_eq!(env.state_of(&b), "done 0");
}

#[test]
fn cancel_queued_and_running_jobs() {
    let env = TestEnv::new("budget = 1\n");
    let pidfile = env.dir.path().join("child.pid");
    let (running, _) = env.submit(
        &[],
        &format!("echo $$ > {}; exec sleep 30", pidfile.display()),
    );
    let (queued, _) = env.submit(&[], "echo never");
    assert!(wait_until(Duration::from_secs(5), || pidfile.exists()));
    assert!(wait_until(Duration::from_secs(5), || env.state_of(&queued) == "queued"));
    let out = env.run(&["cancel", &queued, &running]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env.run(&["wait", "--no-output", &queued, &running]);
    assert_eq!(out.status.code(), Some(143));
    assert_eq!(env.state_of(&queued), "done 143");
    assert_eq!(env.state_of(&running), "done 143");
    let child_pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(3), || unsafe {
            libc::kill(child_pid, 0) != 0
        }),
        "sleep child survived cancel"
    );
    let out = env.run(&["cancel", &queued]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("already finished"));
}

#[test]
fn job_survives_submitter_and_records_history() {
    let env = TestEnv::new("budget = 4\n");
    // The submitter process has already exited by the time submit() returns.
    let (id, _) = env.submit(&[], "sleep 1; echo done");
    let out = env.run(&["wait", &id]);
    assert!(out.status.success());
    let hist = fs::read_to_string(env.state().join("history.jsonl")).unwrap();
    assert_eq!(hist.lines().count(), 1);
    let log = fs::read_to_string(env.state().join(format!("jobs/{id}.log"))).unwrap();
    assert_eq!(log, "done\n");
}

#[test]
fn wait_on_unknown_id_is_an_error() {
    let env = TestEnv::new("budget = 4\n");
    let out = env.run(&["wait", "nope"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn submit_prints_an_estimate_from_history() {
    let env = TestEnv::new("budget = 1\n");
    // Learn ~1.2s for this command.
    let out = env.run(&["exec", "--", "sh", "-c", "sleep 1.2"]);
    assert!(out.status.success());
    let (a, est_a) = env.submit(&[], "sleep 1.2");
    assert!(est_a.contains("done in ~0:01"), "{est_a}");
    assert!(!est_a.contains('?'), "{est_a}");
    // Second one waits behind the first: start in ~0:01.
    let (b, est_b) = env.submit(&[], "sleep 1.2");
    assert!(est_b.contains("start in ~0:01"), "{est_b}");
    // Unknown command: rough estimate.
    let (c, est_c) = env.submit(&[], "true # never seen");
    assert!(est_c.contains('?'), "{est_c}");
    let jobs = String::from_utf8_lossy(&env.run(&["jobs"]).stdout).into_owned();
    assert!(jobs.contains("start in ~"), "{jobs}");
    assert!(env
        .run(&["wait", "--no-output", &a, &b, &c])
        .status
        .success());
}

#[test]
fn gc_removes_old_finished_jobs_only() {
    let env = TestEnv::new("budget = 4\njobs_keep_days = 0\n");
    let (id, _) = env.submit(&[], "true");
    assert!(env.run(&["wait", &id]).status.success());
    // finished_at is "now"; with keep_days = 0 the cutoff is also now, so
    // the job is already older and gets removed.
    thread::sleep(Duration::from_millis(1100));
    let out = env.run(&["gc"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("removed 1 old jobs"), "{text}");
    assert!(env.jobs_json().is_empty());
}

#[test]
fn job_without_a_waiter_is_cancelled_after_grace() {
    let env = TestEnv::new("budget = 4\njobs_orphan_grace_secs = 1\n");
    let pidfile = env.dir.path().join("orphan.pid");
    let (id, _) = env.submit(
        &[],
        &format!("echo $$ > {}; exec sleep 30", pidfile.display()),
    );
    assert!(wait_until(Duration::from_secs(5), || pidfile.exists()));
    assert!(
        wait_until(Duration::from_secs(6), || {
            env.state_of(&id).starts_with("orphaned")
        }),
        "{}",
        env.state_of(&id)
    );
    let child_pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(3), || unsafe {
            libc::kill(child_pid, 0) != 0
        }),
        "orphaned child kept running"
    );
    let out = env.run(&["wait", "--no-output", &id]);
    assert_eq!(out.status.code(), Some(143));
    assert!(String::from_utf8_lossy(&out.stdout).contains("orphaned"));
}

#[test]
fn waiter_keeps_the_job_alive_and_detach_opts_out() {
    // Grace of 3s: on a loaded machine spawning `wait` can take a while.
    let env = TestEnv::new("budget = 4\njobs_orphan_grace_secs = 3\n");
    // A waiter present from the start: the job runs to completion.
    let (a, _) = env.submit(&[], "sleep 3; echo kept");
    let out = env.run(&["wait", &a]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("kept"));
    // --detach: no waiter, still finishes.
    let (b, _) = env.submit(&["--detach"], "sleep 3; echo detached");
    assert!(wait_until(Duration::from_secs(10), || env.state_of(&b) == "done 0"));
    let log = fs::read_to_string(env.state().join(format!("jobs/{b}.log"))).unwrap();
    assert_eq!(log, "detached\n");
}

/// A `git` on PATH whose first call from a job runner sleeps `secs` (the
/// runner calls git to derive the key before it registers), and the PATH
/// to use it.
fn slow_first_git(env: &TestEnv, secs: u32) -> String {
    let bin = env.dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let marker = env.dir.path().join("git-slowed");
    let git = bin.join("git");
    fs::write(
        &git,
        format!(
            "#!/bin/sh\n\
             if ps -o args= -p $PPID | grep -q __run-job && [ ! -e {m} ]; then\n\
             touch {m}; sleep {secs}; fi\nexit 128\n",
            m = marker.display()
        ),
    )
    .unwrap();
    let mut perms = fs::metadata(&git).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&git, perms).unwrap();
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn submit_with_path(env: &TestEnv, path: &str, script: &str) -> Output {
    env.cmd()
        .env("PATH", path)
        .args(["submit", "--", "sh", "-c", script])
        .output()
        .unwrap()
}

#[test]
fn back_to_back_submits_start_in_submit_order() {
    let env = TestEnv::new("budget = 1\n");
    // The first runner is 2 s slow to register; the second submit must
    // still queue behind it.
    let path = slow_first_git(&env, 2);
    let order = env.dir.path().join("order");
    let ids: Vec<String> = (1..=2)
        .map(|n| {
            let out = submit_with_path(&env, &path, &format!("echo {n} >> {}", order.display()));
            assert!(out.status.success());
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        })
        .collect();
    assert!(env
        .run(&["wait", "--no-output", &ids[0], &ids[1]])
        .status
        .success());
    assert_eq!(fs::read_to_string(&order).unwrap(), "1\n2\n");
}

#[test]
fn submit_warns_when_the_runner_is_slow_to_register() {
    let env = TestEnv::new("budget = 4\n");
    // The runner's first `git` call hangs for 7 s; submit gives up after
    // 5 s with a warning.
    let path = slow_first_git(&env, 7);
    let t = Instant::now();
    let out = submit_with_path(&env, &path, "exit 3");
    let took = t.elapsed();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("has not joined the queue within 5s"), "{err}");
    assert!(
        took >= Duration::from_secs(5) && took < Duration::from_secs(7),
        "{took:?}"
    );
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(id.len(), 32);
    // Past the starting grace, a job with no ledger entry is reported
    // lost, as the warning says.
    let out = env.run(&["wait", &id]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stdout).contains("lost"));
}
