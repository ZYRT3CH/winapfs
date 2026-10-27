use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// On-disk data failed a structural or checksum check.
    Corrupt(String),
    /// Valid APFS that uses a feature this crate does not handle yet.
    Unsupported(String),
    NotFound(String),
    /// A name that is already taken.
    Exists(String),
    /// A name that APFS cannot store.
    InvalidName(String),
    /// A folder that still has entries.
    NotEmpty(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Corrupt(m) => write!(f, "corrupt APFS data: {m}"),
            Error::Unsupported(m) => write!(f, "not supported yet: {m}"),
            Error::NotFound(m) => write!(f, "not found: {m}"),
            Error::Exists(m) => write!(f, "already exists: {m}"),
            Error::InvalidName(m) => write!(f, "invalid name: {m}"),
            Error::NotEmpty(m) => write!(f, "folder not empty: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub(crate) fn corrupt(msg: impl Into<String>) -> Error {
    Error::Corrupt(msg.into())
}

pub(crate) fn unsupported(msg: impl Into<String>) -> Error {
    Error::Unsupported(msg.into())
}
