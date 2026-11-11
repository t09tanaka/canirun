//! Descendants of a managed command, found by walking parent links while
//! the command runs. Pure: the OS-specific reader gives the process table
//! ([`Row`]) and this decides.
//!
//! The environment marker ([`crate::marker`]) cannot see a process whose
//! environment the OS hides (on macOS every Apple binary such as `/bin/sh`,
//! and hardened-runtime apps) nor one that cleared it. Those still have a
//! parent chain *while their parent lives*, so the runner records every
//! descendant it sees (pid plus start time) in the sidecar as the command
//! runs. A descendant that is recorded and still running keeps the budget,
//! however it left the process group. One that forks and loses its parent
//! between two samples is missed.
//!
//! # Hidden orphans
//!
//! What both the marker and the record miss is a descendant that is hidden,
//! left the command's session, and was never recorded (it lost its parent
//! between two samples, or was born after the supervisor, and so the
//! tracker, died). All that is left of it is a hidden process whose parent
//! is `init` / `launchd`. So an entry whose child, group, session, marked
//! and recorded processes are all gone is still kept while such a process
//! exists ([`standing`]); releasing it would let the caller start a second
//! copy of a command that still runs, which is the worse failure.
//!
//! Unrelated processes look the same: every job `launchd` starts has parent
//! 1, and so does every orphan of another shell. Three rules tell them
//! apart; each is a proof, not a guess.
//!
//! 1. **Time.** A descendant starts after the command's child, so a hidden
//!    process that started before it is unrelated. The other end closes only
//!    when the tracker covered the child's whole life ([`suspicion_window`]).
//!    It must not close at the moment the last *known* member was seen
//!    alive: an unrecorded escapee that outlives every known member can fork
//!    an hour later, and that child is a descendant too. With a dead
//!    supervisor the window therefore stays open.
//! 2. **Owner.** The scan lists only this user's processes.
//! 3. **Session.** A managed child is started with `setsid`, so it leads a
//!    session of its own, and nothing can join an existing session: a
//!    process is in a session because it created it (`setsid`, the session
//!    id is then its own pid) or because the process that forked it was in
//!    it. Every member of a session therefore descends from its leader, and
//!    every descendant of the child is in the child's session or in one
//!    created by another descendant. So a hidden process is unrelated when
//!    its session is `init`'s (id 1: on macOS many `launchd` jobs, Spotlight
//!    workers among them), or when the session's leader is still running and
//!    started before the window opened (an orphan of a terminal or of a test
//!    runner that was there before the command). The leader cannot be
//!    confused with a later process: a pid is not handed out again while it
//!    is the id of a session that has members. This rule needs the child to
//!    be recorded as its own session leader ([`standing`]'s `own_session`);
//!    for a launch intent, where no child was recorded, it is not used.
//!
//! Rules 1 and 3 compare start times, so they assume the wall clock did not
//! step back between the two starts; a member that claims to be older than
//! its own session leader shows that it did, and is kept.
//!
//! What stays a possible descendant, because nothing readable tells it from
//! a real one: a hidden orphan that leads its own session (a `launchd` job
//! started that way, a real `setsid` escapee, or the child of *another*
//! managed command whose supervisor died: it leads its own session by
//! construction), one whose session leader is gone (what another managed
//! command left behind looks like this), and one whose leader started
//! inside the window. So commands whose supervisors die at the same time
//! can still hold each other while their hidden processes run. On macOS the coalition id
//! (inherited, and not changeable without privilege) would separate the
//! `launchd` jobs among these, but it has to be recorded at spawn, which
//! the sidecar does not do yet. Until then such an entry is held until the
//! process ends or a person settles it with `resolve --force`;
//! [`crate::managed::hidden_suspects`] names the processes.

use crate::marker::{HiddenProc, ProcRef, Scan};
use crate::reap::Probe;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// The most descendants recorded for one command.
pub const MAX_DESCENDANTS: usize = 4096;

/// One line of the process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub proc: ProcRef,
    pub parent: Option<u32>,
}

/// What is recorded about a command's descendants.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tracked {
    pub descendants: Vec<ProcRef>,
    /// More descendants were seen than [`MAX_DESCENDANTS`]: the rest were
    /// not recorded, so a clear scan cannot be believed.
    pub truncated: bool,
}

impl Tracked {
    /// What is known when recording itself failed (the thread panicked or
    /// could not start, or the command could not be read): nothing is
    /// recorded and nothing can be ruled out, so a clear scan is not
    /// believed.
    pub fn unknown() -> Tracked {
        Tracked {
            descendants: Vec::new(),
            truncated: true,
        }
    }
}

/// What goes into the sidecar when the tracker hands over `t`. A tracker
/// that failed (`t` unknown: truncated, nothing recorded) must not erase
/// what earlier writes recorded, and `truncated` never goes back to false.
pub fn persisted(existing: &[ProcRef], existing_truncated: bool, t: &Tracked) -> Tracked {
    let descendants = if t.truncated && t.descendants.is_empty() {
        existing.to_vec()
    } else {
        t.descendants.clone()
    };
    Tracked {
        descendants,
        truncated: t.truncated || existing_truncated,
    }
}

/// Could the command's own start time not be read although it has not
/// ended? Then nothing can be tracked below it. (`ended`: a zombie, which
/// runs nothing, so there is nothing to record.)
pub fn root_unreadable(start_time: Option<u64>, ended: bool) -> bool {
    start_time.is_none() && !ended
}

/// The rows that are running now: a recorded process is the same one when
/// pid and start time both match.
pub fn alive_in(table: &[Row], recorded: &[ProcRef]) -> Vec<ProcRef> {
    let by_pid: HashMap<u32, &Row> = table.iter().map(|r| (r.proc.pid, r)).collect();
    recorded
        .iter()
        .filter(|p| {
            p.start_time.is_some()
                && by_pid
                    .get(&p.pid)
                    .is_some_and(|r| r.proc.start_time == p.start_time)
        })
        .copied()
        .collect()
}

/// Every process below `root` in `table`, and every recorded process in
/// `known` that is still running together with what is below it (so the
/// tree is still followed once `root` itself is gone). `root` is not in the
/// result. A child that started before its parent is a pid reused by an
/// unrelated process, not a child.
pub fn descendants_of(table: &[Row], root: ProcRef, known: &[ProcRef]) -> Vec<ProcRef> {
    let mut children: HashMap<u32, Vec<&Row>> = HashMap::new();
    for r in table {
        if let Some(parent) = r.parent {
            children.entry(parent).or_default().push(r);
        }
    }
    let mut seeds = alive_in(table, &[root]);
    if root.start_time.is_none() {
        // A root whose start time could not be read (it ended unreaped, a
        // zombie) cannot be matched by start time. It is our own unreaped
        // child, so its pid is still ours: seed by pid so its live children
        // are walked and recorded before they are reparented.
        // This only arises for our own zombie child before `wait4`; a pid
        // reused in that window can at worst cause a false hold, or in
        // theory make an unrelated process look like ours: negligible.
        seeds.extend(
            table
                .iter()
                .filter(|r| r.proc.pid == root.pid)
                .map(|r| r.proc),
        );
    }
    seeds.extend(alive_in(table, known));
    let mut seen: HashSet<u32> = seeds.iter().map(|p| p.pid).collect();
    seen.insert(root.pid);
    let mut out: Vec<ProcRef> = seeds
        .iter()
        .filter(|p| p.pid != root.pid)
        .copied()
        .collect();
    // Level by level, at most as many levels as there are rows: a loop in
    // the table (which a real one never has) cannot run away.
    let mut frontier = seeds;
    for _ in 0..table.len() {
        let mut next = Vec::new();
        for parent in &frontier {
            for child in children.get(&parent.pid).into_iter().flatten() {
                let older = matches!(
                    (child.proc.start_time, parent.start_time),
                    (Some(c), Some(p)) if c < p
                );
                if older || !seen.insert(child.proc.pid) {
                    continue;
                }
                out.push(child.proc);
                next.push(child.proc);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    out
}

/// The recorded descendants after one look at the process table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pruned {
    pub kept: Vec<ProcRef>,
    /// Recorded ones that were not in the table this time.
    pub missed: Vec<ProcRef>,
}

/// Drop a recorded descendant only after two looks in a row did not find it:
/// one read of the table can miss a process (it is read while it changes), and
/// a dropped process is no longer followed. `missed` is what the previous
/// look did not find.
pub fn prune(table: &[Row], recorded: &[ProcRef], missed: &[ProcRef]) -> Pruned {
    let alive = alive_in(table, recorded);
    let mut kept = Vec::new();
    let mut now_missed = Vec::new();
    for p in recorded {
        if alive.contains(p) {
            kept.push(*p);
        } else if !missed.contains(p) {
            kept.push(*p);
            now_missed.push(*p);
        }
    }
    Pruned {
        kept,
        missed: now_missed,
    }
}

/// How one look at the table went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// The record grew or became truncated (the caller then persists it).
    pub changed: bool,
    /// The look could not be believed (see [`look_failed`]).
    pub failed: bool,
}

/// Does the table list the command itself? A root without a start time (an
/// unreaped zombie) is matched by pid alone.
pub fn contains_root(table: &[Row], root: ProcRef) -> bool {
    table.iter().any(|r| {
        r.proc.pid == root.pid
            && (root.start_time.is_none() || r.proc.start_time == root.start_time)
    })
}

/// Is the process `me` inside a managed run: in the run's process group, or
/// itself or a descendant (by parent links in `table`) of the run's child or
/// of a recorded descendant? A process that merely carries the run's
/// environment (a daemon started inside the run, a server it left behind)
/// is not, and must not skip the gate. A pid is only matched together with
/// its start time, and a recorded process without one matches nothing.
pub fn inside_run(
    table: &[Row],
    me: u32,
    my_pgid: Option<i32>,
    run_pgid: Option<i32>,
    child: Option<ProcRef>,
    descendants: &[ProcRef],
) -> bool {
    if my_pgid.is_some() && my_pgid == run_pgid {
        return true;
    }
    let mut at = me;
    for _ in 0..=table.len() {
        let Some(row) = table.iter().find(|r| r.proc.pid == at) else {
            return false;
        };
        let known = child.iter().chain(descendants);
        if known
            .into_iter()
            .any(|k| k.start_time.is_some() && *k == row.proc)
        {
            return true;
        }
        // A parent loop ends at the bound of the loop.
        match row.parent {
            Some(parent) => at = parent,
            None => return false,
        }
    }
    false
}

/// A look that cannot be believed: the read failed or came back empty, or it
/// does not list the command although the command is still there
/// (`root_gone` false). Nothing can be ruled out from it.
pub fn look_failed(table: &[Row], root: ProcRef, root_gone: bool) -> bool {
    table.is_empty() || (!root_gone && !contains_root(table, root))
}

/// One look at the table: prune, find what is new and record it. A look that
/// failed is a tracker failure: the record becomes truncated (a change, to be
/// persisted at once) and is otherwise left as it is, so no recorded entry is
/// dropped on the strength of a bad read.
pub fn step(
    table: &[Row],
    root: ProcRef,
    root_gone: bool,
    tracked: &mut Tracked,
    missed: &mut Vec<ProcRef>,
    cap: usize,
) -> Step {
    if look_failed(table, root, root_gone) {
        let changed = !tracked.truncated;
        tracked.truncated = true;
        return Step {
            changed,
            failed: true,
        };
    }
    let pruned = prune(table, &tracked.descendants, missed);
    tracked.descendants = pruned.kept;
    *missed = pruned.missed;
    let found = descendants_of(table, root, &tracked.descendants);
    let m = merge(&tracked.descendants, tracked.truncated, &found, cap);
    let changed = !m.added.is_empty() || m.truncated != tracked.truncated;
    tracked.descendants.extend(m.added);
    tracked.truncated = m.truncated;
    Step {
        changed,
        failed: false,
    }
}

/// How long the sidecar's `recorded_at` may go without a write while the
/// record does not change.
pub const HEARTBEAT: Duration = Duration::from_secs(1);

/// What one tick hands to the recorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// Persist the record now.
    pub write: bool,
    /// Stamp `recorded_at` with it: only a look that could be believed
    /// shows the tracker was working.
    pub stamp: bool,
}

/// What to write after `step`. Always when the record changed, and again
/// every tick while the last attempt to persist it failed (`pending`). A good
/// look that changed nothing still writes (a heartbeat: the tracker was
/// alive then), but at most once per [`HEARTBEAT`]. A failed look stamps
/// nothing, and writes only a change or a retry. `since_write` is the time
/// since the last write that succeeded, `None` before the first.
pub fn plan(step: Step, pending: bool, since_write: Option<Duration>) -> Plan {
    Plan {
        write: step.changed
            || pending
            || (!step.failed && since_write.is_none_or(|d| d >= HEARTBEAT)),
        stamp: !step.failed,
    }
}

/// Is a write still owed after the tick that made `plan` and whether its
/// write (`persisted`) succeeded? A write that failed is retried by the next
/// tick, which carries the then-current record and its `truncated` state.
pub fn still_pending(plan: Plan, persisted: bool) -> bool {
    plan.write && !persisted
}

/// Is the command gone? Known when we reaped it (`reaped`: its pid may be
/// somebody else's by now, so it is not asked about); else only asked
/// (`ask`) when the table does not list it (`listed`).
pub fn root_is_gone(reaped: bool, listed: bool, ask: impl FnOnce() -> bool) -> bool {
    reaped || (!listed && ask())
}

/// What recording `found` changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge {
    /// Not yet recorded, in the order found, and within the cap.
    pub added: Vec<ProcRef>,
    /// Sticky: once something was dropped it stays true.
    pub truncated: bool,
}

/// Add what `found` has that `recorded` lacks, up to `cap` in all. The new
/// ones that do not fit are dropped (not the old ones) and `truncated`
/// is set.
pub fn merge(recorded: &[ProcRef], truncated: bool, found: &[ProcRef], cap: usize) -> Merge {
    let mut added: Vec<ProcRef> = Vec::new();
    let mut truncated = truncated;
    for f in found {
        if recorded.contains(f) || added.contains(f) {
            continue;
        }
        if recorded.len() + added.len() >= cap {
            truncated = true;
        } else {
            added.push(*f);
        }
    }
    Merge { added, truncated }
}

/// Fold the recorded descendants that are running into a marker scan. Any
/// of them running makes it `Alive`; if none does but descendants were
/// dropped, a scan that found nothing cannot be believed, so `Unknown`.
pub fn with_recorded(mut scan: Scan, recorded_alive: Vec<ProcRef>, truncated: bool) -> Scan {
    for p in recorded_alive {
        if !scan.alive.contains(&p) {
            scan.alive.push(p);
        }
    }
    scan.probe = if !scan.alive.is_empty() {
        Probe::Alive
    } else if truncated && scan.probe == Probe::Gone {
        Probe::Unknown
    } else {
        scan.probe
    };
    scan
}

/// How long after the last tracker record a hidden process may still have
/// started and be counted as the command's.
pub const RECORD_SLACK_SECS: u64 = 2;

/// The time the hidden-orphan window closes, if it may close at all: only
/// when the tracker covered the command's whole life and nothing was left
/// to stop afterwards. `now` is read only then, so callers take the stamp
/// after the stop has confirmed everything gone.
pub fn closed_at<T>(covered: bool, descendants_left: bool, now: impl FnOnce() -> T) -> Option<T> {
    (covered && !descendants_left).then(now)
}

/// The window (`since`, `until`; seconds since the epoch, both inclusive)
/// in which a hidden process may be a descendant nobody recorded: from the
/// child's start (the launch intent when that is unknown, else 0) to the
/// last tracker record plus [`RECORD_SLACK_SECS`]. That end holds only when
/// the tracker provably covered the child's whole life (`tracked_to_exit`:
/// the runner saw it exit and took the last look). Otherwise (no record, or
/// the supervisor died mid-run and the child may have forked later) the end
/// is open. Outside the window, a hidden orphan is an unrelated process that
/// merely started meanwhile, such as one the OS starts for the user all the
/// time.
pub fn suspicion_window(
    child_start: Option<u64>,
    intent: Option<u64>,
    recorded: Option<u64>,
    tracked_to_exit: bool,
) -> (u64, Option<u64>) {
    (
        child_start.or(intent).unwrap_or(0),
        recorded
            .filter(|_| tracked_to_exit)
            .map(|r| r.saturating_add(RECORD_SLACK_SECS)),
    )
}

/// Is there a hidden process that could be a child of a launch whose
/// outcome is unknown? One that started within the window (`since`, `until`
/// as in [`suspicion_window`]; an unknown start time counts) and whose
/// parent is `init` / `launchd` (pid 1: it was orphaned) or the supervisor
/// that died. This is the test by time and parent alone; [`standing`] also
/// uses the session.
pub fn suspicious_hidden(
    hidden: &[HiddenProc],
    since: u64,
    until: Option<u64>,
    supervisor: u32,
) -> bool {
    hidden
        .iter()
        .any(|h| orphan_in_window(h, since, until, supervisor))
}

/// Is `h` inside the window and an orphan (or the dead supervisor's
/// child)? The test [`suspicious_hidden`] applies to each process.
pub fn orphan_in_window(h: &HiddenProc, since: u64, until: Option<u64>, supervisor: u32) -> bool {
    h.proc
        .start_time
        .is_none_or(|t| t >= since && until.is_none_or(|u| t <= u))
        && h.parent.is_some_and(|p| p == 1 || p == supervisor)
}

/// What the OS says about the session of a process. The default is
/// "nothing known", which never clears a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionEvidence {
    /// Its session id, read while pid and start time still matched.
    pub sid: Option<u32>,
    /// The start time of the session's leader: the live process whose pid
    /// is the session id and whose own session is that id. `None` when
    /// there is no such process or it could not be read.
    pub leader_start: Option<u64>,
}

/// Where a hidden process stands for a command whose child and everything
/// known below it are gone (see the module documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Standing {
    /// It started outside the window: unrelated.
    OutsideWindow,
    /// Its parent is neither `init` nor the dead supervisor: unrelated (a
    /// descendant with a living parent is found through that parent).
    HasParent,
    /// It is in `init`'s session, which no descendant can be in: unrelated.
    InitSession,
    /// Its session's leader runs and started before the window: unrelated.
    OlderSession,
    /// The command's child was not recorded as the leader of its own
    /// session, so sessions prove nothing: kept.
    NoOwnSession,
    /// Its start time or its session could not be read: kept.
    SessionUnknown,
    /// It leads its own session, as a `setsid` escapee does: kept.
    OwnSession,
    /// Its session's leader is gone, so the session's origin is unknown:
    /// kept.
    LeaderGone,
    /// Its session's leader started inside the window and may be a
    /// descendant itself: kept.
    LeaderInWindow,
    /// It claims to be older than its session's leader: the clock stepped
    /// back, start times prove nothing: kept.
    ClockDisorder,
}

impl Standing {
    /// May it be a descendant nobody recorded?
    pub fn suspicious(self) -> bool {
        !matches!(
            self,
            Standing::OutsideWindow
                | Standing::HasParent
                | Standing::InitSession
                | Standing::OlderSession
        )
    }
}

/// Where the hidden process `h` stands. `since`, `until` and `supervisor`
/// are as in [`suspicious_hidden`]. `own_session`: the command's child is
/// recorded as the leader of its own session. `session` is what the OS says
/// about `h`'s session now. Only proof clears a process; anything unknown
/// keeps it.
pub fn standing(
    h: &HiddenProc,
    since: u64,
    until: Option<u64>,
    supervisor: u32,
    own_session: bool,
    session: SessionEvidence,
) -> Standing {
    let in_window = h
        .proc
        .start_time
        .is_none_or(|t| t >= since && until.is_none_or(|u| t <= u));
    if !in_window {
        return Standing::OutsideWindow;
    }
    if !h.parent.is_some_and(|p| p == 1 || p == supervisor) {
        return Standing::HasParent;
    }
    if !own_session {
        return Standing::NoOwnSession;
    }
    let (Some(start), Some(sid)) = (h.proc.start_time, session.sid) else {
        return Standing::SessionUnknown;
    };
    if sid == 1 {
        return Standing::InitSession;
    }
    if sid == h.proc.pid {
        return Standing::OwnSession;
    }
    match session.leader_start {
        None => Standing::LeaderGone,
        Some(leader) if leader > start => Standing::ClockDisorder,
        Some(leader) if leader >= since => Standing::LeaderInWindow,
        Some(_) => Standing::OlderSession,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, start: u64) -> ProcRef {
        ProcRef {
            pid,
            start_time: Some(start),
        }
    }

    fn row(pid: u32, parent: u32, start: u64) -> Row {
        Row {
            proc: p(pid, start),
            parent: Some(parent),
        }
    }

    fn pids(v: &[ProcRef]) -> Vec<u32> {
        let mut x: Vec<u32> = v.iter().map(|p| p.pid).collect();
        x.sort();
        x
    }

    #[test]
    fn a_recorded_process_is_dropped_only_after_two_misses_in_a_row() {
        let a = p(10, 100);
        let table = [row(10, 1, 100)];
        let empty: [Row; 0] = [];
        // Seen: kept, no miss.
        let r = prune(&table, &[a], &[]);
        assert_eq!((r.kept.clone(), r.missed.clone()), (vec![a], vec![]));
        // First miss: kept, remembered.
        let r = prune(&empty, &[a], &[]);
        assert_eq!((r.kept.clone(), r.missed.clone()), (vec![a], vec![a]));
        // Second miss in a row: dropped.
        let r = prune(&empty, &[a], &[a]);
        assert!(r.kept.is_empty() && r.missed.is_empty());
        // Seen again after one miss: the miss is forgotten.
        let r = prune(&table, &[a], &[a]);
        assert_eq!((r.kept, r.missed), (vec![a], vec![]));
    }

    fn changed(
        table: &[Row],
        root: ProcRef,
        t: &mut Tracked,
        missed: &mut Vec<ProcRef>,
        cap: usize,
    ) -> bool {
        let s = step(table, root, false, t, missed, cap);
        assert!(!s.failed, "{table:?}");
        s.changed
    }

    #[test]
    fn step_survives_one_missed_look_and_follows_the_tree() {
        let root = p(5, 50);
        let child = p(6, 60);
        let full = [row(5, 1, 50), row(6, 5, 60)];
        // The root is listed, its child is not: one missed look.
        let none = [row(5, 1, 50)];
        let mut t = Tracked::default();
        let mut missed = Vec::new();
        assert!(changed(&full, root, &mut t, &mut missed, 10));
        assert_eq!(t.descendants, vec![child]);
        // One look that does not show it: still recorded, no change.
        assert!(!changed(&none, root, &mut t, &mut missed, 10));
        assert_eq!(t.descendants, vec![child]);
        assert_eq!(missed, vec![child]);
        // It is back: nothing new, the miss is forgotten.
        assert!(!changed(&full, root, &mut t, &mut missed, 10));
        assert!(missed.is_empty());
        // Two empty looks drop it.
        changed(&none, root, &mut t, &mut missed, 10);
        changed(&none, root, &mut t, &mut missed, 10);
        assert!(t.descendants.is_empty());
        // The cap marks the record truncated, which is a change.
        let many = [row(5, 1, 50), row(6, 5, 60), row(7, 5, 61)];
        let mut t = Tracked::default();
        assert!(changed(&many, root, &mut t, &mut Vec::new(), 1));
        assert!(t.truncated && t.descendants.len() == 1);
    }

    #[test]
    fn more_descendants_than_the_cap_keep_the_hold_even_after_they_are_gone() {
        let root = p(5, 50);
        let many = [row(5, 1, 50), row(6, 5, 60), row(7, 5, 61), row(8, 5, 62)];
        let mut t = Tracked::default();
        let mut missed = Vec::new();
        assert!(changed(&many, root, &mut t, &mut missed, 2));
        assert!(t.truncated && t.descendants.len() == 2, "{t:?}");
        // They all end: the record says nothing runs, but the dropped ones
        // are unknown, so a clear scan is not believed.
        let gone = [row(5, 1, 50)];
        for _ in 0..3 {
            changed(&gone, root, &mut t, &mut missed, 2);
        }
        assert!(t.descendants.is_empty() && t.truncated, "{t:?}");
        let held = with_recorded(
            Scan::new(Probe::Gone, vec![]),
            alive_in(&gone, &t.descendants),
            t.truncated,
        );
        assert_eq!(held.probe, Probe::Unknown);
        assert!(!step(&gone, root, false, &mut t, &mut missed, 2).changed);
        assert!(t.truncated, "sticky");
    }

    #[test]
    fn a_look_that_cannot_be_believed_truncates_and_drops_nothing() {
        let root = p(5, 50);
        let child = p(6, 60);
        let full = [row(5, 1, 50), row(6, 5, 60)];
        let mut t = Tracked::default();
        let mut missed = Vec::new();
        assert!(changed(&full, root, &mut t, &mut missed, 10));
        let before = t.descendants.clone();
        // Empty, even though the command is gone: a failed read.
        let none: [Row; 0] = [];
        for gone in [true, false] {
            let mut t2 = Tracked {
                descendants: before.clone(),
                truncated: false,
            };
            let mut m2 = vec![child];
            let s = step(&none, root, gone, &mut t2, &mut m2, 10);
            assert_eq!(
                s,
                Step {
                    changed: true,
                    failed: true
                }
            );
            // Not pruned, even though it was already missed once.
            assert_eq!(t2.descendants, vec![child]);
            assert_eq!(m2, vec![child]);
            assert!(t2.truncated);
        }
        // A table without the command although it still runs.
        let other = [row(9, 1, 90)];
        let s = step(&other, root, false, &mut t, &mut missed, 10);
        assert_eq!(
            s,
            Step {
                changed: true,
                failed: true
            }
        );
        assert_eq!(t.descendants, before);
        // Already truncated: failing again changes nothing.
        let s = step(&other, root, false, &mut t, &mut missed, 10);
        assert_eq!(
            s,
            Step {
                changed: false,
                failed: true
            }
        );
        // A command that has ended or is gone is not a failure...
        assert!(!look_failed(&other, root, true));
        // ...nor is one that is listed.
        assert!(!look_failed(&full, root, false));
        assert!(look_failed(&none, root, true));
        // A start-less root is matched by pid alone; a start time must match.
        let bare = ProcRef {
            pid: 5,
            start_time: None,
        };
        assert!(contains_root(&full, bare));
        assert!(!contains_root(&other, bare));
        assert!(!contains_root(&[row(5, 1, 51)], root));
    }

    #[test]
    fn a_good_look_is_a_heartbeat_at_most_once_a_second_and_a_bad_one_is_not() {
        let quiet = Step {
            changed: false,
            failed: false,
        };
        let bad = Step {
            changed: false,
            failed: true,
        };
        let grew = Step {
            changed: true,
            failed: false,
        };
        let failed_now = Step {
            changed: true,
            failed: true,
        };
        let ms = Duration::from_millis;
        let w = |s, pending, since| plan(s, pending, since).write;
        assert!(w(quiet, false, None), "the first look always writes");
        assert!(!w(quiet, false, Some(ms(0))));
        assert!(!w(quiet, false, Some(ms(999))));
        assert!(w(quiet, false, Some(HEARTBEAT)));
        assert!(w(quiet, false, Some(ms(5000))));
        // A change is written at once, whatever was written last.
        assert!(w(grew, false, Some(ms(0))));
        assert!(w(failed_now, false, Some(ms(0))));
        // A failure that changed nothing writes nothing...
        assert!(!w(bad, false, None));
        assert!(!w(bad, false, Some(ms(5000))));
        // ...unless an earlier write is owed: that is retried every tick.
        assert!(w(bad, true, Some(ms(0))));
        assert!(w(quiet, true, Some(ms(0))));
        // Only a good look stamps.
        assert!(plan(quiet, false, None).stamp);
        assert!(plan(grew, false, None).stamp);
        assert!(!plan(bad, true, None).stamp);
        assert!(!plan(failed_now, false, None).stamp);
    }

    #[test]
    fn a_write_that_failed_is_owed_until_one_succeeds() {
        let p = |write| Plan { write, stamp: true };
        assert!(still_pending(p(true), false));
        assert!(!still_pending(p(true), true));
        // Nothing was attempted: nothing is owed.
        assert!(!still_pending(p(false), false));
        assert!(!still_pending(p(false), true));
    }

    #[test]
    fn a_reaped_root_is_gone_without_asking() {
        let asked = std::cell::Cell::new(0);
        let gone = |reaped, listed, answer| {
            root_is_gone(reaped, listed, || {
                asked.set(asked.get() + 1);
                answer
            })
        };
        assert!(gone(true, false, false));
        assert!(gone(true, true, false));
        assert_eq!(asked.get(), 0, "a reaped pid may be someone else's");
        // Not reaped: listed means running, whatever the kernel would say.
        assert!(!gone(false, true, true));
        assert_eq!(asked.get(), 0);
        assert!(gone(false, false, true));
        assert!(!gone(false, false, false));
        assert_eq!(asked.get(), 2);
    }

    #[test]
    fn failed_tracking_is_unknown_and_an_unreadable_root_needs_a_live_one() {
        assert!(Tracked::unknown().truncated && Tracked::unknown().descendants.is_empty());
        assert!(root_unreadable(None, false));
        assert!(!root_unreadable(None, true));
        assert!(!root_unreadable(Some(1), false));
        assert!(!root_unreadable(Some(1), true));
    }

    #[test]
    fn alive_needs_the_pid_and_the_start_time() {
        let table = [row(10, 1, 100), row(11, 1, 100)];
        let rec = [p(10, 100), p(11, 99), p(12, 100)];
        assert_eq!(alive_in(&table, &rec), vec![p(10, 100)]);
        let unknown = ProcRef {
            pid: 10,
            start_time: None,
        };
        assert!(alive_in(&table, &[unknown]).is_empty());
        // A row with no start time never matches either.
        let no_start = [Row {
            proc: unknown,
            parent: Some(1),
        }];
        assert!(alive_in(&no_start, &[unknown]).is_empty());
    }

    #[test]
    fn the_whole_tree_below_the_root_is_found_not_the_root() {
        let table = [
            row(10, 1, 100),
            row(20, 10, 101),
            row(30, 20, 102),
            row(31, 20, 102),
            row(40, 1, 100),
            row(41, 40, 105),
        ];
        let d = descendants_of(&table, p(10, 100), &[]);
        assert_eq!(pids(&d), [20, 30, 31]);
    }

    #[test]
    fn a_child_older_than_its_parent_is_a_reused_pid() {
        // 21 claims parent 10 but started before 10 did.
        let table = [row(10, 1, 100), row(20, 10, 100), row(21, 10, 99)];
        assert_eq!(pids(&descendants_of(&table, p(10, 100), &[])), [20]);
        // Equal start times are a child (the clock has one-second grain).
        assert_eq!(pids(&descendants_of(&table, p(10, 100), &[])).len(), 1);
    }

    #[test]
    fn a_reused_root_pid_is_not_the_root() {
        // pid 10 now started later: a different process; its children are
        // not ours.
        let table = [row(10, 1, 500), row(20, 10, 501)];
        assert!(descendants_of(&table, p(10, 100), &[]).is_empty());
        // (A root with no start time is the one case seeded by pid alone:
        // see `a_root_without_a_start_time_is_seeded_by_pid`.)
    }

    #[test]
    fn a_recorded_process_keeps_the_tree_followed_after_the_root_is_gone() {
        // The root (10) is gone; its child 20 now has parent 1 and a new
        // child 30. 20 was recorded.
        let table = [row(20, 1, 101), row(30, 20, 150), row(50, 1, 100)];
        let d = descendants_of(&table, p(10, 100), &[p(20, 101)]);
        assert_eq!(pids(&d), [20, 30]);
        // A recorded process that has since died or been replaced is not a
        // seed.
        let d = descendants_of(&table, p(10, 100), &[p(20, 7)]);
        assert!(d.is_empty());
    }

    #[test]
    fn cycles_and_self_parents_end() {
        let table = [
            row(10, 1, 100),
            row(20, 30, 100),
            row(30, 20, 100),
            row(40, 40, 100),
            Row {
                proc: p(50, 100),
                parent: None,
            },
        ];
        assert!(descendants_of(&table, p(10, 100), &[]).is_empty());
        let table = [row(10, 20, 100), row(20, 10, 100)];
        assert_eq!(pids(&descendants_of(&table, p(10, 100), &[])), [20]);
    }

    #[test]
    fn a_child_with_no_start_time_is_followed() {
        let table = [
            row(10, 1, 100),
            Row {
                proc: ProcRef {
                    pid: 20,
                    start_time: None,
                },
                parent: Some(10),
            },
            row(30, 20, 5),
        ];
        let d = descendants_of(&table, p(10, 100), &[]);
        assert_eq!(pids(&d), [20, 30]);
    }

    #[test]
    fn merge_adds_only_what_is_new() {
        let rec = [p(1, 1), p(2, 2)];
        let m = merge(&rec, false, &[p(2, 2), p(3, 3), p(3, 3), p(2, 9)], 10);
        assert_eq!(m.added, vec![p(3, 3), p(2, 9)]);
        assert!(!m.truncated);
        let none = merge(&rec, false, &[p(1, 1)], 10);
        assert!(none.added.is_empty() && !none.truncated);
    }

    #[test]
    fn merge_drops_the_new_ones_at_the_cap_and_stays_truncated() {
        let rec = [p(1, 1), p(2, 2)];
        // Exactly at the cap is not truncation.
        let fit = merge(&rec, false, &[p(3, 3)], 3);
        assert_eq!(fit.added, vec![p(3, 3)]);
        assert!(!fit.truncated);
        let over = merge(&rec, false, &[p(3, 3), p(4, 4), p(5, 5)], 3);
        assert_eq!(over.added, vec![p(3, 3)]);
        assert!(over.truncated);
        // Full already: nothing added; an already-recorded one is no loss.
        let full = merge(&rec, false, &[p(1, 1), p(9, 9)], 2);
        assert!(full.added.is_empty());
        assert!(full.truncated);
        let same = merge(&rec, false, &[p(1, 1)], 2);
        assert!(!same.truncated);
        // Sticky.
        assert!(merge(&rec, true, &[], 10).truncated);
        assert_eq!(MAX_DESCENDANTS, 4096);
    }

    #[test]
    fn a_recorded_survivor_makes_a_clear_scan_alive() {
        let gone = Scan::new(Probe::Gone, vec![]);
        let s = with_recorded(gone.clone(), vec![p(5, 5), p(5, 5)], false);
        assert_eq!(s.probe, Probe::Alive);
        assert_eq!(s.alive, vec![p(5, 5)]);
        // Already marked: not listed twice; alive stays alive.
        let marked = Scan::new(Probe::Alive, vec![p(5, 5)]);
        let s = with_recorded(marked, vec![p(5, 5), p(6, 6)], true);
        assert_eq!(s.alive, vec![p(5, 5), p(6, 6)]);
        assert_eq!(s.probe, Probe::Alive);
        // Nothing survives: the scan stands, unless descendants were
        // dropped.
        assert_eq!(
            with_recorded(gone.clone(), vec![], false).probe,
            Probe::Gone
        );
        assert_eq!(with_recorded(gone, vec![], true).probe, Probe::Unknown);
        let unknown = Scan::new(Probe::Unknown, vec![]);
        assert_eq!(
            with_recorded(unknown.clone(), vec![], false).probe,
            Probe::Unknown
        );
        assert_eq!(with_recorded(unknown, vec![], true).probe, Probe::Unknown);
    }

    fn hidden(pid: u32, parent: Option<u32>, start: Option<u64>) -> HiddenProc {
        HiddenProc {
            proc: ProcRef {
                pid,
                start_time: start,
            },
            parent,
        }
    }

    #[test]
    fn a_hidden_orphan_or_child_of_the_supervisor_in_the_window_is_suspicious() {
        assert!(!suspicious_hidden(&[], 100, None, 7));
        // Orphaned (parent 1), at the window start and later.
        assert!(suspicious_hidden(
            &[hidden(1, Some(1), Some(100))],
            100,
            None,
            7
        ));
        assert!(suspicious_hidden(
            &[hidden(1, Some(1), Some(900))],
            100,
            None,
            7
        ));
        // The dead supervisor's child.
        assert!(suspicious_hidden(
            &[hidden(1, Some(7), Some(100))],
            100,
            None,
            7
        ));
        // Before the window, another parent, or no parent: not ours.
        assert!(!suspicious_hidden(
            &[hidden(1, Some(1), Some(99))],
            100,
            None,
            7
        ));
        assert!(!suspicious_hidden(
            &[hidden(1, Some(8), Some(500))],
            100,
            None,
            7
        ));
        assert!(!suspicious_hidden(
            &[hidden(1, None, Some(500))],
            100,
            None,
            7
        ));
        // An unknown start time cannot be ruled out, bounded or not.
        assert!(suspicious_hidden(&[hidden(1, Some(1), None)], 100, None, 7));
        assert!(suspicious_hidden(
            &[hidden(1, Some(1), None)],
            100,
            Some(200),
            7
        ));
        // One suspicious among many.
        assert!(suspicious_hidden(
            &[hidden(1, Some(8), Some(500)), hidden(2, Some(1), Some(500))],
            100,
            None,
            7
        ));
    }

    #[test]
    fn a_hidden_orphan_is_cleared_only_by_proof() {
        // The command: child started at 100, supervisor pid 7, window open.
        // The hidden process: pid 50, started at 150, orphaned.
        let ev =
            |sid: Option<u32>, leader_start: Option<u64>| SessionEvidence { sid, leader_start };
        let orphan = hidden(50, Some(1), Some(150));
        let at = |h: &HiddenProc, own: bool, e: SessionEvidence| standing(h, 100, None, 7, own, e);
        use Standing::*;
        let cases: [(&str, HiddenProc, bool, SessionEvidence, Standing); 22] = [
            ("init session", orphan, true, ev(Some(1), None), InitSession),
            (
                "init session, leader read",
                orphan,
                true,
                ev(Some(1), Some(1)),
                InitSession,
            ),
            (
                "older live leader",
                orphan,
                true,
                ev(Some(40), Some(99)),
                OlderSession,
            ),
            (
                "much older leader",
                orphan,
                true,
                ev(Some(40), Some(0)),
                OlderSession,
            ),
            // A leader in the same second as the child may be a descendant.
            (
                "leader at window start",
                orphan,
                true,
                ev(Some(40), Some(100)),
                LeaderInWindow,
            ),
            (
                "leader in window",
                orphan,
                true,
                ev(Some(40), Some(120)),
                LeaderInWindow,
            ),
            (
                "leader as old as it",
                orphan,
                true,
                ev(Some(40), Some(150)),
                LeaderInWindow,
            ),
            // The clock stepped back: a member older than its leader.
            (
                "older than its leader",
                orphan,
                true,
                ev(Some(40), Some(151)),
                ClockDisorder,
            ),
            // A real setsid escapee: it leads its own session.
            (
                "setsid escapee",
                orphan,
                true,
                ev(Some(50), Some(150)),
                OwnSession,
            ),
            (
                "escapee, old leader claim",
                orphan,
                true,
                ev(Some(50), Some(1)),
                OwnSession,
            ),
            // The leader died (or its pid is now somebody else, not a leader).
            ("leader gone", orphan, true, ev(Some(40), None), LeaderGone),
            (
                "no session read",
                orphan,
                true,
                ev(None, None),
                SessionUnknown,
            ),
            (
                "no session, leader claim",
                orphan,
                true,
                ev(None, Some(1)),
                SessionUnknown,
            ),
            (
                "no start time",
                hidden(50, Some(1), None),
                true,
                ev(Some(1), None),
                SessionUnknown,
            ),
            // An older sidecar, or a launch intent: sessions prove nothing.
            (
                "child not a leader",
                orphan,
                false,
                ev(Some(1), None),
                NoOwnSession,
            ),
            (
                "child not a leader 2",
                orphan,
                false,
                ev(Some(40), Some(1)),
                NoOwnSession,
            ),
            // The dead supervisor's child is judged like an orphan.
            (
                "supervisor child",
                hidden(50, Some(7), Some(150)),
                true,
                ev(Some(40), Some(99)),
                OlderSession,
            ),
            (
                "supervisor child, own",
                hidden(50, Some(7), Some(150)),
                true,
                ev(Some(50), None),
                OwnSession,
            ),
            // Not candidates at all, whatever the session says.
            (
                "before the window",
                hidden(50, Some(1), Some(99)),
                true,
                ev(Some(50), None),
                OutsideWindow,
            ),
            (
                "living parent",
                hidden(50, Some(8), Some(150)),
                true,
                ev(Some(50), None),
                HasParent,
            ),
            (
                "no parent",
                hidden(50, None, Some(150)),
                true,
                ev(Some(50), None),
                HasParent,
            ),
            (
                "at the window start",
                hidden(50, Some(1), Some(100)),
                true,
                ev(Some(40), Some(99)),
                OlderSession,
            ),
        ];
        for (name, h, own, e, want) in cases {
            assert_eq!(at(&h, own, e), want, "{name}");
        }
        // A bounded window: both ends inclusive, an unknown start inside.
        let bounded = |t| {
            standing(
                &hidden(50, Some(1), t),
                100,
                Some(200),
                7,
                true,
                ev(Some(50), None),
            )
        };
        assert_eq!(bounded(Some(200)), OwnSession);
        assert_eq!(bounded(Some(201)), OutsideWindow);
        assert_eq!(bounded(Some(100)), OwnSession);
        assert_eq!(bounded(None), SessionUnknown);
        // No window start known (0): no leader is older, only init clears.
        let open = |e| standing(&hidden(50, Some(1), Some(150)), 0, None, 7, true, e);
        assert_eq!(open(ev(Some(40), Some(0))), LeaderInWindow);
        assert_eq!(open(ev(Some(1), None)), InitSession);
        // Pid 1 as the supervisor does not turn init into a session leader
        // of ours, and a pid reused by the hidden process changes nothing:
        // only the evidence read for it counts.
        assert_eq!(SessionEvidence::default(), ev(None, None));
    }

    #[test]
    fn only_the_four_proofs_clear_a_process() {
        use Standing::*;
        for s in [OutsideWindow, HasParent, InitSession, OlderSession] {
            assert!(!s.suspicious(), "{s:?}");
        }
        for s in [
            NoOwnSession,
            SessionUnknown,
            OwnSession,
            LeaderGone,
            LeaderInWindow,
            ClockDisorder,
        ] {
            assert!(s.suspicious(), "{s:?}");
        }
    }

    #[test]
    fn the_orphan_test_agrees_with_the_standing() {
        let own = SessionEvidence {
            sid: Some(50),
            leader_start: Some(150),
        };
        for (h, until) in [
            (hidden(50, Some(1), Some(150)), None),
            (hidden(50, Some(7), Some(150)), None),
            (hidden(50, Some(1), None), Some(200)),
            (hidden(50, Some(1), Some(99)), None),
            (hidden(50, Some(1), Some(201)), Some(200)),
            (hidden(50, Some(8), Some(150)), None),
            (hidden(50, None, Some(150)), None),
        ] {
            let s = standing(&h, 100, until, 7, true, own);
            let candidate = !matches!(s, Standing::OutsideWindow | Standing::HasParent);
            assert_eq!(orphan_in_window(&h, 100, until, 7), candidate, "{h:?}");
        }
    }

    #[test]
    fn the_window_ends_at_the_last_record_plus_the_slack() {
        let at = |t| [hidden(1, Some(1), Some(t))];
        // Both ends are inclusive.
        assert!(suspicious_hidden(&at(100), 100, Some(200), 7));
        assert!(suspicious_hidden(&at(200), 100, Some(200), 7));
        assert!(!suspicious_hidden(&at(99), 100, Some(200), 7));
        assert!(!suspicious_hidden(&at(201), 100, Some(200), 7));
    }

    #[test]
    fn the_window_closes_only_when_covered_and_nothing_was_left() {
        assert_eq!(closed_at(true, false, || 7), Some(7));
        assert_eq!(closed_at(false, false, || 7), None);
        assert_eq!(closed_at(true, true, || 7), None);
        assert_eq!(closed_at(false, true, || 7), None);
        // The stamp is read only when the window closes.
        let reads = std::cell::Cell::new(0);
        let read = || reads.set(reads.get() + 1);
        closed_at(true, true, read);
        closed_at(false, false, read);
        assert_eq!(reads.get(), 0);
        closed_at(true, false, read);
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn the_window_starts_at_the_child_or_else_the_intent_and_ends_only_when_tracked_to_exit() {
        let w = suspicion_window;
        assert_eq!(w(Some(50), Some(40), Some(100), true), (50, Some(102)));
        assert_eq!(w(None, Some(40), Some(100), true), (40, Some(102)));
        assert_eq!(w(None, None, Some(100), true), (0, Some(102)));
        // No record: open.
        assert_eq!(w(Some(50), None, None, true), (50, None));
        // Not tracked to the exit (the supervisor died mid-run): open, from
        // the lower bound on, whatever was recorded.
        assert_eq!(w(Some(50), Some(40), Some(100), false), (50, None));
        assert_eq!(w(None, Some(40), Some(100), false), (40, None));
        assert_eq!(w(None, None, None, false), (0, None));
        assert_eq!(w(None, None, Some(u64::MAX), true), (0, Some(u64::MAX)));
        assert_eq!(RECORD_SLACK_SECS, 2);
    }

    #[test]
    fn a_failed_tracker_keeps_what_was_recorded_and_stays_truncated() {
        let a = p(10, 100);
        // Unknown: nothing recorded, nothing erased.
        let r = persisted(&[a], false, &Tracked::unknown());
        assert_eq!(
            r,
            Tracked {
                descendants: vec![a],
                truncated: true
            }
        );
        // An ordinary record replaces the set.
        let b = p(11, 101);
        let t = Tracked {
            descendants: vec![b],
            truncated: false,
        };
        assert_eq!(persisted(&[a], false, &t), t);
        // Truncated never goes back.
        let r = persisted(&[a], true, &t);
        assert_eq!(
            r,
            Tracked {
                descendants: vec![b],
                truncated: true
            }
        );
        // Truncated with a full set is the set, not the old one.
        let t = Tracked {
            descendants: vec![b],
            truncated: true,
        };
        assert_eq!(persisted(&[a], false, &t), t);
    }

    #[test]
    fn a_root_without_a_start_time_is_seeded_by_pid() {
        // The root ended unreaped (a zombie: no start time) and still has a
        // live child, which is reparented only once the root is reaped.
        let root = ProcRef {
            pid: 10,
            start_time: None,
        };
        let table = [
            Row {
                proc: root,
                parent: Some(1),
            },
            row(11, 10, 200),
            row(12, 11, 201),
            row(13, 1, 202),
        ];
        assert_eq!(pids(&descendants_of(&table, root, &[])), vec![11, 12]);
        // Not in the table (already reaped): nothing to seed.
        assert!(descendants_of(&table[1..], root, &[]).is_empty());
        // With a start time the pid is not trusted alone: a different
        // process on the same pid is not the root.
        let reused = ProcRef {
            pid: 10,
            start_time: Some(5),
        };
        let table = [row(10, 1, 300), row(11, 10, 301)];
        assert!(descendants_of(&table, reused, &[]).is_empty());
    }

    #[test]
    fn a_process_in_the_runs_group_or_below_its_child_is_inside() {
        let table = [
            row(10, 1, 100),
            row(11, 10, 110),
            row(12, 11, 120),
            row(20, 1, 200),
        ];
        let child = Some(p(10, 100));
        // The group alone.
        assert!(inside_run(&table, 99, Some(7), Some(7), None, &[]));
        // Itself, a child, and a grandchild of the run's child.
        assert!(inside_run(&table, 10, Some(1), Some(7), child, &[]));
        assert!(inside_run(&table, 12, Some(1), Some(7), child, &[]));
        // Below a recorded descendant.
        assert!(inside_run(&table, 12, None, None, None, &[p(11, 110)]));
    }

    #[test]
    fn a_process_that_only_carries_the_environment_is_outside() {
        let table = [row(10, 1, 100), row(20, 1, 200), row(21, 20, 210)];
        let child = Some(p(10, 100));
        assert!(!inside_run(&table, 21, Some(5), Some(7), child, &[]));
        // No group on either side is no match.
        assert!(!inside_run(&table, 21, None, None, child, &[]));
        // Not in the table at all.
        assert!(!inside_run(&table, 99, Some(5), Some(7), child, &[]));
    }

    #[test]
    fn a_reused_pid_or_an_unknown_start_time_is_not_the_run() {
        let table = [row(10, 1, 999), row(21, 10, 210)];
        // Same pid, another start time: another process.
        assert!(!inside_run(&table, 21, None, None, Some(p(10, 100)), &[]));
        // A recorded process whose start time was never learned matches nothing.
        let blind = ProcRef {
            pid: 10,
            start_time: None,
        };
        assert!(!inside_run(&table, 21, None, None, Some(blind), &[blind]));
        // Not even when the table cannot tell either.
        let unreadable = Row {
            proc: blind,
            parent: Some(1),
        };
        assert!(!inside_run(&[unreadable], 10, None, None, Some(blind), &[]));
    }

    #[test]
    fn a_parent_loop_ends_the_walk() {
        let table = [row(10, 10, 100), row(11, 12, 110), row(12, 11, 120)];
        assert!(!inside_run(&table, 10, None, None, Some(p(5, 5)), &[]));
        assert!(!inside_run(&table, 11, None, None, Some(p(5, 5)), &[]));
        let orphan = Row {
            proc: p(30, 300),
            parent: None,
        };
        assert!(!inside_run(&[orphan], 30, None, None, Some(p(5, 5)), &[]));
    }
}
