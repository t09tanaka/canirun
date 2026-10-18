use crate::admit::{self, State};
use crate::config::Config;
use crate::display::{fmt_bytes, fmt_duration, truncate};
use crate::error::Result;
use crate::ledger::Ledger;
use crate::{live, lock, paths};
use chrono::Utc;

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
    let eta_of = |id: &str| {
        crate::eta::estimate(&timed, cfg.budget, id)
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
    for e in &running {
        let since = e.started_at.unwrap_or(e.enqueued_at);
        let elapsed = (now - since).to_std().unwrap_or_default();
        println!(
            "  {:>7}  {} {:>8}  {}{}",
            e.pid,
            weight_label(e.weight, e.using),
            fmt_duration(elapsed),
            truncate(&label(&e.repo, &e.subdir, &e.argv), 70),
            eta_of(&e.id)
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
    Ok(0)
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
    fn weight_label_shows_measured_use() {
        assert_eq!(weight_label(2, None), "w2 ");
        assert_eq!(weight_label(2, Some(5.12)), "w2  (using 5.1)");
        assert_eq!(weight_label(12, Some(0.0)), "w12 (using 0.0)");
    }
}
