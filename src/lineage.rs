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

use crate::marker::{HiddenProc, ProcRef, Scan};
use crate::reap::Probe;
use std::collections::{HashMap, HashSet};

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

/// Is there a hidden process that could be a child of a launch whose
/// outcome is unknown? One that started at or after the launch intent
/// (`since`, seconds since the epoch; an unknown start time counts) and
/// whose parent is `init` / `launchd` (pid 1: it was orphaned) or the
/// supervisor that died.
pub fn suspicious_hidden(hidden: &[HiddenProc], since: u64, supervisor: u32) -> bool {
    hidden.iter().any(|h| {
        h.proc.start_time.is_none_or(|t| t >= since)
            && h.parent.is_some_and(|p| p == 1 || p == supervisor)
    })
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
        let no_start = ProcRef {
            pid: 10,
            start_time: None,
        };
        assert!(descendants_of(&table, no_start, &[]).is_empty());
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
    fn a_hidden_orphan_or_child_of_the_supervisor_since_the_intent_is_suspicious() {
        assert!(!suspicious_hidden(&[], 100, 7));
        // Orphaned (parent 1), at the intent second and later.
        assert!(suspicious_hidden(&[hidden(1, Some(1), Some(100))], 100, 7));
        assert!(suspicious_hidden(&[hidden(1, Some(1), Some(900))], 100, 7));
        // The dead supervisor's child.
        assert!(suspicious_hidden(&[hidden(1, Some(7), Some(100))], 100, 7));
        // Before the intent, another parent, or no parent: not ours.
        assert!(!suspicious_hidden(&[hidden(1, Some(1), Some(99))], 100, 7));
        assert!(!suspicious_hidden(&[hidden(1, Some(8), Some(500))], 100, 7));
        assert!(!suspicious_hidden(&[hidden(1, None, Some(500))], 100, 7));
        // An unknown start time cannot be ruled out.
        assert!(suspicious_hidden(&[hidden(1, Some(1), None)], 100, 7));
        // One suspicious among many.
        assert!(suspicious_hidden(
            &[hidden(1, Some(8), Some(500)), hidden(2, Some(1), Some(500))],
            100,
            7
        ));
    }
}
