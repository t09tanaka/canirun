//! `config.toml` loading with defaults (spec "Configuration").

use crate::error::{Error, Result};
use crate::key::{classify, Kind};
use crate::weight::PatternHint;
use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pattern {
    /// Glob over the argv string.
    #[serde(rename = "match")]
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_weight: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_weight: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub budget: u32,
    pub default_weight: u32,
    /// First weight of a test runner nothing has been learned about.
    pub test_initial_weight: u32,
    /// First weight of a mutation-testing run nothing has been learned
    /// about.
    pub mutation_initial_weight: u32,
    pub history_keep: usize,
    pub progress_interval_secs: u64,
    /// When stderr is not a terminal: a progress line only when the wait
    /// reason or queue position changes, and at most this far apart
    /// otherwise.
    pub progress_interval_secs_non_tty: u64,
    pub memory_reserve_mib: u64,
    /// Hold the next start while global CPU busy % is above this.
    pub cpu_threshold_percent: f64,
    /// After waiting this long the CPU guard stops holding the head.
    pub cpu_guard_max_wait_secs: u64,
    /// A `--front` entry waits while the load average is at or above
    /// `budget` times this (0 disables)...
    pub front_overload_factor: f64,
    /// ...for at most this long.
    pub front_overload_max_wait_secs: u64,
    /// `gc` removes finished job files older than this.
    pub jobs_keep_days: u64,
    /// A job with no `wait`er is cancelled once this many seconds have
    /// passed since submit (unless submitted with `--detach`).
    pub jobs_orphan_grace_secs: u64,
    #[serde(rename = "pattern", skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<Pattern>,
}

pub fn logical_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

impl Default for Config {
    fn default() -> Self {
        Config {
            budget: logical_cpus(),
            default_weight: 2,
            test_initial_weight: 4,
            mutation_initial_weight: 6,
            history_keep: 200,
            progress_interval_secs: 5,
            progress_interval_secs_non_tty: 60,
            memory_reserve_mib: 2048,
            cpu_threshold_percent: 85.0,
            cpu_guard_max_wait_secs: 120,
            front_overload_factor: 2.0,
            front_overload_max_wait_secs: 30,
            jobs_keep_days: 7,
            jobs_orphan_grace_secs: 30,
            patterns: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e.into()),
        };
        Config::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Config> {
        let cfg: Config = toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.budget == 0 {
            return Err(Error::Config("budget must be at least 1".into()));
        }
        if !(self.cpu_threshold_percent > 0.0 && self.cpu_threshold_percent <= 100.0) {
            return Err(Error::Config(
                "cpu_threshold_percent must be in (0, 100]".into(),
            ));
        }
        if self.front_overload_factor.is_nan() || self.front_overload_factor < 0.0 {
            return Err(Error::Config(
                "front_overload_factor must be 0 or more".into(),
            ));
        }
        for p in &self.patterns {
            if p.weight.is_some() && (p.min_weight.is_some() || p.initial_weight.is_some()) {
                return Err(Error::Config(format!(
                    "pattern {:?}: weight is exclusive with min_weight/initial_weight",
                    p.pattern
                )));
            }
            Glob::new(&p.pattern)
                .map_err(|e| Error::Config(format!("pattern {:?}: {e}", p.pattern)))?;
        }
        Ok(())
    }

    /// First matching pattern, top to bottom.
    pub fn hint_for(&self, argv: &str) -> PatternHint {
        for p in &self.patterns {
            let m: GlobMatcher = match Glob::new(&p.pattern) {
                Ok(g) => g.compile_matcher(),
                Err(_) => continue,
            };
            if m.is_match(argv) {
                return PatternHint {
                    weight: p.weight,
                    min_weight: p.min_weight,
                    initial_weight: p.initial_weight,
                };
            }
        }
        PatternHint::default()
    }

    /// [`Config::hint_for`] the argv string, with the initial weight of
    /// the key's [`Kind`] when no pattern sets one.
    pub fn hint_for_key(&self, argv: &str, key_argv: &str) -> PatternHint {
        let mut hint = self.hint_for(argv);
        if hint.initial_weight.is_none() {
            hint.initial_weight = match classify(key_argv) {
                Kind::Tests => Some(self.test_initial_weight),
                Kind::Mutation => Some(self.mutation_initial_weight),
                Kind::Other => None,
            };
        }
        hint
    }

    pub fn cpu_threshold(&self) -> f64 {
        self.cpu_threshold_percent / 100.0
    }

    /// Load average at which a `--front` entry is held (0: never).
    pub fn front_overload_load(&self) -> f64 {
        self.front_overload_factor * f64::from(self.budget)
    }

    pub fn memory_reserve_bytes(&self) -> u64 {
        self.memory_reserve_mib.saturating_mul(1024 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.default_weight, 2);
        assert_eq!(c.budget, logical_cpus());
        assert!(c.patterns.is_empty());
    }

    #[test]
    fn first_match_wins() {
        let c = Config::parse(
            r#"
[[pattern]]
match = "*stryker*"
min_weight = 6
[[pattern]]
match = "*"
weight = 1
"#,
        )
        .unwrap();
        assert_eq!(c.hint_for("npx stryker run").min_weight, Some(6));
        assert_eq!(c.hint_for("npx stryker run").weight, None);
        assert_eq!(c.hint_for("npm run lint").weight, Some(1));
    }

    #[test]
    fn glob_star_crosses_spaces_and_slashes() {
        let c = Config::parse("[[pattern]]\nmatch = \"*test:e2e*\"\nmin_weight = 4\n").unwrap();
        assert_eq!(
            c.hint_for("npm run test:e2e -- --project a/b").min_weight,
            Some(4)
        );
        assert_eq!(c.hint_for("npm run test:unit").min_weight, None);
    }

    #[test]
    fn known_kinds_start_heavier_unless_a_pattern_says_otherwise() {
        let c = Config::parse(
            r#"
test_initial_weight = 3
[[pattern]]
match = "*e2e*"
initial_weight = 8
[[pattern]]
match = "*lint*"
weight = 1
[[pattern]]
match = "*unit*"
min_weight = 2
"#,
        )
        .unwrap();
        let init = |argv: &str, key: &str| c.hint_for_key(argv, key).initial_weight;
        assert_eq!(init("npm test", "npm test"), Some(3));
        assert_eq!(init("x", "npx stryker run"), Some(6));
        assert_eq!(init("npm run build", "npm run build"), None);
        assert_eq!(init("npm run test:e2e", "npm run test:e2e"), Some(8));
        assert_eq!(init("npm run test:unit", "npm run test:unit"), Some(3));
        assert_eq!(
            c.hint_for_key("npm run test:unit", "npm run test:unit")
                .min_weight,
            Some(2)
        );
        assert_eq!(
            c.hint_for_key("npm run lint", "npm run lint").weight,
            Some(1)
        );
        let d = Config::default();
        assert_eq!(
            d.hint_for_key("cargo test", "cargo test").initial_weight,
            Some(4)
        );
    }

    #[test]
    fn rejects_weight_with_min_weight() {
        let e = Config::parse("[[pattern]]\nmatch = \"x\"\nweight = 1\nmin_weight = 2\n");
        assert!(e.is_err());
    }

    #[test]
    fn rejects_zero_budget_and_bad_threshold() {
        assert!(Config::parse("budget = 0").is_err());
        assert!(Config::parse("cpu_threshold_percent = 0").is_err());
        assert!(Config::parse("cpu_threshold_percent = 101").is_err());
        assert!(Config::parse("cpu_threshold_percent = 100").is_ok());
        assert!(Config::parse("front_overload_factor = -1").is_err());
        let c = Config::parse("budget = 12\nfront_overload_factor = 0").unwrap();
        assert_eq!(c.front_overload_load(), 0.0);
        assert_eq!(
            Config::parse("budget = 12").unwrap().front_overload_load(),
            24.0
        );
    }

    #[test]
    fn rejects_bad_glob() {
        assert!(Config::parse("[[pattern]]\nmatch = \"[\"\n").is_err());
    }

    #[test]
    fn missing_file_is_default() {
        let d = tempfile::tempdir().unwrap();
        let c = Config::load(&d.path().join("nope.toml")).unwrap();
        assert_eq!(c, Config::default());
    }
}
