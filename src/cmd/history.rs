use crate::cmd::status::label;
use crate::config::Config;
use crate::display::{fmt_bytes, truncate};
use crate::error::Result;
use crate::history::{self, Record};
use crate::paths;
use crate::weight::{self, Measurement};
use std::collections::BTreeMap;

pub struct Opts {
    pub key: Option<String>,
    pub argv: Vec<String>,
    pub json: bool,
    pub limit: usize,
}

pub fn run(cfg: &Config, opts: Opts) -> Result<i32> {
    let all = history::read_all(&paths::history_file())?;
    let filter_hash = if opts.key.is_some() || !opts.argv.is_empty() {
        Some(crate::cmd::command_key(opts.key.as_deref(), &opts.argv)?.hash)
    } else {
        None
    };
    let records: Vec<&Record> = all
        .iter()
        .filter(|r| filter_hash.as_ref().map(|h| &r.key == h).unwrap_or(true))
        .collect();
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&records)?);
        return Ok(0);
    }
    let mut by_key: BTreeMap<&str, Vec<&Record>> = BTreeMap::new();
    for r in &records {
        by_key.entry(r.key.as_str()).or_default().push(r);
    }
    if by_key.is_empty() {
        println!("no history");
        return Ok(0);
    }
    for (_, recs) in by_key {
        let last = recs[recs.len() - 1];
        let ms: Vec<Measurement> = recs.iter().map(|r| r.measurement()).collect();
        let hint = cfg.hint_for(&last.argv);
        let (next_w, source) =
            weight::derive_weight(None, hint, &ms, cfg.default_weight, cfg.budget);
        let keyed_by = if last.key_argv.is_empty() {
            String::new()
        } else {
            format!("\n  key: {}", truncate(&last.key_argv, 90))
        };
        println!(
            "{}{keyed_by}\n  next: weight {next_w} ({source:?}), memory {}",
            truncate(&label(&last.repo, &last.subdir, &last.argv), 90),
            fmt_bytes(weight::predicted_rss(&ms))
        );
        let skip = recs.len().saturating_sub(opts.limit);
        for r in &recs[skip..] {
            let par = if r.wall_secs > 0.0 {
                r.cpu_secs / r.wall_secs
            } else {
                0.0
            };
            println!(
                "  {}  wall {:>7.1}s  cpu {:>7.1}s  x{:<4.1} rss {:>9}  exit {:<3} w{}{}",
                r.started_at
                    .with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M"),
                r.wall_secs,
                r.cpu_secs,
                par,
                fmt_bytes(r.max_rss_bytes),
                r.exit_code,
                r.weight_used,
                if r.contended { "  (contended)" } else { "" }
            );
        }
    }
    Ok(0)
}
