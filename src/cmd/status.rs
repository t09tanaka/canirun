use canirun::admit::{self, Decision, Reason, State};
use canirun::config::Config;
use canirun::display::{
    fmt_bytes, fmt_duration, is_idle, label, running_for, truncate, IDLE_AFTER, IDLE_CORES,
};
use canirun::error::Result;
use canirun::{exec, lock, Context};

pub fn run(ctx: &Context, cfg: &Config, json: bool) -> Result<i32> {
    let ledger = lock::load_reaped(ctx)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&ledger)?);
        return Ok(0);
    }
    let now = ctx.clock.now();
    let queue = ledger.queue_at(now);
    let used = admit::used_budget(&queue);
    let mut running: Vec<_> = ledger
        .entries
        .iter()
        .filter(|e| e.state == State::Running)
        .collect();
    running.sort_by_key(|e| e.started_at);
    let order = admit::queue_order(&queue);
    let timed = exec::timed_queue(ctx, &ledger);
    let limits = cfg.limits();
    let eta_of = |id: &str| {
        canirun::eta::estimate(&timed, limits, id)
            .map(|e| format!("  ({})", exec::fmt_estimate(&e)))
            .unwrap_or_default()
    };

    println!("budget {used}/{} used", cfg.budget);
    if let Some(s) = ctx.live.snapshot() {
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

/// `w2`, or `w2 (using 5.1)` once the running tree has been measured.
fn weight_label(weight: u32, using: Option<f64>) -> String {
    match using {
        Some(u) => format!("w{weight:<2} (using {u:.1})"),
        None => format!("w{weight:<2}"),
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
