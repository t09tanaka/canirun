use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Config(String),
    Ledger(String),
    Spawn(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Config(m) => write!(f, "config: {m}"),
            Error::Ledger(m) => write!(f, "ledger: {m}"),
            Error::Spawn(m) => write!(f, "{m}"),
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
