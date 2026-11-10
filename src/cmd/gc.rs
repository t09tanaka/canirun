use task_traffic_controller::config::Config;
use task_traffic_controller::error::Result;
use task_traffic_controller::ledger::Ledger;
use task_traffic_controller::{lock, managed, Context};

pub fn run(ctx: &Context, cfg: &Config) -> Result<i32> {
    lock::refresh_verdicts(ctx);
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
    let dropped = managed::trim_history(ctx, cfg.history_keep)?;
    let jobs = crate::cmd::job::gc(ctx, cfg.jobs_keep_days)?;
    let markers = managed::gc_measurements(ctx)?;
    let sidecars = managed::gc_sidecars(ctx, cfg.managed_keep_days)?;
    println!(
        "reaped {reaped} dead entries, trimmed {dropped} history records, removed {jobs} old jobs, \
         removed {markers} old measurement markers, removed {sidecars} old managed receipts"
    );
    Ok(0)
}
