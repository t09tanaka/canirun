//! task-traffic-controller as a library: the single-host resource control behind the
//! `task-traffic-controller` command (admission against a shared budget, the ledger and its
//! leases, ETA estimates, learned weights, live metrics, measurement).
//!
//! **This is not a stable API.** The crate is 0.x and the public items
//! exist so that other programs in this workspace of tools can drive the
//! same machinery; any of them may change in a minor release.
//!
//! # Shape
//!
//! Nothing in the library reads `HOME`, the XDG variables, the working
//! directory or the process signal state, prints, or exits the process.
//! Everything of that kind arrives through a [`Context`] (where state
//! lives, the [`Clock`], the [`LiveProvider`], the [`Supervisor`] that sees
//! stop signals) and through [`exec::Opts`]. How a run ended comes back as
//! a value ([`exec::Gated`]); the CLI turns it into an exit code and
//! re-raises signals.
//!
//! # Running several commands from one process
//!
//! State is all on disk, guarded by file locks, so concurrent
//! [`exec::run_gated`] calls with the same state directory queue against
//! each other exactly like separate processes. What is *not* yet fully
//! thread-ready: signal forwarding to the child relies on `wait4` being
//! interrupted on the thread that receives the signal, so cancel from other
//! threads through [`exec::Hooks::cancelled`] (checked while queued) and
//! kill the child by pid; and the ledger lock timeout polling uses real time
//! rather than the context's clock.

pub mod admit;
pub mod config;
pub mod context;
pub mod display;
pub mod error;
pub mod estimate;
pub mod eta;
pub mod exec;
pub mod forecast;
pub mod gitctx;
pub mod history;
pub mod inspect;
pub mod jobs;
pub mod key;
pub mod ledger;
pub mod lineage;
pub mod live;
pub mod lock;
pub mod managed;
pub mod marker;
pub mod paths;
pub mod process;
pub mod reap;
pub mod receipt;
pub mod runner;
pub mod tracker;
pub mod usage;
pub mod weight;

pub use context::{
    BootIdProvider, Clock, Context, FakeClock, FixedBoot, FixedLive, FixedScan, LiveProvider,
    NoSupervisor, ProcessScan, Supervisor, SystemBoot, SystemClock, SystemLive, SystemScan,
};
pub use paths::Paths;
