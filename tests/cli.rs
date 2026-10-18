//! End-to-end tests for the `canirun` binary.
//!
//! Every test gets its own [`TestEnv`]: a temp directory supplying
//! `HOME` / `XDG_STATE_HOME` / `XDG_CONFIG_HOME` to the spawned binary via
//! `Command::env`, so the real `~/.local/state` is never touched and tests
//! can run in parallel.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct TestEnv {
    dir: TempDir,
}

impl TestEnv {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        TestEnv { dir }
    }

    fn with_config(self, toml: &str) -> Self {
        let p = self.dir.path().join("config/canirun/config.toml");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, toml).unwrap();
        self
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
            .current_dir(self.dir.path());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    fn marker_dir(&self) -> PathBuf {
        let p = self.dir.path().join("markers");
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Spawn `canirun exec <extra> -- sh -c '<script>'` detached from our
    /// stdio so tests can wait on it.
    fn spawn_exec(&self, extra: &[&str], script: &str) -> Child {
        self.cmd()
            .arg("exec")
            .args(extra)
            .args(["--", "sh", "-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn ledger_json(&self) -> serde_json::Value {
        let out = self.run(&["status", "--json"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn entries(&self) -> Vec<serde_json::Value> {
        self.ledger_json()["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
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

/// A script that records start/end timestamps (ns since epoch) in a marker
/// file and sleeps `secs`.
fn marker_script(dir: &Path, name: &str, secs: &str) -> String {
    let p = dir.join(name);
    let now = "perl -MTime::HiRes=time -e 'printf \"%.0f\\n\", time*1e9'";
    format!(
        "{now} > {p}.start; sleep {secs}; {now} > {p}.end",
        p = p.display()
    )
}

fn read_ns(p: &Path) -> u128 {
    fs::read_to_string(p).unwrap().trim().parse().unwrap()
}

#[test]
fn exec_passes_through_exit_code_and_stdout() {
    let env = TestEnv::new();
    let out = env.run(&["exec", "--", "sh", "-c", "echo hi; exit 3"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(stdout(&out), "hi\n");
    assert!(env.entries().is_empty(), "entry must be removed after run");
}

#[test]
fn exec_without_command_is_a_usage_error() {
    let env = TestEnv::new();
    let out = env.run(&["exec", "--"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Usage"));
}

#[test]
fn exec_missing_program_fails_cleanly() {
    let env = TestEnv::new();
    let out = env.run(&["exec", "--", "definitely-not-a-program-xyz"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(env.entries().is_empty());
}

#[test]
fn exec_records_history_and_learns_weight() {
    let env = TestEnv::new().with_config("budget = 8\n");
    // Two processes in parallel, each burning 0.6 s of its own CPU time.
    // Bounded by CPU time rather than the clock, so a loaded machine only
    // stretches the wall time and cpu_secs stays >= 1.2.
    let script = "for i in 1 2; do perl -e '1 while (times)[0] < 0.6' & done; wait";
    let out = env.run(&["exec", "--", "sh", "-c", script]);
    assert!(out.status.success());
    let hist = fs::read_to_string(env.state().join("history.jsonl")).unwrap();
    let rec: serde_json::Value = serde_json::from_str(hist.lines().next().unwrap()).unwrap();
    // One process cannot burn 0.6 s of CPU in less wall time.
    assert!(rec["wall_secs"].as_f64().unwrap() >= 0.5, "{rec}");
    assert!(rec["cpu_secs"].as_f64().unwrap() >= 1.0, "{rec}");
    assert_eq!(rec["exit_code"], 0);
    assert_eq!(rec["weight_used"], 2, "default weight on first run");

    let out = env.run(&["history", "--", "sh", "-c", script]);
    let text = stdout(&out);
    assert!(text.contains("next: weight"), "{text}");
    // On a loaded machine the run is recorded as contended; with no calm
    // run to prefer, it is still what the weight is learned from.
    assert!(text.contains("(Learned)"), "{text}");
}

#[test]
fn concurrency_never_exceeds_budget() {
    let env = TestEnv::new().with_config("budget = 2\n");
    let dir = env.marker_dir();
    let n = 5;
    let children: Vec<Child> = (0..n)
        .map(|i| {
            env.spawn_exec(
                &["--weight", "1"],
                &marker_script(&dir, &format!("m{i}"), "0.6"),
            )
        })
        .collect();
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    let mut spans: Vec<(u128, u128)> = (0..n)
        .map(|i| {
            (
                read_ns(&dir.join(format!("m{i}.start"))),
                read_ns(&dir.join(format!("m{i}.end"))),
            )
        })
        .collect();
    spans.sort();
    // Max overlap at any start instant must be <= budget.
    for (s, _) in &spans {
        let overlapping = spans.iter().filter(|(a, b)| a <= s && s < b).count();
        assert!(overlapping <= 2, "{overlapping} ran at once: {spans:?}");
    }
    assert!(env.entries().is_empty());
}

#[test]
fn head_reserves_and_light_entry_does_not_overtake() {
    let env = TestEnv::new().with_config("budget = 3\n");
    let dir = env.marker_dir();
    let mut big = env.spawn_exec(&["--weight", "2"], &marker_script(&dir, "big", "1"));
    assert!(wait_until(Duration::from_secs(5), || {
        env.entries().iter().any(|e| e["state"] == "running")
    }));
    let mut heavy = env.spawn_exec(&["--weight", "2"], &marker_script(&dir, "heavy", "0.3"));
    assert!(wait_until(Duration::from_secs(5), || {
        env.entries()
            .iter()
            .any(|e| e["weight"] == 2 && e["state"] == "waiting")
    }));
    let mut light = env.spawn_exec(&["--weight", "1"], &marker_script(&dir, "light", "0.3"));
    assert!(big.wait().unwrap().success());
    assert!(heavy.wait().unwrap().success());
    assert!(light.wait().unwrap().success());
    let heavy_start = read_ns(&dir.join("heavy.start"));
    let light_start = read_ns(&dir.join("light.start"));
    assert!(
        light_start >= heavy_start,
        "light overtook the reserving head"
    );
}

#[test]
fn measured_use_beyond_the_declared_weight_holds_the_next_start() {
    let env = TestEnv::new()
        .with_config("budget = 2\ncpu_threshold_percent = 100\nmemory_reserve_mib = 0\n");
    let dir = env.marker_dir();
    // Declared weight 1, but six workers each burning 4 s of their own
    // CPU time. Bounded by CPU time, so load only stretches the wall time.
    let now = "perl -MTime::HiRes=time -e 'printf \"%.0f\\n\", time*1e9'";
    let burn = format!(
        "for i in 1 2 3 4 5 6; do perl -e '1 while (times)[0] < 4' & done; wait; {now} > {}",
        dir.join("hog.end").display()
    );
    let mut hog = env.spawn_exec(&["--weight", "1"], &burn);
    assert!(
        wait_until(Duration::from_secs(20), || {
            env.entries()
                .iter()
                .any(|e| e["using"].as_f64().unwrap_or(0.0) >= 2.5)
        }),
        "the running tree was never measured: {:?}",
        env.entries()
    );
    let status = stdout(&env.run(&["status"]));
    assert!(status.contains("w1  (using "), "{status}");
    // By declared weight (1 + 1 <= 2) this would start at once.
    let mut next = env.spawn_exec(&["--weight", "1"], &marker_script(&dir, "next", "0"));
    assert!(hog.wait().unwrap().success());
    assert!(next.wait().unwrap().success());
    assert!(
        read_ns(&dir.join("next.start")) >= read_ns(&dir.join("hog.end")),
        "started while the measured tree filled the budget"
    );
}

#[test]
fn front_jumps_the_queue() {
    let env = TestEnv::new().with_config("budget = 1\n");
    let dir = env.marker_dir();
    let mut first = env.spawn_exec(&[], &marker_script(&dir, "first", "1"));
    assert!(wait_until(Duration::from_secs(5), || {
        env.entries().iter().any(|e| e["state"] == "running")
    }));
    let mut normal = env.spawn_exec(&[], &marker_script(&dir, "normal", "0.2"));
    assert!(wait_until(Duration::from_secs(5), || env.entries().len() == 2));
    let mut front = env.spawn_exec(&["--front"], &marker_script(&dir, "front", "0.2"));
    assert!(wait_until(Duration::from_secs(5), || env.entries().len() == 3));
    let status = stdout(&env.run(&["status"]));
    assert!(status.contains("[front]"), "{status}");
    for c in [&mut first, &mut normal, &mut front] {
        assert!(c.wait().unwrap().success());
    }
    assert!(read_ns(&dir.join("front.start")) < read_ns(&dir.join("normal.start")));
}

#[test]
fn killed_waiting_head_is_reaped_and_queue_moves_on() {
    let env = TestEnv::new().with_config("budget = 1\n");
    let dir = env.marker_dir();
    let mut running = env.spawn_exec(&[], &marker_script(&dir, "run", "1.5"));
    assert!(wait_until(Duration::from_secs(5), || {
        env.entries().iter().any(|e| e["state"] == "running")
    }));
    let mut doomed = env.spawn_exec(&[], &marker_script(&dir, "doomed", "5"));
    assert!(wait_until(Duration::from_secs(5), || env.entries().len() == 2));
    let mut survivor = env.spawn_exec(&[], &marker_script(&dir, "survivor", "0.1"));
    assert!(wait_until(Duration::from_secs(5), || env.entries().len() == 3));
    // SIGKILL the waiting head: no handler can run, only the flock drops.
    unsafe {
        libc::kill(doomed.id() as i32, libc::SIGKILL);
    }
    doomed.wait().unwrap();
    assert!(running.wait().unwrap().success());
    let t = Instant::now();
    assert!(survivor.wait().unwrap().success());
    assert!(
        t.elapsed() < Duration::from_secs(4),
        "survivor blocked by dead head"
    );
    assert!(!dir.join("doomed.start").exists());
    assert!(env.entries().is_empty());
    assert!(!env.state().join("leases").read_dir().unwrap().any(|_| true));
}

#[test]
fn nested_exec_inherits_lease_and_bogus_lease_is_gated() {
    let env = TestEnv::new().with_config("budget = 1\n");
    let bin = env!("CARGO_BIN_EXE_canirun");
    // Outer takes the whole budget; inner must still run (inherited lease).
    let out = env.run(&[
        "exec",
        "--weight",
        "1",
        "--",
        "sh",
        "-c",
        &format!("{bin} exec --weight 1 -- echo nested-ok"),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout(&out), "nested-ok\n");
    // History is written once (outer only).
    let hist = fs::read_to_string(env.state().join("history.jsonl")).unwrap();
    assert_eq!(hist.lines().count(), 1);

    // A stale lease id gates normally: with budget 1 busy, it waits.
    let dir = env.marker_dir();
    let mut hog = env.spawn_exec(&[], &marker_script(&dir, "hog", "1"));
    assert!(wait_until(Duration::from_secs(5), || {
        env.entries().iter().any(|e| e["state"] == "running")
    }));
    let mut stale = env
        .cmd()
        .env("CANIRUN_LEASE", "deadbeef")
        .args([
            "exec",
            "--",
            "sh",
            "-c",
            &marker_script(&dir, "stale", "0.1"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || env.entries().len() == 2));
    assert!(hog.wait().unwrap().success());
    assert!(stale.wait().unwrap().success());
    assert!(read_ns(&dir.join("stale.start")) >= read_ns(&dir.join("hog.end")));
}

#[test]
fn sigterm_is_forwarded_and_exit_mirrors_child() {
    let env = TestEnv::new();
    let dir = env.marker_dir();
    let pidfile = dir.join("child.pid");
    let mut child = env.spawn_exec(
        &[],
        &format!("echo $$ > {}; exec sleep 30", pidfile.display()),
    );
    assert!(wait_until(Duration::from_secs(5), || pidfile.exists()));
    assert!(wait_until(Duration::from_secs(5), || {
        env.entries().iter().any(|e| e["state"] == "running")
    }));
    let child_pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let status = child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(libc::SIGTERM), "{status:?}");
    assert!(
        wait_until(Duration::from_secs(3), || unsafe {
            libc::kill(child_pid, 0) != 0
        }),
        "child sleep still alive"
    );
    assert!(env.entries().is_empty());
}

#[test]
fn sigint_while_waiting_removes_entry() {
    let env = TestEnv::new().with_config("budget = 1\n");
    let dir = env.marker_dir();
    let mut hog = env.spawn_exec(&[], &marker_script(&dir, "hog", "1"));
    assert!(wait_until(Duration::from_secs(5), || {
        env.entries().iter().any(|e| e["state"] == "running")
    }));
    let mut waiter = env.spawn_exec(&[], "echo never");
    assert!(wait_until(Duration::from_secs(5), || env.entries().len() == 2));
    unsafe {
        libc::kill(waiter.id() as i32, libc::SIGINT);
    }
    let status = waiter.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(libc::SIGINT));
    assert!(wait_until(Duration::from_secs(2), || env.entries().len() == 1));
    assert!(hog.wait().unwrap().success());
}

#[test]
fn progress_is_printed_while_waiting() {
    let env = TestEnv::new().with_config("budget = 1\nprogress_interval_secs = 1\n");
    let dir = env.marker_dir();
    let mut hog = env.spawn_exec(&[], &marker_script(&dir, "hog", "1.5"));
    assert!(wait_until(Duration::from_secs(5), || {
        env.entries().iter().any(|e| e["state"] == "running")
    }));
    let waiter = env.spawn_exec(&[], "true");
    let out = waiter.wait_with_output().unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("canirun: waiting"), "{err}");
    assert!(err.contains("running 1 (1/1 used)"), "{err}");
    assert!(hog.wait().unwrap().success());
}

/// Wait behind a 3 s hog with stderr piped (not a terminal) and return
/// the number of progress lines.
fn progress_lines_behind_hog(config: &str) -> usize {
    let env = TestEnv::new().with_config(config);
    let dir = env.marker_dir();
    let mut hog = env.spawn_exec(&[], &marker_script(&dir, "hog", "3"));
    assert!(wait_until(Duration::from_secs(5), || {
        env.entries().iter().any(|e| e["state"] == "running")
    }));
    let waiter = env.spawn_exec(&[], "true");
    let out = waiter.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(hog.wait().unwrap().success());
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.starts_with("canirun: waiting"))
        .count()
}

#[test]
fn non_tty_progress_prints_only_on_change() {
    // The reason stays "budget" for ~3 s: one line, not one per second.
    let n = progress_lines_behind_hog("budget = 1\nprogress_interval_secs = 1\n");
    assert_eq!(n, 1);
}

#[test]
fn non_tty_progress_interval_is_configurable() {
    let n = progress_lines_behind_hog(
        "budget = 1\nprogress_interval_secs = 60\nprogress_interval_secs_non_tty = 1\n",
    );
    assert!(n >= 2, "{n}");
}

#[test]
fn exec_one_quoted_argument_gets_a_hint() {
    let env = TestEnv::new();
    let out = env.run(&["exec", "--", "no-such-program-xyz run x"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("hint:"), "{err}");
    assert!(err.contains("`-- no-such-program-xyz run x`"), "{err}");
    // A plain missing program gets no hint.
    let out = env.run(&["exec", "--", "no-such-program-xyz"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("hint:"));
}

#[test]
fn submit_one_quoted_argument_gets_a_hint() {
    let env = TestEnv::new();
    let out = env.run(&["submit", "--", "no-such-program-xyz run x"]);
    assert!(out.status.success());
    let id = stdout(&out).trim().to_string();
    let out = env.run(&["wait", &id]);
    assert_eq!(out.status.code(), Some(2));
    let log = stdout(&out);
    assert!(log.contains("hint:"), "{log}");
    assert!(log.contains("`-- no-such-program-xyz run x`"), "{log}");
}

#[test]
fn fixed_weight_pattern_overrides_history() {
    let env = TestEnv::new().with_config(
        "budget = 4\n[[pattern]]\nmatch = \"*lint*\"\nweight = 1\n[[pattern]]\nmatch = \"*heavy*\"\nmin_weight = 3\n",
    );
    env.run(&["exec", "--", "sh", "-c", "true # lint"]);
    env.run(&["exec", "--", "sh", "-c", "true # heavy"]);
    let hist = fs::read_to_string(env.state().join("history.jsonl")).unwrap();
    let recs: Vec<serde_json::Value> = hist
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(recs[0]["weight_used"], 1);
    assert_eq!(recs[1]["weight_used"], 3);
}

#[test]
fn weight_flag_is_recorded() {
    let env = TestEnv::new();
    env.run(&["exec", "--weight", "3", "--", "true"]);
    let hist = fs::read_to_string(env.state().join("history.jsonl")).unwrap();
    let rec: serde_json::Value = serde_json::from_str(hist.lines().next().unwrap()).unwrap();
    assert_eq!(rec["weight_used"], 3);
}

#[test]
fn key_override_groups_history() {
    let env = TestEnv::new();
    env.run(&["exec", "--key", "e2e", "--", "sh", "-c", "true 1"]);
    env.run(&["exec", "--key", "e2e", "--", "sh", "-c", "true 2"]);
    let out = env.run(&["history", "--key", "e2e", "--json"]);
    let v: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.len(), 2);
    assert_eq!(v[0]["key"], v[1]["key"]);
}

/// A stand-in `npx` on PATH so the heavy-command rules can be exercised
/// without Node.
fn with_fake_npx(env: &TestEnv, c: &mut Command) {
    use std::os::unix::fs::PermissionsExt;
    let bin = env.dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let npx = bin.join("npx");
    fs::write(&npx, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&npx, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::var("PATH").unwrap_or_default();
    c.env("PATH", format!("{}:{path}", bin.display()));
}

#[test]
fn wrapped_commands_share_the_key_of_their_heavy_command() {
    let env = TestEnv::new();
    let run = |args: &[&str]| {
        let mut c = env.cmd();
        with_fake_npx(&env, &mut c);
        let out = c.args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["exec", "--", "npx", "tool"]);
    run(&[
        "exec",
        "--",
        "sh",
        "-c",
        "npx tool > out.log 2>&1; echo done",
    ]);
    run(&["exec", "--", "bash", "-c", "cd . && npx tool | cat"]);
    // Scripts in a temporary directory (the test's own temp dir) are read.
    for name in ["one.sh", "two.sh"] {
        let p = env.dir.path().join(name);
        fs::write(&p, "#!/bin/sh\nset -e\nnpx tool\n").unwrap();
        run(&["exec", "--", "sh", p.to_str().unwrap()]);
    }

    let out = env.run(&["history", "--json", "--", "npx", "tool"]);
    let v: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.len(), 5, "{v:?}");
    assert!(v.iter().all(|r| r["key"] == v[0]["key"]));
    assert!(v[0].get("key_argv").is_none(), "{}", v[0]);
    assert_eq!(v[1]["key_argv"], "npx tool");
    assert_eq!(v[1]["argv"], "sh -c npx tool > out.log 2>&1; echo done");

    let text = stdout(&env.run(&["history"]));
    assert!(text.contains("key: npx tool"), "{text}");
}

#[test]
fn shell_string_without_a_heavy_command_keeps_its_own_key() {
    let env = TestEnv::new();
    env.run(&["exec", "--", "sh", "-c", "true 1"]);
    env.run(&["exec", "--", "sh", "-c", "true 2"]);
    let out = env.run(&["history", "--json"]);
    let v: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.len(), 2);
    assert_ne!(v[0]["key"], v[1]["key"]);
}

#[test]
fn worktrees_share_a_key() {
    let env = TestEnv::new();
    let repo = env.dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str], cwd: &Path| {
        let o = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    };
    git(&["init", "-q", "-b", "main"], &repo);
    git(&["commit", "-q", "--allow-empty", "-m", "init"], &repo);
    let wt = env.dir.path().join("wt");
    git(&["worktree", "add", "-q", wt.to_str().unwrap()], &repo);
    env.cmd()
        .current_dir(&repo)
        .args(["exec", "--", "true"])
        .output()
        .unwrap();
    env.cmd()
        .current_dir(&wt)
        .args(["exec", "--", "true"])
        .output()
        .unwrap();
    let hist = fs::read_to_string(env.state().join("history.jsonl")).unwrap();
    let recs: Vec<serde_json::Value> = hist
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0]["key"], recs[1]["key"]);
    assert_eq!(recs[0]["repo"], recs[1]["repo"]);
}

#[test]
fn subprojects_of_one_repo_learn_separately() {
    let env = TestEnv::new();
    let repo = env.dir.path().join("mono");
    fs::create_dir_all(repo.join("api")).unwrap();
    fs::create_dir_all(repo.join("client")).unwrap();
    let git = |args: &[&str]| {
        let o = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    env.cmd()
        .current_dir(repo.join("api"))
        .args(["exec", "--", "true"])
        .output()
        .unwrap();
    env.cmd()
        .current_dir(repo.join("client"))
        .args(["exec", "--", "true"])
        .output()
        .unwrap();
    env.cmd()
        .current_dir(&repo)
        .args(["exec", "--", "true"])
        .output()
        .unwrap();
    let hist = fs::read_to_string(env.state().join("history.jsonl")).unwrap();
    let recs: Vec<serde_json::Value> = hist
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(recs.len(), 3);
    assert_ne!(recs[0]["key"], recs[1]["key"]);
    assert_ne!(recs[1]["key"], recs[2]["key"]);
    assert_eq!(recs[0]["subdir"], "api");
    assert_eq!(recs[2]["subdir"], "");
    let out = env
        .cmd()
        .current_dir(repo.join("api"))
        .args(["history"])
        .output()
        .unwrap();
    assert!(stdout(&out).contains("mono/api: true"), "{}", stdout(&out));
}

#[test]
fn config_and_gc_commands() {
    let env = TestEnv::new().with_config("budget = 3\nhistory_keep = 1\n");
    let out = env.run(&["config"]);
    assert!(stdout(&out).contains("budget = 3"));
    let out = env.run(&["config", "--path"]);
    assert!(stdout(&out).trim().ends_with("config/canirun/config.toml"));
    env.run(&["exec", "--", "true"]);
    env.run(&["exec", "--", "true"]);
    let out = env.run(&["gc"]);
    assert!(
        stdout(&out).contains("trimmed 1 history records"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn invalid_config_is_reported() {
    let env = TestEnv::new().with_config("budget = 0\n");
    let out = env.run(&["status"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("budget"));
}
