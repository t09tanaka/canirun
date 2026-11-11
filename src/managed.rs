//! Commands whose lifecycle a caller (another program driving task-traffic-controller as a
//! library) owns, as opposed to a plain `task-traffic-controller exec`.
//!
//! The caller picks a stable `command_id` and a `spec_digest` before
//! anything is registered, so its own durable record ("accepted", kept
//! outside task-traffic-controller) can name the ledger entry the command will occupy.
//! From there the stages are (see [`crate::receipt`]):
//!
//! 1. [`register`]: takes the lease, writes the sidecar, then pushes the
//!    ledger entry. The receipt is returned only after the entry is saved.
//!    Registering the same id and digest again returns the existing
//!    registration; a different digest is a [`Error::Conflict`].
//! 2. [`try_admit`] (poll it): admission under the ledger lock, with the
//!    reason when the answer is to wait.
//! 3. [`run_command`]: persists the launch intent, spawns the child in its
//!    own process group, records its identity, waits.
//! 4. [`finish`]: records the terminal outcome, *then* removes the ledger
//!    entry and the lease.
//! 5. [`record_measurement`]: applies the measurement to the history once
//!    per measurement id (the id is stored in the history record). A failure here never re-runs anything.
//!
//! # The sidecar
//!
//! `managed/<command_id>.json` next to the ledger holds what the ledger
//! cannot: the spec digest, the caller's correlation data, the stage, the
//! boot id, the child's pid, start time and process group. A ledger holding
//! a managed entry is saved in a shape an older task-traffic-controller cannot parse (see
//! [`crate::ledger::Ledger`]), so it neither reaps nor rewrites it. The
//! sidecar is written *before* the ledger entry, so an entry without a
//! sidecar is a plain entry, never a managed one whose file went missing.
//!
//! # Reaping
//!
//! The reaper releases a plain entry as soon as its lease is gone. A
//! managed entry is released only when [`crate::reap::verdict`] says so:
//! terminal recorded, machine rebooted, never launched, or the recorded
//! child, its process group and every process carrying the command's
//! `TASK_TRAFFIC_CONTROLLER_MANAGED_ID` are gone. Otherwise it stays in the ledger with its
//! charge (quarantine), and `inspect` reports it. The marker finds
//! descendants that left the process group (`setsid`, double fork); one that
//! cleared its environment (`env -i`) is not found. A launch intent (the
//! spawn may or may not have happened) is settled as "nothing is running"
//! once the lease is gone, 60 seconds have passed and no marked process
//! exists; an unknown scan or a missing timestamp never releases it. An
//! unreadable sidecar is never treated as free capacity by the reaper.
//! [`resolve_checked`] (`task-traffic-controller resolve <id>`) settles a quarantined entry
//! by hand after the same checks; [`resolve`] is for a recovery process that
//! has investigated on its own.
//!
//! A `command_id` is single use: once its terminal outcome is recorded,
//! registering it again returns [`Registration::AlreadyTerminal`] and never
//! creates an entry. That holds for as long as the receipt (the sidecar) is
//! kept: `task-traffic-controller gc` removes the receipt of a command that ended more than
//! `managed_keep_days` (default 7) ago, after which the id is free again.
//! A run that was past `Registered` when the machine rebooted is settled the
//! same way whether the reaper or a new registration notices first: as
//! interrupted, and never run again.

use crate::admit::{self, Decision, Limits, QueueEntry, Reason, State};
use crate::error::{Error, Result};
use crate::eta;
use crate::exec::{self, Opts, Plan};
use crate::history::{self, Record};
use crate::ledger::{Entry, Ledger};
use crate::lineage::{self, Tracked};
use crate::lock::{self, Lock};
use crate::marker::{HiddenProc, ProcRef};
use crate::process::{self, ChildIdentity};
use crate::reap::{self, Probe, SidecarFacts, Verdict};
use crate::receipt::{self, Phase};
use crate::runner::{self, Exit, Outcome};
use crate::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

pub const SIDECAR_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalKind {
    Exited {
        code: i32,
    },
    Signaled {
        signal: i32,
    },
    SpawnFailed,
    Cancelled,
    /// Released by the reaper after the launch happened: how it ended is
    /// unknown.
    Interrupted,
    /// Released by the reaper before anything was spawned.
    Abandoned,
}

impl From<Exit> for TerminalKind {
    fn from(e: Exit) -> Self {
        match e {
            Exit::Code(code) => TerminalKind::Exited { code },
            Exit::Signal(signal) => TerminalKind::Signaled { signal },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalRecord {
    #[serde(flatten)]
    pub kind: TerminalKind,
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sidecar {
    pub version: u32,
    pub command_id: String,
    pub spec_digest: String,
    pub phase: Phase,
    /// Boot id when registered; `None` when the OS gave none.
    pub boot_id: Option<String>,
    pub registered_at: DateTime<Utc>,
    pub owner_pid: u32,
    pub owner_start_time: Option<u64>,
    /// The caller's correlation data (attempt, node, ...), opaque here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<serde_json::Value>,
    /// Services the caller says belong to this command, for its cleanup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_intent_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child: Option<ChildIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalRecord>,
    /// Descendants of the child seen while it ran (pid and start time), at
    /// most [`lineage::MAX_DESCENDANTS`]. Optional: older sidecars have
    /// none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub descendants: Vec<ProcRef>,
    /// More were seen than fit: the rest are not recorded.
    #[serde(default, skip_serializing_if = "is_false")]
    pub descendants_truncated: bool,
    /// When the tracker last wrote its record (descendants or an unknown
    /// outcome). A hidden process that started long after this is not a
    /// descendant the tracker missed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<DateTime<Utc>>,
    /// The tracker provably covered the child's whole life: the supervisor
    /// saw it exit, took the last look and persisted it, and `recorded_at`
    /// is that look. Only then does the hidden-orphan window end at
    /// `recorded_at`; without it (the supervisor died mid-run) a hidden
    /// process that started at any time after the child may be its
    /// descendant, so the entry stays held until `resolve --force`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub tracked_to_exit: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Sidecar {
    /// The recorded descendants, for the process scan.
    pub fn tracked(&self) -> Tracked {
        Tracked {
            descendants: self.descendants.clone(),
            truncated: self.descendants_truncated,
        }
    }
}

/// What reading a sidecar found.
#[derive(Debug, Clone, PartialEq)]
pub enum Loaded {
    Missing,
    Corrupt(String),
    Ok(Box<Sidecar>),
}

/// Ids become file names; keep them to a safe alphabet.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && !id.starts_with('.')
}

fn check_id(kind: &str, id: &str) -> Result<()> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(Error::Conflict(format!(
            "{kind} {id:?} must be 1-128 of [A-Za-z0-9._-] and not start with '.'"
        )))
    }
}

pub fn load_sidecar(ctx: &Context, id: &str) -> Loaded {
    if !valid_id(id) {
        return Loaded::Missing;
    }
    match std::fs::read(ctx.paths.managed_file(id)) {
        Ok(bytes) => match serde_json::from_slice::<Sidecar>(&bytes) {
            Ok(sc) if sc.command_id == id && sc.version == SIDECAR_VERSION => {
                Loaded::Ok(Box::new(sc))
            }
            Ok(_) => Loaded::Corrupt("id or version mismatch".into()),
            Err(e) => Loaded::Corrupt(e.to_string()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Loaded::Missing,
        Err(e) => Loaded::Corrupt(e.to_string()),
    }
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer_pretty(&mut tmp, value)?;
    tmp.write_all(b"\n")?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn write_sidecar(ctx: &Context, sc: &Sidecar) -> Result<()> {
    write_json_atomic(&ctx.paths.managed_file(&sc.command_id), sc)
}

/// Move the sidecar to `to` (legal moves only) and apply `f`.
fn advance_sidecar(
    ctx: &Context,
    id: &str,
    to: Phase,
    f: impl FnOnce(&mut Sidecar),
) -> Result<Sidecar> {
    let Loaded::Ok(mut sc) = load_sidecar(ctx, id) else {
        return Err(Error::Ledger(format!(
            "sidecar for {id} is missing or unreadable"
        )));
    };
    let moved = receipt::advance(sc.phase, to).map_err(|e| Error::Ledger(format!("{id}: {e}")))?;
    if moved == receipt::Advance::Noop {
        // Already there: what is recorded (a terminal outcome above all)
        // stays as it is.
        return Ok(*sc);
    }
    sc.phase = to;
    f(&mut sc);
    write_sidecar(ctx, &sc)?;
    Ok(*sc)
}

/// The reaper's call for one ledger entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Assessment {
    pub verdict: Verdict,
    pub sidecar: Loaded,
}

/// Gather the facts for `id` and decide. Probes processes only for a
/// managed entry with no lease holder, and only as far as the answer can
/// still change. Writes nothing.
pub fn assess(ctx: &Context, id: &str) -> Assessment {
    assess_with(ctx, id, false, true, false)
}

/// [`assess`] without any process scan: where a scan would be needed the
/// answer stays unknown, so it can keep an entry but never release one that
/// needs the scan. Cheap enough to run under the ledger lock.
pub(crate) fn assess_unscanned(ctx: &Context, id: &str) -> Assessment {
    assess_with(ctx, id, false, false, false)
}

/// [`assess`], for a person settling the entry by hand when `manual`.
/// `scan` false skips the (slow) marker scans. `ignore_group` leaves the
/// recorded process group out of the answer (the group number may have been
/// reused by unrelated processes): the child, the marker and the recorded
/// descendants decide alone.
fn assess_with(
    ctx: &Context,
    id: &str,
    manual: bool,
    scan: bool,
    ignore_group: bool,
) -> Assessment {
    assess_detailed(ctx, id, manual, scan, ignore_group).0
}

/// A hidden process that keeps an entry unresolved: it may be a descendant
/// of the command that nobody recorded (see [`crate::lineage`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HiddenSuspect {
    pub proc: ProcRef,
    pub parent: Option<u32>,
    /// Its session id, when it could be read.
    pub sid: Option<u32>,
    pub standing: lineage::Standing,
}

impl HiddenSuspect {
    /// Why it cannot be ruled out, in words.
    pub fn reason(&self) -> &'static str {
        use lineage::Standing;
        match self.standing {
            Standing::NoOwnSession => {
                "the command's child is not recorded as the leader of its own session"
            }
            Standing::SessionUnknown => "its start time or session could not be read",
            Standing::OwnSession => "it leads its own session, as a setsid escapee does",
            Standing::LeaderGone => "its session's leader is gone",
            Standing::LeaderInWindow => "its session's leader started after the command",
            Standing::ClockDisorder => "it is older than its session's leader (the clock moved)",
            _ => "it is not a possible descendant",
        }
    }
}

impl std::fmt::Display for HiddenSuspect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pid {}", self.proc.pid)?;
        if let Some(t) = self.proc.start_time {
            write!(f, " started {t}")?;
        }
        if let Some(s) = self.sid {
            write!(f, " session {s}")?;
        }
        write!(f, ": {}", self.reason())
    }
}

/// The hidden processes in `hidden` that may be unrecorded descendants.
fn suspects(
    ctx: &Context,
    hidden: &[HiddenProc],
    (since, until): (u64, Option<u64>),
    supervisor: u32,
    own_session: bool,
) -> Vec<HiddenSuspect> {
    hidden
        .iter()
        .filter(|h| lineage::orphan_in_window(h, since, until, supervisor))
        .filter_map(|h| {
            // The machine is asked only where the answer can clear it.
            let session = if own_session {
                ctx.scan.session_evidence(&h.proc)
            } else {
                lineage::SessionEvidence::default()
            };
            let standing = lineage::standing(h, since, until, supervisor, own_session, session);
            standing.suspicious().then_some(HiddenSuspect {
                proc: h.proc,
                parent: h.parent,
                sid: session.sid,
                standing,
            })
        })
        .collect()
}

/// The hidden processes that keep `id` from being resolved right now: each
/// may be a descendant of the command that nobody recorded, and
/// [`HiddenSuspect::reason`] says why it cannot be ruled out. Empty when the
/// entry is not kept for that reason (it is released, its owner or a known
/// process still runs, or a scan could not read every process). Judged as
/// [`resolve_checked`] does; writes nothing.
pub fn hidden_suspects(ctx: &Context, id: &str) -> Vec<HiddenSuspect> {
    assess_detailed(ctx, id, true, true, false).1
}

/// [`assess_with`], also naming the hidden processes that made the scan
/// unknown.
fn assess_detailed(
    ctx: &Context,
    id: &str,
    manual: bool,
    scan: bool,
    ignore_group: bool,
) -> (Assessment, Vec<HiddenSuspect>) {
    let mut held_by: Vec<HiddenSuspect> = Vec::new();
    let settled = |tracked: &Tracked| {
        if scan {
            runner::settled_scan(ctx, id, tracked)
        } else {
            crate::marker::Scan::new(Probe::Unknown, Vec::new())
        }
    };
    let lease_held = lock::lease_is_held(ctx, id);
    let sidecar = load_sidecar(ctx, id);
    let (facts, child) = match &sidecar {
        Loaded::Missing => (SidecarFacts::Missing, None),
        Loaded::Corrupt(_) => (SidecarFacts::Corrupt, None),
        Loaded::Ok(sc) => (
            SidecarFacts::Managed {
                phase: sc.phase,
                boot_id: sc.boot_id.clone(),
                launch_age_secs: sc
                    .launch_intent_at
                    .map(|t| (ctx.clock.now() - t).num_seconds()),
            },
            sc.child.clone(),
        ),
    };
    let (tracked, intent, supervisor, recorded, to_exit) = match &sidecar {
        Loaded::Ok(sc) => (
            sc.tracked(),
            sc.launch_intent_at,
            sc.owner_pid,
            sc.recorded_at,
            sc.tracked_to_exit,
        ),
        _ => (Tracked::default(), None, 0, None, false),
    };
    let secs = |t: DateTime<Utc>| t.timestamp().max(0) as u64;
    let (mut child_probe, mut group_probe, mut marker) =
        (Probe::Unknown, Probe::Unknown, Probe::Unknown);
    if !lease_held {
        match &facts {
            SidecarFacts::Managed {
                phase: Phase::ChildStarted,
                ..
            } => {
                if let Some(c) = &child {
                    child_probe = process::probe_child(c);
                    group_probe = if ignore_group {
                        Probe::Gone
                    } else {
                        process::probe_group(c.pgid)
                    };
                    // Whatever left the group but not the session (and
                    // whatever cleared its environment) still belongs to it.
                    if scan && child_probe != Probe::Alive && group_probe != Probe::Alive {
                        group_probe = reap::merge_probes(group_probe, ctx.scan.probe_session(c));
                    }
                }
                // A live child or group already keeps the entry.
                if child_probe != Probe::Alive && group_probe != Probe::Alive {
                    let scan = settled(&tracked);
                    marker = scan.probe;
                    // Nothing marked or recorded runs (a recorded one alive
                    // would make the scan `Alive`) and the child and its
                    // group are gone. A hidden orphan that began after the
                    // child may still be its descendant.
                    if marker == Probe::Gone
                        && child_probe == Probe::Gone
                        && group_probe == Probe::Gone
                    {
                        let (since, until) = lineage::suspicion_window(
                            child.as_ref().and_then(|c| c.start_time),
                            intent.map(secs),
                            recorded.map(secs),
                            to_exit,
                        );
                        // The child leads its own session (`setsid` at the
                        // spawn), so a session that is provably older is
                        // not the command's.
                        let own_session = child.as_ref().is_some_and(|c| {
                            c.sid.and_then(|s| u32::try_from(s).ok()) == Some(c.pid)
                        });
                        held_by =
                            suspects(ctx, &scan.hidden, (since, until), supervisor, own_session);
                        if !held_by.is_empty() {
                            marker = Probe::Unknown;
                        }
                    }
                }
            }
            SidecarFacts::Managed {
                phase: Phase::LaunchIntent,
                launch_age_secs,
                ..
            } if manual || reap::grace_passed(*launch_age_secs) => {
                let scan = settled(&tracked);
                // A clear scan does not show a child whose environment the
                // OS hides. One that began after the intent and is an
                // orphan (or the dead supervisor's child) may be it.
                // No child was recorded, so nothing says it leads a session
                // of its own (a fork may not have reached its `setsid`):
                // sessions clear nothing here.
                let window =
                    lineage::suspicion_window(None, intent.map(secs), recorded.map(secs), to_exit);
                if scan.probe == Probe::Gone {
                    held_by = suspects(ctx, &scan.hidden, window, supervisor, false);
                }
                marker = if held_by.is_empty() {
                    scan.probe
                } else {
                    Probe::Unknown
                };
            }
            SidecarFacts::Corrupt if manual => marker = settled(&Tracked::default()).probe,
            _ => {}
        }
    }
    let verdict = reap::verdict(&reap::Inputs {
        lease_held,
        sidecar: facts,
        current_boot: ctx.boot.boot_id(),
        child: child_probe,
        group: group_probe,
        marker,
        manual,
    });
    if verdict != Verdict::Keep(reap::Keep::ProbeUnknown) {
        held_by.clear();
    }
    (Assessment { verdict, sidecar }, held_by)
}

/// `what`, plus the hidden processes that hold the entry (the first few).
fn with_suspects(what: &str, held_by: &[HiddenSuspect]) -> String {
    const SHOWN: usize = 3;
    if held_by.is_empty() {
        return what.to_string();
    }
    let named: Vec<String> = held_by.iter().take(SHOWN).map(|h| h.to_string()).collect();
    let more = match held_by.len().saturating_sub(SHOWN) {
        0 => String::new(),
        n => format!("; and {n} more"),
    };
    format!(
        "{what}; held by hidden process {}{more}",
        named.join("; hidden process ")
    )
}

/// On release by the reaper, settle a managed sidecar that has no terminal
/// yet, so the owner's caller can learn how the command ended.
pub(crate) fn settle_released(ctx: &Context, a: &Assessment) {
    let _ = try_settle_released(ctx, a);
}

/// [`settle_released`], telling whether the outcome could be written.
fn try_settle_released(ctx: &Context, a: &Assessment) -> Result<()> {
    let Loaded::Ok(sc) = &a.sidecar else {
        return Ok(());
    };
    if sc.phase == Phase::Terminal {
        return Ok(());
    }
    let kind = if sc.phase == Phase::Registered {
        TerminalKind::Abandoned
    } else {
        TerminalKind::Interrupted
    };
    let at = ctx.clock.now();
    advance_sidecar(ctx, &sc.command_id, Phase::Terminal, |s| {
        s.terminal = Some(TerminalRecord {
            kind,
            at,
            detail: Some(format!("released by the reaper: {:?}", a.verdict)),
        });
    })
    .map(|_| ())
}

/// How `inspect` shows a managed entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagedView {
    pub command_id: String,
    pub phase: Option<Phase>,
    pub lease_held: bool,
    /// The lease is gone but the entry is kept, its charge held.
    pub quarantined: bool,
    /// Empty when not quarantined.
    pub reason: String,
    /// How to settle a quarantined entry; empty otherwise.
    pub hint: String,
    pub child: Option<ChildIdentity>,
    pub boot_id: Option<String>,
}

/// Managed entries among `entries`, read-only. Plain entries are left out.
pub fn views(ctx: &Context, entries: &[Entry]) -> Vec<ManagedView> {
    entries
        .iter()
        .filter_map(|e| {
            let a = assess(ctx, &e.id);
            let lease_held = a.verdict == Verdict::Keep(reap::Keep::LeaseHeld);
            let (phase, child, boot_id) = match &a.sidecar {
                Loaded::Missing => return None,
                Loaded::Corrupt(_) => (None, None, None),
                Loaded::Ok(sc) => (Some(sc.phase), sc.child.clone(), sc.boot_id.clone()),
            };
            let quarantined = !lease_held && matches!(a.verdict, Verdict::Keep(_));
            Some(ManagedView {
                command_id: e.id.clone(),
                phase,
                lease_held,
                quarantined,
                reason: if quarantined {
                    format!("{:?}", a.verdict)
                } else {
                    String::new()
                },
                hint: if quarantined {
                    format!("task-traffic-controller resolve {}", e.id)
                } else {
                    String::new()
                },
                child,
                boot_id,
            })
        })
        .collect()
}

/// What to register.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub command_id: String,
    pub spec_digest: String,
    pub opts: Opts,
    pub attempt: Option<serde_json::Value>,
    pub services: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredReceipt {
    pub command_id: String,
    pub spec_digest: String,
    pub phase: Phase,
    pub seq: u64,
    /// False when this call found an existing registration.
    pub newly_registered: bool,
    pub boot_id: Option<String>,
}

/// Holds the command's lease: the proof that its supervisor is alive.
/// Dropping it releases the lease but leaves the entry for the reaper.
#[derive(Debug)]
pub struct OwnerGuard {
    _lease: Lock,
    id: String,
    managed: bool,
    /// The recorder of the command's descendants failed in this process, so
    /// nothing can be ruled out: [`finish`] does not release.
    tracker_unknown: std::sync::atomic::AtomicBool,
}

impl OwnerGuard {
    pub(crate) fn new(lease: Lock, id: &str, managed: bool) -> Self {
        OwnerGuard {
            _lease: lease,
            id: id.to_string(),
            managed,
            tracker_unknown: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn command_id(&self) -> &str {
        &self.id
    }
}

#[derive(Debug)]
pub enum Registration {
    Registered(RegisteredReceipt, OwnerGuard),
    /// This id already ended; no entry was created and nothing must run.
    AlreadyTerminal(Box<Sidecar>),
}

/// What `push_entry` did.
pub(crate) enum Pushed {
    Interrupted(i32),
    New { seq: u64 },
    Existing { seq: u64, sidecar: Box<Sidecar> },
    Terminal(Box<Sidecar>),
}

pub(crate) struct ManagedNew<'a> {
    pub spec_digest: &'a str,
    pub attempt: Option<&'a serde_json::Value>,
    pub services: &'a [String],
}

/// Reap, then (idempotently) put the entry for `id` in the ledger. With
/// `managed`, the sidecar is written first. `stop` is polled between
/// attempts at the ledger lock; without it a busy lock is an error after
/// the context's timeout.
pub(crate) fn push_entry(
    ctx: &Context,
    id: &str,
    opts: &Opts,
    plan: &Plan,
    managed: Option<ManagedNew>,
    mut stop: Option<&mut dyn FnMut() -> Option<i32>>,
) -> Result<Pushed> {
    loop {
        if let Some(stop) = stop.as_deref_mut() {
            if let Some(sig) = stop() {
                return Ok(Pushed::Interrupted(sig));
            }
        }
        lock::refresh_verdicts(ctx);
        let _guard = match stop {
            Some(_) => match lock::try_ledger(ctx)? {
                Some(g) => g,
                None => continue,
            },
            None => lock::ledger(ctx)?,
        };
        let mut ledger = Ledger::load(&ctx.paths.ledger_file())?;
        lock::reap_dead(ctx, &mut ledger);
        let mut existing = None;
        if let Some(m) = &managed {
            match (ledger.find(id).map(|e| e.seq), load_sidecar(ctx, id)) {
                (Some(seq), Loaded::Ok(sc)) => {
                    if sc.spec_digest != m.spec_digest {
                        return Err(Error::Conflict(format!(
                            "command {id} is registered with a different spec digest"
                        )));
                    }
                    existing = Some((seq, sc));
                }
                (Some(_), _) => {
                    return Err(Error::Conflict(format!(
                        "ledger entry {id} exists but has no readable sidecar"
                    )))
                }
                (None, Loaded::Ok(sc)) => {
                    // A sidecar settled by hand with nothing readable left
                    // has no digest to compare: it still answers for its id
                    // (a finished outcome), instead of blocking it.
                    let unknown_settled = sc.phase == Phase::Terminal && sc.spec_digest.is_empty();
                    if sc.spec_digest != m.spec_digest && !unknown_settled {
                        return Err(Error::Conflict(format!(
                            "command {id} was used with a different spec digest"
                        )));
                    }
                    match sc.phase {
                        Phase::Terminal => return Ok(Pushed::Terminal(sc)),
                        // Crashed between the sidecar and the entry.
                        Phase::Registered => {}
                        _ => {
                            return Err(Error::Ledger(format!(
                                "sidecar {id} is at {:?} with no ledger entry",
                                sc.phase
                            )))
                        }
                    }
                }
                (None, Loaded::Corrupt(why)) => {
                    return Err(Error::Conflict(format!(
                        "sidecar {id} is unreadable: {why}"
                    )))
                }
                (None, Loaded::Missing) => {}
            }
        }
        if let Some((seq, mut sc)) = existing {
            // A new supervisor adopting the registration.
            let boot = ctx.boot.boot_id();
            match reap::adopt(sc.phase, sc.boot_id.as_deref(), boot.as_deref()) {
                reap::Adopt::SettleStale => {
                    // The run belongs to an earlier boot and got past
                    // Registered: its outcome is recorded exactly as the
                    // reaper records it, and the id stays used up. The
                    // caller learns it was interrupted; it is never run
                    // again under the same id.
                    let a = Assessment {
                        verdict: Verdict::Release(reap::Release::BootChanged),
                        sidecar: Loaded::Ok(sc),
                    };
                    try_settle_released(ctx, &a)?;
                    ledger.remove(id);
                    ledger.save(&ctx.paths.ledger_file())?;
                    return match load_sidecar(ctx, id) {
                        Loaded::Ok(done) => Ok(Pushed::Terminal(done)),
                        _ => Err(Error::Ledger(format!(
                            "sidecar for {id} is missing or unreadable"
                        ))),
                    };
                }
                adopt => {
                    if adopt == reap::Adopt::RefreshBoot {
                        sc.boot_id = boot;
                    }
                    sc.owner_pid = std::process::id();
                    sc.owner_start_time = process::start_time(std::process::id());
                    write_sidecar(ctx, &sc)?;
                    return Ok(Pushed::Existing { seq, sidecar: sc });
                }
            }
        }
        let seq = ledger.take_seq();
        if let Some(m) = &managed {
            write_sidecar(
                ctx,
                &Sidecar {
                    version: SIDECAR_VERSION,
                    command_id: id.to_string(),
                    spec_digest: m.spec_digest.to_string(),
                    phase: Phase::Registered,
                    boot_id: ctx.boot.boot_id(),
                    registered_at: ctx.clock.now(),
                    owner_pid: std::process::id(),
                    owner_start_time: process::start_time(std::process::id()),
                    attempt: m.attempt.cloned(),
                    services: m.services.to_vec(),
                    launch_intent_at: None,
                    child: None,
                    terminal: None,
                    descendants: Vec::new(),
                    descendants_truncated: false,
                    recorded_at: None,
                    tracked_to_exit: false,
                },
            )?;
        }
        ledger.entries.push(Entry {
            id: id.to_string(),
            pid: std::process::id(),
            state: State::Waiting,
            front: opts.front,
            weight: plan.weight,
            predicted_rss: plan.predicted_rss,
            expected_secs: plan.expected_secs,
            repo: plan.key.repo.clone(),
            subdir: plan.key.subdir.clone(),
            argv: plan.key.argv.clone(),
            key: plan.key.hash.clone(),
            seq,
            enqueued_at: ctx.clock.now(),
            started_at: None,
            using: None,
            backfilled: false,
        });
        ledger.save(&ctx.paths.ledger_file())?;
        return Ok(Pushed::New { seq });
    }
}

/// Register `spec` (see the module docs). The lease is taken first, so an
/// entry is never visible without a held lease.
pub fn register(
    ctx: &Context,
    cfg: &crate::config::Config,
    spec: &CommandSpec,
) -> Result<Registration> {
    check_id("command_id", &spec.command_id)?;
    if spec.spec_digest.is_empty() {
        return Err(Error::Conflict("spec_digest must not be empty".into()));
    }
    let id = spec.command_id.as_str();
    let plan = exec::plan(ctx, cfg, &spec.opts)?;
    let Some(lease) = lock::try_take_lease(ctx, id)? else {
        return Err(Error::AlreadyOwned(format!(
            "another process holds the lease for {id}"
        )));
    };
    let pushed = push_entry(
        ctx,
        id,
        &spec.opts,
        &plan,
        Some(ManagedNew {
            spec_digest: &spec.spec_digest,
            attempt: spec.attempt.as_ref(),
            services: &spec.services,
        }),
        None,
    );
    let (seq, newly, phase, boot_id) = match pushed {
        Ok(Pushed::New { seq }) => (seq, true, Phase::Registered, ctx.boot.boot_id()),
        Ok(Pushed::Existing { seq, sidecar }) => (seq, false, sidecar.phase, sidecar.boot_id),
        Ok(Pushed::Terminal(sc)) => {
            drop(lease);
            let _ = std::fs::remove_file(ctx.paths.lease_file(id));
            return Ok(Registration::AlreadyTerminal(sc));
        }
        Ok(Pushed::Interrupted(_)) => unreachable!("register passes no stop hook"),
        Err(e) => {
            drop(lease);
            // Only if nobody registered it meanwhile.
            if !matches!(e, Error::Conflict(_)) {
                let _ = std::fs::remove_file(ctx.paths.lease_file(id));
            }
            return Err(e);
        }
    };
    Ok(Registration::Registered(
        RegisteredReceipt {
            command_id: id.to_string(),
            spec_digest: spec.spec_digest.clone(),
            phase,
            seq,
            newly_registered: newly,
            boot_id,
        },
        OwnerGuard::new(lease, id, true),
    ))
}

/// Why `try_admit` did not start the command.
#[derive(Debug, Clone, PartialEq)]
pub struct Waiting {
    pub reason: Reason,
    pub queue: Vec<QueueEntry>,
    pub estimate: Option<eta::Estimate>,
    pub holders: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TryAdmit {
    /// The entry is now `running` in the ledger.
    Admitted,
    Wait(Box<Waiting>),
    /// The ledger lock stayed busy for a round; try again.
    LockBusy,
}

/// One admission attempt for the registered `id`, as of the latest ledger
/// state under its lock. `waited_secs` is the queue age to judge with.
pub fn try_admit(ctx: &Context, limits: Limits, id: &str, waited_secs: f64) -> Result<TryAdmit> {
    // Process scans for entries kept without a lease holder happen here,
    // outside the lock (and at most every few seconds per entry).
    lock::refresh_verdicts(ctx);
    // Phase 1 (under the lock, no OS sampling): queue position and budget.
    // Only a budget-admissible head pays for a live snapshot.
    let (first, queue, est, holders, predicted_rss) = {
        let Some(_guard) = lock::try_ledger(ctx)? else {
            return Ok(TryAdmit::LockBusy);
        };
        let mut ledger = Ledger::load(&ctx.paths.ledger_file())?;
        if !lock::reap_dead(ctx, &mut ledger).is_empty() {
            ledger.save(&ctx.paths.ledger_file())?;
        }
        let queue = ledger.queue_at(ctx.clock.now());
        let est = eta::estimate(&exec::timed_queue(ctx, &ledger), limits, id);
        let holders = exec::holders(ctx, &ledger);
        let predicted_rss = ledger.find(id).map(|e| e.predicted_rss).unwrap_or(0);
        (
            admit::admit(&queue, id, predicted_rss, waited_secs, limits, None),
            queue,
            est,
            holders,
            predicted_rss,
        )
    };
    let waiting = |reason: Reason, queue: Vec<QueueEntry>| {
        Ok(TryAdmit::Wait(Box::new(Waiting {
            reason,
            queue,
            estimate: est,
            holders: holders.clone(),
        })))
    };
    let Decision::Admitted = first else {
        let Decision::Wait(reason) = first else {
            unreachable!()
        };
        return waiting(reason, queue);
    };
    // Phase 2: sample outside the lock, then re-check and flip to running
    // atomically.
    let snapshot = if queue.iter().any(|e| e.state == State::Running) {
        ctx.live.snapshot()
    } else {
        None
    };
    let Some(_guard) = lock::try_ledger(ctx)? else {
        return Ok(TryAdmit::LockBusy);
    };
    let mut ledger = Ledger::load(&ctx.paths.ledger_file())?;
    let reaped = !lock::reap_dead(ctx, &mut ledger).is_empty();
    let queue = ledger.queue_at(ctx.clock.now());
    let decision = admit::admit(&queue, id, predicted_rss, waited_secs, limits, snapshot);
    if decision == Decision::Admitted {
        // Started ahead of someone: the starvation cap in `admit` must be
        // able to tell it from the rest.
        let backfilled = admit::queue_order(&queue)
            .first()
            .is_some_and(|head| head.id != id);
        if let Some(e) = ledger.find_mut(id) {
            e.state = State::Running;
            e.started_at = Some(ctx.clock.now());
            e.backfilled = backfilled;
        }
    }
    if reaped || decision == Decision::Admitted {
        ledger.save(&ctx.paths.ledger_file())?;
    }
    match decision {
        Decision::Admitted => Ok(TryAdmit::Admitted),
        Decision::Wait(reason) => waiting(reason, queue),
    }
}

/// How an admitted command ran.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ran {
    pub outcome: Outcome,
    pub started_at: DateTime<Utc>,
    /// CPU was above the threshold when the run started.
    pub contended: bool,
}

/// Run an admitted command. For a managed command the launch intent is
/// persisted before the spawn and the child's identity right after it; the
/// child leads its own process group. The ledger entry and lease are *not*
/// released here: record the outcome with [`finish`] first.
pub fn run_command(ctx: &Context, guard: &OwnerGuard, opts: &Opts, limits: Limits) -> Result<Ran> {
    run_command_with_tracker(ctx, guard, opts, limits, None)
}

/// [`run_command`] with the recorder's start replaced (for tests that make
/// it fail).
#[doc(hidden)]
pub fn run_command_with_tracker(
    ctx: &Context,
    guard: &OwnerGuard,
    opts: &Opts,
    limits: Limits,
    start_tracker: Option<runner::StartTracker>,
) -> Result<Ran> {
    let id = guard.command_id();
    if guard.managed {
        // Only a command that never reached its launch may spawn. After a
        // LaunchIntent a child may already exist, and a spawn would run it
        // twice; `resolve` is how a human settles that.
        match load_sidecar(ctx, id) {
            Loaded::Ok(sc) if sc.phase == Phase::Registered => {}
            Loaded::Ok(sc) => {
                return Err(Error::Ledger(format!(
                    "{id} is in phase {:?}: a child may already exist, so it is not started \
                     again; settle it with `resolve` after checking",
                    sc.phase
                )))
            }
            _ => {
                return Err(Error::Ledger(format!(
                    "sidecar for {id} is missing or unreadable; settle it with `resolve`"
                )))
            }
        }
        let at = ctx.clock.now();
        // Stamped here and again at the spawn: the boot a child runs on is
        // what the reaper's reboot shortcut must compare with.
        let boot = ctx.boot.boot_id();
        advance_sidecar(ctx, id, Phase::LaunchIntent, |s| {
            s.launch_intent_at = Some(at);
            s.boot_id = boot;
        })?;
    }
    // One sample right before the start: a run that begins on a saturated
    // machine is recorded as contended so its parallelism is not learned.
    let contended = ctx
        .live
        .snapshot()
        .map(|l| l.cpu_busy > limits.cpu_threshold)
        .unwrap_or(false);
    let started_at = ctx.clock.now();
    let on_spawn = |pid: u32| {
        let identity = ChildIdentity::capture_leader(pid);
        // Best effort: if this fails the phase stays LaunchIntent, which
        // the reaper keeps (quarantine) rather than frees.
        let at = ctx.clock.now();
        let boot = ctx.boot.boot_id();
        let _ = advance_sidecar(ctx, id, Phase::ChildStarted, |s| {
            s.boot_id = boot;
            s.child = Some(identity);
            // The start of the heartbeat: with no `tracked_to_exit` the
            // window is open-ended, so this stamp is not an upper bound.
            s.recorded_at = Some(at);
        });
    };
    let record: crate::tracker::Record = {
        let (ctx, id) = (ctx.clone(), id.to_string());
        std::sync::Arc::new(move |t: &Tracked, stamp: bool| {
            // Whether it was persisted: the tracker retries every tick
            // until a write carrying its current state succeeds.
            match load_sidecar(&ctx, &id) {
                Loaded::Ok(mut sc) if sc.phase != Phase::Terminal => {
                    let kept = lineage::persisted(&sc.descendants, sc.descendants_truncated, t);
                    sc.descendants = kept.descendants;
                    sc.descendants_truncated = kept.truncated;
                    if stamp {
                        sc.recorded_at = Some(ctx.clock.now());
                    }
                    write_sidecar(&ctx, &sc).is_ok()
                }
                // Nothing left to record into.
                Loaded::Ok(_) => true,
                _ => false,
            }
        })
    };
    let outcome = runner::run_with(
        ctx,
        &opts.argv,
        &opts.cwd,
        id,
        true,
        runner::Spawn {
            own_process_group: guard.managed,
            marker_id: guard.managed.then_some(id),
            on_spawn: guard.managed.then_some(&on_spawn),
            on_descendants: guard.managed.then_some(record),
            start_tracker,
            env: opts.env.as_ref(),
            stdout_log: opts.stdout_log.as_deref(),
        },
    )?;
    if let Some(at) = outcome.tracked_to_exit_at {
        // The tracker covered the child's whole life and the last look was
        // at `at`: close the hidden-orphan window there. Best effort: if
        // this fails the window stays open, which only holds the entry.
        if let Loaded::Ok(mut sc) = load_sidecar(ctx, id) {
            if sc.phase != Phase::Terminal {
                sc.tracked_to_exit = true;
                sc.recorded_at = Some(at);
                let _ = write_sidecar(ctx, &sc);
            }
        }
    }
    if outcome.tracker_unknown {
        guard
            .tracker_unknown
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    Ok(Ran {
        outcome,
        started_at,
        contended,
    })
}

/// Record how the command ended, then release its ledger entry and lease.
/// The order matters: a crash in between leaves a terminal sidecar on a
/// still-registered entry, which the reaper releases; the reverse would
/// free the budget with nothing recorded.
pub fn finish(ctx: &Context, guard: OwnerGuard, kind: TerminalKind) -> Result<()> {
    let id = guard.id.clone();
    if guard.managed {
        if guard
            .tracker_unknown
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(Error::Ledger(format!(
                "{id}: the descendants of the command could not be recorded, so the entry \
                 keeps its budget (see `task-traffic-controller resolve {id}`)"
            )));
        }
        // Budget is only released once the whole process group, and anything
        // marked that left it, is known to be stopped. `run_command` stops
        // what it can; if members remain the entry is kept (the reaper then
        // holds it as DescendantsAlive until they are gone) and nothing is
        // recorded as terminal.
        if let Loaded::Ok(sc) = load_sidecar(ctx, &id) {
            if sc.phase != Phase::Terminal {
                if let Some(pgid) = sc.child.as_ref().and_then(|c| c.pgid) {
                    if process::probe_group(Some(pgid)) == Probe::Alive {
                        return Err(Error::Ledger(format!(
                            "{id}: process group {pgid} still has members; the entry keeps \
                             its budget until they stop"
                        )));
                    }
                }
                if let Some(c) = &sc.child {
                    if ctx.scan.probe_session(c) != Probe::Gone {
                        return Err(Error::Ledger(format!(
                            "{id}: the command's session still has members; the entry keeps \
                             its budget until they stop"
                        )));
                    }
                }
                // Before the launch intent nothing can have been started.
                if sc.phase != Phase::Registered && !runner::stop_marked(ctx, &id, &sc.tracked()) {
                    return Err(Error::Ledger(format!(
                        "{id}: a process carrying {} still runs or could not be checked; \
                         the entry keeps its budget (see `task-traffic-controller resolve {id}`)",
                        crate::marker::ENV_VAR
                    )));
                }
            }
        }
        let at = ctx.clock.now();
        advance_sidecar(ctx, &id, Phase::Terminal, |s| {
            s.terminal = Some(TerminalRecord {
                kind,
                at,
                detail: None,
            })
        })?;
    }
    exec::remove_entry(ctx, &id);
    drop(guard);
    Ok(())
}

/// Settle a managed entry by hand after investigating it (for example a
/// quarantined one whose sidecar is unreadable): record `kind` as its
/// terminal outcome, rewriting the sidecar if need be, and release the
/// entry. Refuses while the owner's lease is held. It trusts the caller;
/// [`resolve_checked`] is the version that checks first.
pub fn resolve(ctx: &Context, id: &str, kind: TerminalKind, detail: &str) -> Result<()> {
    check_id("command_id", id)?;
    let _guard = lock::ledger(ctx)?;
    if lock::lease_is_held(ctx, id) {
        return Err(Error::AlreadyOwned(format!("{id} still has a live owner")));
    }
    settle_by_hand(ctx, id, kind, detail)
}

/// The `spec_digest` of a sidecar that cannot be parsed as a whole, when the
/// file is still JSON that names one; empty otherwise.
fn salvaged_digest(ctx: &Context, id: &str) -> String {
    std::fs::read(ctx.paths.managed_file(id))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("spec_digest")?.as_str().map(String::from))
        .unwrap_or_default()
}

/// The body of [`resolve`], with the ledger lock held and the lease known
/// to be free.
fn settle_by_hand(ctx: &Context, id: &str, kind: TerminalKind, detail: &str) -> Result<()> {
    let at = ctx.clock.now();
    let terminal = TerminalRecord {
        kind,
        at,
        detail: Some(detail.to_string()),
    };
    match load_sidecar(ctx, id) {
        Loaded::Ok(mut sc) => {
            sc.phase = Phase::Terminal;
            sc.terminal = Some(terminal);
            write_sidecar(ctx, &sc)?;
        }
        _ => {
            // Nothing trustworthy left: write a minimal terminal record.
            write_sidecar(
                ctx,
                &Sidecar {
                    version: SIDECAR_VERSION,
                    command_id: id.to_string(),
                    spec_digest: salvaged_digest(ctx, id),
                    phase: Phase::Terminal,
                    boot_id: ctx.boot.boot_id(),
                    registered_at: at,
                    owner_pid: 0,
                    owner_start_time: None,
                    attempt: None,
                    services: Vec::new(),
                    launch_intent_at: None,
                    child: None,
                    terminal: Some(terminal),
                    descendants: Vec::new(),
                    descendants_truncated: false,
                    recorded_at: None,
                    tracked_to_exit: false,
                },
            )?;
        }
    }
    let mut ledger = Ledger::load(&ctx.paths.ledger_file())?;
    if ledger.remove(id) {
        ledger.save(&ctx.paths.ledger_file())?;
    }
    let _ = std::fs::remove_file(ctx.paths.lease_file(id));
    Ok(())
}

/// What [`resolve_checked`] found and did, for `task-traffic-controller resolve`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    pub command_id: String,
    /// The sidecar's stage; `None` when there is no readable sidecar.
    pub phase: Option<Phase>,
    pub lease_held: bool,
    /// False when the ledger has no entry with this id (nothing was
    /// checked or changed).
    pub found: bool,
    pub released: bool,
    /// Released by `--force` although a process could not be checked.
    #[serde(default)]
    pub forced: bool,
    /// The decision, as the reaper names it.
    pub verdict: String,
    /// What was checked and what it showed.
    pub checked: String,
}

fn describe(v: Verdict) -> &'static str {
    use reap::{Keep, Release};
    match v {
        Verdict::Keep(Keep::LeaseHeld) => "its owner is still running",
        Verdict::Keep(Keep::SidecarCorrupt) => "the sidecar is unreadable",
        Verdict::Keep(Keep::LaunchUnknown) => "the launch intent is too recent to settle",
        Verdict::Keep(Keep::DescendantsAlive) => {
            "the child, its process group or a process carrying the id is still running"
        }
        Verdict::Keep(Keep::ProbeUnknown) => {
            "a process could not be checked (a scan could not read every process of this user, \
             the descendants recorded were more than fit, or a hidden process may be the child)"
        }
        Verdict::Release(Release::Plain) => "a plain entry whose owner is gone",
        Verdict::Release(Release::Terminal) => "the outcome was already recorded",
        Verdict::Release(Release::BootChanged) => "the machine rebooted since registration",
        Verdict::Release(Release::NeverLaunched) => "nothing was ever started",
        Verdict::Release(Release::Stopped) => {
            "the child, its process group and every process carrying the id are gone"
        }
        Verdict::Release(Release::LaunchSettled) => {
            "no process carries the id and the owner is gone: nothing is running"
        }
        Verdict::Release(Release::NoMarkedProcess) => {
            "no process carries the id (the sidecar was unreadable, so nothing else could be checked)"
        }
    }
}

/// Settle the entry `id` by hand, but only if it is safe: the same decision
/// as the reaper's, except that the launch-intent grace period is skipped
/// and an unreadable sidecar is judged by the process scan alone. A held
/// lease, a running child, group or marked process, or a scan that could not
/// read every process all keep the entry. When released, the outcome is
/// recorded as `Interrupted` with the reason.
pub fn resolve_checked(ctx: &Context, id: &str) -> Result<Resolution> {
    resolve_checked_with(ctx, id, false)
}

/// [`resolve_checked`]; with `force`, an entry kept only because a process
/// *could not be checked* (an unreadable process, descendants that did not
/// fit the record, a hidden process that may be the child) is released
/// anyway, recorded as `Interrupted` with `forced: <reason>`. Nothing known
/// to be running is ever forced: a held lease, a running child, marked
/// process or recorded descendant keeps the entry. A live process group
/// alone does not, when none of its members is the child, carries the
/// run's id or is a recorded descendant, *and* a different process now leads
/// that group (see [`reap::group_reused`]). Members without the marker are not
/// proof of reuse: a process started with an empty environment, or one born
/// after the tracker stopped, may still belong to the original group, and the
/// group number cannot be reused while the group lives. With no leader, or a
/// leader whose start time cannot be compared, the group is taken to be the
/// original one and the entry stays.
pub fn resolve_checked_with(ctx: &Context, id: &str, force: bool) -> Result<Resolution> {
    check_id("command_id", id)?;
    let _guard = lock::ledger(ctx)?;
    let ledger = Ledger::load(&ctx.paths.ledger_file())?;
    // A sidecar that is not terminal but has no ledger entry is stranded
    // (an older binary dropped the entry, or the ledger was edited): it
    // blocks the id, and is settled here by the same checks.
    let stranded = match load_sidecar(ctx, id) {
        Loaded::Ok(sc) => sc.phase != Phase::Terminal,
        Loaded::Corrupt(_) => true,
        Loaded::Missing => false,
    };
    if ledger.find(id).is_none() && !stranded {
        return Ok(Resolution {
            command_id: id.to_string(),
            phase: None,
            lease_held: false,
            found: false,
            released: false,
            forced: false,
            verdict: "NoEntry".into(),
            checked: "there is no ledger entry with this id".into(),
        });
    }
    let (a, held_by) = assess_detailed(ctx, id, true, true, false);
    let phase = match &a.sidecar {
        Loaded::Ok(sc) => Some(sc.phase),
        _ => None,
    };
    let mut r = Resolution {
        command_id: id.to_string(),
        phase,
        lease_held: a.verdict == Verdict::Keep(reap::Keep::LeaseHeld),
        found: true,
        released: false,
        forced: false,
        verdict: format!("{:?}", a.verdict),
        checked: with_suspects(describe(a.verdict), &held_by),
    };
    if force && a.verdict == Verdict::Keep(reap::Keep::ProbeUnknown) {
        let detail = format!("forced: {}", r.checked);
        settle_by_hand(ctx, id, TerminalKind::Interrupted, &detail)?;
        r.released = true;
        r.forced = true;
        r.checked = detail;
        return Ok(r);
    }
    // A group number reused by an unrelated group: a different process leads
    // the recorded group, and nothing marked or recorded runs with the child
    // gone. `--force` frees it. A live group with no such leader may still be
    // this run's (members the marker does not show), so it stays.
    if force && a.verdict == Verdict::Keep(reap::Keep::DescendantsAlive) {
        let child = match &a.sidecar {
            Loaded::Ok(sc) => sc.child.as_ref(),
            _ => None,
        };
        let reused = child.is_some_and(|c| {
            let leader = c
                .pgid
                .and_then(|g| u32::try_from(g).ok())
                .and_then(process::start_time);
            reap::group_reused(leader, c.start_time)
        });
        if reused
            && assess_with(ctx, id, true, true, true).verdict
                == Verdict::Release(reap::Release::Stopped)
        {
            let detail = "forced: the recorded process group number now belongs to a different \
                 process group (its leader started after this run's child; none of its \
                 members carries the run's id or is a recorded descendant, and the child is gone)"
                .to_string();
            settle_by_hand(ctx, id, TerminalKind::Interrupted, &detail)?;
            r.released = true;
            r.forced = true;
            r.checked = detail;
            return Ok(r);
        }
    }
    let Verdict::Release(why) = a.verdict else {
        if force {
            r.checked = format!(
                "{} (--force releases only what could not be checked, not this)",
                r.checked
            );
        }
        return Ok(r);
    };
    let kind = match why {
        reap::Release::NeverLaunched => TerminalKind::Abandoned,
        _ => TerminalKind::Interrupted,
    };
    let detail = format!(
        "settled by `task-traffic-controller resolve`: {}",
        describe(a.verdict)
    );
    match (&a.sidecar, why) {
        // Nothing to record: a plain entry, or an outcome already written.
        (Loaded::Missing, _) | (_, reap::Release::Terminal) => {
            let mut ledger = ledger;
            if ledger.remove(id) {
                ledger.save(&ctx.paths.ledger_file())?;
            }
            let _ = std::fs::remove_file(ctx.paths.lease_file(id));
        }
        _ => settle_by_hand(ctx, id, kind, &detail)?,
    }
    r.released = true;
    Ok(r)
}

/// Trim the history to `keep` records per key, under the history lock so a
/// concurrent [`record_measurement`] cannot append between the read and the
/// rewrite (which would drop the record but leave its `.applied` marker).
/// Returns the records dropped.
pub fn trim_history(ctx: &Context, keep: usize) -> Result<usize> {
    let _lock = lock::acquire(&ctx.paths.history_lock(), ctx.ledger_lock_timeout)?;
    history::trim(ctx.paths.history_file(), keep)
}

/// Remove the `.applied` markers whose measurement no longer has a record in
/// the history. A marker is kept for as long as its record is: the history
/// keeps the newest records per key ([`history::trim`]), and what it still
/// holds is exactly what a replayed measurement could duplicate. Run after
/// [`trim_history`]. Returns how many were removed.
pub fn gc_measurements(ctx: &Context) -> Result<usize> {
    let _lock = lock::acquire(&ctx.paths.history_lock(), ctx.ledger_lock_timeout)?;
    let Ok(rd) = std::fs::read_dir(ctx.paths.measurements_dir()) else {
        return Ok(0);
    };
    let recorded: std::collections::HashSet<String> = history::read_all(ctx.paths.history_file())?
        .into_iter()
        .filter_map(|r| r.measurement_id)
        .collect();
    let mut removed = 0;
    for p in rd.filter_map(|e| e.ok()).map(|e| e.path()) {
        let stem = p.file_stem().and_then(|s| s.to_str());
        let applied = p.extension().is_some_and(|x| x == "applied");
        if applied
            && stem.is_some_and(|s| !recorded.contains(s))
            && std::fs::remove_file(&p).is_ok()
        {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Remove the sidecars of managed commands that ended more than `keep_days`
/// ago and have no ledger entry (the receipt of a finished run is only
/// needed so a repeated registration finds its outcome), and temporary files
/// of that age that an interrupted write left behind. A sidecar that is not
/// terminal, one that cannot be read, and one whose entry is still in the
/// ledger are never removed. Returns how many files were removed.
pub fn gc_sidecars(ctx: &Context, keep_days: u64) -> Result<usize> {
    // Under the ledger lock, so a registration cannot slip in between the
    // look at the ledger and the removal.
    let _guard = lock::ledger(ctx)?;
    let Ok(rd) = std::fs::read_dir(ctx.paths.managed_dir()) else {
        return Ok(0);
    };
    let ledger = Ledger::load(&ctx.paths.ledger_file())?;
    let cutoff = ctx.clock.now() - chrono::Duration::days(keep_days as i64);
    let mut removed = 0;
    for p in rd.filter_map(|e| e.ok()).map(|e| e.path()) {
        let old = match p.file_stem().and_then(|s| s.to_str()) {
            Some(id) if p.extension().is_some_and(|x| x == "json") => match load_sidecar(ctx, id) {
                Loaded::Ok(sc) => {
                    sc.phase == Phase::Terminal
                        && ledger.find(id).is_none()
                        && sc.terminal.as_ref().map_or(sc.registered_at, |t| t.at) < cutoff
                }
                _ => false,
            },
            // Not a sidecar: what a crashed atomic write leaves.
            _ => p
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| DateTime::<Utc>::from(t) < cutoff),
        };
        if old && std::fs::remove_file(&p).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measured {
    Applied,
    /// This measurement id was applied before.
    Duplicate,
}

/// Apply `rec` to the history once per `measurement_id`. Safe to call
/// again after a crash or a failure: a marker is written before the append
/// and another after, and a half-done attempt is recognised by finding a
/// history record that carries this measurement id. The id is written into
/// the record; a record without one (older history) or with another id is
/// never taken for this measurement, even when its content is identical.
pub fn record_measurement(ctx: &Context, measurement_id: &str, rec: &Record) -> Result<Measured> {
    check_id("measurement_id", measurement_id)?;
    let _lock = lock::acquire(&ctx.paths.history_lock(), ctx.ledger_lock_timeout)?;
    let dir = ctx.paths.measurements_dir();
    std::fs::create_dir_all(&dir)?;
    let applied = dir.join(format!("{measurement_id}.applied"));
    let pending = dir.join(format!("{measurement_id}.pending"));
    if applied.exists() {
        return Ok(Measured::Duplicate);
    }
    if pending.exists() {
        let present = history::read_all(ctx.paths.history_file())?
            .iter()
            .any(|r| r.measurement_id.as_deref() == Some(measurement_id));
        if present {
            std::fs::write(&applied, b"")?;
            let _ = std::fs::remove_file(&pending);
            return Ok(Measured::Duplicate);
        }
    }
    std::fs::write(&pending, b"")?;
    let rec = Record {
        measurement_id: Some(measurement_id.to_string()),
        ..rec.clone()
    };
    history::append(ctx.paths.history_file(), &rec)?;
    std::fs::write(&applied, b"")?;
    let _ = std::fs::remove_file(&pending);
    Ok(Measured::Applied)
}
