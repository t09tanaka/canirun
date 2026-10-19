//! Rough completion estimates from the ledger and learned wall times.
//!
//! Pure: a simple event simulation of the queue against the budget, which
//! asks [`admit`] (without a live snapshot) who may start at each step, so
//! head reservation and backfill are modelled as admission applies them.
//! Live guards (memory, CPU) and measured use are ignored, so this is a
//! lower bound and is labelled as rough everywhere it is shown.

use crate::admit::{admit, queue_order, Decision, Limits, QueueEntry, State};

/// What the simulation knows about one ledger entry. The learned wall
/// time is `entry.expected_secs`.
#[derive(Debug, Clone, PartialEq)]
pub struct Timed {
    pub entry: QueueEntry,
    /// Seconds the entry has already been running (0 while waiting).
    pub elapsed_secs: f64,
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
pub fn estimate(items: &[Timed], limits: Limits, target: &str) -> Option<Estimate> {
    let limits = Limits {
        budget: limits.budget.max(1),
        ..limits
    };
    let mut rough = false;
    let dur = |e: &QueueEntry| -> f64 { e.expected_secs.unwrap_or(UNKNOWN_SECS) };
    // Running: (finish time, entry charged at its declared weight).
    let mut running: Vec<(f64, QueueEntry)> = items
        .iter()
        .filter(|t| t.entry.state == State::Running)
        .map(|t| {
            if t.entry.expected_secs.is_none() {
                rough = true;
            }
            (
                (dur(&t.entry) - t.elapsed_secs).max(0.0),
                QueueEntry {
                    using: None,
                    ..t.entry.clone()
                },
            )
        })
        .collect();
    if let Some((finish, _)) = running.iter().find(|(_, e)| e.id == target) {
        return Some(Estimate {
            start_in: 0.0,
            finish_in: *finish,
            rough,
        });
    }
    let mut waiting: Vec<QueueEntry> = items
        .iter()
        .filter(|t| t.entry.state == State::Waiting)
        .map(|t| t.entry.clone())
        .collect();
    let mut now: f64 = 0.0;
    // Each pass starts whatever admission lets start at `now`, then
    // retires at least the earliest-finishing entry, so `len + 1` passes
    // always suffice; a loop that needs more cannot make progress and
    // yields None (as does a target that is not in the queue).
    for _ in 0..=items.len() {
        loop {
            let entries: Vec<QueueEntry> = running
                .iter()
                .map(|(_, e)| e.clone())
                .chain(waiting.iter().map(|w| QueueEntry {
                    waited_secs: w.waited_secs + now,
                    ..w.clone()
                }))
                .collect();
            let order = queue_order(&entries);
            let Some(pos) = order.iter().position(|q| {
                admit(&entries, &q.id, 0, q.waited_secs, limits, None) == Decision::Admitted
            }) else {
                break;
            };
            let q = order[pos];
            if q.expected_secs.is_none() {
                rough = true;
            }
            let finish = now + dur(q);
            if q.id == target {
                return Some(Estimate {
                    start_in: now,
                    finish_in: finish,
                    rough,
                });
            }
            let started = QueueEntry {
                state: State::Running,
                waited_secs: 0.0,
                backfilled: pos > 0,
                ..q.clone()
            };
            waiting.retain(|w| w.id != started.id);
            running.push((finish, started));
        }
        let (next, _) = running
            .iter()
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))?;
        now = now.max(*next);
        running.retain(|(f, _)| *f > now);
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
                using: None,
                waited_secs: 0.0,
                expected_secs: exp,
                backfilled: false,
            },
            elapsed_secs: elapsed,
        }
    }

    /// Budget `budget`, guards off (they are not simulated anyway).
    fn lim(budget: u32) -> Limits {
        Limits {
            budget,
            memory_reserve: 0,
            cpu_threshold: 1.0,
            cpu_guard_max_wait_secs: 0.0,
            front_overload_load: 0.0,
            front_overload_max_wait_secs: 30.0,
            backfill_max_job_secs: 300.0,
            backfill_max_wait_secs: 300.0,
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
        let e = estimate(&items, lim(4), "r").unwrap();
        assert_eq!(e.start_in, 0.0);
        assert_eq!(e.finish_in, 20.0);
        assert!(!e.rough);
    }

    #[test]
    fn remaining_never_negative() {
        let items = [t("r", State::Running, 2, 1, 99.0, Some(30.0))];
        assert_eq!(estimate(&items, lim(4), "r").unwrap().finish_in, 0.0);
    }

    #[test]
    fn empty_budget_starts_now() {
        let items = [t("w", State::Waiting, 2, 1, 0.0, Some(30.0))];
        let e = estimate(&items, lim(4), "w").unwrap();
        assert_eq!((e.start_in, e.finish_in), (0.0, 30.0));
    }

    #[test]
    fn waits_for_running_to_free_budget() {
        let items = [
            t("r", State::Running, 4, 1, 5.0, Some(25.0)),
            t("w", State::Waiting, 2, 2, 0.0, Some(10.0)),
        ];
        let e = estimate(&items, lim(4), "w").unwrap();
        assert_eq!((e.start_in, e.finish_in), (20.0, 30.0));
    }

    #[test]
    fn head_reservation_delays_followers_without_history() {
        let items = [
            t("r", State::Running, 3, 1, 0.0, Some(10.0)),
            t("heavy", State::Waiting, 4, 2, 0.0, Some(5.0)),
            t("light", State::Waiting, 1, 3, 0.0, None),
        ];
        // light fits now, but nothing is known about it, so heavy reserves
        // until r ends; then heavy fills the budget until it finishes.
        let e = estimate(&items, lim(4), "light").unwrap();
        assert_eq!(e.start_in, 15.0);
        assert_eq!(e.finish_in, 15.0 + UNKNOWN_SECS);
        let h = estimate(&items, lim(4), "heavy").unwrap();
        assert_eq!((h.start_in, h.finish_in), (10.0, 15.0));
    }

    #[test]
    fn a_short_follower_backfills_behind_a_head_that_does_not_fit() {
        let items = [
            t("r", State::Running, 3, 1, 0.0, Some(10.0)),
            t("heavy", State::Waiting, 4, 2, 0.0, Some(5.0)),
            t("light", State::Waiting, 1, 3, 0.0, Some(1.0)),
        ];
        let e = estimate(&items, lim(4), "light").unwrap();
        assert_eq!((e.start_in, e.finish_in), (0.0, 1.0));
        let h = estimate(&items, lim(4), "heavy").unwrap();
        assert_eq!((h.start_in, h.finish_in), (10.0, 15.0));
    }

    #[test]
    fn backfill_closes_once_the_head_held_up_by_it_has_waited() {
        // r (2 of 4) ends at 10. b1 backfills at 0 and runs until 50. At 10
        // the head (4) is held up only by b1; whether b2 may also pass it
        // depends on how long the head has waited by then.
        let items = |head_waited| {
            let mut head = t("head", State::Waiting, 4, 2, 0.0, Some(5.0));
            head.entry.waited_secs = head_waited;
            [
                t("r", State::Running, 2, 1, 0.0, Some(10.0)),
                head,
                t("b1", State::Waiting, 1, 3, 0.0, Some(50.0)),
                t("b2", State::Waiting, 2, 4, 0.0, Some(50.0)),
            ]
        };
        let fresh = items(0.0);
        assert_eq!(estimate(&fresh, lim(4), "b1").unwrap().start_in, 0.0);
        assert_eq!(estimate(&fresh, lim(4), "b2").unwrap().start_in, 10.0);
        assert_eq!(estimate(&fresh, lim(4), "head").unwrap().start_in, 60.0);
        let old = items(295.0);
        assert_eq!(estimate(&old, lim(4), "b1").unwrap().start_in, 0.0);
        assert_eq!(estimate(&old, lim(4), "head").unwrap().start_in, 50.0);
        assert_eq!(estimate(&old, lim(4), "b2").unwrap().start_in, 55.0);
    }

    #[test]
    fn parallel_waiters_share_budget() {
        let items = [
            t("a", State::Waiting, 2, 1, 0.0, Some(10.0)),
            t("b", State::Waiting, 2, 2, 0.0, Some(10.0)),
            t("c", State::Waiting, 2, 3, 0.0, Some(10.0)),
        ];
        assert_eq!(estimate(&items, lim(4), "b").unwrap().start_in, 0.0);
        assert_eq!(estimate(&items, lim(4), "c").unwrap().start_in, 10.0);
    }

    #[test]
    fn later_running_entry_still_occupies_budget_after_earlier_finishes() {
        let items = [
            t("r1", State::Running, 2, 1, 0.0, Some(5.0)),
            t("r2", State::Running, 2, 2, 0.0, Some(20.0)),
            t("w", State::Waiting, 3, 3, 0.0, Some(1.0)),
        ];
        // At t=5 only 2 of 4 are free; w (3) must wait for r2 at t=20.
        let e = estimate(&items, lim(4), "w").unwrap();
        assert_eq!((e.start_in, e.finish_in), (20.0, 21.0));
    }

    #[test]
    fn unknown_history_uses_default_and_marks_rough() {
        let items = [
            t("r", State::Running, 4, 1, 0.0, None),
            t("w", State::Waiting, 1, 2, 0.0, Some(5.0)),
        ];
        let e = estimate(&items, lim(4), "w").unwrap();
        assert_eq!(e.start_in, UNKNOWN_SECS);
        assert!(e.rough);
        let items = [t("w", State::Waiting, 1, 1, 0.0, None)];
        let e = estimate(&items, lim(4), "w").unwrap();
        assert_eq!(e.finish_in, UNKNOWN_SECS);
        assert!(e.rough);
    }

    #[test]
    fn oversized_weight_is_clamped_to_budget() {
        let items = [
            t("r", State::Running, 1, 1, 0.0, Some(3.0)),
            t("w", State::Waiting, 99, 2, 0.0, Some(1.0)),
        ];
        assert_eq!(estimate(&items, lim(4), "w").unwrap().start_in, 3.0);
    }

    #[test]
    fn unknown_target_is_none() {
        let items = [t("w", State::Waiting, 1, 1, 0.0, Some(1.0))];
        assert_eq!(estimate(&items, lim(4), "zz"), None);
    }
}
