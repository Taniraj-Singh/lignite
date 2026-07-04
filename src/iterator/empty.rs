//! A permanently-invalid iterator.
//!
//! Useful as a placeholder for a level with no files, or as the degenerate child
//! of a merge, so callers never have to special-case "no source here".

use crate::error::{Error, Result};
use crate::iterator::InternalIterator;

/// An iterator that yields nothing.
///
/// It may optionally carry a deferred error, which lets a source that failed to
/// open still be handed to a merge as an (empty, erroring) child rather than
/// aborting the whole read.
pub struct EmptyIterator {
    error: Option<String>,
}

impl EmptyIterator {
    /// An empty, non-erroring iterator.
    pub fn new() -> Self {
        EmptyIterator { error: None }
    }

    /// An empty iterator that reports `err` from [`InternalIterator::status`].
    pub fn with_error(err: impl Into<String>) -> Self {
        EmptyIterator {
            error: Some(err.into()),
        }
    }
}

impl Default for EmptyIterator {
    fn default() -> Self {
        EmptyIterator::new()
    }
}

impl InternalIterator for EmptyIterator {
    fn valid(&self) -> bool {
        false
    }
    fn seek_to_first(&mut self) {}
    fn seek_to_last(&mut self) {}
    fn seek(&mut self, _target: &[u8]) {}
    fn next(&mut self) {}
    fn prev(&mut self) {}
    fn key(&self) -> &[u8] {
        &[]
    }
    fn value(&self) -> &[u8] {
        &[]
    }
    fn status(&self) -> Result<()> {
        match &self.error {
            Some(m) => Err(Error::corruption(m.clone())),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_invalid() {
        let mut it = EmptyIterator::new();
        it.seek_to_first();
        assert!(!it.valid());
        it.seek(b"anything");
        assert!(!it.valid());
        assert!(it.status().is_ok());
    }

    #[test]
    fn carries_error() {
        let it = EmptyIterator::with_error("open failed");
        assert!(it.status().is_err());
    }
}
