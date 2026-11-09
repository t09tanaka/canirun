# Changelog

All notable changes to canirun are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/) (pre-1.0: a breaking change bumps
the minor version).

## [Unreleased]

### Fixed

- A failed descendant recorder (a panicked or unstarted thread, an
  unreadable running command) is "could not be checked", not "nothing
  recorded". A recorded descendant is dropped only after two looks in a row
  did not find it.
- macOS: a refused `KERN_PROCARGS2` is hidden only for `EINVAL`/`EPERM` on a
  process the kernel confirmed is ours; other errors make the scan unknown.
- A recorder failure is persisted to the sidecar and also stops the
  supervisor that saw it from releasing (`finish` keeps the entry); the
  reaper reads it as "could not be checked" until `resolve --force`.
- A hidden orphan only keeps an entry when it started from the child's start
  (the launch intent when unknown) to two seconds after the last recorder
  write (`recorded_at`), so processes `launchd` starts later no longer hold
  it. A hold that remains is expected; `resolve --force` is the way out.
- A command that ended unreaped (no readable start time) now has its live
  children recorded (the walk is seeded by pid).
- macOS: the command's start time is also read with `proc_pidinfo` when the
  process table does not list it (the short form has no start time, so only
  the full one is used); the retry stays for other platforms.
- macOS: if the kernel cannot tell the owner of a process of yours, a hidden
  process of yours is unreadable, the scan is unknown and the entry is held
  until `resolve --force`.
- A started entry whose child and group are gone is also kept when a hidden
  orphan began after the child (as for a launch intent).
- On macOS an entry can stay held (`ProbeUnknown`) because of a hidden
  process `launchd` started (an Apple binary); `canirun resolve <id> --force`
  releases it.

### Added

- Library: managed commands (`canirun::managed`). `register` takes a
  caller-chosen command id and spec digest (idempotent; a different digest
  is a conflict) and returns a registered receipt only once the ledger
  entry exists. `try_admit`, `run_command`, `finish` and `record_measurement`
  split the admission path into stages. Ownership lives in a sidecar
  (`managed/<id>.json`); ledger v1 is unchanged, so an older binary saving
  the ledger leaves it alone.
- The reaper no longer frees a managed entry just because its owner's lease
  is gone: it keeps the charge until the recorded child and its process
  group are gone, or the boot id changed. An unreadable sidecar is never
  treated as free. `inspect` lists such entries under `managed`. Plain
  `exec` entries are reaped as before.
- Managed commands carry `CANIRUN_MANAGED_ID=<id>` in their environment.
  After the leader exits, and before a managed entry is released, every
  process of the user still carrying the id is stopped (SIGTERM, then
  SIGKILL, only when pid and start time still match) and the entry is kept
  until none is left. This covers descendants that left the process group
  (`setsid`, double fork); one that cleared its environment is not found.
  A scan that cannot read every process never releases anything.
- A launch intent no longer blocks the shared budget forever: with the
  owner's lease gone, 60 seconds since the intent and no marked process, the
  reaper settles it as `Interrupted`. A missing timestamp or an unknown scan
  keeps it.
- `canirun resolve <id> [--json] [--force]` releases an entry whose owner is
  gone after the same checks, and says what it checked. `--force` also
  releases an entry kept only because something could not be checked
  (recorded as `interrupted`, `forced: <reason>`); never one known to be
  running. `status` lists held
  entries and `inspect` adds a `hint` to them.
- `history.jsonl` records written by `managed::record_measurement` carry a
  `measurement_id` (optional; older history still reads). A repeated
  measurement is a duplicate only when its id is in the history, never
  because another record has the same content.
- `canirun inspect --json`: a read-only, versioned snapshot of the ledger,
  limits, live metrics and referenced history, with snapshot and policy
  digests. It never reaps or writes; entries without a held lease are
  flagged `suspect_dead` and still counted.
- `canirun estimate --json -- <command>`: when would this command start and
  finish if queued now? Replays the ledger through `admit` with the same
  limits and queue rules. Unknown values are `null` with a reason, a fallback
  runtime is only ever a `ranking`, and overrunning entries have unknown
  remaining time.
- Library: `inspect`, `forecast` (WorkloadPlan with dependencies, external
  delays, shared setup, an overlay of future demand with de-duplication
  against the ledger, and `coverage`), `estimate::command_plan` and
  `lock::snapshot_read_only`. `Limits` and `LiveSnapshot` are now
  serializable. `error::Error` gained a `Plan` variant.

### Fixed

- Managed commands, hidden environments: on macOS the kernel withholds the
  environment of Apple's own binaries and hardened-runtime apps (about two
  thirds of one measured user's processes), and these were read as "no
  marker", so a `sh -c` command whose child left the process group went
  unseen and its budget was freed while it ran. Such a process is now
  *hidden*, kept apart from absent (and never makes a scan unknown). The
  runner also records the descendants of a running command (pid plus start
  time, read from the process table every 250 ms, at most 4096) in the
  sidecar; a recorded process that still runs keeps the budget and is
  stopped with the marked ones, and a record that overflowed keeps the entry
  as "could not be checked". A launch intent is now also kept while a hidden
  process that began after it is an orphan or the dead supervisor's child.
  Another user's processes are told apart by the kernel's owner (macOS:
  `proc_pidinfo`), not skipped by guesswork.
- Managed commands, review fixes: on macOS the arguments of a process were
  searched for the marker as well as its environment, so an argument could
  hide or fake it; only the environment is read now. The marker also
  carries `CANIRUN_MANAGED_SCOPE` (a hash of the state directory), and only
  a process with the same id and scope is waited for or signalled, so the
  same id in another state directory is left alone. A clear scan has to be
  repeated before it is believed, and an unknown one is retried after the
  signals too. A process whose owner cannot be read is read anyway
  instead of being skipped. Nested managed commands track the innermost one.
- `canirun resolve` on an id the ledger does not have says `no such entry`
  (exit 3, `"found": false` in `--json`) instead of "kept, still charged".
- `canirun gc` removes old `measurements/*.applied` markers (it keeps the
  newest `history_keep`); they used to pile up.
- A job that received SIGTERM twice in a row while queued could die before
  its exit file was written and show as lost. The library no longer restores
  default signal handling on its own; the CLI does it just before it
  re-raises.

## [0.4.2] - 2026-11-05

### Fixed

- **macOS: available memory no longer drops to 0 when much is compressed.**
  The memory guard counted free + inactive + purgeable pages minus the
  pages the compressor occupies, which are none of those. With a few tens
  of GiB compressed, `status` showed `machine 0 KiB available` and every
  command waited for `memory` while anything else ran, even though most
  memory was reclaimable. canirun now reads the page counts itself and no
  longer subtracts the compressor.

## [0.4.1] - 2026-11-04

### Added

- **Short commands no longer wait behind a head that does not fit.** When
  the head of the queue does not fit the free budget, a command behind it
  whose learned wall time is at most `backfill_max_job_secs` (new, default
  300; 0 restores strict FIFO) starts first if it fits. Commands with no
  history never pass anyone, and an entry ahead that will start is never
  overtaken. To keep the head from starving, nothing new passes an entry
  that backfilled commands are holding up once it is `--front` or has
  waited `backfill_max_wait_secs` (new, default 300); a head held up by
  something else, such as a development server that never exits, is
  passed for as long as that lasts. Estimates model the same rule.
- **The waiting line names who holds the budget.** A line waiting on the
  budget ends with `; held by <pid> w2 idle 52:30 <command>, ...` (the
  biggest three running entries). `status` marks running entries `idle`
  (running 5+ minutes, under 0.2 cores) and `[backfill]`, and says what
  a head waiting on the budget needs and how much is free. Idle entries
  are still charged their full weight: charging only measured use would
  under-count work running in a Docker daemon or VM, and an idle server
  can start a rebuild at any time (README, "Idle entries are charged
  their weight").

### Fixed

- **A busy ledger lock no longer fails a queued command.** On a saturated
  machine a holder of `ledger.lock` can stall for seconds, and every
  `exec` waiting on it gave up after 10 s with `ledger: timed out` and
  exit code 2, losing its place in the queue (one test with 40 waiters at
  load ~250 lost 39 of them). `exec` (and the runner behind `submit`) now
  skips that round and tries again, both when it joins the queue and
  while it waits, still answering signals and `cancel` within about a
  second. `status`, `jobs`, `wait`, `cancel` and `submit` read the ledger without
  the lock when it stays busy for a second (saves are atomic renames) and
  leave out entries whose lease is gone instead of failing; `wait` does
  not count such a read toward calling a job lost, since its runner may
  be waiting for the same lock to register. `gc` still fails after 10 s.
- **Fewer ledger writes while waiting.** The head of the queue no longer
  rewrites the ledger on every poll it is held back by the CPU or memory
  guard, and a running command rewrites its `using` value only when it
  changes, so the lock is held less often.
- The `front_jumps_the_queue` test no longer fails on a heavily loaded
  machine: its first job now holds the slot until the test releases it
  instead of sleeping for a fixed second.

## [0.4.0] - 2026-10-18

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

### Fixed

- **A history test no longer flakes when its run ends under the 1 s learning
  floor**: it now burns 1.2 s of user + sys CPU time per process.

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
