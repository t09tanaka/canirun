//! The CLI's [`Context`]: file locations from `HOME` / `XDG_STATE_HOME` /
//! `XDG_CONFIG_HOME` (so tests and containers get their own scope, exactly
//! like portool), the temp-dir roots, the ledger lock timeout override, and
//! the process signal handlers.

use crate::signals::ProcessSignals;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use task_traffic_controller::{Context, Paths};

/// Overrides the ledger lock timeout, in milliseconds (for tests).
const LEDGER_LOCK_TIMEOUT_ENV: &str = "TASK_TRAFFIC_CONTROLLER_LEDGER_LOCK_TIMEOUT_MS";

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn paths() -> Paths {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
        .join("task-traffic-controller");
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("task-traffic-controller/config.toml");
    Paths::new(state, config)
}

pub fn context() -> Context {
    let mut ctx = Context::new(paths())
        .with_supervisor(Arc::new(ProcessSignals))
        .with_tmp_roots(std::env::var("TMPDIR").into_iter().collect());
    if let Some(ms) = std::env::var(LEDGER_LOCK_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
    {
        ctx = ctx.with_ledger_lock_timeout(Duration::from_millis(ms));
    }
    ctx
}
