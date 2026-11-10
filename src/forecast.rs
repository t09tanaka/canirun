//! Side-effect-free forecast: when would this workload start and finish if
//! it were added to the queue now?
//!
//! A discrete-event simulation of the ledger snapshot, the overlay of
//! demands that are not in the ledger yet, and the target plan. It asks the
//! same [`admit`] function the runtime asks, with the same limits, charges
//! and queue ages, so FIFO, `--front`, backfill and the budget behave as
//! they do for a real run. A node enters the virtual queue when it becomes
//! ready, not before.
//!
//! Live guards (memory, CPU, overload) are evaluated once, against the
//! metrics in the [`Inspection`]; after that the machine is assumed to add
//! no new external load. Whatever depends on that is reported as
//! `conditional`. Values that cannot be computed are `null` with a reason
//! in `null_reasons`; nothing unknown is read as 0 seconds or free budget.
//! A runtime that is only the default fallback never appears as a measured
//! value: it is returned under `ranking`, for ordering candidates only.
//!
//! Times are milliseconds from the snapshot (`Inspection::taken_at`).

use crate::admit::{admit, queue_order, Decision, Limits, LiveSnapshot, QueueEntry, Reason, State};
use crate::error::{Error, Result};
use crate::eta::UNKNOWN_SECS;
use crate::exec::describe;
use crate::inspect::Inspection;
use crate::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const SCHEMA_VERSION: u32 = 1;
/// How long a forecast stays valid.
pub const TTL_MS: u64 = 2000;
/// History older than this many days is reported as stale.
pub const STALE_HISTORY_DAYS: i64 = 30;
/// Runtime used to rank candidates when nothing was measured.
pub const FALLBACK_MS: u64 = (UNKNOWN_SECS * 1000.0) as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSource {
    /// No usable number; the node never finishes inside the simulation.
    Unknown,
    /// Only the default; usable for ranking, never reported as measured.
    Fallback,
    /// Learned from recorded runs.
    Measured,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Runtime {
    pub secs: Option<f64>,
    pub source: RuntimeSource,
    pub history_count: usize,
    pub latest_sample: Option<DateTime<Utc>>,
}

impl Runtime {
    pub fn measured(secs: f64, history_count: usize) -> Self {
        Runtime {
            secs: Some(secs),
            source: RuntimeSource::Measured,
            history_count,
            latest_sample: None,
        }
    }

    pub fn fallback() -> Self {
        Runtime {
            secs: None,
            source: RuntimeSource::Fallback,
            history_count: 0,
            latest_sample: None,
        }
    }

    pub fn unknown() -> Self {
        Runtime {
            secs: None,
            source: RuntimeSource::Unknown,
            history_count: 0,
            latest_sample: None,
        }
    }

    /// Simulated duration in ms, and whether it is a measured value.
    fn duration_ms(&self) -> (Option<u64>, bool) {
        match (self.source, self.secs) {
            (RuntimeSource::Measured, Some(s)) if s >= 0.0 => ((Some((s * 1000.0) as u64)), true),
            (RuntimeSource::Unknown, _) => (None, false),
            _ => (Some(FALLBACK_MS), false),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Normal,
    Front,
}

/// One command that will be admitted through task-traffic-controller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandNode {
    pub node_id: String,
    /// History identity (the command key hash).
    pub identity: String,
    pub label: String,
    pub weight: u32,
    /// Predicted peak memory; 0 when unknown.
    pub predicted_rss: u64,
    pub runtime: Runtime,
    pub priority: Priority,
    /// Nodes that must finish first.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Earliest registration time, ms from now.
    #[serde(default)]
    pub ready_in_ms: u64,
    /// Preparation outside admission (transfer, ...), starting when the
    /// dependencies are done (or now, without any).
    #[serde(default)]
    pub external_delay_ms: u64,
    /// The ledger entry this node registered as, when it already did.
    #[serde(default)]
    pub ledger_entry: Option<String>,
    /// Nodes of one plan sharing this id are one piece of work.
    #[serde(default)]
    pub shared_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkloadPlan {
    pub attempt_id: String,
    pub plan_digest: String,
    #[serde(default)]
    pub expansion_seq: u64,
    /// How many of the plan's nodes the executor keeps registered (waiting
    /// or running) at once; None for no limit.
    #[serde(default)]
    pub max_in_flight: Option<usize>,
    pub nodes: Vec<CommandNode>,
}

/// Work somebody else already committed to run here but that the ledger
/// does not show yet (accepted but unregistered, or sent but unacked).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FutureDemand {
    pub attempt_id: String,
    pub plan_digest: String,
    pub expansion_seq: u64,
    pub node_id: String,
    pub weight: u32,
    #[serde(default)]
    pub predicted_rss: u64,
    pub runtime: Runtime,
    pub priority: Priority,
    #[serde(default)]
    pub ready_in_ms: u64,
    /// The ledger entry it registers as. Counted from the ledger instead
    /// as soon as the inspected ledger has it.
    #[serde(default)]
    pub ledger_entry: Option<String>,
    #[serde(default)]
    pub shared_id: Option<String>,
    /// Confirmed finished: no longer demand.
    #[serde(default)]
    pub terminal: bool,
}

impl FutureDemand {
    fn key(&self) -> (String, String, u64, String) {
        (
            self.attempt_id.clone(),
            self.plan_digest.clone(),
            self.expansion_seq,
            self.node_id.clone(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    None,
    Low,
    Medium,
    High,
}

impl Confidence {
    fn lowered(self) -> Self {
        match self {
            Confidence::High => Confidence::Medium,
            Confidence::Medium => Confidence::Low,
            other => other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitReason {
    pub kind: String,
    pub detail: String,
}

/// Fallback-based numbers, for ordering candidates only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ranking {
    pub start_in_ms: Option<u64>,
    pub finish_in_ms: Option<u64>,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timing {
    pub ready_in_ms: Option<u64>,
    pub start_in_ms: Option<u64>,
    pub queue_wait_after_ready_ms: Option<u64>,
    pub execution_ms: Option<u64>,
    pub finish_in_ms: Option<u64>,
    /// Why each null above is null.
    pub null_reasons: BTreeMap<String, String>,
    pub ranking: Option<Ranking>,
    pub confidence: Confidence,
    pub runtime_source: RuntimeSource,
    pub history_count: usize,
    pub conditional: bool,
    pub assumptions: Vec<String>,
    /// What admission says now (None: it could start immediately).
    pub wait_reason: Option<WaitReason>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeForecast {
    pub node_id: String,
    #[serde(flatten)]
    pub timing: Timing,
}

/// The plan as a whole: `ready_in_ms` and `start_in_ms` are the earliest
/// over the nodes, `finish_in_ms` the latest. A null node value makes the
/// aggregate null.
///
/// A sum of node times is not a time on the clock once nodes can run side
/// by side (four 20 s nodes in parallel finish after 20 s, not 80 s), so
/// for a plan of two or more nodes `queue_wait_after_ready_ms` and
/// `execution_ms` are null, with the reason in `null_reasons`. What the
/// plan reports instead is its critical path: starting from the node that
/// finishes last, `critical_path` follows what decided each start back in
/// time (the dependency that finished last, or for a node without
/// dependencies the plan node whose finish freed the budget it started on;
/// it stops where neither exists) and lists those nodes in running order.
/// `critical_path_execution_ms` adds up their execution times and
/// `critical_path_wait_ms` their queue waits after becoming ready, except
/// that a wait spent behind the next path node is already part of that
/// node's execution and is not counted twice. Together with any ready
/// floor or external delay on the path they make up `finish_in_ms`. For a
/// single node the two values equal that node's own. A null value on the
/// path makes the matching total null (reason in `null_reasons`); a node
/// that never finishes in the simulation leaves the path empty and both
/// totals null.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanForecast {
    pub attempt_id: String,
    pub plan_digest: String,
    pub expansion_seq: u64,
    #[serde(flatten)]
    pub timing: Timing,
    #[serde(default)]
    pub critical_path: Vec<String>,
    #[serde(default)]
    pub critical_path_wait_ms: Option<u64>,
    #[serde(default)]
    pub critical_path_execution_ms: Option<u64>,
    pub nodes: Vec<NodeForecast>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageItem {
    pub attempt_id: String,
    pub plan_digest: String,
    pub expansion_seq: u64,
    pub node_ids: Vec<String>,
}

/// What happened to each overlay demand (`attempt_id/node_id`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayReport {
    pub applied: Vec<String>,
    pub replaced_by_ledger: Vec<String>,
    pub terminal_removed: Vec<String>,
    pub duplicates_dropped: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Forecast {
    pub schema_version: u32,
    pub snapshot_digest: String,
    pub policy_digest: String,
    pub ttl_ms: u64,
    pub coverage: Vec<CoverageItem>,
    pub overlay: OverlayReport,
    pub target: PlanForecast,
}

impl WorkloadPlan {
    /// A plan with a single node and nothing else.
    pub fn single(attempt_id: &str, plan_digest: &str, node: CommandNode) -> Self {
        WorkloadPlan {
            attempt_id: attempt_id.into(),
            plan_digest: plan_digest.into(),
            expansion_seq: 0,
            max_in_flight: None,
            nodes: vec![node],
        }
    }

    /// Unique ids, known dependencies, no cycle.
    pub fn validate(&self) -> Result<()> {
        let mut ids = HashSet::new();
        for n in &self.nodes {
            if !ids.insert(n.node_id.as_str()) {
                return Err(Error::Plan(format!("duplicate node id {}", n.node_id)));
            }
        }
        for n in &self.nodes {
            for d in &n.depends_on {
                if !ids.contains(d.as_str()) {
                    return Err(Error::Plan(format!(
                        "node {} depends on unknown node {d}",
                        n.node_id
                    )));
                }
            }
        }
        // Each pass settles at least one node unless there is a cycle.
        let mut done: HashSet<&str> = HashSet::new();
        for _ in 0..self.nodes.len() {
            for n in &self.nodes {
                if n.depends_on.iter().all(|d| done.contains(d.as_str())) {
                    done.insert(&n.node_id);
                }
            }
        }
        if done.len() < self.nodes.len() {
            return Err(Error::Plan("dependency cycle".into()));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Simulation
// ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum St {
    Pending,
    Waiting,
    Running,
    Done,
}

struct Job {
    id: String,
    weight: u32,
    front: bool,
    rss: u64,
    measured: bool,
    /// None: never finishes.
    dur_ms: Option<u64>,
    /// Learned wall time as backfill sees it (measured only).
    expected_secs: Option<f64>,
    deps: Vec<usize>,
    floor_ms: u64,
    delay_ms: u64,
    in_plan: bool,
    seq: u64,
    using: Option<f64>,
    backfilled: bool,
    st: St,
    enqueue_ms: i64,
    ready_ms: Option<u64>,
    start_ms: Option<u64>,
    run_finish: Option<u64>,
    finish_ms: Option<u64>,
    t0_wait: Option<Reason>,
    live_hold: bool,
    /// Why `dur_ms` is None.
    unknown_note: Option<String>,
    /// Recorded runs behind the runtime.
    history_count: usize,
    /// Read from the ledger rather than created by the forecast.
    from_ledger: bool,
}

impl Job {
    fn new(id: String, weight: u32, rss: u64, front: bool, runtime: &Runtime) -> Job {
        let (dur_ms, measured) = runtime.duration_ms();
        Job {
            id,
            weight,
            front,
            rss,
            measured,
            dur_ms,
            expected_secs: if measured { runtime.secs } else { None },
            deps: Vec::new(),
            floor_ms: 0,
            delay_ms: 0,
            in_plan: false,
            seq: 0,
            using: None,
            backfilled: false,
            st: St::Pending,
            enqueue_ms: 0,
            ready_ms: None,
            start_ms: None,
            run_finish: None,
            finish_ms: None,
            t0_wait: None,
            live_hold: false,
            unknown_note: if dur_ms.is_none() {
                Some("runtime unknown".into())
            } else {
                None
            },
            history_count: runtime.history_count,
            from_ledger: false,
        }
    }
}

fn ready_at(jobs: &[Job], i: usize) -> Option<u64> {
    let j = &jobs[i];
    let mut dep_done = 0;
    for &d in &j.deps {
        if jobs[d].st != St::Done {
            return None;
        }
        dep_done = dep_done.max(jobs[d].finish_ms?);
    }
    Some(j.floor_ms.max(dep_done + j.delay_ms))
}

/// Move every pending job that is ready at `now` (and fits the in-flight
/// limit) into the queue, in plan order. Releasing a job only ever makes
/// the limit tighter, so one pass settles it.
fn release(jobs: &mut [Job], now: u64, cap: Option<usize>, next_seq: &mut u64) {
    for i in 0..jobs.len() {
        if jobs[i].st != St::Pending {
            continue;
        }
        let Some(at) = ready_at(jobs, i) else {
            continue;
        };
        if at > now {
            continue;
        }
        let full = cap.is_some_and(|c| {
            jobs[i].in_plan
                && jobs
                    .iter()
                    .filter(|j| j.in_plan && matches!(j.st, St::Waiting | St::Running))
                    .count()
                    >= c
        });
        if full {
            continue;
        }
        let j = &mut jobs[i];
        j.st = St::Waiting;
        j.enqueue_ms = now as i64;
        j.ready_ms = Some(at);
        j.seq = *next_seq;
        *next_seq += 1;
    }
}

fn queue_entries(jobs: &[Job], now: u64) -> Vec<QueueEntry> {
    jobs.iter()
        .filter(|j| matches!(j.st, St::Waiting | St::Running))
        .map(|j| {
            let running = j.st == St::Running;
            QueueEntry {
                id: j.id.clone(),
                state: if running {
                    State::Running
                } else {
                    State::Waiting
                },
                front: j.front,
                weight: j.weight,
                seq: j.seq,
                using: if running { j.using } else { None },
                waited_secs: if running {
                    0.0
                } else {
                    (now as i64 - j.enqueue_ms).max(0) as f64 / 1000.0
                },
                expected_secs: j.expected_secs,
                backfilled: j.backfilled,
            }
        })
        .collect()
}

fn is_live_reason(r: &Reason) -> bool {
    matches!(
        r,
        Reason::Memory { .. } | Reason::Cpu { .. } | Reason::Overload { .. }
    )
}

/// Passes of the event loop that always suffice: each pass retires a running
/// job or releases a pending one.
fn iteration_cap(jobs: usize) -> usize {
    jobs * 4 + 16
}

fn simulate(
    jobs: &mut [Job],
    limits: Limits,
    live: Option<LiveSnapshot>,
    cap: Option<usize>,
    mut next_seq: u64,
) {
    let limits = Limits {
        budget: limits.budget.max(1),
        ..limits
    };
    let index: HashMap<String, usize> = jobs
        .iter()
        .enumerate()
        .map(|(i, j)| (j.id.clone(), i))
        .collect();
    let mut now: u64 = 0;
    for _ in 0..iteration_cap(jobs.len()) {
        release(jobs, now, cap, &mut next_seq);
        for _ in 0..=jobs.len() {
            let entries = queue_entries(jobs, now);
            let order = queue_order(&entries);
            let live_now = if now == 0 { live } else { None };
            let mut started = None;
            let mut holds = Vec::new();
            for (pos, q) in order.iter().enumerate() {
                let i = index[&q.id];
                match admit(
                    &entries,
                    &q.id,
                    jobs[i].rss,
                    q.waited_secs,
                    limits,
                    live_now,
                ) {
                    Decision::Admitted => {
                        started = Some((i, pos));
                        break;
                    }
                    Decision::Wait(r) => holds.push((i, r)),
                }
            }
            if now == 0 {
                for (i, r) in holds {
                    if is_live_reason(&r) {
                        jobs[i].live_hold = true;
                    }
                    jobs[i].t0_wait = Some(r);
                }
            }
            let Some((i, pos)) = started else {
                break;
            };
            let j = &mut jobs[i];
            j.st = St::Running;
            j.start_ms = Some(now);
            j.run_finish = j.dur_ms.map(|d| now + d);
            j.backfilled = pos > 0;
            if now == 0 {
                j.t0_wait = None;
            }
        }
        let next = jobs
            .iter()
            .enumerate()
            .filter_map(|(i, j)| match j.st {
                St::Running => j.run_finish,
                St::Pending => ready_at(jobs, i).filter(|t| *t > now),
                _ => None,
            })
            .min();
        let Some(t) = next else {
            break;
        };
        now = now.max(t);
        for j in jobs.iter_mut() {
            if j.st == St::Running && j.run_finish.is_some_and(|f| f <= now) {
                j.st = St::Done;
                j.finish_ms = j.run_finish;
            }
        }
    }
}

// ---------------------------------------------------------------------
// Forecast
// ---------------------------------------------------------------------

fn wait_reason(r: &Reason) -> WaitReason {
    let kind = match r {
        Reason::NotHead { .. } => "queue",
        Reason::Reserved { .. } => "reserved",
        Reason::Budget { .. } => "budget",
        Reason::Memory { .. } => "memory",
        Reason::Cpu { .. } => "cpu",
        Reason::Overload { .. } => "overload",
    };
    WaitReason {
        kind: kind.into(),
        detail: describe(r),
    }
}

struct Globals {
    stale_snapshot: bool,
    unknown_running: usize,
    suspect_dead: usize,
    live_missing: bool,
    any_live_hold: bool,
    overlay_applied: usize,
    /// Ledger entries whose queue or run age is read off their wall-clock
    /// (UTC) stamps: an entry carries no monotonic time.
    legacy_clock_entries: usize,
}

impl Globals {
    fn conditions(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.stale_snapshot {
            v.push(format!("snapshot older than the {TTL_MS} ms ttl"));
        }
        if self.unknown_running > 0 {
            v.push(format!(
                "{} running entries have unknown remaining time (overrun, unknown runtime or owner gone); assumed to keep their budget",
                self.unknown_running
            ));
        }
        if self.suspect_dead > 0 {
            v.push(format!(
                "{} ledger entries have no live lease; still counted, not treated as free",
                self.suspect_dead
            ));
        }
        if self.live_missing {
            v.push("live machine metrics unavailable; memory and cpu guards not evaluated".into());
        }
        if self.any_live_hold {
            v.push(
                "a live guard holds the queue now; assumed to clear as running entries finish"
                    .into(),
            );
        }
        if self.overlay_applied > 0 {
            v.push(format!(
                "includes {} demands that are not in the ledger yet",
                self.overlay_applied
            ));
        }
        v
    }
}

struct NodeMeta<'a> {
    runtime: &'a Runtime,
    rss: u64,
    registered: Option<&'a str>,
}

fn timing_of(
    jobs: &[Job],
    i: usize,
    meta: &NodeMeta,
    g: &Globals,
    now: DateTime<Utc>,
    others_running_at_start: bool,
) -> Timing {
    let j = &jobs[i];
    let mut nulls = BTreeMap::new();
    let mut assumptions = vec!["no new external load".to_string()];
    assumptions.extend(g.conditions());
    let mut conditional = assumptions.len() > 1;
    if g.legacy_clock_entries > 0 {
        assumptions.push(format!(
            "legacy_clock: the age of {} ledger entries is estimated from their UTC timestamps, \
             which a wall-clock step can skew",
            g.legacy_clock_entries
        ));
    }

    if let Some(id) = meta.registered {
        assumptions.push(format!("already registered as ledger entry {id}"));
    }
    if meta.rss == 0 && others_running_at_start {
        assumptions.push(
            "predicted memory unknown; the memory guard was not assessed for this node".into(),
        );
        conditional = true;
    }
    if meta.runtime.source == RuntimeSource::Measured {
        if let Some(l) = meta.runtime.latest_sample {
            if (now - l).num_days() > STALE_HISTORY_DAYS {
                assumptions.push(format!(
                    "latest history sample is older than {STALE_HISTORY_DAYS} days"
                ));
                conditional = true;
            }
        }
    }
    if j.live_hold {
        assumptions.push("a live guard holds this node at snapshot time".into());
        conditional = true;
    }

    let start = j.start_ms;
    // Ledger entries whose finish opened the budget this node started on:
    // a start that rests on an entry with little history is as uncertain
    // as the entry's runtime.
    let mut history_floor = meta.runtime.history_count;
    if let Some(s) = start {
        for y in jobs {
            if y.from_ledger && y.finish_ms == Some(s) {
                history_floor = history_floor.min(y.history_count);
                if y.history_count < 3 {
                    assumptions.push(format!(
                        "start follows ledger entry {} with only {} recorded runs",
                        y.id, y.history_count
                    ));
                }
            }
        }
    }
    let tainted_start = start.is_some_and(|s| {
        jobs.iter()
            .enumerate()
            .any(|(k, y)| k != i && !y.measured && y.finish_ms.is_some_and(|f| f <= s))
    });
    let measured_start = start.filter(|_| !tainted_start);
    let ready = j.ready_ms;
    if ready.is_none() {
        nulls.insert(
            "ready_in_ms".to_string(),
            "a dependency does not finish in the simulation".to_string(),
        );
    }
    if start.is_none() {
        let why = if ready.is_none() {
            "not ready: a dependency does not finish in the simulation"
        } else {
            "no admission opening: held by running entries with unknown remaining time"
        };
        nulls.insert("start_in_ms".into(), why.into());
    } else if tainted_start {
        nulls.insert(
            "start_in_ms".into(),
            "depends on runtimes that were not measured; see ranking".into(),
        );
    }
    let queue_wait = match (measured_start, ready) {
        (Some(s), Some(r)) => Some(s.saturating_sub(r)),
        _ => None,
    };
    if queue_wait.is_none() {
        nulls.insert(
            "queue_wait_after_ready_ms".into(),
            "start or ready time not known as measured".into(),
        );
    }
    let exec = if j.measured { j.dur_ms } else { None };
    if exec.is_none() {
        let why = j
            .unknown_note
            .clone()
            .unwrap_or_else(|| "runtime not measured (fallback default); see ranking".into());
        nulls.insert("execution_ms".into(), why);
    }
    let finish = match (measured_start, exec, j.finish_ms) {
        (Some(_), Some(_), Some(f)) => Some(f),
        _ => None,
    };
    if finish.is_none() {
        nulls.insert(
            "finish_in_ms".into(),
            "start or runtime not known as measured".into(),
        );
    }
    let ranking = if start.is_some() && (tainted_start || !j.measured) {
        Some(Ranking {
            start_in_ms: start,
            finish_in_ms: j.finish_ms,
            note: format!(
                "uses a default runtime of {} s where nothing was measured; for ordering candidates only, not an ETA",
                FALLBACK_MS / 1000
            ),
        })
    } else {
        None
    };

    let mut confidence = if finish.is_some() {
        match history_floor {
            0..=2 => Confidence::Medium,
            _ => Confidence::High,
        }
    } else if ranking.is_some() {
        Confidence::Low
    } else {
        Confidence::None
    };
    if conditional {
        confidence = confidence.lowered();
    }
    let wait_reason = if j.ready_ms == Some(0) || j.start_ms == Some(0) {
        j.t0_wait.as_ref().map(wait_reason)
    } else {
        Some(WaitReason {
            kind: "not_ready".into(),
            detail: "not ready to register yet".into(),
        })
    };
    Timing {
        ready_in_ms: ready,
        start_in_ms: measured_start,
        queue_wait_after_ready_ms: queue_wait,
        execution_ms: exec,
        finish_in_ms: finish,
        null_reasons: nulls,
        ranking,
        confidence,
        runtime_source: meta.runtime.source,
        history_count: meta.runtime.history_count,
        conditional,
        assumptions,
        wait_reason,
    }
}

fn sum_all(it: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    let mut total = 0u64;
    for v in it {
        total = total.checked_add(v?)?;
    }
    Some(total)
}

/// What the simulation says about one plan node, for the critical path.
struct PathInfo {
    /// Indices (into the plan's nodes) of the dependencies.
    deps: Vec<usize>,
    ready: Option<u64>,
    start: Option<u64>,
    finish: Option<u64>,
}

struct CriticalPath {
    nodes: Vec<String>,
    wait_ms: Option<u64>,
    execution_ms: Option<u64>,
    null_reasons: BTreeMap<String, String>,
}

/// Walk back from the node that finishes last (see [`PlanForecast`]).
fn critical_path(nodes: &[NodeForecast], info: &[PathInfo]) -> CriticalPath {
    let mut out = CriticalPath {
        nodes: Vec::new(),
        wait_ms: Some(0),
        execution_ms: Some(0),
        null_reasons: BTreeMap::new(),
    };
    if nodes.is_empty() {
        return out;
    }
    let finishes: Option<Vec<u64>> = info.iter().map(|n| n.finish).collect();
    let Some(finishes) = finishes else {
        let why = "a node does not finish in the simulation";
        out.wait_ms = None;
        out.execution_ms = None;
        out.null_reasons
            .insert("critical_path_wait_ms".into(), why.into());
        out.null_reasons
            .insert("critical_path_execution_ms".into(), why.into());
        return out;
    };
    let mut cur = 0;
    for (k, f) in finishes.iter().enumerate() {
        if *f > finishes[cur] {
            cur = k;
        }
    }
    // (node, its wait is covered by the next node on the path)
    let mut path: Vec<(usize, bool)> = Vec::new();
    let mut visited = vec![false; nodes.len()];
    // At most one step per node: the visited checks already guarantee it,
    // the bound only keeps a read-only forecast from hanging if they ever
    // fail.
    for _ in 0..nodes.len() {
        visited[cur] = true;
        let n = &info[cur];
        let mut pred = None;
        for &d in &n.deps {
            // A node already on the path must not be chosen again: after a
            // resource-wait jump the new node's dependency can point back
            // at a visited node, and the walk would never end.
            if !visited[d] && pred.is_none_or(|p: usize| finishes[d] > finishes[p]) {
                pred = Some(d);
            }
        }
        let mut covered = false;
        // Every node on a finished path has started, so `start` is known.
        if pred.is_none() && n.start > n.ready {
            let s = n.start.unwrap_or(0);
            pred = (0..nodes.len()).find(|&k| !visited[k] && finishes[k] == s);
            covered = pred.is_some();
        }
        path.push((cur, covered));
        // Neither a dependency nor the search (both unvisited only) can
        // lead back to a node already on the path.
        match pred {
            Some(p) => cur = p,
            None => break,
        }
    }
    path.reverse();
    let mut wait = Some(0u64);
    let mut exec = Some(0u64);
    let mut why_wait = Vec::new();
    let mut why_exec = Vec::new();
    for &(k, covered) in &path {
        let t = &nodes[k].timing;
        out.nodes.push(nodes[k].node_id.clone());
        if !covered {
            match t.queue_wait_after_ready_ms {
                Some(w) => wait = wait.and_then(|a| a.checked_add(w)),
                None => {
                    wait = None;
                    why_wait.push(null_reason(&nodes[k], "queue_wait_after_ready_ms"));
                }
            }
        }
        match t.execution_ms {
            Some(x) => exec = exec.and_then(|a| a.checked_add(x)),
            None => {
                exec = None;
                why_exec.push(null_reason(&nodes[k], "execution_ms"));
            }
        }
    }
    out.wait_ms = wait;
    out.execution_ms = exec;
    if wait.is_none() {
        out.null_reasons
            .insert("critical_path_wait_ms".into(), why_wait.join("; "));
    }
    if exec.is_none() {
        out.null_reasons
            .insert("critical_path_execution_ms".into(), why_exec.join("; "));
    }
    out
}

fn null_reason(n: &NodeForecast, key: &str) -> String {
    format!(
        "{}: {}",
        n.node_id,
        n.timing
            .null_reasons
            .get(key)
            .map_or("unknown", String::as_str)
    )
}

fn summarize(nodes: &[NodeForecast]) -> Timing {
    let t = |f: fn(&Timing) -> Option<u64>| nodes.iter().map(move |n| f(&n.timing));
    let mut nulls = BTreeMap::new();
    let mut agg = |name: &str, v: Option<u64>| {
        if v.is_none() {
            let why: Vec<String> = nodes
                .iter()
                .filter_map(|n| {
                    n.timing
                        .null_reasons
                        .get(name)
                        .map(|r| format!("{}: {r}", n.node_id))
                })
                .collect();
            nulls.insert(name.to_string(), why.join("; "));
        }
        v
    };
    let min_of = |v: Vec<Option<u64>>| {
        v.into_iter()
            .collect::<Option<Vec<_>>>()
            .and_then(|v| v.into_iter().min())
    };
    let max_of = |v: Vec<Option<u64>>| {
        v.into_iter()
            .collect::<Option<Vec<_>>>()
            .and_then(|v| v.into_iter().max())
    };
    let ready = agg("ready_in_ms", min_of(t(|x| x.ready_in_ms).collect()));
    let start = agg("start_in_ms", min_of(t(|x| x.start_in_ms).collect()));
    // Several nodes may run side by side: their sum is not a clock value.
    let parallel = nodes.len() >= 2;
    let wait = if parallel {
        None
    } else {
        agg(
            "queue_wait_after_ready_ms",
            sum_all(t(|x| x.queue_wait_after_ready_ms)),
        )
    };
    let exec = if parallel {
        None
    } else {
        agg("execution_ms", sum_all(t(|x| x.execution_ms)))
    };
    let finish = agg("finish_in_ms", max_of(t(|x| x.finish_in_ms).collect()));
    if parallel {
        for name in ["queue_wait_after_ready_ms", "execution_ms"] {
            nulls.insert(
                name.to_string(),
                "a plan with several nodes may run them in parallel, so a sum is not a time on the clock; see critical_path_*".to_string(),
            );
        }
    }
    let ranking = if nodes.iter().any(|n| n.timing.ranking.is_some()) {
        let all = |f: fn(&Ranking) -> Option<u64>| {
            nodes
                .iter()
                .map(move |n| match &n.timing.ranking {
                    Some(r) => f(r),
                    None => f(&Ranking {
                        start_in_ms: n.timing.start_in_ms,
                        finish_in_ms: n.timing.finish_in_ms,
                        note: String::new(),
                    }),
                })
                .collect::<Option<Vec<_>>>()
        };
        Some(Ranking {
            start_in_ms: all(|r| r.start_in_ms).and_then(|v| v.into_iter().min()),
            finish_in_ms: all(|r| r.finish_in_ms).and_then(|v| v.into_iter().max()),
            note: nodes
                .iter()
                .find_map(|n| n.timing.ranking.as_ref().map(|r| r.note.clone()))
                .unwrap_or_default(),
        })
    } else {
        None
    };
    let mut assumptions: Vec<String> = Vec::new();
    for n in nodes {
        for a in &n.timing.assumptions {
            if !assumptions.contains(a) {
                assumptions.push(a.clone());
            }
        }
    }
    Timing {
        ready_in_ms: ready,
        start_in_ms: start,
        queue_wait_after_ready_ms: wait,
        execution_ms: exec,
        finish_in_ms: finish,
        null_reasons: nulls,
        ranking,
        confidence: nodes
            .iter()
            .map(|n| n.timing.confidence)
            .min()
            .unwrap_or(Confidence::None),
        runtime_source: nodes
            .iter()
            .map(|n| n.timing.runtime_source)
            .min()
            .unwrap_or(RuntimeSource::Unknown),
        history_count: nodes
            .iter()
            .map(|n| n.timing.history_count)
            .min()
            .unwrap_or(0),
        conditional: nodes.iter().any(|n| n.timing.conditional),
        assumptions,
        wait_reason: nodes.iter().find_map(|n| n.timing.wait_reason.clone()),
    }
}

fn ledger_job(
    e: &crate::inspect::InspectedEntry,
    taken_at: DateTime<Utc>,
    history_count: usize,
) -> Job {
    let entry = &e.entry;
    let running = entry.state == State::Running;
    let rt = Runtime {
        secs: entry.expected_secs,
        source: if entry.expected_secs.is_some() {
            RuntimeSource::Measured
        } else {
            RuntimeSource::Fallback
        },
        history_count: 0,
        latest_sample: None,
    };
    let mut j = Job::new(
        entry.id.clone(),
        entry.weight,
        entry.predicted_rss,
        entry.front,
        &rt,
    );
    j.seq = entry.seq;
    j.in_plan = false;
    j.history_count = history_count;
    j.from_ledger = true;
    if running {
        let elapsed_ms = entry
            .started_at
            .map(|s| (taken_at - s).num_milliseconds().max(0) as u64);
        j.st = St::Running;
        j.start_ms = Some(0);
        j.ready_ms = Some(0);
        j.using = entry.using;
        j.backfilled = entry.backfilled;
        match (j.dur_ms, elapsed_ms) {
            (Some(d), Some(el)) if d > el && !e.suspect_dead => j.dur_ms = Some(d - el),
            _ => {
                j.dur_ms = None;
                j.measured = false;
                j.unknown_note = Some(if e.suspect_dead {
                    "owner lease not held; entry still counted, remaining time unknown".into()
                } else if elapsed_ms.is_none() {
                    "start time not recorded; remaining time unknown".into()
                } else {
                    "running longer than its history; remaining time unknown (overrun)".into()
                });
            }
        }
        j.run_finish = j.dur_ms;
    } else {
        j.st = St::Waiting;
        j.ready_ms = Some(0);
        j.enqueue_ms = -((taken_at - entry.enqueued_at).num_milliseconds().max(0));
        j.backfilled = false;
    }
    j
}

/// Forecast `plan` on top of the inspected ledger and the `overlay`.
/// Reads nothing and writes nothing: `ctx` supplies the clock only, to
/// judge how old the snapshot and the history are.
pub fn forecast(
    ctx: &Context,
    insp: &Inspection,
    plan: &WorkloadPlan,
    overlay: &[FutureDemand],
) -> Result<Forecast> {
    // Nodes sharing a shared_id are one piece of work: a dependency between
    // two of them cannot be honoured once they are one.
    let shared_of: HashMap<&str, &str> = plan
        .nodes
        .iter()
        .filter_map(|n| Some((n.node_id.as_str(), n.shared_id.as_deref()?)))
        .collect();
    for n in &plan.nodes {
        for d in &n.depends_on {
            if let (Some(a), Some(b)) =
                (shared_of.get(n.node_id.as_str()), shared_of.get(d.as_str()))
            {
                if a == b && d != &n.node_id {
                    return Err(Error::Plan(format!(
                        "shared_id {a}: nodes {} and {d} are merged but {} depends on {d}",
                        n.node_id, n.node_id
                    )));
                }
            }
        }
    }
    // Nodes sharing a shared_id are one piece of work: keep the first.
    let mut remap: HashMap<&str, &str> = HashMap::new();
    let mut first_shared: HashMap<&str, &str> = HashMap::new();
    for n in &plan.nodes {
        if let Some(s) = &n.shared_id {
            match first_shared.get(s.as_str()) {
                Some(keep) => {
                    remap.insert(&n.node_id, keep);
                }
                None => {
                    first_shared.insert(s, &n.node_id);
                }
            }
        }
    }
    let nodes: Vec<CommandNode> = plan
        .nodes
        .iter()
        .filter(|n| !remap.contains_key(n.node_id.as_str()))
        .map(|n| {
            let mut n = n.clone();
            n.depends_on = n
                .depends_on
                .iter()
                .map(|d| remap.get(d.as_str()).copied().unwrap_or(d).to_string())
                .collect();
            n
        })
        .collect();
    let plan = WorkloadPlan {
        nodes,
        ..plan.clone()
    };
    plan.validate()?;

    let now = ctx.clock.now();
    let mut jobs: Vec<Job> = insp
        .entries
        .iter()
        .map(|e| {
            let records = insp
                .history
                .iter()
                .find(|h| h.key == e.entry.key)
                .map_or(0, |h| h.records);
            ledger_job(e, insp.taken_at, records)
        })
        .collect();
    let by_ledger_id: HashMap<String, usize> = jobs
        .iter()
        .enumerate()
        .map(|(i, j)| (j.id.clone(), i))
        .collect();
    let max_seq = jobs.iter().map(|j| j.seq).max().unwrap_or(0);

    // Overlay.
    let mut report = OverlayReport::default();
    let mut seen: HashSet<(String, String, u64, String)> = plan
        .nodes
        .iter()
        .map(|n| {
            (
                plan.attempt_id.clone(),
                plan.plan_digest.clone(),
                plan.expansion_seq,
                n.node_id.clone(),
            )
        })
        .collect();
    for gone in remap.keys() {
        seen.insert((
            plan.attempt_id.clone(),
            plan.plan_digest.clone(),
            plan.expansion_seq,
            gone.to_string(),
        ));
    }
    let mut shared_seen: HashSet<String> = plan
        .nodes
        .iter()
        .filter_map(|n| n.shared_id.clone())
        .collect();
    let mut coverage: BTreeMap<(String, String, u64), Vec<String>> = BTreeMap::new();
    for n in &plan.nodes {
        coverage
            .entry((
                plan.attempt_id.clone(),
                plan.plan_digest.clone(),
                plan.expansion_seq,
            ))
            .or_default()
            .push(n.node_id.clone());
    }
    for d in overlay {
        let label = format!("{}/{}", d.attempt_id, d.node_id);
        if d.terminal {
            report.terminal_removed.push(label);
            continue;
        }
        let cover = |coverage: &mut BTreeMap<(String, String, u64), Vec<String>>| {
            coverage
                .entry((d.attempt_id.clone(), d.plan_digest.clone(), d.expansion_seq))
                .or_default()
                .push(d.node_id.clone());
        };
        if !seen.insert(d.key()) {
            report.duplicates_dropped.push(label);
            continue;
        }
        if d.ledger_entry
            .as_ref()
            .is_some_and(|id| by_ledger_id.contains_key(id))
        {
            // Later demands of the same shared work are that entry too.
            if let Some(s) = &d.shared_id {
                shared_seen.insert(s.clone());
            }
            cover(&mut coverage);
            report.replaced_by_ledger.push(label);
            continue;
        }
        if let Some(s) = &d.shared_id {
            if !shared_seen.insert(s.clone()) {
                cover(&mut coverage);
                report.duplicates_dropped.push(label);
                continue;
            }
        }
        let mut j = Job::new(
            format!("overlay:{label}"),
            d.weight,
            d.predicted_rss,
            d.priority == Priority::Front,
            &d.runtime,
        );
        j.floor_ms = d.ready_in_ms;
        jobs.push(j);
        cover(&mut coverage);
        report.applied.push(label);
    }

    // Plan nodes: a node already in the ledger is that entry.
    let mut node_job: Vec<usize> = Vec::new();
    let mut registered: Vec<Option<String>> = Vec::new();
    for n in &plan.nodes {
        if let Some(&i) = n.ledger_entry.as_ref().and_then(|id| by_ledger_id.get(id)) {
            jobs[i].in_plan = true;
            node_job.push(i);
            registered.push(Some(jobs[i].id.clone()));
        } else {
            let mut j = Job::new(
                format!("plan:{}", n.node_id),
                n.weight,
                n.predicted_rss,
                n.priority == Priority::Front,
                &n.runtime,
            );
            j.in_plan = true;
            j.floor_ms = n.ready_in_ms;
            j.delay_ms = n.external_delay_ms;
            jobs.push(j);
            node_job.push(jobs.len() - 1);
            registered.push(None);
        }
    }
    let node_index: HashMap<&str, usize> = plan
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.node_id.as_str(), i))
        .collect();
    for (k, n) in plan.nodes.iter().enumerate() {
        if registered[k].is_none() {
            jobs[node_job[k]].deps = n
                .depends_on
                .iter()
                .map(|d| node_job[node_index[d.as_str()]])
                .collect();
        }
    }

    simulate(
        &mut jobs,
        insp.limits,
        insp.live,
        plan.max_in_flight,
        max_seq.saturating_add(1),
    );

    let ledger_running_unknown = insp
        .entries
        .iter()
        .zip(&jobs)
        .filter(|(e, j)| e.entry.state == State::Running && j.dur_ms.is_none())
        .count();
    let g = Globals {
        stale_snapshot: (now - insp.taken_at).num_milliseconds() > TTL_MS as i64,
        unknown_running: ledger_running_unknown,
        suspect_dead: insp.entries.iter().filter(|e| e.suspect_dead).count(),
        live_missing: insp.live.is_none(),
        any_live_hold: jobs.iter().any(|j| j.live_hold),
        overlay_applied: report.applied.len(),
        legacy_clock_entries: insp.entries.len(),
    };
    let t0_running = insp.entries.iter().any(|e| e.entry.state == State::Running);
    let node_forecasts: Vec<NodeForecast> = plan
        .nodes
        .iter()
        .enumerate()
        .map(|(k, n)| NodeForecast {
            node_id: n.node_id.clone(),
            timing: timing_of(
                &jobs,
                node_job[k],
                &NodeMeta {
                    runtime: &n.runtime,
                    rss: n.predicted_rss,
                    registered: registered[k].as_deref(),
                },
                &g,
                now,
                t0_running,
            ),
        })
        .collect();
    let mut timing = summarize(&node_forecasts);
    let job_node: HashMap<usize, usize> =
        node_job.iter().enumerate().map(|(k, &i)| (i, k)).collect();
    let info: Vec<PathInfo> = node_job
        .iter()
        .map(|&i| PathInfo {
            deps: jobs[i]
                .deps
                .iter()
                .filter_map(|d| job_node.get(d).copied())
                .collect(),
            ready: jobs[i].ready_ms,
            start: jobs[i].start_ms,
            finish: jobs[i].finish_ms,
        })
        .collect();
    let path = critical_path(&node_forecasts, &info);
    timing.null_reasons.extend(path.null_reasons);
    // A merged node reports what the node it was merged into reports.
    let mut node_forecasts = node_forecasts;
    let mut merged: Vec<(&str, &str)> = remap.iter().map(|(a, b)| (*a, *b)).collect();
    merged.sort();
    for (gone, kept) in merged {
        // The kept node is always in the plan, so the lookup finds it.
        let kept_timing = node_forecasts
            .iter()
            .filter(|n| n.node_id == kept)
            .map(|n| n.timing.clone())
            .next();
        node_forecasts.extend(kept_timing.map(|timing| NodeForecast {
            node_id: gone.to_string(),
            timing,
        }));
        coverage
            .entry((
                plan.attempt_id.clone(),
                plan.plan_digest.clone(),
                plan.expansion_seq,
            ))
            .or_default()
            .push(gone.to_string());
    }
    Ok(Forecast {
        schema_version: SCHEMA_VERSION,
        snapshot_digest: insp.snapshot_digest.clone(),
        policy_digest: insp.policy_digest.clone(),
        ttl_ms: TTL_MS,
        coverage: coverage
            .into_iter()
            .map(|((a, p, e), mut ids)| {
                ids.sort();
                ids.dedup();
                CoverageItem {
                    attempt_id: a,
                    plan_digest: p,
                    expansion_seq: e,
                    node_ids: ids,
                }
            })
            .collect(),
        overlay: report,
        target: PlanForecast {
            attempt_id: plan.attempt_id.clone(),
            plan_digest: plan.plan_digest.clone(),
            expansion_seq: plan.expansion_seq,
            timing,
            critical_path: path.nodes,
            critical_path_wait_ms: path.wait_ms,
            critical_path_execution_ms: path.execution_ms,
            nodes: node_forecasts,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{FakeClock, FixedLive};
    use crate::inspect::{InspectedEntry, SCHEMA_VERSION as INSPECT_SCHEMA};
    use crate::ledger::Entry;
    use crate::paths::Paths;
    use chrono::Duration as Cd;
    use std::sync::Arc;

    fn t0() -> DateTime<Utc> {
        "2030-01-01T00:00:00Z".parse().unwrap()
    }

    fn ctx() -> (Context, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock::new(t0()));
        let c = Context::new(Paths::new("/nonexistent/state", "/nonexistent/config.toml"))
            .with_clock(clock.clone())
            .with_live(Arc::new(FixedLive(None)));
        (c, clock)
    }

    fn limits(budget: u32) -> Limits {
        Limits {
            budget,
            memory_reserve: 0,
            cpu_threshold: 0.9,
            cpu_guard_max_wait_secs: 30.0,
            front_overload_load: 0.0,
            front_overload_max_wait_secs: 10.0,
            backfill_max_job_secs: 0.0,
            backfill_max_wait_secs: 60.0,
        }
    }

    fn calm() -> Option<LiveSnapshot> {
        Some(LiveSnapshot {
            available_memory: 1 << 40,
            cpu_busy: 0.1,
            load_avg: 0.0,
        })
    }

    /// A running ledger entry started `elapsed_s` ago.
    fn running(id: &str, weight: u32, expected: Option<f64>, elapsed_s: i64) -> InspectedEntry {
        InspectedEntry {
            entry: Entry {
                id: id.into(),
                pid: 1,
                state: State::Running,
                front: false,
                weight,
                predicted_rss: 0,
                expected_secs: expected,
                repo: String::new(),
                subdir: String::new(),
                argv: id.into(),
                key: id.into(),
                seq: 1,
                enqueued_at: t0() - Cd::seconds(elapsed_s),
                started_at: Some(t0() - Cd::seconds(elapsed_s)),
                using: None,
                backfilled: false,
            },
            suspect_dead: false,
        }
    }

    fn waiting(id: &str, weight: u32, expected: Option<f64>, seq: u64) -> InspectedEntry {
        let mut e = running(id, weight, expected, 0);
        e.entry.state = State::Waiting;
        e.entry.started_at = None;
        e.entry.seq = seq;
        e
    }

    fn insp(budget: u32, live: Option<LiveSnapshot>, entries: Vec<InspectedEntry>) -> Inspection {
        Inspection {
            schema_version: INSPECT_SCHEMA,
            taken_at: t0(),
            snapshot_digest: "sha256:snap".into(),
            policy_digest: "sha256:policy".into(),
            limits: limits(budget),
            live,
            ledger_locked: true,
            entries,
            history: Vec::new(),
            history_quality: Vec::new(),
        }
    }

    fn node(id: &str, weight: u32, runtime: Runtime) -> CommandNode {
        CommandNode {
            node_id: id.into(),
            identity: format!("id-{id}"),
            label: id.into(),
            weight,
            predicted_rss: 1,
            runtime,
            priority: Priority::Normal,
            depends_on: Vec::new(),
            ready_in_ms: 0,
            external_delay_ms: 0,
            ledger_entry: None,
            shared_id: None,
        }
    }

    fn plan(nodes: Vec<CommandNode>) -> WorkloadPlan {
        WorkloadPlan {
            attempt_id: "att".into(),
            plan_digest: "sha256:plan".into(),
            expansion_seq: 0,
            max_in_flight: None,
            nodes,
        }
    }

    fn demand(node_id: &str, weight: u32, secs: f64) -> FutureDemand {
        FutureDemand {
            attempt_id: "other".into(),
            plan_digest: "sha256:other".into(),
            expansion_seq: 1,
            node_id: node_id.into(),
            weight,
            predicted_rss: 1,
            runtime: Runtime::measured(secs, 4),
            priority: Priority::Normal,
            ready_in_ms: 0,
            ledger_entry: None,
            shared_id: None,
            terminal: false,
        }
    }

    fn run(i: &Inspection, p: &WorkloadPlan, o: &[FutureDemand]) -> Forecast {
        forecast(&ctx().0, i, p, o).unwrap()
    }

    fn single(i: &Inspection, n: CommandNode) -> Timing {
        run(i, &plan(vec![n]), &[]).target.timing
    }

    // A07: ready after 10 s, the queue clears at 30 s, runs 20 s: 50 s,
    // not 10 + 30 + 20.
    #[test]
    fn transfer_overlaps_the_queue() {
        let i = with_history(
            insp(1, calm(), vec![running("r", 1, Some(30.0), 0)]),
            "r",
            5,
        );
        let mut n = node("n", 1, Runtime::measured(20.0, 5));
        n.external_delay_ms = 10_000;
        let t = single(&i, n);
        assert_eq!(t.ready_in_ms, Some(10_000));
        assert_eq!(t.start_in_ms, Some(30_000));
        assert_eq!(t.queue_wait_after_ready_ms, Some(20_000));
        assert_eq!(t.execution_ms, Some(20_000));
        assert_eq!(t.finish_in_ms, Some(50_000));
        assert!(t.null_reasons.is_empty());
        assert!(!t.conditional);
        assert_eq!(t.confidence, Confidence::High);
        assert_eq!(t.runtime_source, RuntimeSource::Measured);
        assert_eq!(t.history_count, 5);
        assert_eq!(t.wait_reason.unwrap().kind, "not_ready");
    }

    #[test]
    fn empty_queue_starts_now() {
        let t = single(
            &insp(1, calm(), vec![]),
            node("n", 1, Runtime::measured(20.0, 1)),
        );
        assert_eq!(t.start_in_ms, Some(0));
        assert_eq!(t.finish_in_ms, Some(20_000));
        assert_eq!(t.wait_reason, None);
        assert_eq!(t.confidence, Confidence::Medium);
    }

    #[test]
    fn running_elapsed_shortens_the_remaining_time() {
        let i = insp(1, calm(), vec![running("r", 1, Some(30.0), 12)]);
        let t = single(&i, node("n", 1, Runtime::measured(1.0, 3)));
        assert_eq!(t.start_in_ms, Some(18_000));
        assert_eq!(t.wait_reason.unwrap().kind, "budget");
        assert!(!t.conditional);
    }

    #[test]
    fn overrun_is_unknown_not_zero() {
        let i = insp(1, calm(), vec![running("r", 1, Some(10.0), 20)]);
        let t = single(&i, node("n", 1, Runtime::measured(1.0, 3)));
        assert_eq!(t.start_in_ms, None);
        assert_eq!(t.finish_in_ms, None);
        assert_eq!(t.ready_in_ms, Some(0));
        assert!(t.null_reasons["start_in_ms"].contains("unknown remaining time"));
        assert_eq!(t.confidence, Confidence::None);
        assert!(t.conditional);
        assert!(t
            .assumptions
            .iter()
            .any(|a| a.contains("unknown remaining time")));
        // Room beside the overrun entry: still a measured answer.
        let i = insp(2, calm(), vec![running("r", 1, Some(10.0), 20)]);
        let t = single(&i, node("n", 1, Runtime::measured(1.0, 3)));
        assert_eq!(t.finish_in_ms, Some(1000));
        assert!(t.conditional);
    }

    #[test]
    fn a_dead_looking_owner_still_holds_its_budget() {
        let mut dead = running("r", 1, Some(100.0), 1);
        dead.suspect_dead = true;
        let t = single(
            &insp(1, calm(), vec![dead]),
            node("n", 1, Runtime::measured(1.0, 3)),
        );
        assert_eq!(t.start_in_ms, None);
        assert!(t.assumptions.iter().any(|a| a.contains("no live lease")));
    }

    #[test]
    fn a_waiting_entry_with_no_lease_keeps_its_place() {
        let mut w = waiting("w", 1, Some(10.0), 2);
        w.suspect_dead = true;
        let t = single(
            &insp(1, calm(), vec![running("r", 1, Some(5.0), 0), w]),
            node("n", 1, Runtime::measured(1.0, 3)),
        );
        assert_eq!(t.start_in_ms, Some(15_000));
    }

    #[test]
    fn fallback_runtime_is_a_ranking_not_an_eta() {
        let t = single(&insp(1, calm(), vec![]), node("n", 1, Runtime::fallback()));
        assert_eq!(t.start_in_ms, Some(0));
        assert_eq!(t.execution_ms, None);
        assert_eq!(t.finish_in_ms, None);
        let r = t.ranking.unwrap();
        assert_eq!(r.start_in_ms, Some(0));
        assert_eq!(r.finish_in_ms, Some(60_000));
        assert_eq!(t.confidence, Confidence::Low);
        assert_eq!(t.runtime_source, RuntimeSource::Fallback);
        assert!(t.null_reasons["execution_ms"].contains("fallback"));
    }

    #[test]
    fn a_start_that_waited_on_an_unmeasured_run_is_a_ranking() {
        let i = insp(1, calm(), vec![running("r", 1, None, 0)]);
        let t = single(&i, node("n", 1, Runtime::measured(5.0, 3)));
        assert_eq!(t.start_in_ms, None);
        assert_eq!(t.queue_wait_after_ready_ms, None);
        assert_eq!(t.finish_in_ms, None);
        assert_eq!(t.execution_ms, Some(5000));
        let r = t.ranking.unwrap();
        assert_eq!(
            (r.start_in_ms, r.finish_in_ms),
            (Some(60_000), Some(65_000))
        );
        assert_eq!(t.confidence, Confidence::Low);
    }

    #[test]
    fn unknown_runtime_never_frees_the_budget() {
        let a = node("a", 1, Runtime::unknown());
        let mut b = node("b", 1, Runtime::measured(1.0, 3));
        b.depends_on = vec!["a".into()];
        let f = run(&insp(1, calm(), vec![]), &plan(vec![a, b]), &[]);
        let na = &f.target.nodes[0].timing;
        assert_eq!(na.start_in_ms, Some(0));
        assert_eq!(na.finish_in_ms, None);
        assert!(na.null_reasons["execution_ms"].contains("unknown"));
        let nb = &f.target.nodes[1].timing;
        assert_eq!(nb.ready_in_ms, None);
        assert!(nb.null_reasons["start_in_ms"].contains("not ready"));
        assert!(nb.null_reasons["ready_in_ms"].contains("dependency"));
        assert_eq!(f.target.timing.finish_in_ms, None);
        assert_eq!(f.target.timing.confidence, Confidence::None);
        assert_eq!(f.target.timing.runtime_source, RuntimeSource::Unknown);
    }

    #[test]
    fn dag_is_simulated_not_summed() {
        let a = node("a", 1, Runtime::measured(10.0, 3));
        let mut b = node("b", 1, Runtime::measured(20.0, 3));
        b.depends_on = vec!["a".into()];
        b.external_delay_ms = 5000;
        let c = node("c", 1, Runtime::measured(3.0, 3));
        let f = run(&insp(2, calm(), vec![]), &plan(vec![a, b, c]), &[]);
        let at = |k: usize| &f.target.nodes[k].timing;
        assert_eq!(at(1).ready_in_ms, Some(15_000));
        assert_eq!(at(1).start_in_ms, Some(15_000));
        assert_eq!(at(1).finish_in_ms, Some(35_000));
        let t = &f.target.timing;
        assert_eq!(t.ready_in_ms, Some(0));
        assert_eq!(t.start_in_ms, Some(0));
        assert_eq!(t.finish_in_ms, Some(35_000));
        // Two or more nodes: no sums, the critical path instead.
        assert_eq!(t.execution_ms, None);
        assert_eq!(t.queue_wait_after_ready_ms, None);
        assert!(t.null_reasons["execution_ms"].contains("parallel"));
        assert!(t.null_reasons["queue_wait_after_ready_ms"].contains("parallel"));
        assert_eq!(f.target.critical_path, ["a", "b"]);
        // 10 s + 20 s of execution; the 5 s transfer is neither.
        assert_eq!(f.target.critical_path_execution_ms, Some(30_000));
        assert_eq!(f.target.critical_path_wait_ms, Some(0));
    }

    #[test]
    fn ready_floor_and_delay_combine() {
        let mut n = node("n", 1, Runtime::measured(1.0, 3));
        n.ready_in_ms = 7000;
        n.external_delay_ms = 3000;
        assert_eq!(
            single(&insp(1, calm(), vec![]), n.clone()).ready_in_ms,
            Some(7000)
        );
        n.ready_in_ms = 1000;
        assert_eq!(single(&insp(1, calm(), vec![]), n).ready_in_ms, Some(3000));
    }

    #[test]
    fn in_flight_limit_holds_ready_nodes_back() {
        let mut p = plan(vec![
            node("x", 1, Runtime::measured(5.0, 3)),
            node("y", 1, Runtime::measured(5.0, 3)),
        ]);
        p.max_in_flight = Some(1);
        let f = run(&insp(2, calm(), vec![]), &p, &[]);
        let y = &f.target.nodes[1].timing;
        assert_eq!(y.ready_in_ms, Some(0));
        assert_eq!(y.start_in_ms, Some(5000));
        assert_eq!(y.queue_wait_after_ready_ms, Some(5000));
        assert_eq!(y.finish_in_ms, Some(10_000));
        p.max_in_flight = None;
        let f = run(&insp(2, calm(), vec![]), &p, &[]);
        assert_eq!(f.target.nodes[1].timing.start_in_ms, Some(0));
    }

    #[test]
    fn invalid_plans_are_rejected() {
        let i = insp(1, calm(), vec![]);
        let (c, _) = ctx();
        let a = node("a", 1, Runtime::fallback());
        let dup = plan(vec![a.clone(), a.clone()]);
        assert!(forecast(&c, &i, &dup, &[]).is_err());
        let mut x = a.clone();
        x.depends_on = vec!["nope".into()];
        assert!(forecast(&c, &i, &plan(vec![x]), &[]).is_err());
        let mut p = a.clone();
        p.depends_on = vec!["q".into()];
        let mut q = node("q", 1, Runtime::fallback());
        q.depends_on = vec!["a".into()];
        let err = forecast(&c, &i, &plan(vec![p, q]), &[]).unwrap_err();
        assert!(err.to_string().contains("cycle"));
    }

    #[test]
    fn fifo_follows_the_time_nodes_became_ready() {
        let i = insp(1, calm(), vec![running("r", 1, Some(30.0), 0)]);
        let mut late = node("late", 1, Runtime::measured(5.0, 3));
        late.ready_in_ms = 10_000;
        let early = node("early", 1, Runtime::measured(5.0, 3));
        let f = run(&i, &plan(vec![late, early]), &[]);
        assert_eq!(f.target.nodes[1].timing.start_in_ms, Some(30_000));
        assert_eq!(f.target.nodes[0].timing.start_in_ms, Some(35_000));
        assert_eq!(
            f.target.nodes[0].timing.queue_wait_after_ready_ms,
            Some(25_000)
        );
    }

    #[test]
    fn the_cpu_guard_gives_way_after_a_long_wait() {
        let busy = Some(LiveSnapshot {
            available_memory: 1 << 40,
            cpu_busy: 0.95,
            load_avg: 0.0,
        });
        let mut q = waiting("q", 1, Some(10.0), 2);
        let mut n = node("n", 1, Runtime::measured(10.0, 3));
        n.ledger_entry = Some("q".into());
        q.entry.enqueued_at = t0() - Cd::seconds(40);
        let i = insp(2, busy, vec![running("r", 1, Some(30.0), 0), q.clone()]);
        let t = single(&i, n.clone());
        assert_eq!(t.start_in_ms, Some(0));
        assert_eq!(t.wait_reason, None);
        q.entry.enqueued_at = t0() - Cd::seconds(5);
        let i = insp(2, busy, vec![running("r", 1, Some(30.0), 0), q]);
        let t = single(&i, n);
        assert_eq!(t.wait_reason.unwrap().kind, "cpu");
        assert_eq!(t.start_in_ms, Some(30_000));
    }

    #[test]
    fn a_zero_budget_is_treated_as_one() {
        let t = single(
            &insp(0, calm(), vec![]),
            node("n", 1, Runtime::measured(1.0, 3)),
        );
        assert_eq!(t.start_in_ms, Some(0));
    }

    #[test]
    fn work_started_by_backfill_keeps_holding_the_head() {
        // Budget 4: a big run ends at 10 s; the long head (w4) cannot start
        // while the short w1 job that backfilled ahead of it runs. The head
        // has waited long enough that nothing else may pass it either.
        let mut head = waiting("head", 4, Some(100.0), 2);
        head.entry.enqueued_at = t0() - Cd::seconds(70);
        let mut i = insp(4, calm(), vec![running("big", 3, Some(10.0), 0), head]);
        i.limits.backfill_max_job_secs = 60.0;
        let s1 = node("s1", 1, Runtime::measured(40.0, 3));
        let mut s2 = node("s2", 1, Runtime::measured(5.0, 3));
        s2.ready_in_ms = 12_000;
        let f = run(&i, &plan(vec![s1, s2]), &[]);
        assert_eq!(f.target.nodes[0].timing.start_in_ms, Some(0));
        // head runs 40 s..140 s; s2 may not slip past it.
        assert_eq!(f.target.nodes[1].timing.start_in_ms, Some(140_000));
    }

    #[test]
    fn a_head_that_started_normally_is_not_treated_as_backfill() {
        let mut head = waiting("head", 2, Some(100.0), 1);
        head.entry.enqueued_at = t0() - Cd::seconds(70);
        let mut i = insp(4, calm(), vec![head]);
        i.limits.backfill_max_job_secs = 60.0;
        let mut x = node("x", 3, Runtime::measured(100.0, 3));
        x.priority = Priority::Front;
        let short = node("short", 1, Runtime::measured(5.0, 3));
        let f = run(&i, &plan(vec![x, short]), &[]);
        assert_eq!(f.target.nodes[0].timing.start_in_ms, Some(0));
        assert_eq!(f.target.nodes[1].timing.start_in_ms, Some(0));
    }

    #[test]
    fn a_fallback_node_with_no_start_has_no_ranking() {
        let i = insp(1, calm(), vec![running("r", 1, Some(10.0), 20)]);
        let t = single(&i, node("n", 1, Runtime::fallback()));
        assert_eq!(t.start_in_ms, None);
        assert_eq!(t.ranking, None);
        assert_eq!(t.confidence, Confidence::None);
    }

    #[test]
    fn a_run_exactly_at_its_expected_time_is_an_overrun() {
        let i = insp(1, calm(), vec![running("r", 1, Some(10.0), 10)]);
        let t = single(&i, node("n", 1, Runtime::measured(1.0, 3)));
        assert_eq!(t.start_in_ms, None);
    }

    #[test]
    fn front_overlay_demand_passes_the_queue() {
        let mut q = waiting("q", 2, Some(100.0), 2);
        q.entry.enqueued_at = t0();
        let i = insp(2, calm(), vec![running("r", 1, Some(30.0), 0), q]);
        let p = plan(vec![node("n", 2, Runtime::measured(10.0, 3))]);
        let mut d = demand("d", 1, 5.0);
        d.priority = Priority::Front;
        let f = run(&i, &p, std::slice::from_ref(&d));
        assert_eq!(f.target.timing.start_in_ms, Some(130_000));
        d.priority = Priority::Normal;
        let f = run(&i, &p, &[d]);
        assert_eq!(f.target.timing.start_in_ms, Some(135_000));
    }

    #[test]
    fn lowering_confidence_stops_at_low() {
        assert_eq!(Confidence::High.lowered(), Confidence::Medium);
        assert_eq!(Confidence::Medium.lowered(), Confidence::Low);
        assert_eq!(Confidence::Low.lowered(), Confidence::Low);
        assert_eq!(Confidence::None.lowered(), Confidence::None);
    }

    #[test]
    fn single_node_plan_helper() {
        let p = WorkloadPlan::single("a", "sha256:x", node("n", 1, Runtime::fallback()));
        assert_eq!(
            (p.attempt_id.as_str(), p.expansion_seq, p.nodes.len()),
            ("a", 0, 1)
        );
        assert_eq!(p.max_in_flight, None);
    }

    #[test]
    fn empty_plan_has_nothing_to_say() {
        let f = run(&insp(1, calm(), vec![]), &plan(vec![]), &[]);
        assert_eq!(f.target.timing.confidence, Confidence::None);
        assert_eq!(f.target.timing.runtime_source, RuntimeSource::Unknown);
        assert_eq!(f.target.timing.history_count, 0);
        assert_eq!(f.target.timing.ready_in_ms, None);
    }

    #[test]
    fn shared_setup_is_counted_once() {
        let mut s1 = node("s1", 1, Runtime::measured(10.0, 3));
        s1.shared_id = Some("setup".into());
        let mut s2 = node("s2", 1, Runtime::measured(10.0, 3));
        s2.shared_id = Some("setup".into());
        let mut t = node("t", 1, Runtime::measured(5.0, 3));
        t.depends_on = vec!["s2".into()];
        let f = run(&insp(1, calm(), vec![]), &plan(vec![s1, s2, t]), &[]);
        // budget 1: two copies of the setup would push `t` to 25 s.
        let t = f.target.nodes.iter().find(|n| n.node_id == "t").unwrap();
        assert_eq!(t.timing.finish_in_ms, Some(15_000));
        let s2 = f.target.nodes.iter().find(|n| n.node_id == "s2").unwrap();
        let s1 = f.target.nodes.iter().find(|n| n.node_id == "s1").unwrap();
        assert_eq!(s1.timing, s2.timing);
        assert_eq!(f.coverage[0].node_ids, ["s1", "s2", "t"]);
        assert_eq!(f.target.critical_path, ["s1", "t"]);
        assert_eq!(f.target.critical_path_execution_ms, Some(15_000));
    }

    #[test]
    fn plan_node_already_in_the_ledger_is_that_entry() {
        let i = insp(
            1,
            calm(),
            vec![
                running("r", 1, Some(30.0), 0),
                waiting("w", 1, Some(10.0), 2),
            ],
        );
        let mut n = node("n", 1, Runtime::measured(10.0, 3));
        n.ledger_entry = Some("w".into());
        let t = single(&i, n.clone());
        assert_eq!(t.start_in_ms, Some(30_000));
        assert_eq!(t.finish_in_ms, Some(40_000));
        assert!(t.assumptions.iter().any(|a| a.contains("ledger entry w")));
        // An id the ledger does not have is a new node.
        n.ledger_entry = Some("absent".into());
        let t = single(&i, n);
        assert_eq!(t.start_in_ms, Some(40_000));
    }

    #[test]
    fn front_passes_the_normal_queue() {
        let i = insp(
            1,
            calm(),
            vec![
                running("r", 1, Some(30.0), 0),
                waiting("q", 1, Some(10.0), 2),
            ],
        );
        let mut n = node("n", 1, Runtime::measured(20.0, 3));
        assert_eq!(single(&i, n.clone()).start_in_ms, Some(40_000));
        n.priority = Priority::Front;
        let t = single(&i, n);
        assert_eq!(t.start_in_ms, Some(30_000));
        assert_eq!(t.finish_in_ms, Some(50_000));
    }

    #[test]
    fn short_commands_backfill_like_at_runtime() {
        let entries = vec![
            running("r", 1, Some(30.0), 0),
            waiting("head", 2, Some(100.0), 2),
        ];
        let n = node("n", 1, Runtime::measured(5.0, 3));
        let mut i = insp(2, calm(), entries);
        assert_eq!(single(&i, n.clone()).start_in_ms, Some(130_000));
        i.limits.backfill_max_job_secs = 60.0;
        let t = single(&i, n);
        assert_eq!(t.start_in_ms, Some(0));
        assert_eq!(t.finish_in_ms, Some(5000));
    }

    #[test]
    fn live_memory_guard_is_conditional() {
        let live = Some(LiveSnapshot {
            available_memory: 100 << 20,
            cpu_busy: 0.1,
            load_avg: 0.0,
        });
        let i = with_history(insp(2, live, vec![running("r", 1, Some(30.0), 0)]), "r", 5);
        let mut n = node("n", 1, Runtime::measured(5.0, 3));
        n.predicted_rss = 1 << 30;
        let t = single(&i, n);
        assert_eq!(t.wait_reason.as_ref().unwrap().kind, "memory");
        assert_eq!(t.start_in_ms, Some(30_000));
        assert!(t.conditional);
        assert_eq!(t.confidence, Confidence::Medium);
        assert!(t.assumptions.iter().any(|a| a.contains("live guard")));
        assert!(t.assumptions.iter().any(|a| a == "no new external load"));
    }

    #[test]
    fn live_cpu_and_overload_guards_are_reported() {
        let busy = Some(LiveSnapshot {
            available_memory: 1 << 40,
            cpu_busy: 0.95,
            load_avg: 9.0,
        });
        let mut i = insp(2, busy, vec![running("r", 1, Some(30.0), 0)]);
        let n = node("n", 1, Runtime::measured(5.0, 3));
        assert_eq!(single(&i, n.clone()).wait_reason.unwrap().kind, "cpu");
        i.limits.front_overload_load = 4.0;
        let mut f = n;
        f.priority = Priority::Front;
        assert_eq!(single(&i, f).wait_reason.unwrap().kind, "overload");
    }

    #[test]
    fn wait_reason_kinds() {
        let kinds = [
            (Reason::NotHead { ahead: 1 }, "queue"),
            (Reason::Reserved { ahead: 1 }, "reserved"),
            (Reason::Budget { used: 1, budget: 1 }, "budget"),
            (
                Reason::Memory {
                    available: 1,
                    need: 2,
                },
                "memory",
            ),
            (
                Reason::Cpu {
                    busy: 0.9,
                    threshold: 0.5,
                },
                "cpu",
            ),
            (
                Reason::Overload {
                    load: 9.0,
                    limit: 4.0,
                },
                "overload",
            ),
        ];
        for (r, k) in kinds {
            let w = wait_reason(&r);
            assert_eq!(w.kind, k);
            assert!(!w.detail.is_empty());
        }
    }

    #[test]
    fn queue_wait_reason_when_someone_is_ahead() {
        let i = insp(
            1,
            calm(),
            vec![
                running("r", 1, Some(30.0), 0),
                waiting("q", 1, Some(10.0), 2),
            ],
        );
        let t = single(&i, node("n", 1, Runtime::measured(1.0, 3)));
        assert_eq!(t.wait_reason.unwrap().kind, "queue");
        assert!(!t.conditional);
    }

    #[test]
    fn uncertainty_is_reflected_in_quality() {
        // Unknown memory with something running, no live metrics.
        let i = insp(2, None, vec![running("r", 1, Some(30.0), 0)]);
        let mut n = node("n", 1, Runtime::measured(5.0, 9));
        n.predicted_rss = 0;
        let t = single(&i, n);
        assert!(t.conditional);
        assert!(t.assumptions.iter().any(|a| a.contains("memory unknown")));
        assert!(t
            .assumptions
            .iter()
            .any(|a| a.contains("metrics unavailable")));
        assert_eq!(t.confidence, Confidence::Medium);
        // Nothing running: unknown memory is not an issue.
        let mut n = node("n", 1, Runtime::measured(5.0, 9));
        n.predicted_rss = 0;
        let t = single(&insp(2, calm(), vec![]), n);
        assert!(!t.conditional);
        // Old history.
        let mut rt = Runtime::measured(5.0, 9);
        rt.latest_sample = Some(t0() - Cd::days(STALE_HISTORY_DAYS + 1));
        let t = single(&insp(2, calm(), vec![]), node("n", 1, rt.clone()));
        assert!(t.conditional);
        assert!(t.assumptions.iter().any(|a| a.contains("older than")));
        rt.latest_sample = Some(t0() - Cd::days(STALE_HISTORY_DAYS));
        assert!(!single(&insp(2, calm(), vec![]), node("n", 1, rt)).conditional);
    }

    #[test]
    fn a_stale_snapshot_is_flagged() {
        let (c, clock) = ctx();
        let i = insp(1, calm(), vec![]);
        let p = plan(vec![node("n", 1, Runtime::measured(1.0, 3))]);
        clock.advance(std::time::Duration::from_millis(TTL_MS));
        assert!(!forecast(&c, &i, &p, &[]).unwrap().target.timing.conditional);
        clock.advance(std::time::Duration::from_millis(1));
        let t = forecast(&c, &i, &p, &[]).unwrap().target.timing;
        assert!(t.conditional);
        assert!(t.assumptions.iter().any(|a| a.contains("ttl")));
    }

    #[test]
    fn overlay_demand_is_queued_ahead_of_the_target() {
        let i = insp(1, calm(), vec![]);
        let p = plan(vec![node("n", 1, Runtime::measured(20.0, 3))]);
        let d = demand("d", 1, 30.0);
        let f = run(&i, &p, std::slice::from_ref(&d));
        assert_eq!(f.target.timing.start_in_ms, Some(30_000));
        assert_eq!(f.target.timing.finish_in_ms, Some(50_000));
        assert_eq!(f.overlay.applied, ["other/d"]);
        assert!(f.target.timing.conditional);
        assert!(f
            .target
            .timing
            .assumptions
            .iter()
            .any(|a| a.contains("not in the ledger yet")));
        // Later-ready demand does not jump the queue.
        let mut late = d;
        late.ready_in_ms = 5000;
        let f = run(&i, &p, &[late]);
        assert_eq!(f.target.timing.start_in_ms, Some(0));
    }

    #[test]
    fn overlay_becomes_the_ledger_entry_once_registered() {
        let p = plan(vec![node("n", 1, Runtime::measured(20.0, 3))]);
        let mut d = demand("d", 1, 30.0);
        d.ledger_entry = Some("L".into());
        // Registered: counted once, from the ledger.
        let with = insp(1, calm(), vec![running("L", 1, Some(30.0), 0)]);
        let f = run(&with, &p, std::slice::from_ref(&d));
        assert_eq!(f.target.timing.start_in_ms, Some(30_000));
        assert_eq!(f.overlay.replaced_by_ledger, ["other/d"]);
        assert!(f.overlay.applied.is_empty());
        assert_eq!(f.coverage.len(), 2);
        // Not registered yet: still counted from the overlay.
        let without = insp(1, calm(), vec![]);
        let f = run(&without, &p, &[d]);
        assert_eq!(f.target.timing.start_in_ms, Some(30_000));
        assert_eq!(f.overlay.applied, ["other/d"]);
    }

    #[test]
    fn overlay_terminal_duplicate_and_shared_are_not_double_counted() {
        let i = insp(1, calm(), vec![]);
        let mut shared_plan = node("n", 1, Runtime::measured(20.0, 3));
        shared_plan.shared_id = Some("setup".into());
        let p = plan(vec![shared_plan]);
        let mut done = demand("done", 1, 99.0);
        done.terminal = true;
        let d = demand("d", 1, 10.0);
        let mut other_shared = demand("s", 1, 99.0);
        other_shared.shared_id = Some("setup".into());
        let mut first_shared = demand("s1", 1, 7.0);
        first_shared.shared_id = Some("builder".into());
        let mut second_shared = demand("s2", 1, 7.0);
        second_shared.shared_id = Some("builder".into());
        // Same key as the plan's own node: the parent re-added it.
        let mut again = demand("n", 1, 99.0);
        again.attempt_id = "att".into();
        again.plan_digest = "sha256:plan".into();
        again.expansion_seq = 0;
        let o = [
            done,
            d.clone(),
            d,
            other_shared,
            first_shared,
            second_shared,
            again,
        ];
        let f = run(&i, &p, &o);
        assert_eq!(f.overlay.terminal_removed, ["other/done"]);
        assert_eq!(f.overlay.applied, ["other/d", "other/s1"]);
        assert_eq!(
            f.overlay.duplicates_dropped,
            ["other/d", "other/s", "other/s2", "att/n"]
        );
        // d (10 s) then s1 (7 s) ahead of the target (20 s).
        assert_eq!(f.target.timing.start_in_ms, Some(17_000));
        let other = f.coverage.iter().find(|c| c.attempt_id == "other").unwrap();
        assert_eq!(other.node_ids, ["d", "s", "s1", "s2"]);
        assert_eq!(other.expansion_seq, 1);
        assert!(!other.node_ids.contains(&"done".to_string()));
    }

    #[test]
    fn the_forecast_serializes_nulls_and_reasons() {
        let i = insp(1, calm(), vec![running("r", 1, Some(1.0), 5)]);
        let f = run(&i, &plan(vec![node("n", 1, Runtime::fallback())]), &[]);
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["ttl_ms"], 2000);
        assert_eq!(v["snapshot_digest"], "sha256:snap");
        assert!(v["target"]["finish_in_ms"].is_null());
        assert!(v["target"]["null_reasons"]["finish_in_ms"].is_string());
        assert_eq!(v["target"]["nodes"][0]["node_id"], "n");
        assert_eq!(v["target"]["runtime_source"], "fallback");
        let back: Forecast = serde_json::from_value(v).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn missing_ready_makes_the_plan_ready_time_null() {
        let a = node("a", 1, Runtime::unknown());
        let mut b = node("b", 1, Runtime::measured(1.0, 3));
        b.depends_on = vec!["a".into()];
        let f = run(&insp(2, calm(), vec![]), &plan(vec![a, b]), &[]);
        assert_eq!(f.target.timing.ready_in_ms, None);
        assert!(f.target.timing.null_reasons.contains_key("ready_in_ms"));
        assert_eq!(f.target.timing.queue_wait_after_ready_ms, None);
    }

    #[test]
    fn measured_runtime_accepts_only_non_negative_numbers() {
        assert_eq!(Runtime::measured(1.5, 1).duration_ms(), (Some(1500), true));
        assert_eq!(
            Runtime::measured(-1.0, 1).duration_ms(),
            (Some(FALLBACK_MS), false)
        );
        let no_number = Runtime {
            secs: None,
            source: RuntimeSource::Measured,
            history_count: 1,
            latest_sample: None,
        };
        assert_eq!(no_number.duration_ms(), (Some(FALLBACK_MS), false));
        assert_eq!(Runtime::unknown().duration_ms(), (None, false));
        assert_eq!(Runtime::fallback().duration_ms(), (Some(60_000), false));
    }

    #[test]
    fn the_measured_charge_of_a_running_entry_counts() {
        // Declared weight 1, measured at 2 cores: budget 2 has no room.
        let mut r = running("r", 1, Some(30.0), 0);
        r.entry.using = Some(2.0);
        let t = single(
            &insp(2, calm(), vec![r]),
            node("n", 1, Runtime::measured(1.0, 3)),
        );
        assert_eq!(t.start_in_ms, Some(30_000));
    }

    fn nodes_of(f: &Forecast) -> Vec<&str> {
        f.target.critical_path.iter().map(String::as_str).collect()
    }

    #[test]
    fn four_parallel_nodes_are_not_summed() {
        let ns: Vec<_> = (1..=4)
            .map(|k| node(&format!("n{k}"), 1, Runtime::measured(20.0, 3)))
            .collect();
        let f = run(&insp(4, calm(), vec![]), &plan(ns), &[]);
        let t = &f.target.timing;
        assert_eq!(t.finish_in_ms, Some(20_000));
        assert_eq!((t.execution_ms, t.queue_wait_after_ready_ms), (None, None));
        assert!(t.null_reasons["execution_ms"].contains("parallel"));
        assert_eq!(nodes_of(&f), ["n1"]);
        assert_eq!(f.target.critical_path_execution_ms, Some(20_000));
        assert_eq!(f.target.critical_path_wait_ms, Some(0));
    }

    #[test]
    fn serial_nodes_behind_the_budget_form_the_path() {
        // Budget 1: b waits for a; the wait is a's execution, not extra.
        let a = node("a", 1, Runtime::measured(10.0, 3));
        let b = node("b", 1, Runtime::measured(20.0, 3));
        let f = run(&insp(1, calm(), vec![]), &plan(vec![a, b]), &[]);
        assert_eq!(f.target.timing.finish_in_ms, Some(30_000));
        assert_eq!(nodes_of(&f), ["a", "b"]);
        assert_eq!(f.target.critical_path_execution_ms, Some(30_000));
        assert_eq!(f.target.critical_path_wait_ms, Some(0));
    }

    #[test]
    fn a_later_start_chain_is_followed_through_every_waiter() {
        // Budget 1: x 0-5, a 5-15, b 15-35, all independent.
        let x = node("x", 1, Runtime::measured(5.0, 3));
        let a = node("a", 1, Runtime::measured(10.0, 3));
        let b = node("b", 1, Runtime::measured(20.0, 3));
        let f = run(&insp(1, calm(), vec![]), &plan(vec![x, a, b]), &[]);
        assert_eq!(nodes_of(&f), ["x", "a", "b"]);
        assert_eq!(f.target.critical_path_execution_ms, Some(35_000));
    }

    #[test]
    fn a_node_that_did_not_wait_has_no_resource_predecessor() {
        // c is held back by its ready floor, not by the budget, although a
        // happens to finish at the moment c starts.
        let a = node("a", 1, Runtime::measured(10.0, 3));
        let mut c = node("c", 1, Runtime::measured(5.0, 3));
        c.ready_in_ms = 10_000;
        let f = run(&insp(4, calm(), vec![]), &plan(vec![a, c]), &[]);
        assert_eq!(f.target.timing.finish_in_ms, Some(15_000));
        assert_eq!(nodes_of(&f), ["c"]);
        assert_eq!(f.target.critical_path_execution_ms, Some(5_000));
        assert_eq!(f.target.critical_path_wait_ms, Some(0));
    }

    #[test]
    fn a_dependency_wins_over_the_budget_and_its_wait_counts() {
        // Budget 1. p runs 0-10, q 10-15, r (after p) is ready at 10 but
        // waits for q until 15 and ends at 18.
        let p = node("p", 1, Runtime::measured(10.0, 3));
        let q = node("q", 1, Runtime::measured(5.0, 3));
        let mut r = node("r", 1, Runtime::measured(3.0, 3));
        r.depends_on = vec!["p".into()];
        let f = run(&insp(1, calm(), vec![]), &plan(vec![p, q, r]), &[]);
        assert_eq!(f.target.timing.finish_in_ms, Some(18_000));
        assert_eq!(nodes_of(&f), ["p", "r"]);
        assert_eq!(f.target.critical_path_execution_ms, Some(13_000));
        assert_eq!(f.target.critical_path_wait_ms, Some(5_000));
    }

    #[test]
    fn the_diamond_follows_its_slower_side() {
        let a = node("a", 1, Runtime::measured(10.0, 3));
        let mut b = node("b", 1, Runtime::measured(20.0, 3));
        b.depends_on = vec!["a".into()];
        let mut c = node("c", 1, Runtime::measured(5.0, 3));
        c.depends_on = vec!["a".into()];
        let mut d = node("d", 1, Runtime::measured(10.0, 3));
        d.depends_on = vec!["c".into(), "b".into()];
        let f = run(&insp(4, calm(), vec![]), &plan(vec![a, b, c, d]), &[]);
        assert_eq!(f.target.timing.finish_in_ms, Some(40_000));
        assert_eq!(nodes_of(&f), ["a", "b", "d"]);
        assert_eq!(f.target.critical_path_execution_ms, Some(40_000));
        assert_eq!(f.target.critical_path_wait_ms, Some(0));
        // The first of equally late dependencies is taken.
        let a = node("a", 1, Runtime::measured(10.0, 3));
        let b = node("b", 1, Runtime::measured(10.0, 3));
        let mut d = node("d", 1, Runtime::measured(1.0, 3));
        d.depends_on = vec!["b".into(), "a".into()];
        let f = run(&insp(4, calm(), vec![]), &plan(vec![a, b, d]), &[]);
        assert_eq!(nodes_of(&f), ["b", "d"]);
    }

    #[test]
    fn a_wait_on_the_ledger_stops_the_path_and_counts() {
        let i = insp(1, calm(), vec![running("r", 1, Some(30.0), 0)]);
        let f = run(
            &i,
            &plan(vec![
                node("n", 1, Runtime::measured(10.0, 3)),
                node("m", 1, Runtime::measured(1.0, 3)),
            ]),
            &[],
        );
        assert_eq!(nodes_of(&f), ["n", "m"]);
        // n waited 30 s on the ledger entry; m followed n.
        assert_eq!(f.target.critical_path_wait_ms, Some(30_000));
        assert_eq!(f.target.critical_path_execution_ms, Some(11_000));
        assert_eq!(f.target.timing.finish_in_ms, Some(41_000));
    }

    #[test]
    fn a_single_node_path_equals_its_own_numbers() {
        let i = insp(1, calm(), vec![running("r", 1, Some(30.0), 0)]);
        let f = run(
            &i,
            &plan(vec![node("n", 1, Runtime::measured(10.0, 3))]),
            &[],
        );
        assert_eq!(nodes_of(&f), ["n"]);
        assert_eq!(f.target.critical_path_wait_ms, Some(30_000));
        assert_eq!(f.target.critical_path_execution_ms, Some(10_000));
        assert_eq!(f.target.timing.execution_ms, Some(10_000));
        assert_eq!(f.target.timing.queue_wait_after_ready_ms, Some(30_000));
    }

    #[test]
    fn an_empty_plan_has_an_empty_path() {
        let f = run(&insp(1, calm(), vec![]), &plan(vec![]), &[]);
        assert!(f.target.critical_path.is_empty());
        assert_eq!(f.target.critical_path_wait_ms, Some(0));
        assert_eq!(f.target.critical_path_execution_ms, Some(0));
    }

    #[test]
    fn a_node_that_never_finishes_leaves_no_path() {
        let a = node("a", 1, Runtime::unknown());
        let b = node("b", 1, Runtime::measured(1.0, 3));
        let f = run(&insp(2, calm(), vec![]), &plan(vec![a, b]), &[]);
        assert!(f.target.critical_path.is_empty());
        assert_eq!(f.target.critical_path_wait_ms, None);
        assert_eq!(f.target.critical_path_execution_ms, None);
        let r = &f.target.timing.null_reasons;
        assert!(r["critical_path_wait_ms"].contains("does not finish"));
        assert!(r["critical_path_execution_ms"].contains("does not finish"));
    }

    #[test]
    fn a_fallback_node_on_the_path_makes_its_totals_null_with_a_reason() {
        let a = node("a", 1, Runtime::fallback());
        let mut b = node("b", 1, Runtime::measured(1.0, 3));
        b.depends_on = vec!["a".into()];
        let f = run(&insp(2, calm(), vec![]), &plan(vec![a, b]), &[]);
        assert_eq!(nodes_of(&f), ["a", "b"]);
        assert_eq!(f.target.critical_path_execution_ms, None);
        assert!(f.target.timing.null_reasons["critical_path_execution_ms"].contains("a: "));
        // b started after an unmeasured run: its start and wait are null too.
        assert_eq!(f.target.critical_path_wait_ms, None);
        assert!(f.target.timing.null_reasons["critical_path_wait_ms"].contains("b: "));
    }

    #[test]
    fn zero_length_nodes_do_not_loop_the_path() {
        // Budget 1, two instant nodes: each "freed" the other's start.
        let a = node("a", 1, Runtime::measured(0.0, 3));
        let b = node("b", 1, Runtime::measured(0.0, 3));
        let f = run(&insp(1, calm(), vec![]), &plan(vec![a, b]), &[]);
        assert!(!f.target.critical_path.is_empty());
        assert!(f.target.critical_path.len() <= 2);
    }

    #[test]
    fn a_second_overlay_of_shared_work_behind_a_ledger_entry_is_not_new() {
        let mut first = demand("x", 1, 10.0);
        first.shared_id = Some("setup".into());
        first.ledger_entry = Some("w".into());
        let mut second = demand("y", 1, 10.0);
        second.shared_id = Some("setup".into());
        let i = insp(1, calm(), vec![waiting("w", 1, Some(10.0), 2)]);
        let f = run(
            &i,
            &plan(vec![node("n", 1, Runtime::measured(1.0, 3))]),
            &[first, second],
        );
        assert_eq!(f.overlay.replaced_by_ledger, ["other/x"]);
        assert_eq!(f.overlay.duplicates_dropped, ["other/y"]);
        assert!(f.overlay.applied.is_empty());
        // w (10 s) then n: nothing extra.
        assert_eq!(f.target.timing.finish_in_ms, Some(11_000));
    }

    #[test]
    fn an_overlay_with_the_key_of_a_merged_away_node_is_a_duplicate() {
        let mut s1 = node("s1", 1, Runtime::measured(10.0, 3));
        s1.shared_id = Some("setup".into());
        let mut s2 = node("s2", 1, Runtime::measured(10.0, 3));
        s2.shared_id = Some("setup".into());
        let mut d = demand("s2", 1, 10.0);
        d.attempt_id = "att".into();
        d.plan_digest = "sha256:plan".into();
        d.expansion_seq = 0;
        let f = run(&insp(1, calm(), vec![]), &plan(vec![s1, s2]), &[d]);
        assert_eq!(f.overlay.duplicates_dropped, ["att/s2"]);
        assert!(f.overlay.applied.is_empty());
        assert_eq!(f.target.timing.finish_in_ms, Some(10_000));
    }

    #[test]
    fn a_dependency_between_merged_nodes_is_named() {
        let mut s1 = node("s1", 1, Runtime::measured(1.0, 3));
        s1.shared_id = Some("setup".into());
        let mut s2 = node("s2", 1, Runtime::measured(1.0, 3));
        s2.shared_id = Some("setup".into());
        s2.depends_on = vec!["s1".into()];
        let err = forecast(
            &ctx().0,
            &insp(1, calm(), vec![]),
            &plan(vec![s1.clone(), s2]),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("shared_id setup"), "{err}");
        assert!(err.contains("s1") && err.contains("s2"), "{err}");
        assert!(!err.contains("cycle"), "{err}");
        // Different shared ids, or one without, are fine.
        let mut other = node("o", 1, Runtime::measured(1.0, 3));
        other.shared_id = Some("other".into());
        other.depends_on = vec!["s1".into()];
        let mut plain = node("p", 1, Runtime::measured(1.0, 3));
        plain.depends_on = vec!["s1".into()];
        assert!(forecast(
            &ctx().0,
            &insp(1, calm(), vec![]),
            &plan(vec![s1, other, plain]),
            &[]
        )
        .is_ok());
    }

    #[test]
    fn only_the_entry_that_freed_the_budget_counts_for_confidence() {
        // Budget 2: r (well known) frees a slot at 30 s, q (unknown) runs on.
        let i = with_history(
            with_history(
                insp(
                    2,
                    calm(),
                    vec![
                        running("r", 1, Some(30.0), 0),
                        running("q", 1, Some(100.0), 0),
                    ],
                ),
                "r",
                5,
            ),
            "q",
            0,
        );
        let t = single(&i, node("n", 1, Runtime::measured(1.0, 5)));
        assert_eq!(t.start_in_ms, Some(30_000));
        assert_eq!(t.confidence, Confidence::High);
        // A plan node that finishes right before is not a ledger entry.
        let a = node("a", 1, Runtime::measured(10.0, 1));
        let mut b = node("b", 1, Runtime::measured(1.0, 5));
        b.depends_on = vec!["a".into()];
        let f = run(&insp(2, calm(), vec![]), &plan(vec![a, b]), &[]);
        let b = &f.target.nodes[1].timing;
        assert_eq!(b.start_in_ms, Some(10_000));
        assert_eq!(b.confidence, Confidence::High);
        assert!(!b.assumptions.iter().any(|x| x.contains("recorded runs")));
    }

    fn with_history(mut i: Inspection, key: &str, records: usize) -> Inspection {
        i.history.push(crate::inspect::HistoryRef {
            key: key.into(),
            records,
            latest: None,
        });
        i
    }

    #[test]
    fn a_start_behind_a_thin_ledger_history_lowers_confidence() {
        let n = node("n", 1, Runtime::measured(10.0, 5));
        let thin = with_history(
            insp(1, calm(), vec![running("r", 1, Some(30.0), 0)]),
            "r",
            1,
        );
        let t = single(&thin, n.clone());
        assert_eq!(t.finish_in_ms, Some(40_000));
        assert_eq!(t.confidence, Confidence::Medium);
        assert!(t.assumptions.iter().any(|a| a.contains("entry r")));
        let rich = with_history(
            insp(1, calm(), vec![running("r", 1, Some(30.0), 0)]),
            "r",
            5,
        );
        let t = single(&rich, n.clone());
        assert_eq!(t.confidence, Confidence::High);
        assert!(!t.assumptions.iter().any(|a| a.contains("entry r")));
        // Three runs are enough, two are not.
        for (records, want) in [(2, Confidence::Medium), (3, Confidence::High)] {
            let i = with_history(
                insp(1, calm(), vec![running("r", 1, Some(30.0), 0)]),
                "r",
                records,
            );
            let t = single(&i, n.clone());
            assert_eq!(t.confidence, want, "{records}");
            let noted = t.assumptions.iter().any(|a| a.contains("recorded runs"));
            assert_eq!(noted, records < 3, "{records}");
        }
        // Not behind the entry: its history does not matter.
        let idle = with_history(
            insp(2, calm(), vec![running("r", 1, Some(30.0), 0)]),
            "r",
            0,
        );
        assert_eq!(single(&idle, n).confidence, Confidence::High);
    }

    #[test]
    fn a_running_entry_without_a_start_time_has_unknown_remaining_time() {
        let mut r = running("r", 1, Some(30.0), 5);
        r.entry.started_at = None;
        let t = single(
            &insp(1, calm(), vec![r]),
            node("n", 1, Runtime::measured(1.0, 3)),
        );
        assert_eq!(t.start_in_ms, None);
        assert!(t.null_reasons["start_in_ms"].contains("unknown"));
        assert!(t
            .assumptions
            .iter()
            .any(|a| a.contains("1 running entries have unknown remaining time")));
    }

    /// Zero-length nodes `w` (0), `p` (1), `q` (2) and an earlier `r` (3);
    /// w depends on p, q on w and r. p waited for a budget slot (start after
    /// ready) until t, and w, p, q finish at t. The resource-wait jump from
    /// p lands on q, whose dependency w is already on the path. Before the
    /// `visited` check on dependencies the walk went q -> w -> ... and never
    /// returned (the length cap only bounds it now); with it q's next step
    /// is the unvisited r.
    #[test]
    fn critical_path_does_not_loop_through_visited_dependencies() {
        let t = 5_000u64;
        let node = |id: &str| NodeForecast {
            node_id: id.into(),
            timing: Timing {
                ready_in_ms: Some(0),
                start_in_ms: Some(t),
                queue_wait_after_ready_ms: Some(0),
                execution_ms: Some(0),
                finish_in_ms: Some(t),
                null_reasons: BTreeMap::new(),
                ranking: None,
                confidence: Confidence::High,
                runtime_source: RuntimeSource::Measured,
                history_count: 3,
                conditional: false,
                assumptions: Vec::new(),
                wait_reason: None,
            },
        };
        let pi = |deps: Vec<usize>, ready: u64| PathInfo {
            deps,
            ready: Some(ready),
            start: Some(t),
            finish: Some(t),
        };
        let mut r = node("r");
        r.timing.start_in_ms = Some(0);
        r.timing.finish_in_ms = Some(0);
        let nodes = vec![node("w"), node("p"), node("q"), r];
        // p: waited for budget (ready 0, start t); q: ready 0 too.
        let info = vec![
            pi(vec![1], t),
            pi(vec![], 0),
            pi(vec![0, 3], 0),
            PathInfo {
                deps: vec![],
                ready: Some(0),
                start: Some(0),
                finish: Some(0),
            },
        ];
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(critical_path(&nodes, &info).nodes);
        });
        let path = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("critical_path must terminate");
        assert_eq!(path, ["r", "q", "p", "w"]);
    }
}
