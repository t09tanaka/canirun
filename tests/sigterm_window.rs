//! Regression insurance: a queued job that gets SIGTERM twice in a row is
//! still recorded as 143, never as lost.
//!
//! This does not hit the window between the supervisor's handlers and the
//! caller's own exit (it passes without the fix too). That window is pinned
//! by `an_interrupted_wait_leaves_the_signal_handlers_to_the_caller` in
//! tests/managed.rs.

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn cmd(dir: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_task-traffic-controller"));
    c.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .current_dir(dir)
        .stdin(Stdio::null());
    c
}

fn out(dir: &std::path::Path, args: &[&str]) -> String {
    let o = cmd(dir).args(args).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn submit(dir: &std::path::Path, script: &str) -> String {
    let o = cmd(dir)
        .args(["submit", "--detach", "--", "sh", "-c", script])
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

#[test]
fn two_sigterms_while_queued_still_record_143() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let cfg = dir.join("config/task-traffic-controller/config.toml");
    fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    fs::write(cfg, "budget = 1\n").unwrap();

    let holder = submit(dir, "exec sleep 30");
    let queued = submit(dir, "echo never");
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = loop {
        assert!(Instant::now() < deadline, "the second job never queued");
        let status: serde_json::Value =
            serde_json::from_str(&out(dir, &["status", "--json"])).unwrap_or_default();
        let found = status["entries"].as_array().and_then(|es| {
            es.iter()
                .find(|e| e["id"] == queued.as_str() && e["state"] == "waiting")
                .map(|e| e["pid"].as_i64().unwrap())
        });
        if let Some(p) = found {
            break p as libc::pid_t;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    unsafe {
        libc::kill(pid, libc::SIGTERM);
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let state = loop {
        let jobs: Vec<serde_json::Value> =
            serde_json::from_str(&out(dir, &["jobs", "--json"])).unwrap();
        let st = jobs
            .iter()
            .find(|j| j["id"] == queued.as_str())
            .map(|j| j["state"].as_str().unwrap().to_string())
            .unwrap();
        if st != "queued" || Instant::now() > deadline {
            break st;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = out(dir, &["cancel", &holder]);
    assert_eq!(state, "done 143");
}
