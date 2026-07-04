//! The crate-wide error type.
//!
//! Every fallible operation returns [`Result`]. The variants mirror the failure
//! classes a storage engine actually encounters: I/O faults from the filesystem,
//! corruption discovered while decoding a persisted structure, misuse of the API,
//! and the sentinel [`Error::NotFound`] used internally by lookups.

use std::fmt;

/// A specialised [`std::result::Result`] for storage-engine operations.
pub type Result<T> = std::result::Result<T, Error>;

/// All error conditions the engine can surface.
#[derive(Debug)]
pub enum Error {
    /// The requested key is not present. Callers usually map this to
    /// `Ok(None)`; it exists as an error so the internal lookup path can use
    /// the `?` operator uniformly.
    NotFound,
    /// A persisted structure failed to decode or failed its checksum.
    Corruption(String),
    /// The caller used the API in a way that cannot be honoured (for example a
    /// write batch that exceeds the configured limit).
    InvalidArgument(String),
    /// An operation was attempted on a database that has been closed.
    DbClosed,
    /// A feature or code path that is intentionally not implemented.
    Unsupported(String),
    /// An underlying I/O error, annotated with the operation that triggered it.
    Io {
        /// Human-readable context (e.g. the file being written).
        context: String,
        /// The originating error kind.
        kind: std::io::ErrorKind,
    },
}

impl Error {
    /// Construct a [`Error::Corruption`] from anything printable.
    pub fn corruption(msg: impl Into<String>) -> Self {
        Error::Corruption(msg.into())
    }

    /// Construct an [`Error::InvalidArgument`].
    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::InvalidArgument(msg.into())
    }

    /// Wrap a [`std::io::Error`] with contextual text.
    pub fn io(context: impl Into<String>, err: &std::io::Error) -> Self {
        Error::Io {
            context: context.into(),
            kind: err.kind(),
        }
    }

    /// True if this is the [`Error::NotFound`] sentinel.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::NotFound)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound => write!(f, "not found"),
            Error::Corruption(m) => write!(f, "corruption: {m}"),
            Error::InvalidArgument(m) => write!(f, "invalid argument: {m}"),
            Error::DbClosed => write!(f, "database is closed"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
            Error::Io { context, kind } => write!(f, "io error ({kind:?}) while {context}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io {
            context: "performing io".to_string(),
            kind: err.kind(),
        }
    }
}
