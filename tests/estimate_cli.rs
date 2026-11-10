//! `canirun inspect --json` and `canirun estimate --json -- <command>`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct TestEnv {
    dir: tempfile::TempDir,
}

impl TestEnv {
    fn new() -> Self {
        TestEnv {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state/canirun")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_canirun"))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.dir.path())
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .current_dir(self.dir.path())
            .args(args)
            .output()
            .unwrap()
    }
}

fn files(dir: &Path) -> Vec<String> {
    fn walk(d: &Path, root: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(d) else {
            return;
        };
        for e in rd.flatten() {
            out.push(e.path().strip_prefix(root).unwrap().display().to_string());
            if e.path().is_dir() {
                walk(&e.path(), root, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn inspect_prints_a_versioned_snapshot_and_creates_nothing() {
    let env = TestEnv::new();
    let out = env.run(&["inspect", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert!(v["snapshot_digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(v["policy_digest"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(v["entries"].as_array().unwrap().len(), 0);
    assert!(v["limits"]["budget"].as_u64().unwrap() >= 1);
    assert!(!env.state().exists());
}

#[test]
fn estimate_without_history_is_a_ranking_and_leaves_the_state_alone() {
    let env = TestEnv::new();
    // Create the state first so there is something to compare.
    assert!(env.run(&["gc"]).status.success());
    let before = files(&env.state());
    for _ in 0..3 {
        let out = env.run(&["estimate", "--json", "--", "fixture-tool", "--flag"]);
        assert!(out.status.success(), "{out:?}");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["ttl_ms"], 2000);
        let t = &v["target"];
        assert_eq!(t["runtime_source"], "fallback");
        assert_eq!(t["history_count"], 0);
        assert!(t["finish_in_ms"].is_null());
        assert!(t["null_reasons"]["finish_in_ms"].is_string());
        assert_eq!(t["ranking"]["finish_in_ms"], 60_000);
        assert_eq!(t["start_in_ms"], 0);
        assert_eq!(t["confidence"], "low");
        assert_eq!(v["coverage"][0]["node_ids"][0], "command");
    }
    assert_eq!(files(&env.state()), before);
}

#[test]
fn estimate_prints_a_line_without_json() {
    let env = TestEnv::new();
    let out = env.run(&["estimate", "--", "fixture-tool"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("no measured estimate"), "{text}");
}
