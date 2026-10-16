//! XDG path resolution. Honours `XDG_STATE_HOME` / `XDG_CONFIG_HOME` so
//! tests and containers get their own scope, exactly like portool.

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
        .join("canirun")
}

pub fn config_file() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("canirun/config.toml")
}

pub fn ledger_file() -> PathBuf {
    state_dir().join("ledger.json")
}

pub fn ledger_lock() -> PathBuf {
    state_dir().join("ledger.lock")
}

pub fn leases_dir() -> PathBuf {
    state_dir().join("leases")
}

pub fn lease_file(id: &str) -> PathBuf {
    leases_dir().join(format!("{id}.lock"))
}

pub fn history_file() -> PathBuf {
    state_dir().join("history.jsonl")
}
