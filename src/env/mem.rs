//! An in-memory [`Env`] implementation.
//!
//! Files are `Vec<u8>` buffers behind a single mutex. It is the default
//! environment for tests and for the current in-memory database build: fast,
//! deterministic and free of any real I/O, while still exercising the same
//! create / append / read / rename / remove operations a disk-backed environment
//! must support.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::env::Env;
use crate::error::{Error, Result};

/// A process-local, in-memory file namespace.
pub struct MemEnv {
    files: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemEnv {
    /// Create an empty environment.
    pub fn new() -> Self {
        MemEnv {
            files: Mutex::new(HashMap::new()),
        }
    }

    /// Number of files currently present.
    pub fn file_count(&self) -> usize {
        self.files.lock().unwrap().len()
    }

    /// Total bytes stored across all files.
    pub fn total_bytes(&self) -> usize {
        self.files.lock().unwrap().values().map(|v| v.len()).sum()
    }
}

impl Default for MemEnv {
    fn default() -> Self {
        MemEnv::new()
    }
}

impl Env for MemEnv {
    fn create(&self, name: &str) -> Result<()> {
        self.files.lock().unwrap().insert(name.to_string(), Vec::new());
        Ok(())
    }

    fn append(&self, name: &str, data: &[u8]) -> Result<()> {
        let mut files = self.files.lock().unwrap();
        files.entry(name.to_string()).or_default().extend_from_slice(data);
        Ok(())
    }

    fn read(&self, name: &str) -> Result<Vec<u8>> {
        let files = self.files.lock().unwrap();
        files
            .get(name)
            .cloned()
            .ok_or_else(|| Error::io(format!("reading {name}"), &not_found()))
    }

    fn read_at(&self, name: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        let files = self.files.lock().unwrap();
        let data = files
            .get(name)
            .ok_or_else(|| Error::io(format!("reading {name}"), &not_found()))?;
        let start = offset as usize;
        let end = start
            .checked_add(len)
            .filter(|&e| e <= data.len())
            .ok_or_else(|| Error::corruption(format!("read past end of {name}")))?;
        Ok(data[start..end].to_vec())
    }

    fn exists(&self, name: &str) -> bool {
        self.files.lock().unwrap().contains_key(name)
    }

    fn remove(&self, name: &str) -> Result<()> {
        let mut files = self.files.lock().unwrap();
        if files.remove(name).is_none() {
            return Err(Error::io(format!("removing {name}"), &not_found()));
        }
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let mut files = self.files.lock().unwrap();
        let data = files
            .remove(from)
            .ok_or_else(|| Error::io(format!("renaming {from}"), &not_found()))?;
        files.insert(to.to_string(), data);
        Ok(())
    }

    fn size(&self, name: &str) -> Result<u64> {
        let files = self.files.lock().unwrap();
        files
            .get(name)
            .map(|v| v.len() as u64)
            .ok_or_else(|| Error::io(format!("sizing {name}"), &not_found()))
    }

    fn list(&self) -> Vec<String> {
        self.files.lock().unwrap().keys().cloned().collect()
    }
}

fn not_found() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, "no such file")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_append_read() {
        let env = MemEnv::new();
        env.create("wal.log").unwrap();
        env.append("wal.log", b"hello ").unwrap();
        env.append("wal.log", b"world").unwrap();
        assert_eq!(env.read("wal.log").unwrap(), b"hello world");
        assert_eq!(env.size("wal.log").unwrap(), 11);
    }

    #[test]
    fn append_creates_missing_file() {
        let env = MemEnv::new();
        env.append("new", b"data").unwrap();
        assert!(env.exists("new"));
    }

    #[test]
    fn read_at_slices() {
        let env = MemEnv::new();
        env.append("f", b"0123456789").unwrap();
        assert_eq!(env.read_at("f", 3, 4).unwrap(), b"3456");
        assert!(env.read_at("f", 8, 5).is_err()); // past end
    }

    #[test]
    fn rename_and_remove() {
        let env = MemEnv::new();
        env.append("a", b"x").unwrap();
        env.rename("a", "b").unwrap();
        assert!(!env.exists("a"));
        assert!(env.exists("b"));
        env.remove("b").unwrap();
        assert!(!env.exists("b"));
        assert!(env.remove("b").is_err());
    }

    #[test]
    fn missing_file_reads_error() {
        let env = MemEnv::new();
        assert!(env.read("ghost").is_err());
        assert!(env.size("ghost").is_err());
    }

    #[test]
    fn list_and_counts() {
        let env = MemEnv::new();
        env.append("a", b"12").unwrap();
        env.append("b", b"345").unwrap();
        assert_eq!(env.file_count(), 2);
        assert_eq!(env.total_bytes(), 5);
        let mut names = env.list();
        names.sort();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn create_truncates() {
        let env = MemEnv::new();
        env.append("f", b"old-contents").unwrap();
        env.create("f").unwrap();
        assert_eq!(env.size("f").unwrap(), 0);
    }
}
