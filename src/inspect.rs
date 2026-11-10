//! Read-only view of everything a forecast depends on: the ledger, the
//! policy (configuration), live machine metrics and the history the ledger
//! entries refer to.
//!
//! [`inspect`] never writes. It does not reap dead entries, save the ledger,
//! create leases or jobs, or append history. An entry whose lease is no
//! longer held is returned with `suspect_dead` set and is still counted by
//! the forecast, because a vanished owner does not prove its process tree
//! stopped using the budget.

use crate::admit::{Limits, LiveSnapshot};
use crate::config::Config;
use crate::error::Result;
use crate::ledger::Entry;
use crate::{history, lock, Context};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;

/// A ledger entry as read, with whether its owner still holds the lease.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectedEntry {
    #[serde(flatten)]
    pub entry: Entry,
    pub suspect_dead: bool,
}

/// What the history holds for one command key that a ledger entry uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryRef {
    pub key: String,
    pub records: usize,
    pub latest: Option<DateTime<Utc>>,
}

/// How far the history of one key can be trusted: how many runs, how fresh
/// and how spread out. Kept beside [`HistoryRef`] (not in it) so the
/// snapshot digest of existing data does not move.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryQuality {
    pub key: String,
    pub records: usize,
    pub oldest: Option<DateTime<Utc>>,
    pub latest: Option<DateTime<Utc>>,
    /// Seconds from the latest run to `taken_at` (never negative).
    pub latest_age_secs: Option<i64>,
    pub wall_mean_secs: Option<f64>,
    /// Standard deviation over mean of the wall time; None below 2 runs or
    /// with a zero mean.
    pub wall_cv: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inspection {
    pub schema_version: u32,
    pub taken_at: DateTime<Utc>,
    /// sha256 of the normalised ledger and the history it refers to.
    pub snapshot_digest: String,
    /// sha256 of the effective configuration.
    pub policy_digest: String,
    pub limits: Limits,
    /// Live machine metrics, or None when they could not be read.
    pub live: Option<LiveSnapshot>,
    /// False when the ledger was read without its lock.
    pub ledger_locked: bool,
    pub entries: Vec<InspectedEntry>,
    pub history: Vec<HistoryRef>,
    /// Freshness and spread of the same histories, one per key in `history`.
    #[serde(default)]
    pub history_quality: Vec<HistoryQuality>,
}

fn sha256_of(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

/// Inspect with the configuration file named by `ctx.paths`.
pub fn inspect(ctx: &Context) -> Result<Inspection> {
    let cfg = Config::load(ctx.paths.config_file())?;
    inspect_with(ctx, &cfg)
}

/// Inspect with an already loaded configuration.
pub fn inspect_with(ctx: &Context, cfg: &Config) -> Result<Inspection> {
    let snap = lock::snapshot_read_only(ctx)?;
    let mut entries: Vec<InspectedEntry> = snap
        .ledger
        .entries
        .iter()
        .map(|e| InspectedEntry {
            entry: e.clone(),
            suspect_dead: snap.suspect_dead.contains(&e.id),
        })
        .collect();
    entries.sort_by_key(|e| e.entry.seq);

    let mut per_key: BTreeMap<&str, HistoryRef> = BTreeMap::new();
    for e in &entries {
        per_key.entry(&e.entry.key).or_insert_with(|| HistoryRef {
            key: e.entry.key.clone(),
            records: 0,
            latest: None,
        });
    }
    let mut walls: BTreeMap<String, (Option<DateTime<Utc>>, Vec<f64>)> = BTreeMap::new();
    if !per_key.is_empty() {
        for r in history::read_all(ctx.paths.history_file())? {
            if let Some(h) = per_key.get_mut(r.key.as_str()) {
                h.records += 1;
                h.latest = h.latest.max(Some(r.started_at));
                let w = walls.entry(r.key.clone()).or_default();
                w.0 = Some(w.0.map_or(r.started_at, |o| o.min(r.started_at)));
                w.1.push(r.wall_secs);
            }
        }
    }
    let taken_at = ctx.clock.now();
    let history: Vec<HistoryRef> = per_key.into_values().collect();
    let history_quality: Vec<HistoryQuality> = history
        .iter()
        .map(|h| {
            let (oldest, w) = walls.get(&h.key).cloned().unwrap_or_default();
            let n = w.len() as f64;
            let mean = (!w.is_empty()).then(|| w.iter().sum::<f64>() / n);
            let cv = match mean {
                Some(m) if w.len() >= 2 && m > 0.0 => {
                    let var = w.iter().map(|x| (x - m).powi(2)).sum::<f64>() / n;
                    Some(var.sqrt() / m)
                }
                _ => None,
            };
            HistoryQuality {
                key: h.key.clone(),
                records: h.records,
                oldest,
                latest: h.latest,
                latest_age_secs: h.latest.map(|l| (taken_at - l).num_seconds().max(0)),
                wall_mean_secs: mean,
                wall_cv: cv,
            }
        })
        .collect();

    let snapshot_digest = sha256_of(&(&entries, &history))?;
    let policy_digest = sha256_of(cfg)?;
    Ok(Inspection {
        schema_version: SCHEMA_VERSION,
        taken_at,
        snapshot_digest,
        policy_digest,
        limits: cfg.limits(),
        live: ctx.live.snapshot(),
        ledger_locked: snap.locked,
        entries,
        history,
        history_quality,
    })
}
