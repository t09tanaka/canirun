use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Config(String),
    Ledger(String),
    /// The ledger lock stayed busy for the whole timeout.
    LockTimeout(String),
    Spawn(String),
    /// A workload plan that cannot be simulated (duplicate or unknown node
    /// ids, a dependency cycle).
    Plan(String),
    /// The same command id was registered with a different spec digest, or
    /// an id clashes with an entry that is not managed.
    Conflict(String),
    /// Another process holds the lease for this command id.
    AlreadyOwned(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Config(m) => write!(f, "config: {m}"),
            Error::Ledger(m) | Error::LockTimeout(m) => write!(f, "ledger: {m}"),
            Error::Spawn(m) => write!(f, "{m}"),
            Error::Plan(m) => write!(f, "plan: {m}"),
            Error::Conflict(m) => write!(f, "conflict: {m}"),
            Error::AlreadyOwned(m) => write!(f, "already owned: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Ledger(e.to_string())
    }
}
