//! Ledger schema and load/save (spec "Ledger and locking").
//!
//! Callers must hold the ledger lock (`lock::ledger`) around load+save.

use crate::admit::{QueueEntry, State};
use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

pub const SCHEMA_VERSION: u32 = 1;
/// The version of a ledger that holds managed entries (see [`Ledger`]).
pub const MANAGED_SCHEMA_VERSION: u32 = 2;

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

/// The ledger. In memory (and in `status --json`) it is always `entries`;
/// the file has two shapes, chosen by [`Ledger::save`].
///
/// A ledger without managed entries is written exactly as before
/// (`version` 1, `entries`), so older binaries keep reading it. A ledger
/// that holds a managed entry (one with a sidecar in the `managed` directory
/// beside the file) is written with `version` 2 and its entries under
/// `entries_v2`, with no `entries` key at all. An older binary then fails
/// to parse it (a required field is missing) and so neither reaps nor
/// rewrites it: it sees only plain entries and does not know that managed
/// ones keep their budget after the owner's lease is gone, so failing
/// closed is the only safe answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    pub version: u32,
    pub next_seq: u64,
    pub entries: Vec<Entry>,
}

/// The file's shapes. Borrowed to write, owned to read.
#[derive(Serialize, Deserialize)]
struct Wire<E> {
    version: u32,
    next_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entries: Option<E>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entries_v2: Option<E>,
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
            Ok(bytes) => {
                let w: Wire<Vec<Entry>> = serde_json::from_slice(&bytes)?;
                let entries = match (w.entries, w.entries_v2) {
                    (Some(e), None) | (None, Some(e)) => e,
                    _ => {
                        return Err(Error::Ledger(
                            "a ledger has exactly one of `entries` and `entries_v2`".into(),
                        ))
                    }
                };
                Ok(Ledger {
                    version: w.version,
                    next_seq: w.next_seq,
                    entries,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomic write: temp file in the same directory, then rename.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;
        let managed_dir = dir.join("managed");
        let managed = self
            .entries
            .iter()
            .any(|e| managed_dir.join(format!("{}.json", e.id)).exists());
        let wire = if managed {
            Wire {
                version: MANAGED_SCHEMA_VERSION,
                next_seq: self.next_seq,
                entries: None,
                entries_v2: Some(&self.entries),
            }
        } else {
            Wire {
                version: SCHEMA_VERSION,
                next_seq: self.next_seq,
                entries: Some(&self.entries),
                entries_v2: None,
            }
        };
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer_pretty(&mut tmp, &wire)?;
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
        self.queue_at(Utc::now())
    }

    /// [`Ledger::queue`] as of `now`.
    pub fn queue_at(&self, now: DateTime<Utc>) -> Vec<QueueEntry> {
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

    /// The ledger types of `origin/main` before managed entries existed,
    /// copied as they were.
    mod old {
        #![allow(dead_code)]
        use crate::admit::State;
        use chrono::{DateTime, Utc};
        use serde::Deserialize;

        #[derive(Debug, Deserialize)]
        pub struct Entry {
            pub id: String,
            pub pid: u32,
            pub state: State,
            pub front: bool,
            pub weight: u32,
            pub predicted_rss: u64,
            #[serde(default)]
            pub expected_secs: Option<f64>,
            pub repo: String,
            #[serde(default)]
            pub subdir: String,
            pub argv: String,
            pub key: String,
            pub seq: u64,
            pub enqueued_at: DateTime<Utc>,
            #[serde(default)]
            pub started_at: Option<DateTime<Utc>>,
            #[serde(default)]
            pub using: Option<f64>,
            #[serde(default)]
            pub backfilled: bool,
        }

        #[derive(Debug, Deserialize)]
        pub struct Ledger {
            pub version: u32,
            pub next_seq: u64,
            pub entries: Vec<Entry>,
        }
    }

    fn save_with_sidecars(l: &Ledger, dir: &Path, managed: &[&str]) -> Vec<u8> {
        std::fs::create_dir_all(dir.join("managed")).unwrap();
        for id in managed {
            std::fs::write(dir.join("managed").join(format!("{id}.json")), b"{}").unwrap();
        }
        l.save(&dir.join("ledger.json")).unwrap();
        std::fs::read(dir.join("ledger.json")).unwrap()
    }

    #[test]
    fn an_older_binary_reads_a_ledger_without_managed_entries() {
        let d = tempfile::tempdir().unwrap();
        let mut l = Ledger::default();
        l.entries.push(entry("a", 1));
        let json = save_with_sidecars(&l, d.path(), &[]);
        let old: old::Ledger = serde_json::from_slice(&json).unwrap();
        assert_eq!(old.version, SCHEMA_VERSION);
        assert_eq!(old.entries.len(), 1);
        let json = save_with_sidecars(&Ledger::default(), d.path(), &[]);
        assert!(serde_json::from_slice::<old::Ledger>(&json).is_ok());
    }

    #[test]
    fn an_older_binary_refuses_a_ledger_with_a_managed_entry() {
        let d = tempfile::tempdir().unwrap();
        let mut l = Ledger::default();
        l.entries.push(entry("a", 1));
        l.entries.push(entry("m", 2));
        let json = save_with_sidecars(&l, d.path(), &["m"]);
        let err = serde_json::from_slice::<old::Ledger>(&json).unwrap_err();
        assert!(err.to_string().contains("entries"), "{err}");
        // The current code reads it back whole, and the plain-JSON view
        // (status --json) is unchanged.
        let back = Ledger::load(&d.path().join("ledger.json")).unwrap();
        assert_eq!(back.entries.len(), 2);
        let shown = serde_json::to_value(&back).unwrap();
        assert_eq!(shown["entries"].as_array().unwrap().len(), 2);
        // It drops to the old shape when the managed entry is gone.
        let mut back = back;
        back.remove("m");
        std::fs::remove_file(d.path().join("managed/m.json")).unwrap();
        let json = save_with_sidecars(&back, d.path(), &[]);
        assert!(serde_json::from_slice::<old::Ledger>(&json).is_ok());
    }

    #[test]
    fn a_ledger_with_both_or_neither_entries_key_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("ledger.json");
        for body in [
            r#"{"version":1,"next_seq":1}"#,
            r#"{"version":1,"next_seq":1,"entries":[],"entries_v2":[]}"#,
        ] {
            std::fs::write(&p, body).unwrap();
            assert!(Ledger::load(&p).is_err(), "{body}");
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
