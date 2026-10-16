use crate::config::Config;
use crate::error::Result;
use crate::ledger::Ledger;
use crate::{history, lock, paths};

pub fn run(cfg: &Config) -> Result<i32> {
    let reaped = {
        let _guard = lock::ledger()?;
        let mut ledger = Ledger::load(&paths::ledger_file())?;
        let dead = lock::reap_dead(&mut ledger);
        lock::sweep_orphan_leases(&ledger);
        if !dead.is_empty() {
            ledger.save(&paths::ledger_file())?;
        }
        dead.len()
    };
    let dropped = history::trim(&paths::history_file(), cfg.history_keep)?;
    let jobs = crate::cmd::job::gc(cfg.jobs_keep_days)?;
    println!(
        "reaped {reaped} dead entries, trimmed {dropped} history records, removed {jobs} old jobs"
    );
    Ok(0)
}
