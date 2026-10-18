# Changelog

All notable changes to canirun are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/) (pre-1.0: a breaking change bumps
the minor version).

## [Unreleased]

### Added

- **Running commands are charged what they actually use.** Each running
  `canirun` samples its command's process tree once a second (macOS and
  Linux) and writes the cores used (peak of the last three samples) to
  its ledger entry. Admission charges a running entry `max(weight,
  round(using))`, so a command declared at weight 1 or 2 that runs five
  cores of workers no longer lets the next one start on top of it.
  `status` shows it as `w2 (using 5.1)` and counts it in `budget X/Y
  used`; the ledger field is `using`. Without a measurement (first second,
  or a platform that cannot be read) the declared weight is used. The
  command itself is never slowed or stopped.

- **Test runners and mutation tools start heavier.** A command nothing
  has been learned about starts at `test_initial_weight` (new, default 4)
  when it is a test runner and `mutation_initial_weight` (new, default 6)
  when it is mutation testing, instead of `default_weight`. A pattern's
  `initial_weight` or `weight` still wins.
- **`--front` waits briefly on an overloaded machine.** When the load
  average is at least `front_overload_factor` (new, default 2.0) x
  `budget`, a `--front` head waits for at most
  `front_overload_max_wait_secs` (new, default 30) before starting, with
  a progress line `overloaded: load N, limit M`. Previously `--front`
  skipped every CPU check, so a pre-push hook could start on a machine
  already running at many times its cores. Set the factor to 0 for the
  old behaviour. The same cap bounds how long measured use beyond the
  running entries' weights can hold a `--front` entry; after it, only
  declared weights count for that entry.

### Changed

- **Wrapped commands learn from their earlier runs.** `sh -c` / `bash -c`
  / `zsh -c` lines and scripts under a temporary directory used to get a
  new history key on every run (different redirections, temp paths, a
  new scratch file name), so they ran at `default_weight` forever. They
  are now keyed by the first heavy command inside them (`npm run <script>`,
  `npx`, `node`, `vitest`, `jest`, `stryker`, `cargo test`, `dart test`,
  `flutter test`, ...), with redirections dropped and temporary paths
  masked; a temporary script is read (first 8 KiB) for the same purpose,
  or keyed by its file name. Plain commands keep exactly the key they had
  before. History recorded for a wrapped command under 0.3 is not
  migrated: the new key starts learning from its first run (or shares
  what the plain command already learned). `--key` still wins.

## [0.3.0] - 2026-10-17

### Added

- **A hint for a command passed as one quoted argument.** When
  `canirun exec -- "npm run x"` (or `submit`) fails because no program
  named `npm run x` exists, the error suggests `-- npm run x`, or
  `sh -c '...'` for shell syntax.

### Changed

- **Fewer progress lines when stderr is not a terminal.** A waiting line
  is printed only when the wait reason (queue / budget / memory / cpu) or
  the queue position changes, and otherwise every
  `progress_interval_secs_non_tty` (new, default 60). On a terminal the
  `progress_interval_secs` cadence is unchanged. Long waits under an agent
  no longer fill its tool output with identical lines.

### Fixed

- **Back-to-back `submit`s keep their order.** `submit` returned before
  its detached runner had joined the queue, so a second `submit` could
  start first, and its estimate (and `jobs`) ignored the job just
  submitted. `submit` now returns once the runner is in the queue
  (typically under 0.1 s) and computes the estimate from the real queue.
  If the runner has not joined within 5 s, `submit` prints a warning and
  returns the id anyway.

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
