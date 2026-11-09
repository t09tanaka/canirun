//! Forecast for one command that has not been submitted: what `exec` would
//! derive for it (key, weight, memory, learned wall time), as a
//! single-node [`WorkloadPlan`].
//!
//! Reads configuration, history and the git context of `opts.cwd`; writes
//! nothing.

use crate::config::Config;
use crate::error::Result;
use crate::exec::{self, Opts};
use crate::forecast::{CommandNode, Priority, Runtime, RuntimeSource, WorkloadPlan};
use crate::{history, Context};
use sha2::{Digest, Sha256};

/// The single-command plan `exec` would run, ready for
/// [`crate::forecast::forecast`].
pub fn command_plan(ctx: &Context, cfg: &Config, opts: &Opts) -> Result<WorkloadPlan> {
    let plan = exec::plan(ctx, cfg, opts)?;
    let recs = history::for_key(ctx.paths.history_file(), &plan.key.hash)?;
    let runtime = Runtime {
        secs: plan.expected_secs,
        source: if plan.expected_secs.is_some() {
            RuntimeSource::Measured
        } else {
            RuntimeSource::Fallback
        },
        history_count: recs.len(),
        latest_sample: recs.iter().map(|r| r.started_at).max(),
    };
    let priority = if opts.front {
        Priority::Front
    } else {
        Priority::Normal
    };
    let digest_input = serde_json::to_vec(&(
        &plan.key.hash,
        plan.weight,
        plan.predicted_rss,
        &runtime,
        priority,
    ))?;
    let node = CommandNode {
        node_id: "command".into(),
        identity: plan.key.hash.clone(),
        label: plan.key.argv.clone(),
        weight: plan.weight,
        predicted_rss: plan.predicted_rss,
        runtime,
        priority,
        depends_on: Vec::new(),
        ready_in_ms: 0,
        external_delay_ms: 0,
        ledger_entry: None,
        shared_id: None,
    };
    Ok(WorkloadPlan::single(
        "estimate",
        &format!("sha256:{:x}", Sha256::digest(digest_input)),
        node,
    ))
}
