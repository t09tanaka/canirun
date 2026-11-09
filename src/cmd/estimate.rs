use canirun::config::Config;
use canirun::display::fmt_duration;
use canirun::error::Result;
use canirun::exec::Opts;
use canirun::forecast::{self, Timing};
use canirun::{estimate, inspect, Context};
use std::time::Duration;

pub fn run(ctx: &Context, cfg: &Config, opts: Opts, json: bool) -> Result<i32> {
    let plan = estimate::command_plan(ctx, cfg, &opts)?;
    let insp = inspect::inspect_with(ctx, cfg)?;
    let fc = forecast::forecast(ctx, &insp, &plan, &[])?;
    if json {
        println!("{}", serde_json::to_string_pretty(&fc)?);
        return Ok(0);
    }
    println!("{}", summary(&fc.target.timing));
    Ok(0)
}

fn ms(v: u64) -> String {
    fmt_duration(Duration::from_millis(v))
}

fn summary(t: &Timing) -> String {
    match (t.start_in_ms, t.finish_in_ms) {
        (Some(s), Some(f)) => format!("start in ~{}, done in ~{}", ms(s), ms(f)),
        _ => match &t.ranking {
            Some(r) => format!(
                "no measured estimate (rough ranking only: start in ~{})",
                r.start_in_ms.map(ms).unwrap_or_else(|| "?".into())
            ),
            None => "no estimate".into(),
        },
    }
}
