# Changelog

All notable changes to canirun are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/) (pre-1.0: a breaking change bumps
the minor version).

## [0.2.1] - 2026-10-16

Feedback from the first day of use on a machine where most load did not go
through canirun yet.

### Changed

- **`--front` entries skip the CPU guard** (the memory guard still applies).
  A human waiting on a hook must not be held by load canirun does not
  manage.
- **The CPU guard gives up after `cpu_guard_max_wait_secs`** (default 120):
  once the head has waited that long it is admitted on the budget and
  memory checks alone.
- **Runs that start on a saturated machine are recorded as `contended`**
  (one CPU sample right before the start) and are left out of weight and
  memory learning whenever a calm run exists. Under contention
  `cpu / wall` under-reports parallelism, which would admit too much.
  `history` marks such runs.
- **Keys include the subdirectory within the repository**
  (`git rev-parse --show-prefix`), so `api/` and `client/` of one monorepo
  learn separately. Root-level keys hash exactly as before, so their
  history carries over. `status` / `jobs` / `history` show `repo/subdir`.

## [0.2.0] - 2026-10-16

### Added

- `canirun submit [--detach] [--weight N] [--front] [--key K] -- <command>`:
  start a job in a detached runner and print its id. The runner follows
  the same admission, measurement and liveness path as `exec`; its merged
  log, descriptor and exit code live under `jobs/` in the state directory.
- `canirun wait [--no-output] <id>...`: block until the jobs finish, print
  each log in the order given, exit with the first non-zero code. `wait`
  holds a shared lock per job; a job with no holder is cancelled by its
  runner once `jobs_orphan_grace_secs` (default 30) have passed since
  submit, unless submitted with `--detach`.
- `canirun jobs [--json]` and `canirun cancel <id>...`.
- Rough completion estimates from learned wall times and the current
  queue: shown by `submit`, in the `exec` progress line, and in `status` and
  `jobs`. Unknown commands count as 60 s and are marked with `?`.
- `jobs_keep_days` (default 7): `gc` removes older finished job files.
- A signal that arrives between the last queue check and admission, or
  between handler installation and spawn, is no longer lost.

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
