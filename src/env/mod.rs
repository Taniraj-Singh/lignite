//! The storage environment abstraction.
//!
//! Every byte the engine persists — write-ahead logs, SSTable images, the
//! MANIFEST — flows through an [`Env`], which models a flat namespace of
//! append-only/readable files. Abstracting the filesystem this way lets the same
//! engine run against a real disk, an in-memory store for tests, or a fault
//! injector, without touching the LSM logic.
//!
//! This build ships the in-memory [`MemEnv`]. A POSIX-backed environment is the
//! natural next implementation; it must honour the same contract, in particular
//! that [`Env::append`] is atomic with respect to concurrent [`Env::read`]s of
//! the same file (readers see a prefix, never a torn write).

pub mod mem;

pub use mem::MemEnv;

use crate::error::Result;

/// A flat namespace of named byte files.
pub trait Env: Send + Sync {
    /// Create `name`, truncating it if it already exists.
    fn create(&self, name: &str) -> Result<()>;

    /// Append `data` to `name`, creating it if necessary.
    fn append(&self, name: &str, data: &[u8]) -> Result<()>;

    /// Read the entire contents of `name`.
    fn read(&self, name: &str) -> Result<Vec<u8>>;

    /// Read `len` bytes of `name` starting at `offset`.
    fn read_at(&self, name: &str, offset: u64, len: usize) -> Result<Vec<u8>>;

    /// Whether `name` exists.
    fn exists(&self, name: &str) -> bool;

    /// Remove `name`. Removing a missing file is an error.
    fn remove(&self, name: &str) -> Result<()>;

    /// Rename `from` to `to`, replacing `to` if present.
    fn rename(&self, from: &str, to: &str) -> Result<()>;

    /// Size of `name` in bytes.
    fn size(&self, name: &str) -> Result<u64>;

    /// List every file name currently present, in unspecified order.
    fn list(&self) -> Vec<String>;
}
