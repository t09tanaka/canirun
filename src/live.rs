//! Live machine snapshot via `sysinfo` (spec "Live guards").

use crate::admit::LiveSnapshot;
use std::time::Duration;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

/// Two CPU samples this far apart give the busy fraction "right now".
pub const CPU_SAMPLE_INTERVAL: Duration = Duration::from_millis(300);

pub fn snapshot() -> Option<LiveSnapshot> {
    let mut sys = System::new_with_specifics(
        RefreshKind::nothing()
            .with_memory(MemoryRefreshKind::nothing().with_ram())
            .with_cpu(CpuRefreshKind::nothing().with_cpu_usage()),
    );
    if sys.total_memory() == 0 {
        return None;
    }
    std::thread::sleep(CPU_SAMPLE_INTERVAL.max(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL));
    sys.refresh_cpu_usage();
    let cpu_busy = (sys.global_cpu_usage() as f64 / 100.0).clamp(0.0, 1.0);
    Some(LiveSnapshot {
        available_memory: sys.available_memory(),
        cpu_busy,
        load_avg: System::load_average().one.max(0.0),
    })
}
