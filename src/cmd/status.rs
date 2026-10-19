use crate::admit::{self, Decision, Reason, State};
use crate::config::Config;
use crate::display::{fmt_bytes, fmt_duration, truncate};
use crate::error::Result;
use crate::ledger::{Entry, Ledger};
use crate::{live, lock, paths};
use chrono::{DateTime, Utc};
use std::time::Duration;

/// A running entry using less than this many cores...
const IDLE_CORES: f64 = 0.2;
/// ...after running this long is marked `idle`: most likely a server or a
/// watch mode, which holds its weight until it exits.
const IDLE_AFTER: Duration = Duration::from_secs(300);

pub fn run(cfg: &Config, json: bool) -> Result<i32> {
    let ledger = {
        let _guard = lock::ledger()?;
        let mut ledger = Ledger::load(&paths::ledger_file())?;
        if !lock::reap_dead(&mut ledger).is_empty() {
            ledger.save(&paths::ledger_file())?;
        }
        ledger
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&ledger)?);
        return Ok(0);
    }
    let queue = ledger.queue();
    let used = admit::used_budget(&queue);
    let now = Utc::now();
    let mut running: Vec<_> = ledger
        .entries
        .iter()
        .filter(|e| e.state == State::Running)
        .collect();
    running.sort_by_key(|e| e.started_at);
    let order = admit::queue_order(&queue);
    let timed = crate::cmd::exec::timed_queue(&ledger);
    let limits = cfg.limits();
    let eta_of = |id: &str| {
        crate::eta::estimate(&timed, limits, id)
            .map(|e| format!("  ({})", crate::cmd::exec::fmt_estimate(&e)))
            .unwrap_or_default()
    };

    println!("budget {used}/{} used", cfg.budget);
    if let Some(s) = live::snapshot() {
        println!(
            "machine {} available, cpu {:.0}% busy",
            fmt_bytes(s.available_memory),
            s.cpu_busy * 100.0
        );
    }
    println!();
    println!("running ({})", running.len());
    let mut any_idle = false;
    for e in &running {
        let elapsed = running_for(e, now);
        any_idle |= is_idle(e.using, elapsed);
        println!(
            "  {:>7}  {}{} {:>8}  {}{}{}",
            e.pid,
            weight_label(e.weight, e.using),
            if is_idle(e.using, elapsed) {
                " idle"
            } else {
                ""
            },
            fmt_duration(elapsed),
            if e.backfilled { "[backfill] " } else { "" },
            truncate(&label(&e.repo, &e.subdir, &e.argv), 70),
            eta_of(&e.id)
        );
    }
    if any_idle {
        println!(
            "  (idle: running {}+ and using under {IDLE_CORES} cores; a server or watch mode holds its weight until it exits)",
            fmt_duration(IDLE_AFTER)
        );
    }
    println!();
    println!("waiting ({})", order.len());
    for q in &order {
        let Some(e) = ledger.find(&q.id) else {
            continue;
        };
        let waited = (now - e.enqueued_at).to_std().unwrap_or_default();
        println!(
            "  {:>7}  w{:<2} {:>8}  {}{}{}",
            e.pid,
            e.weight,
            fmt_duration(waited),
            if e.front { "[front] " } else { "" },
            truncate(&label(&e.repo, &e.subdir, &e.argv), 70),
            eta_of(&e.id)
        );
    }
    if let Some(head) = order.first() {
        let decision = admit::admit(&queue, &head.id, 0, head.waited_secs, limits, None);
        if let Decision::Wait(Reason::Budget { used, budget }) = decision {
            println!();
            println!(
                "the head needs w{}, {} of {budget} free; held by the running entries above",
                head.weight.min(budget).max(1),
                budget.saturating_sub(used)
            );
        }
    }
    Ok(0)
}

fn running_for(e: &Entry, now: DateTime<Utc>) -> Duration {
    (now - e.started_at.unwrap_or(e.enqueued_at))
        .to_std()
        .unwrap_or_default()
}

/// Measured at almost nothing after running a while.
fn is_idle(using: Option<f64>, running_for: Duration) -> bool {
    running_for >= IDLE_AFTER && using.is_some_and(|u| u < IDLE_CORES)
}

/// One running entry for a waiting line: `48211 w2 idle 52:10 api: make
/// api`, the label cut to `max` characters.
pub fn holder(e: &Entry, now: DateTime<Utc>, max: usize) -> String {
    let elapsed = running_for(e, now);
    let use_part = match e.using {
        _ if is_idle(e.using, elapsed) => " idle".to_string(),
        Some(u) => format!(" using {u:.1}"),
        None => String::new(),
    };
    format!(
        "{} w{}{use_part} {} {}",
        e.pid,
        e.weight,
        fmt_duration(elapsed),
        truncate(&label(&e.repo, &e.subdir, &e.argv), max)
    )
}

/// `w2`, or `w2 (using 5.1)` once the running tree has been measured.
fn weight_label(weight: u32, using: Option<f64>) -> String {
    match using {
        Some(u) => format!("w{weight:<2} (using {u:.1})"),
        None => format!("w{weight:<2}"),
    }
}

pub fn label(repo: &str, subdir: &str, argv: &str) -> String {
    if repo.is_empty() {
        argv.to_string()
    } else {
        format!("{}: {argv}", crate::key::project_label(repo, subdir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_means_measured_near_zero_after_a_while() {
        let long = IDLE_AFTER;
        assert!(is_idle(Some(0.0), long));
        assert!(is_idle(Some(0.19), long));
        assert!(!is_idle(Some(0.2), long));
        assert!(!is_idle(None, long));
        assert!(!is_idle(Some(0.0), long - Duration::from_secs(1)));
    }

    #[test]
    fn holder_names_pid_weight_use_and_command() {
        let now = Utc::now();
        let mut e = Entry {
            id: "a".into(),
            pid: 48211,
            state: State::Running,
            front: false,
            weight: 2,
            predicted_rss: 0,
            expected_secs: None,
            repo: String::new(),
            subdir: String::new(),
            argv: "make api".into(),
            key: "k".into(),
            seq: 1,
            enqueued_at: now,
            started_at: Some(now - chrono::Duration::seconds(3130)),
            using: Some(0.0),
            backfilled: false,
        };
        assert_eq!(holder(&e, now, 40), "48211 w2 idle 52:10 make api");
        e.using = Some(1.26);
        assert_eq!(holder(&e, now, 40), "48211 w2 using 1.3 52:10 make api");
        e.using = None;
        e.started_at = Some(now - chrono::Duration::seconds(5));
        assert_eq!(holder(&e, now, 4), "48211 w2 0:05 mak…");
    }

    #[test]
    fn weight_label_shows_measured_use() {
        assert_eq!(weight_label(2, None), "w2 ");
        assert_eq!(weight_label(2, Some(5.12)), "w2  (using 5.1)");
        assert_eq!(weight_label(12, Some(0.0)), "w12 (using 0.0)");
    }
}
