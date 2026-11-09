use canirun::config::Config;
use canirun::error::Result;
use canirun::ledger::Ledger;
use canirun::{history, lock, managed, Context};

pub fn run(ctx: &Context, cfg: &Config) -> Result<i32> {
    let reaped = {
        let _guard = lock::ledger(ctx)?;
        let mut ledger = Ledger::load(&ctx.paths.ledger_file())?;
        let dead = lock::reap_dead(ctx, &mut ledger);
        lock::sweep_orphan_leases(ctx, &ledger);
        if !dead.is_empty() {
            ledger.save(&ctx.paths.ledger_file())?;
        }
        dead.len()
    };
    let dropped = history::trim(ctx.paths.history_file(), cfg.history_keep)?;
    let jobs = crate::cmd::job::gc(ctx, cfg.jobs_keep_days)?;
    let markers = managed::gc_measurements(ctx, cfg.history_keep)?;
    println!(
        "reaped {reaped} dead entries, trimmed {dropped} history records, removed {jobs} old jobs, \
         removed {markers} old measurement markers"
    );
    Ok(0)
}
