//! Pure weight and memory prediction (spec "Weight derivation").

/// One measured run, as far as weight derivation cares.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    pub wall_secs: f64,
    pub cpu_secs: f64,
    pub max_rss_bytes: u64,
    /// The machine was already saturated when the run started, so
    /// `cpu_secs / wall_secs` under-reports the command's parallelism.
    pub contended: bool,
}

/// The part of a matched config pattern that influences the weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PatternHint {
    /// Fixed weight: history is ignored.
    pub weight: Option<u32>,
    /// Learned weight may not drop below this.
    pub min_weight: Option<u32>,
    /// Used only when there is no usable history.
    pub initial_weight: Option<u32>,
}

/// How many of the most recent usable records are considered.
pub const WINDOW: usize = 5;
/// Records shorter than this are too short to measure parallelism.
pub const MIN_USABLE_WALL_SECS: f64 = 1.0;

/// Where the final weight came from, for `history` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    CommandLine,
    Fixed,
    Learned,
    Initial,
    Default,
}

/// The most recent `WINDOW` usable records, newest last. `records` must be
/// ordered oldest first. Runs measured on a saturated machine are left out
/// whenever at least one calm run exists, because their parallelism reads
/// low and would admit too much.
pub fn usable_window(records: &[Measurement]) -> Vec<Measurement> {
    let usable: Vec<Measurement> = records
        .iter()
        .copied()
        .filter(|m| m.wall_secs >= MIN_USABLE_WALL_SECS)
        .collect();
    let calm: Vec<Measurement> = usable.iter().copied().filter(|m| !m.contended).collect();
    let mut chosen = if calm.is_empty() { usable } else { calm };
    let drop = chosen.len().saturating_sub(WINDOW);
    chosen.drain(..drop);
    chosen
}

/// Learned parallelism: ceil of the max cpu/wall over the window, or None
/// when there is no usable record.
pub fn learned_weight(records: &[Measurement]) -> Option<u32> {
    let window = usable_window(records);
    let max = window
        .iter()
        .map(|m| m.cpu_secs / m.wall_secs)
        .fold(None, |acc: Option<f64>, v| {
            Some(acc.map_or(v, |a| a.max(v)))
        })?;
    Some((max.ceil() as u32).max(1))
}

/// Peak RSS over the window, or 0 when unknown.
pub fn predicted_rss(records: &[Measurement]) -> u64 {
    usable_window(records)
        .iter()
        .map(|m| m.max_rss_bytes)
        .max()
        .unwrap_or(0)
}

/// Full priority order from the spec, then floor 1 and ceiling `budget`.
pub fn derive_weight(
    cli_override: Option<u32>,
    hint: PatternHint,
    records: &[Measurement],
    default_weight: u32,
    budget: u32,
) -> (u32, Source) {
    let (raw, source) = if let Some(w) = cli_override {
        (w, Source::CommandLine)
    } else if let Some(w) = hint.weight {
        (w, Source::Fixed)
    } else if let Some(learned) = learned_weight(records) {
        (learned.max(hint.min_weight.unwrap_or(0)), Source::Learned)
    } else if let Some(w) = hint.initial_weight {
        (w.max(hint.min_weight.unwrap_or(0)), Source::Initial)
    } else {
        (
            default_weight.max(hint.min_weight.unwrap_or(0)),
            Source::Default,
        )
    };
    (raw.max(1).min(budget.max(1)), source)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(wall: f64, cpu: f64, rss: u64) -> Measurement {
        Measurement {
            wall_secs: wall,
            cpu_secs: cpu,
            max_rss_bytes: rss,
            contended: false,
        }
    }
    fn c(wall: f64, cpu: f64, rss: u64) -> Measurement {
        Measurement {
            contended: true,
            ..m(wall, cpu, rss)
        }
    }
    const NONE: PatternHint = PatternHint {
        weight: None,
        min_weight: None,
        initial_weight: None,
    };

    #[test]
    fn usable_window_filters_before_truncating() {
        let mut r = vec![m(10.0, 80.0, 1); 1];
        r.extend(std::iter::repeat_n(m(0.5, 0.1, 2), 5));
        let w = usable_window(&r);
        assert_eq!(w, vec![m(10.0, 80.0, 1)]);
    }

    #[test]
    fn usable_window_keeps_newest_five() {
        let r: Vec<_> = (1..=7).map(|i| m(2.0, i as f64, i)).collect();
        let w = usable_window(&r);
        assert_eq!(w.len(), 5);
        assert_eq!(w[0].cpu_secs, 3.0);
        assert_eq!(w[4].cpu_secs, 7.0);
    }

    #[test]
    fn contended_runs_are_ignored_when_a_calm_run_exists() {
        // Six parallel workers on a saturated machine look like x1.8.
        let r = [m(10.0, 60.0, 5), c(20.0, 36.0, 9), c(20.0, 38.0, 9)];
        assert_eq!(usable_window(&r), vec![m(10.0, 60.0, 5)]);
        assert_eq!(learned_weight(&r), Some(6));
        assert_eq!(predicted_rss(&r), 5);
    }

    #[test]
    fn contended_runs_are_used_when_nothing_else_exists() {
        let r = [c(20.0, 36.0, 9), c(0.5, 1.0, 1)];
        assert_eq!(usable_window(&r), vec![c(20.0, 36.0, 9)]);
        assert_eq!(learned_weight(&r), Some(2));
    }

    #[test]
    fn window_is_taken_after_dropping_contended() {
        let mut r: Vec<Measurement> = (1..=6).map(|i| m(2.0, i as f64, i)).collect();
        r.push(c(2.0, 100.0, 100));
        let w = usable_window(&r);
        assert_eq!(w.len(), 5);
        assert_eq!(w[0].cpu_secs, 2.0);
        assert_eq!(w[4].cpu_secs, 6.0);
    }

    #[test]
    fn usable_boundary_is_one_second_inclusive() {
        assert_eq!(usable_window(&[m(1.0, 1.0, 1)]).len(), 1);
        assert_eq!(usable_window(&[m(0.999, 1.0, 1)]).len(), 0);
    }

    #[test]
    fn learned_is_ceil_of_max_ratio() {
        let r = [m(10.0, 21.0, 0), m(10.0, 35.0, 0), m(10.0, 5.0, 0)];
        assert_eq!(learned_weight(&r), Some(4));
    }

    #[test]
    fn learned_floor_is_one() {
        assert_eq!(learned_weight(&[m(10.0, 0.0, 0)]), Some(1));
    }

    #[test]
    fn learned_exact_integer_is_not_rounded_up() {
        assert_eq!(learned_weight(&[m(10.0, 30.0, 0)]), Some(3));
    }

    #[test]
    fn learned_none_without_usable_history() {
        assert_eq!(learned_weight(&[]), None);
        assert_eq!(learned_weight(&[m(0.1, 0.1, 0)]), None);
    }

    #[test]
    fn predicted_rss_is_window_max() {
        let r = [m(2.0, 1.0, 900), m(2.0, 1.0, 300), m(0.2, 1.0, 5000)];
        assert_eq!(predicted_rss(&r), 900);
        assert_eq!(predicted_rss(&[]), 0);
    }

    #[test]
    fn priority_cli_over_everything() {
        let hint = PatternHint {
            weight: Some(9),
            ..NONE
        };
        let r = [m(10.0, 100.0, 0)];
        assert_eq!(
            derive_weight(Some(3), hint, &r, 2, 12),
            (3, Source::CommandLine)
        );
    }

    #[test]
    fn priority_fixed_over_learned() {
        let hint = PatternHint {
            weight: Some(1),
            ..NONE
        };
        let r = [m(10.0, 100.0, 0)];
        assert_eq!(derive_weight(None, hint, &r, 2, 12), (1, Source::Fixed));
    }

    #[test]
    fn learned_respects_min_weight() {
        let hint = PatternHint {
            min_weight: Some(6),
            ..NONE
        };
        let r = [m(10.0, 10.0, 0)];
        assert_eq!(derive_weight(None, hint, &r, 2, 12), (6, Source::Learned));
        let r = [m(10.0, 80.0, 0)];
        assert_eq!(derive_weight(None, hint, &r, 2, 12), (8, Source::Learned));
    }

    #[test]
    fn initial_used_only_without_history() {
        let hint = PatternHint {
            initial_weight: Some(4),
            ..NONE
        };
        assert_eq!(derive_weight(None, hint, &[], 2, 12), (4, Source::Initial));
        let r = [m(10.0, 10.0, 0)];
        assert_eq!(derive_weight(None, hint, &r, 2, 12), (1, Source::Learned));
    }

    #[test]
    fn initial_respects_min_weight() {
        let hint = PatternHint {
            initial_weight: Some(2),
            min_weight: Some(5),
            ..NONE
        };
        assert_eq!(derive_weight(None, hint, &[], 2, 12), (5, Source::Initial));
    }

    #[test]
    fn default_when_nothing_known() {
        assert_eq!(derive_weight(None, NONE, &[], 2, 12), (2, Source::Default));
        let hint = PatternHint {
            min_weight: Some(3),
            ..NONE
        };
        assert_eq!(derive_weight(None, hint, &[], 2, 12), (3, Source::Default));
    }

    #[test]
    fn clamp_floor_and_ceiling() {
        assert_eq!(derive_weight(Some(0), NONE, &[], 2, 12).0, 1);
        assert_eq!(derive_weight(Some(99), NONE, &[], 2, 12).0, 12);
        assert_eq!(derive_weight(Some(5), NONE, &[], 2, 0).0, 1);
    }
}
