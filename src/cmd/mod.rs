//! The CLI adapter: one module per subcommand, over the `task-traffic-controller` library.

pub mod config;
pub mod estimate;
pub mod exec;
pub mod gc;
pub mod history;
pub mod inspect;
pub mod job;
pub mod resolve;
pub mod status;
