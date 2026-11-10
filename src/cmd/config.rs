use task_traffic_controller::config::Config;
use task_traffic_controller::error::Result;
use task_traffic_controller::Context;

pub fn run(ctx: &Context, cfg: &Config, path_only: bool) -> Result<i32> {
    let path = ctx.paths.config_file();
    if path_only {
        println!("{}", path.display());
        return Ok(0);
    }
    println!(
        "# {} ({})",
        path.display(),
        if path.exists() {
            "loaded"
        } else {
            "absent, defaults"
        }
    );
    print!(
        "{}",
        toml::to_string_pretty(cfg)
            .map_err(|e| task_traffic_controller::error::Error::Config(e.to_string()))?
    );
    Ok(0)
}
