//! Pure admission decision (spec "Admission" and "Live guards").
//!
//! Nothing in this module touches the file system, the clock, or the OS.
//! The caller loads the ledger, reaps dead entries, takes a live snapshot
//! and hands everything in; this module only decides.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Waiting,
    Running,
}

/// The slice of a ledger entry that admission needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub id: String,
    pub state: State,
    pub front: bool,
    pub weight: u32,
    /// Enqueue order; lower is earlier.
    pub seq: u64,
}

/// What the machine looks like right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveSnapshot {
    pub available_memory: u64,
    /// Global CPU busy fraction, 0.0..=1.0, sampled just now.
    pub cpu_busy: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub budget: u32,
    pub memory_reserve: u64,
    /// Hold the next start while `cpu_busy` is above this (0.0..=1.0).
    pub cpu_threshold: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reason {
    /// Someone is ahead in the queue; `ahead` is how many.
    NotHead {
        ahead: usize,
    },
    Budget {
        used: u32,
        budget: u32,
    },
    Memory {
        available: u64,
        need: u64,
    },
    Cpu {
        busy: f64,
        threshold: f64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Admitted,
    Wait(Reason),
}

/// Waiting entries in admission order: `--front` entries first (by seq),
/// then the rest (by seq).
pub fn queue_order(entries: &[QueueEntry]) -> Vec<&QueueEntry> {
    let mut waiting: Vec<&QueueEntry> = entries
        .iter()
        .filter(|e| e.state == State::Waiting)
        .collect();
    waiting.sort_by_key(|e| (!e.front, e.seq));
    waiting
}

/// Sum of weights of running entries.
pub fn used_budget(entries: &[QueueEntry]) -> u32 {
    entries
        .iter()
        .filter(|e| e.state == State::Running)
        .map(|e| e.weight)
        .sum()
}

/// Decide whether the waiting entry `me` may start now.
///
/// `predicted_rss` is the memory the candidate is expected to need (0 when
/// unknown). `live` may be `None` when the caller could not read the OS; the
/// guards are then skipped. The starvation rule (skip guards when nothing
/// managed is running) is applied here so it is covered by unit tests.
pub fn admit(
    entries: &[QueueEntry],
    me: &str,
    predicted_rss: u64,
    limits: Limits,
    live: Option<LiveSnapshot>,
) -> Decision {
    let order = queue_order(entries);
    let Some(pos) = order.iter().position(|e| e.id == me) else {
        // Not in the queue at all: treat as "someone is ahead" so the
        // caller re-registers rather than starting un-gated.
        return Decision::Wait(Reason::NotHead { ahead: usize::MAX });
    };
    if pos != 0 {
        return Decision::Wait(Reason::NotHead { ahead: pos });
    }
    let candidate = order[0];
    let used = used_budget(entries);
    let weight = candidate.weight.min(limits.budget).max(1);
    if used + weight > limits.budget {
        return Decision::Wait(Reason::Budget {
            used,
            budget: limits.budget,
        });
    }
    let anything_running = entries.iter().any(|e| e.state == State::Running);
    if !anything_running {
        return Decision::Admitted;
    }
    let Some(live) = live else {
        return Decision::Admitted;
    };
    let need = predicted_rss.saturating_add(limits.memory_reserve);
    if live.available_memory < need {
        return Decision::Wait(Reason::Memory {
            available: live.available_memory,
            need,
        });
    }
    if live.cpu_busy > limits.cpu_threshold {
        return Decision::Wait(Reason::Cpu {
            busy: live.cpu_busy,
            threshold: limits.cpu_threshold,
        });
    }
    Decision::Admitted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: &str, state: State, front: bool, weight: u32, seq: u64) -> QueueEntry {
        QueueEntry {
            id: id.into(),
            state,
            front,
            weight,
            seq,
        }
    }
    const LIMITS: Limits = Limits {
        budget: 12,
        memory_reserve: 2 << 30,
        cpu_threshold: 0.85,
    };
    fn calm() -> LiveSnapshot {
        LiveSnapshot {
            available_memory: 64 << 30,
            cpu_busy: 0.1,
        }
    }

    #[test]
    fn empty_queue_admits_head() {
        let q = [e("a", State::Waiting, false, 4, 1)];
        assert_eq!(admit(&q, "a", 0, LIMITS, Some(calm())), Decision::Admitted);
    }

    #[test]
    fn unknown_id_waits() {
        let q = [e("a", State::Waiting, false, 4, 1)];
        assert_eq!(
            admit(&q, "zz", 0, LIMITS, None),
            Decision::Wait(Reason::NotHead { ahead: usize::MAX })
        );
    }

    #[test]
    fn fifo_by_seq() {
        let q = [
            e("b", State::Waiting, false, 1, 2),
            e("a", State::Waiting, false, 1, 1),
        ];
        assert_eq!(admit(&q, "a", 0, LIMITS, None), Decision::Admitted);
        assert_eq!(
            admit(&q, "b", 0, LIMITS, None),
            Decision::Wait(Reason::NotHead { ahead: 1 })
        );
    }

    #[test]
    fn front_jumps_but_is_fifo_among_fronts() {
        let q = [
            e("a", State::Waiting, false, 1, 1),
            e("f2", State::Waiting, true, 1, 3),
            e("f1", State::Waiting, true, 1, 2),
        ];
        let ids: Vec<&str> = queue_order(&q).iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["f1", "f2", "a"]);
        assert_eq!(admit(&q, "f1", 0, LIMITS, None), Decision::Admitted);
        assert_eq!(
            admit(&q, "a", 0, LIMITS, None),
            Decision::Wait(Reason::NotHead { ahead: 2 })
        );
    }

    #[test]
    fn head_reserves_until_budget_fits() {
        let q = [
            e("r", State::Running, false, 10, 1),
            e("heavy", State::Waiting, false, 4, 2),
            e("light", State::Waiting, false, 1, 3),
        ];
        assert_eq!(
            admit(&q, "heavy", 0, LIMITS, Some(calm())),
            Decision::Wait(Reason::Budget {
                used: 10,
                budget: 12
            })
        );
        // light would fit, but it is not the head.
        assert_eq!(
            admit(&q, "light", 0, LIMITS, Some(calm())),
            Decision::Wait(Reason::NotHead { ahead: 1 })
        );
    }

    #[test]
    fn exact_fit_is_admitted() {
        let q = [
            e("r", State::Running, false, 8, 1),
            e("w", State::Waiting, false, 4, 2),
        ];
        assert_eq!(admit(&q, "w", 0, LIMITS, Some(calm())), Decision::Admitted);
    }

    #[test]
    fn oversized_weight_is_clamped_to_budget() {
        let q = [e("w", State::Waiting, false, 99, 1)];
        assert_eq!(admit(&q, "w", 0, LIMITS, Some(calm())), Decision::Admitted);
        let q = [
            e("r", State::Running, false, 1, 1),
            e("w", State::Waiting, false, 99, 2),
        ];
        assert_eq!(
            admit(&q, "w", 0, LIMITS, Some(calm())),
            Decision::Wait(Reason::Budget {
                used: 1,
                budget: 12
            })
        );
    }

    #[test]
    fn zero_weight_counts_as_one() {
        let q = [
            e("r", State::Running, false, 12, 1),
            e("w", State::Waiting, false, 0, 2),
        ];
        assert_eq!(
            admit(&q, "w", 0, LIMITS, None),
            Decision::Wait(Reason::Budget {
                used: 12,
                budget: 12
            })
        );
    }

    #[test]
    fn memory_guard_holds_head() {
        let q = [
            e("r", State::Running, false, 2, 1),
            e("w", State::Waiting, false, 2, 2),
        ];
        let live = LiveSnapshot {
            available_memory: 1 << 30,
            ..calm()
        };
        assert_eq!(
            admit(&q, "w", 3 << 30, LIMITS, Some(live)),
            Decision::Wait(Reason::Memory {
                available: 1 << 30,
                need: 5 << 30
            })
        );
    }

    #[test]
    fn memory_guard_uses_reserve_even_when_rss_unknown() {
        let q = [
            e("r", State::Running, false, 2, 1),
            e("w", State::Waiting, false, 2, 2),
        ];
        let live = LiveSnapshot {
            available_memory: (2 << 30) - 1,
            ..calm()
        };
        assert!(matches!(
            admit(&q, "w", 0, LIMITS, Some(live)),
            Decision::Wait(Reason::Memory { .. })
        ));
        let live = LiveSnapshot {
            available_memory: 2 << 30,
            ..calm()
        };
        assert_eq!(admit(&q, "w", 0, LIMITS, Some(live)), Decision::Admitted);
    }

    #[test]
    fn cpu_guard_holds_head() {
        let q = [
            e("r", State::Running, false, 2, 1),
            e("w", State::Waiting, false, 2, 2),
        ];
        let live = LiveSnapshot {
            cpu_busy: 0.9,
            ..calm()
        };
        assert_eq!(
            admit(&q, "w", 0, LIMITS, Some(live)),
            Decision::Wait(Reason::Cpu {
                busy: 0.9,
                threshold: 0.85
            })
        );
        let live = LiveSnapshot {
            cpu_busy: 0.85,
            ..calm()
        };
        assert_eq!(admit(&q, "w", 0, LIMITS, Some(live)), Decision::Admitted);
    }

    #[test]
    fn starvation_rule_skips_guards_when_nothing_runs() {
        let q = [e("w", State::Waiting, false, 2, 1)];
        let live = LiveSnapshot {
            available_memory: 0,
            cpu_busy: 1.0,
        };
        assert_eq!(
            admit(&q, "w", 8 << 30, LIMITS, Some(live)),
            Decision::Admitted
        );
    }

    #[test]
    fn missing_snapshot_skips_guards() {
        let q = [
            e("r", State::Running, false, 2, 1),
            e("w", State::Waiting, false, 2, 2),
        ];
        assert_eq!(admit(&q, "w", 8 << 30, LIMITS, None), Decision::Admitted);
    }

    #[test]
    fn budget_is_checked_before_guards() {
        let q = [
            e("r", State::Running, false, 12, 1),
            e("w", State::Waiting, false, 1, 2),
        ];
        let live = LiveSnapshot {
            available_memory: 0,
            ..calm()
        };
        assert!(matches!(
            admit(&q, "w", 0, LIMITS, Some(live)),
            Decision::Wait(Reason::Budget { .. })
        ));
    }

    #[test]
    fn used_budget_ignores_waiting() {
        let q = [
            e("r", State::Running, false, 3, 1),
            e("r2", State::Running, false, 4, 2),
            e("w", State::Waiting, false, 100, 3),
        ];
        assert_eq!(used_budget(&q), 7);
    }
}
