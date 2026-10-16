# Changelog

All notable changes to canirun are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/) (pre-1.0: a breaking change bumps
the minor version).

## [0.1.0] - 2026-10-16

First release.

### Added

- `canirun exec [--weight N] [--front] [--key K] -- <command>`: wait for
  budget, run the command in canirun's process group with inherited stdio,
  forward SIGTERM/SIGHUP, mirror the exit code or signal death, and record
  CPU time, wall time and peak RSS via `wait4`.
- Learned weights: ceiling of the highest CPU parallelism over the last
  five usable runs of the same `(repository, argv)`; peak RSS learned the
  same way. `config.toml` patterns provide `weight` (fixed), `min_weight`
  and `initial_weight` hints.
- Live guards: available memory must cover predicted RSS plus
  `memory_reserve_mib`, and global CPU usage must be under
  `cpu_threshold_percent`, before the head of the queue starts. Skipped
  when nothing managed is running.
- FIFO queue with head reservation; `--front` for interactive callers.
- Crash-safe liveness via per-entry `flock` lease files.
- Nested `exec` inherits a live `CANIRUN_LEASE`; a stale one is gated.
- `canirun status [--json]`, `canirun history`, `canirun config [--path]`,
  `canirun gc`.
