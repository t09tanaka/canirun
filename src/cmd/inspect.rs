use task_traffic_controller::config::Config;
use task_traffic_controller::error::Result;
use task_traffic_controller::{inspect, managed, Context};

pub fn run(ctx: &Context, cfg: &Config) -> Result<i32> {
    let insp = inspect::inspect_with(ctx, cfg)?;
    let mut value = serde_json::to_value(&insp)?;
    // Managed entries (and which are quarantined) are listed beside the
    // entries, only when there are any, so the shape is unchanged for plain
    // use and the snapshot digest does not move.
    let entries: Vec<_> = insp.entries.iter().map(|e| e.entry.clone()).collect();
    let views = managed::views(ctx, &entries);
    if !views.is_empty() {
        value["managed"] = serde_json::to_value(&views)?;
    }
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(0)
}
