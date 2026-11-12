//! `top`: the `status` screen, redrawn in place until interrupted.

use std::io::Write;
use std::time::Duration;

use task_traffic_controller::config::Config;
use task_traffic_controller::error::Result;
use task_traffic_controller::Context;

use super::status;

pub fn run(ctx: &Context, cfg: &Config, interval: f64) -> Result<i32> {
    let interval = Duration::from_secs_f64(interval.clamp(0.2, 3600.0));
    loop {
        let body = status::render(ctx, cfg)?;
        let mut out = std::io::stdout().lock();
        // Home the cursor, clear the screen, then draw: one write per frame so it does not flicker.
        let frame = format!(
            "\x1b[H\x1b[2J{}  (every {:.0?}, Ctrl-C to quit)\n\n{body}",
            chrono::Local::now().format("%H:%M:%S"),
            interval
        );
        out.write_all(frame.as_bytes())?;
        out.flush()?;
        drop(out);
        std::thread::sleep(interval);
    }
}
