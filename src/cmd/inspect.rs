use canirun::config::Config;
use canirun::error::Result;
use canirun::{inspect, Context};

pub fn run(ctx: &Context, cfg: &Config) -> Result<i32> {
    let insp = inspect::inspect_with(ctx, cfg)?;
    println!("{}", serde_json::to_string_pretty(&insp)?);
    Ok(0)
}
