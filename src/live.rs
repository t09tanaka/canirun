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
        available_memory: available_memory().unwrap_or_else(|| sys.available_memory()),
        cpu_busy,
        load_avg: System::load_average().one.max(0.0),
    })
}

/// Memory a new command can get without swapping: free (which includes
/// speculative), inactive and purgeable pages. `sysinfo` 0.33 also
/// subtracts the pages the compressor occupies, which are none of these, so
/// with a few tens of GiB compressed it reports 0 and holds every command
/// that does not run alone.
#[cfg(target_os = "macos")]
fn available_memory() -> Option<u64> {
    let mut stat: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    #[allow(deprecated)]
    let host = unsafe { libc::mach_host_self() };
    let ok = unsafe {
        libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            &mut stat as *mut libc::vm_statistics64 as *mut _,
            &mut count,
        )
    } == libc::KERN_SUCCESS;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if !ok || page <= 0 {
        return None;
    }
    Some(pages_available(&stat).saturating_mul(page as u64))
}

#[cfg(target_os = "macos")]
fn pages_available(stat: &libc::vm_statistics64) -> u64 {
    u64::from(stat.free_count)
        .saturating_add(u64::from(stat.inactive_count))
        .saturating_add(u64::from(stat.purgeable_count))
}

#[cfg(not(target_os = "macos"))]
fn available_memory() -> Option<u64> {
    None
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn compressed_pages_do_not_shrink_what_is_available() {
        let mut stat: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        stat.free_count = 85_000;
        stat.inactive_count = 1_800_000;
        stat.purgeable_count = 4_000;
        stat.compressor_page_count = 2_100_000;
        assert_eq!(pages_available(&stat), 1_889_000);
    }

    #[test]
    fn reads_the_machine() {
        let bytes = available_memory().expect("host_statistics64");
        assert!(bytes > 0);
    }
}
