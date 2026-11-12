use crate::ledger::Entry;
use chrono::{DateTime, Utc};
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

/// A running entry using less than this many cores...
pub const IDLE_CORES: f64 = 0.2;
/// ...after running this long is marked `idle`: most likely a server or a
/// watch mode, which holds its weight until it exits.
pub const IDLE_AFTER: Duration = Duration::from_secs(300);

pub fn running_for(e: &Entry, now: DateTime<Utc>) -> Duration {
    (now - e.started_at.unwrap_or(e.enqueued_at))
        .to_std()
        .unwrap_or_default()
}

/// Measured at almost nothing after running a while.
pub fn is_idle(using: Option<f64>, running_for: Duration) -> bool {
    running_for >= IDLE_AFTER && using.is_some_and(|u| u < IDLE_CORES)
}

/// One running entry for a waiting line: `48211 w2 idle 52:10 api: make
/// api`, the label cut to `max` characters.
pub fn holder(e: &Entry, now: DateTime<Utc>, max: usize) -> String {
    let elapsed = running_for(e, now);
    let use_part = match e.using {
        _ if is_idle(e.using, elapsed) => " idle".to_string(),
        Some(u) => format!(" using {u:.1}"),
        None => String::new(),
    };
    format!(
        "{} w{}{use_part} {} {}",
        e.pid,
        e.weight,
        fmt_duration(elapsed),
        truncate(&entry_label(e), max)
    )
}

pub fn label(repo: &str, subdir: &str, argv: &str) -> String {
    if repo.is_empty() {
        argv.to_string()
    } else {
        format!("{}: {argv}", crate::key::project_label(repo, subdir))
    }
}

/// [`label`] for an entry: its `--label` stands in for the command line.
pub fn entry_label(e: &Entry) -> String {
    label(&e.repo, &e.subdir, e.label.as_deref().unwrap_or(&e.argv))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admit::State;

    #[test]
    fn idle_means_measured_near_zero_after_a_while() {
        let long = IDLE_AFTER;
        assert!(is_idle(Some(0.0), long));
        assert!(is_idle(Some(0.19), long));
        assert!(!is_idle(Some(0.2), long));
        assert!(!is_idle(None, long));
        assert!(!is_idle(Some(0.0), long - Duration::from_secs(1)));
    }

    #[test]
    fn holder_names_pid_weight_use_and_command() {
        let now = Utc::now();
        let mut e = Entry {
            id: "a".into(),
            pid: 48211,
            state: State::Running,
            front: false,
            weight: 2,
            predicted_rss: 0,
            expected_secs: None,
            repo: String::new(),
            subdir: String::new(),
            argv: "make api".into(),
            key: "k".into(),
            seq: 1,
            enqueued_at: now,
            started_at: Some(now - chrono::Duration::seconds(3130)),
            using: Some(0.0),
            backfilled: false,
            label: None,
        };
        assert_eq!(holder(&e, now, 40), "48211 w2 idle 52:10 make api");
        e.using = Some(1.26);
        assert_eq!(holder(&e, now, 40), "48211 w2 using 1.3 52:10 make api");
        e.using = None;
        e.started_at = Some(now - chrono::Duration::seconds(5));
        assert_eq!(holder(&e, now, 4), "48211 w2 0:05 mak…");
    }

    #[test]
    fn a_label_stands_in_for_the_command_line() {
        let now = Utc::now();
        let mut e = Entry {
            id: "a".into(),
            pid: 1,
            state: State::Running,
            front: false,
            weight: 1,
            predicted_rss: 0,
            expected_secs: None,
            repo: "/src/web".into(),
            subdir: "api".into(),
            argv: "npm run test".into(),
            key: "k".into(),
            seq: 1,
            enqueued_at: now,
            started_at: None,
            using: None,
            backfilled: false,
            label: None,
        };
        assert_eq!(entry_label(&e), "web/api: npm run test");
        e.label = Some("unit tests".into());
        assert_eq!(entry_label(&e), "web/api: unit tests");
    }

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
