use crate::config::Config;
use crate::error::Result;
use crate::paths;

pub fn run(cfg: &Config, path_only: bool) -> Result<i32> {
    let path = paths::config_file();
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
        toml::to_string_pretty(cfg).map_err(|e| crate::error::Error::Config(e.to_string()))?
    );
    Ok(0)
}
