//! Rough completion estimates from the ledger and learned wall times.
//!
//! Pure: a simple event simulation of the FIFO-with-reservation queue
//! against the budget. Live guards (memory, CPU) are ignored, so this is a
//! lower bound and is labelled as rough everywhere it is shown.

use crate::admit::{queue_order, QueueEntry, State};

/// What the simulation knows about one ledger entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Timed {
    pub entry: QueueEntry,
    /// Seconds the entry has already been running (0 while waiting).
    pub elapsed_secs: f64,
    /// Learned wall time, or None when unknown.
    pub expected_secs: Option<f64>,
}

/// Fallback wall time for commands with no usable history.
pub const UNKNOWN_SECS: f64 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    /// Seconds from now until the entry is expected to start.
    pub start_in: f64,
    /// Seconds from now until the entry is expected to finish.
    pub finish_in: f64,
    /// True when any entry that influences the result had no history.
    pub rough: bool,
}

/// Expected wall time from the most recent usable measurements
/// (`wall_secs >= 1`), newest last: the max of the last five.
pub fn expected_wall(walls: &[f64]) -> Option<f64> {
    let usable: Vec<f64> = walls.iter().copied().filter(|w| *w >= 1.0).collect();
    let tail = &usable[usable.len().saturating_sub(5)..];
    tail.iter().copied().fold(None, |acc: Option<f64>, w| {
        Some(acc.map_or(w, |a| a.max(w)))
    })
}

/// Estimate for `target` (a waiting or running entry id).
pub fn estimate(items: &[Timed], budget: u32, target: &str) -> Option<Estimate> {
    let budget = budget.max(1);
    let mut rough = false;
    let dur = |t: &Timed| -> f64 { t.expected_secs.unwrap_or(UNKNOWN_SECS) };
    // Running: (finish time, weight).
    let mut running: Vec<(f64, u32, String)> = items
        .iter()
        .filter(|t| t.entry.state == State::Running)
        .map(|t| {
            if t.expected_secs.is_none() {
                rough = true;
            }
            (
                (dur(t) - t.elapsed_secs).max(0.0),
                t.entry.weight.min(budget).max(1),
                t.entry.id.clone(),
            )
        })
        .collect();
    if let Some((finish, _, _)) = running.iter().find(|(_, _, id)| id == target) {
        return Some(Estimate {
            start_in: 0.0,
            finish_in: *finish,
            rough,
        });
    }
    let entries: Vec<QueueEntry> = items.iter().map(|t| t.entry.clone()).collect();
    let order = queue_order(&entries);
    let by_id = |id: &str| items.iter().find(|t| t.entry.id == id);
    let mut now: f64 = 0.0;
    for q in order {
        let t = by_id(&q.id)?;
        if t.expected_secs.is_none() {
            rough = true;
        }
        let w = q.weight.min(budget).max(1);
        // Advance time until the head fits.
        loop {
            let used: u32 = running.iter().map(|(_, w, _)| *w).sum();
            if used + w <= budget {
                break;
            }
            let (next, _, _) = running
                .iter()
                .cloned()
                .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))?;
            now = now.max(next);
            let before = running.len();
            running.retain(|(f, _, _)| *f > now);
            if running.len() == before {
                // No entry finished: the queue cannot make progress.
                return None;
            }
        }
        let finish = now + dur(t);
        if q.id == target {
            return Some(Estimate {
                start_in: now,
                finish_in: finish,
                rough,
            });
        }
        running.push((finish, w, q.id.clone()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, state: State, weight: u32, seq: u64, elapsed: f64, exp: Option<f64>) -> Timed {
        Timed {
            entry: QueueEntry {
                id: id.into(),
                state,
                front: false,
                weight,
                seq,
            },
            elapsed_secs: elapsed,
            expected_secs: exp,
        }
    }

    #[test]
    fn expected_wall_filters_then_takes_max_of_last_five() {
        assert_eq!(expected_wall(&[]), None);
        assert_eq!(expected_wall(&[0.5, 0.2]), None);
        assert_eq!(
            expected_wall(&[90.0, 1.0, 2.0, 3.0, 4.0, 5.0, 0.1]),
            Some(5.0)
        );
        assert_eq!(expected_wall(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), Some(6.0));
    }

    #[test]
    fn running_target_reports_remaining() {
        let items = [t("r", State::Running, 2, 1, 10.0, Some(30.0))];
        let e = estimate(&items, 4, "r").unwrap();
        assert_eq!(e.start_in, 0.0);
        assert_eq!(e.finish_in, 20.0);
        assert!(!e.rough);
    }

    #[test]
    fn remaining_never_negative() {
        let items = [t("r", State::Running, 2, 1, 99.0, Some(30.0))];
        assert_eq!(estimate(&items, 4, "r").unwrap().finish_in, 0.0);
    }

    #[test]
    fn empty_budget_starts_now() {
        let items = [t("w", State::Waiting, 2, 1, 0.0, Some(30.0))];
        let e = estimate(&items, 4, "w").unwrap();
        assert_eq!((e.start_in, e.finish_in), (0.0, 30.0));
    }

    #[test]
    fn waits_for_running_to_free_budget() {
        let items = [
            t("r", State::Running, 4, 1, 5.0, Some(25.0)),
            t("w", State::Waiting, 2, 2, 0.0, Some(10.0)),
        ];
        let e = estimate(&items, 4, "w").unwrap();
        assert_eq!((e.start_in, e.finish_in), (20.0, 30.0));
    }

    #[test]
    fn head_reservation_delays_followers() {
        let items = [
            t("r", State::Running, 3, 1, 0.0, Some(10.0)),
            t("heavy", State::Waiting, 4, 2, 0.0, Some(5.0)),
            t("light", State::Waiting, 1, 3, 0.0, Some(1.0)),
        ];
        // light fits now, but heavy is the head and reserves until r ends;
        // then heavy fills the budget until it finishes.
        let e = estimate(&items, 4, "light").unwrap();
        assert_eq!(e.start_in, 15.0);
        assert_eq!(e.finish_in, 16.0);
        let h = estimate(&items, 4, "heavy").unwrap();
        assert_eq!((h.start_in, h.finish_in), (10.0, 15.0));
    }

    #[test]
    fn parallel_waiters_share_budget() {
        let items = [
            t("a", State::Waiting, 2, 1, 0.0, Some(10.0)),
            t("b", State::Waiting, 2, 2, 0.0, Some(10.0)),
            t("c", State::Waiting, 2, 3, 0.0, Some(10.0)),
        ];
        assert_eq!(estimate(&items, 4, "b").unwrap().start_in, 0.0);
        assert_eq!(estimate(&items, 4, "c").unwrap().start_in, 10.0);
    }

    #[test]
    fn later_running_entry_still_occupies_budget_after_earlier_finishes() {
        let items = [
            t("r1", State::Running, 2, 1, 0.0, Some(5.0)),
            t("r2", State::Running, 2, 2, 0.0, Some(20.0)),
            t("w", State::Waiting, 3, 3, 0.0, Some(1.0)),
        ];
        // At t=5 only 2 of 4 are free; w (3) must wait for r2 at t=20.
        let e = estimate(&items, 4, "w").unwrap();
        assert_eq!((e.start_in, e.finish_in), (20.0, 21.0));
    }

    #[test]
    fn unknown_history_uses_default_and_marks_rough() {
        let items = [
            t("r", State::Running, 4, 1, 0.0, None),
            t("w", State::Waiting, 1, 2, 0.0, Some(5.0)),
        ];
        let e = estimate(&items, 4, "w").unwrap();
        assert_eq!(e.start_in, UNKNOWN_SECS);
        assert!(e.rough);
        let items = [t("w", State::Waiting, 1, 1, 0.0, None)];
        let e = estimate(&items, 4, "w").unwrap();
        assert_eq!(e.finish_in, UNKNOWN_SECS);
        assert!(e.rough);
    }

    #[test]
    fn oversized_weight_is_clamped_to_budget() {
        let items = [
            t("r", State::Running, 1, 1, 0.0, Some(3.0)),
            t("w", State::Waiting, 99, 2, 0.0, Some(1.0)),
        ];
        assert_eq!(estimate(&items, 4, "w").unwrap().start_in, 3.0);
    }

    #[test]
    fn unknown_target_is_none() {
        let items = [t("w", State::Waiting, 1, 1, 0.0, Some(1.0))];
        assert_eq!(estimate(&items, 4, "zz"), None);
    }
}
