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
    if !per_key.is_empty() {
        for r in history::read_all(ctx.paths.history_file())? {
            if let Some(h) = per_key.get_mut(r.key.as_str()) {
                h.records += 1;
                h.latest = h.latest.max(Some(r.started_at));
            }
        }
    }
    let history: Vec<HistoryRef> = per_key.into_values().collect();

    let snapshot_digest = sha256_of(&(&entries, &history))?;
    let policy_digest = sha256_of(cfg)?;
    Ok(Inspection {
        schema_version: SCHEMA_VERSION,
        taken_at: ctx.clock.now(),
        snapshot_digest,
        policy_digest,
        limits: cfg.limits(),
        live: ctx.live.snapshot(),
        ledger_locked: snap.locked,
        entries,
        history,
    })
}
