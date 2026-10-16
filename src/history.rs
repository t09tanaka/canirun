//! Append-only measurement history (`history.jsonl`).

use crate::error::Result;
use crate::weight::Measurement;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub key: String,
    pub repo: String,
    pub argv: String,
    pub started_at: DateTime<Utc>,
    pub wall_secs: f64,
    pub cpu_secs: f64,
    pub max_rss_bytes: u64,
    pub exit_code: i32,
    pub weight_used: u32,
}

impl Record {
    pub fn measurement(&self) -> Measurement {
        Measurement {
            wall_secs: self.wall_secs,
            cpu_secs: self.cpu_secs,
            max_rss_bytes: self.max_rss_bytes,
        }
    }
}

pub fn append(path: &Path, rec: &Record) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut line = serde_json::to_string(rec)?;
    line.push('\n');
    f.write_all(line.as_bytes())?;
    Ok(())
}

/// All records, oldest first. Malformed lines are skipped.
pub fn read_all(path: &Path) -> Result<Vec<Record>> {
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(r) = serde_json::from_str::<Record>(&line) {
            out.push(r);
        }
    }
    Ok(out)
}

pub fn for_key(path: &Path, key: &str) -> Result<Vec<Record>> {
    Ok(read_all(path)?
        .into_iter()
        .filter(|r| r.key == key)
        .collect())
}

/// Keep only the newest `keep` records per key. Returns records dropped.
pub fn trim(path: &Path, keep: usize) -> Result<usize> {
    let all = read_all(path)?;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for r in &all {
        *counts.entry(r.key.as_str()).or_default() += 1;
    }
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut kept = Vec::new();
    for r in &all {
        let n = seen.entry(r.key.as_str()).or_default();
        *n += 1;
        if counts[r.key.as_str()] - *n < keep {
            kept.push(r);
        }
    }
    let dropped = all.len() - kept.len();
    if dropped > 0 {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        for r in &kept {
            let mut line = serde_json::to_string(r)?;
            line.push('\n');
            tmp.write_all(line.as_bytes())?;
        }
        tmp.persist(path).map_err(|e| e.error)?;
    }
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(key: &str, wall: f64) -> Record {
        Record {
            key: key.into(),
            repo: String::new(),
            argv: "x".into(),
            started_at: Utc::now(),
            wall_secs: wall,
            cpu_secs: 1.0,
            max_rss_bytes: 1,
            exit_code: 0,
            weight_used: 1,
        }
    }

    #[test]
    fn append_read_filter() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("h/history.jsonl");
        append(&p, &rec("a", 1.0)).unwrap();
        append(&p, &rec("b", 2.0)).unwrap();
        append(&p, &rec("a", 3.0)).unwrap();
        let a = for_key(&p, "a").unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[1].wall_secs, 3.0);
        assert!(read_all(&d.path().join("missing")).unwrap().is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("history.jsonl");
        std::fs::write(&p, "garbage\n\n").unwrap();
        append(&p, &rec("a", 1.0)).unwrap();
        assert_eq!(read_all(&p).unwrap().len(), 1);
    }

    #[test]
    fn trim_keeps_newest_per_key() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("history.jsonl");
        for i in 0..5 {
            append(&p, &rec("a", i as f64)).unwrap();
        }
        append(&p, &rec("b", 9.0)).unwrap();
        assert_eq!(trim(&p, 2).unwrap(), 3);
        let a = for_key(&p, "a").unwrap();
        assert_eq!(
            a.iter().map(|r| r.wall_secs).collect::<Vec<_>>(),
            [3.0, 4.0]
        );
        assert_eq!(for_key(&p, "b").unwrap().len(), 1);
        assert_eq!(trim(&p, 2).unwrap(), 0);
    }
}
