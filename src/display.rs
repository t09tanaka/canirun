use std::time::Duration;

pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

pub fn fmt_bytes(b: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let b = b as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.0} MiB", b / MIB)
    } else {
        format!("{:.0} KiB", b / 1024.0)
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(Duration::from_secs(42)), "0:42");
        assert_eq!(fmt_duration(Duration::from_secs(3661)), "1:01:01");
    }

    #[test]
    fn bytes() {
        assert_eq!(fmt_bytes(1536), "2 KiB");
        assert_eq!(fmt_bytes(3 << 20), "3 MiB");
        assert_eq!(fmt_bytes(3 << 30), "3.0 GiB");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "ab…");
    }
}
