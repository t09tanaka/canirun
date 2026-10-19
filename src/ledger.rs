//! Ledger schema and load/save (spec "Ledger and locking").
//!
//! Callers must hold the ledger lock (`lock::ledger`) around load+save.

use crate::admit::{QueueEntry, State};
use crate::error::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub id: String,
    pub pid: u32,
    pub state: State,
    pub front: bool,
    pub weight: u32,
    pub predicted_rss: u64,
    /// Learned wall time, for estimates. None when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_secs: Option<f64>,
    pub repo: String,
    #[serde(default)]
    pub subdir: String,
    pub argv: String,
    pub key: String,
    pub seq: u64,
    pub enqueued_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// Cores the running command's process tree was last measured using,
    /// written by the running `canirun` itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub using: Option<f64>,
    /// Started ahead of a waiting entry that did not fit the budget.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub backfilled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Ledger {
    pub version: u32,
    pub next_seq: u64,
    pub entries: Vec<Entry>,
}

impl Default for Ledger {
    fn default() -> Self {
        Ledger {
            version: SCHEMA_VERSION,
            next_seq: 1,
            entries: Vec::new(),
        }
    }
}

impl Ledger {
    pub fn load(path: &Path) -> Result<Ledger> {
        match std::fs::read(path) {
            Ok(bytes) if bytes.is_empty() => Ok(Ledger::default()),
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomic write: temp file in the same directory, then rename.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer_pretty(&mut tmp, self)?;
        tmp.write_all(b"\n")?;
        tmp.as_file().sync_all()?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    pub fn take_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }

    /// What admission sees of every entry, with waiting times as of now.
    pub fn queue(&self) -> Vec<QueueEntry> {
        let now = Utc::now();
        self.entries
            .iter()
            .map(|e| QueueEntry {
                id: e.id.clone(),
                state: e.state,
                front: e.front,
                weight: e.weight,
                seq: e.seq,
                using: e.using,
                waited_secs: match e.state {
                    State::Waiting => {
                        (now - e.enqueued_at).num_milliseconds().max(0) as f64 / 1000.0
                    }
                    State::Running => 0.0,
                },
                expected_secs: e.expected_secs,
                backfilled: e.backfilled,
            })
            .collect()
    }

    pub fn find(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        before != self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, seq: u64) -> Entry {
        Entry {
            id: id.into(),
            pid: 1,
            state: State::Waiting,
            front: false,
            weight: 1,
            predicted_rss: 0,
            expected_secs: None,
            repo: String::new(),
            subdir: String::new(),
            argv: "x".into(),
            key: "k".into(),
            seq,
            enqueued_at: Utc::now(),
            started_at: None,
            using: None,
            backfilled: false,
        }
    }

    #[test]
    fn round_trip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/ledger.json");
        let mut l = Ledger::default();
        let s = l.take_seq();
        l.entries.push(entry("a", s));
        l.save(&p).unwrap();
        let back = Ledger::load(&p).unwrap();
        assert_eq!(back, l);
        assert_eq!(back.next_seq, 2);
        assert!(back.find("a").is_some());
    }

    #[test]
    fn missing_and_empty_are_default() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            Ledger::load(&d.path().join("x")).unwrap(),
            Ledger::default()
        );
        let p = d.path().join("empty");
        std::fs::write(&p, b"").unwrap();
        assert_eq!(Ledger::load(&p).unwrap(), Ledger::default());
    }

    #[test]
    fn remove_reports_presence() {
        let mut l = Ledger::default();
        l.entries.push(entry("a", 1));
        assert!(l.remove("a"));
        assert!(!l.remove("a"));
    }
}
