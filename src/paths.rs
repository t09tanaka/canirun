//! Where canirun keeps its files. The library never looks at `HOME` or the
//! XDG variables itself: whoever builds the [`crate::Context`] decides the
//! state directory, so tests and embedders get their own scope.

use std::path::{Path, PathBuf};

/// The state directory, the config file and the history file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    state_dir: PathBuf,
    config_file: PathBuf,
    history_file: PathBuf,
}

impl Paths {
    /// State under `state_dir` (`ledger.json`, `leases/`, `jobs/`,
    /// `history.jsonl`), configuration read from `config_file`.
    pub fn new(state_dir: impl Into<PathBuf>, config_file: impl Into<PathBuf>) -> Self {
        let state_dir = state_dir.into();
        let history_file = state_dir.join("history.jsonl");
        Paths {
            state_dir,
            config_file: config_file.into(),
            history_file,
        }
    }

    /// Keep the history somewhere else than `<state_dir>/history.jsonl`.
    pub fn with_history_file(mut self, history_file: impl Into<PathBuf>) -> Self {
        self.history_file = history_file.into();
        self
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    pub fn history_file(&self) -> &Path {
        &self.history_file
    }

    pub fn ledger_file(&self) -> PathBuf {
        self.state_dir.join("ledger.json")
    }

    pub fn ledger_lock(&self) -> PathBuf {
        self.state_dir.join("ledger.lock")
    }

    pub fn leases_dir(&self) -> PathBuf {
        self.state_dir.join("leases")
    }

    pub fn lease_file(&self, id: &str) -> PathBuf {
        self.leases_dir().join(format!("{id}.lock"))
    }

    pub fn jobs_dir(&self) -> PathBuf {
        self.state_dir.join("jobs")
    }

    pub fn job_json(&self, id: &str) -> PathBuf {
        self.jobs_dir().join(format!("{id}.json"))
    }

    pub fn job_log(&self, id: &str) -> PathBuf {
        self.jobs_dir().join(format!("{id}.log"))
    }

    pub fn job_waiter(&self, id: &str) -> PathBuf {
        self.jobs_dir().join(format!("{id}.waiter"))
    }

    pub fn job_exit(&self, id: &str) -> PathBuf {
        self.jobs_dir().join(format!("{id}.exit"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_hangs_off_the_given_state_dir() {
        let p = Paths::new("/s", "/c/config.toml");
        assert_eq!(p.ledger_file(), Path::new("/s/ledger.json"));
        assert_eq!(p.lease_file("a"), Path::new("/s/leases/a.lock"));
        assert_eq!(p.job_exit("j"), Path::new("/s/jobs/j.exit"));
        assert_eq!(p.history_file(), Path::new("/s/history.jsonl"));
        assert_eq!(p.config_file(), Path::new("/c/config.toml"));
    }

    #[test]
    fn history_can_live_elsewhere() {
        let p = Paths::new("/s", "/c").with_history_file("/h/h.jsonl");
        assert_eq!(p.history_file(), Path::new("/h/h.jsonl"));
        assert_eq!(p.ledger_file(), Path::new("/s/ledger.json"));
    }
}
