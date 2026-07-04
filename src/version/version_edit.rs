//! `VersionEdit`: the delta records that make up the MANIFEST log.
//!
//! Every change to the set of live files — a flush adding a level-0 table, a
//! compaction deleting inputs and adding outputs, or an advance of the
//! next-file/last-sequence counters — is expressed as a [`VersionEdit`] and
//! appended (encoded) to the MANIFEST. Recovering a database replays the MANIFEST,
//! applying each edit in turn to rebuild the current [`Version`](super::Version).
//!
//! The encoding is a flat sequence of tagged fields; unknown tags are a
//! corruption. Optional scalar fields appear at most once; the file and pointer
//! collections may repeat.
//!
//! This codec is self-contained and fully tested. It is the persistence format
//! the engine's on-disk environment builds on; the in-memory environment used by
//! the current [`Db`](crate::Db) keeps the same information live rather than
//! serialising it, so the two representations must stay in agreement.

use std::collections::BTreeSet;

use crate::error::{Error, Result};
use crate::util::coding::{
    decode_varint32, decode_varint64, put_length_prefixed_slice, put_varint32, put_varint64,
    take_length_prefixed_slice,
};

// Field tags. Values match the LevelDB MANIFEST tags so the format is familiar.
const TAG_COMPARATOR: u32 = 1;
const TAG_LOG_NUMBER: u32 = 2;
const TAG_NEXT_FILE_NUMBER: u32 = 3;
const TAG_LAST_SEQUENCE: u32 = 4;
const TAG_COMPACT_POINTER: u32 = 5;
const TAG_DELETED_FILE: u32 = 6;
const TAG_NEW_FILE: u32 = 7;
const TAG_PREV_LOG_NUMBER: u32 = 9;

/// A compact description of one file referenced by the MANIFEST.
///
/// This intentionally omits the in-memory table image: the MANIFEST records
/// *which* files exist and their key bounds, while the bytes live in the storage
/// environment keyed by file number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditFile {
    /// Level the file lives at.
    pub level: usize,
    /// File number.
    pub number: u64,
    /// Encoded size in bytes.
    pub file_size: u64,
    /// Smallest internal key.
    pub smallest: Vec<u8>,
    /// Largest internal key.
    pub largest: Vec<u8>,
}

/// A batch of changes to apply to a version.
#[derive(Debug, Clone, Default)]
pub struct VersionEdit {
    /// Comparator name, set only in the first MANIFEST record.
    pub comparator: Option<String>,
    /// Log file number in force after this edit.
    pub log_number: Option<u64>,
    /// Previous log number (legacy; retained for format compatibility).
    pub prev_log_number: Option<u64>,
    /// Next unused file number.
    pub next_file_number: Option<u64>,
    /// Highest sequence number written.
    pub last_sequence: Option<u64>,
    /// Per-level compaction cursor keys.
    pub compact_pointers: Vec<(usize, Vec<u8>)>,
    /// Files removed by this edit, as `(level, number)`.
    pub deleted_files: BTreeSet<(usize, u64)>,
    /// Files added by this edit.
    pub new_files: Vec<EditFile>,
}

impl VersionEdit {
    /// A fresh, empty edit.
    pub fn new() -> Self {
        VersionEdit::default()
    }

    /// Reset to empty.
    pub fn clear(&mut self) {
        *self = VersionEdit::default();
    }

    /// Set the comparator name.
    pub fn set_comparator(&mut self, name: impl Into<String>) {
        self.comparator = Some(name.into());
    }

    /// Set the log number.
    pub fn set_log_number(&mut self, n: u64) {
        self.log_number = Some(n);
    }

    /// Set the next file number.
    pub fn set_next_file_number(&mut self, n: u64) {
        self.next_file_number = Some(n);
    }

    /// Set the last sequence number.
    pub fn set_last_sequence(&mut self, seq: u64) {
        self.last_sequence = Some(seq);
    }

    /// Record a compaction cursor for `level`.
    pub fn set_compact_pointer(&mut self, level: usize, key: Vec<u8>) {
        self.compact_pointers.push((level, key));
    }

    /// Mark a file removed.
    pub fn delete_file(&mut self, level: usize, number: u64) {
        self.deleted_files.insert((level, number));
    }

    /// Add a file at `level`.
    pub fn add_file(
        &mut self,
        level: usize,
        number: u64,
        file_size: u64,
        smallest: Vec<u8>,
        largest: Vec<u8>,
    ) {
        self.new_files.push(EditFile {
            level,
            number,
            file_size,
            smallest,
            largest,
        });
    }

    /// Serialise this edit.
    pub fn encode(&self) -> Vec<u8> {
        let mut dst = Vec::new();
        if let Some(name) = &self.comparator {
            put_varint32(&mut dst, TAG_COMPARATOR);
            put_length_prefixed_slice(&mut dst, name.as_bytes());
        }
        if let Some(n) = self.log_number {
            put_varint32(&mut dst, TAG_LOG_NUMBER);
            put_varint64(&mut dst, n);
        }
        if let Some(n) = self.prev_log_number {
            put_varint32(&mut dst, TAG_PREV_LOG_NUMBER);
            put_varint64(&mut dst, n);
        }
        if let Some(n) = self.next_file_number {
            put_varint32(&mut dst, TAG_NEXT_FILE_NUMBER);
            put_varint64(&mut dst, n);
        }
        if let Some(seq) = self.last_sequence {
            put_varint32(&mut dst, TAG_LAST_SEQUENCE);
            put_varint64(&mut dst, seq);
        }
        for (level, key) in &self.compact_pointers {
            put_varint32(&mut dst, TAG_COMPACT_POINTER);
            put_varint32(&mut dst, *level as u32);
            put_length_prefixed_slice(&mut dst, key);
        }
        for (level, number) in &self.deleted_files {
            put_varint32(&mut dst, TAG_DELETED_FILE);
            put_varint32(&mut dst, *level as u32);
            put_varint64(&mut dst, *number);
        }
        for f in &self.new_files {
            put_varint32(&mut dst, TAG_NEW_FILE);
            put_varint32(&mut dst, f.level as u32);
            put_varint64(&mut dst, f.number);
            put_varint64(&mut dst, f.file_size);
            put_length_prefixed_slice(&mut dst, &f.smallest);
            put_length_prefixed_slice(&mut dst, &f.largest);
        }
        dst
    }

    /// Parse an edit from its serialised form.
    pub fn decode(mut input: &[u8]) -> Result<VersionEdit> {
        let mut edit = VersionEdit::new();
        while !input.is_empty() {
            let (tag, rest) = decode_varint32(input)
                .ok_or_else(|| Error::corruption("version edit: truncated tag"))?;
            input = rest;
            match tag {
                TAG_COMPARATOR => {
                    let (name, rest) = take_lp(input, "comparator")?;
                    edit.comparator = Some(String::from_utf8_lossy(name).into_owned());
                    input = rest;
                }
                TAG_LOG_NUMBER => {
                    let (n, rest) = take_u64(input, "log_number")?;
                    edit.log_number = Some(n);
                    input = rest;
                }
                TAG_PREV_LOG_NUMBER => {
                    let (n, rest) = take_u64(input, "prev_log_number")?;
                    edit.prev_log_number = Some(n);
                    input = rest;
                }
                TAG_NEXT_FILE_NUMBER => {
                    let (n, rest) = take_u64(input, "next_file_number")?;
                    edit.next_file_number = Some(n);
                    input = rest;
                }
                TAG_LAST_SEQUENCE => {
                    let (n, rest) = take_u64(input, "last_sequence")?;
                    edit.last_sequence = Some(n);
                    input = rest;
                }
                TAG_COMPACT_POINTER => {
                    let (level, rest) = take_u32(input, "compact_pointer level")?;
                    let (key, rest) = take_lp(rest, "compact_pointer key")?;
                    edit.compact_pointers.push((level as usize, key.to_vec()));
                    input = rest;
                }
                TAG_DELETED_FILE => {
                    let (level, rest) = take_u32(input, "deleted level")?;
                    let (number, rest) = take_u64(rest, "deleted number")?;
                    edit.deleted_files.insert((level as usize, number));
                    input = rest;
                }
                TAG_NEW_FILE => {
                    let (level, rest) = take_u32(input, "new file level")?;
                    let (number, rest) = take_u64(rest, "new file number")?;
                    let (file_size, rest) = take_u64(rest, "new file size")?;
                    let (smallest, rest) = take_lp(rest, "new file smallest")?;
                    let (largest, rest) = take_lp(rest, "new file largest")?;
                    edit.new_files.push(EditFile {
                        level: level as usize,
                        number,
                        file_size,
                        smallest: smallest.to_vec(),
                        largest: largest.to_vec(),
                    });
                    input = rest;
                }
                other => {
                    return Err(Error::corruption(format!(
                        "version edit: unknown tag {other}"
                    )));
                }
            }
        }
        Ok(edit)
    }
}

fn take_u32<'a>(input: &'a [u8], what: &str) -> Result<(u32, &'a [u8])> {
    decode_varint32(input).ok_or_else(|| Error::corruption(format!("version edit: bad {what}")))
}

fn take_u64<'a>(input: &'a [u8], what: &str) -> Result<(u64, &'a [u8])> {
    decode_varint64(input).ok_or_else(|| Error::corruption(format!("version edit: bad {what}")))
}

fn take_lp<'a>(input: &'a [u8], what: &str) -> Result<(&'a [u8], &'a [u8])> {
    take_length_prefixed_slice(input)
        .ok_or_else(|| Error::corruption(format!("version edit: bad {what}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_edit_roundtrips() {
        let e = VersionEdit::new();
        let bytes = e.encode();
        let decoded = VersionEdit::decode(&bytes).unwrap();
        assert!(decoded.comparator.is_none());
        assert!(decoded.new_files.is_empty());
        assert!(decoded.deleted_files.is_empty());
    }

    #[test]
    fn scalar_fields_roundtrip() {
        let mut e = VersionEdit::new();
        e.set_comparator("lignite.BytewiseComparator");
        e.set_log_number(11);
        e.set_next_file_number(23);
        e.set_last_sequence(9001);
        e.prev_log_number = Some(7);
        let decoded = VersionEdit::decode(&e.encode()).unwrap();
        assert_eq!(decoded.comparator.as_deref(), Some("lignite.BytewiseComparator"));
        assert_eq!(decoded.log_number, Some(11));
        assert_eq!(decoded.prev_log_number, Some(7));
        assert_eq!(decoded.next_file_number, Some(23));
        assert_eq!(decoded.last_sequence, Some(9001));
    }

    #[test]
    fn files_and_pointers_roundtrip() {
        let mut e = VersionEdit::new();
        e.add_file(0, 100, 4096, b"aaaa\x00\x00\x00\x00\x00\x00\x00\x01".to_vec(), b"zzzz\x00\x00\x00\x00\x00\x00\x00\x01".to_vec());
        e.add_file(1, 101, 8192, b"m".to_vec(), b"q".to_vec());
        e.delete_file(2, 55);
        e.delete_file(2, 56);
        e.set_compact_pointer(3, b"cursor-key".to_vec());

        let decoded = VersionEdit::decode(&e.encode()).unwrap();
        assert_eq!(decoded.new_files.len(), 2);
        assert_eq!(decoded.new_files[0].number, 100);
        assert_eq!(decoded.new_files[0].file_size, 4096);
        assert_eq!(decoded.new_files[1].level, 1);
        assert!(decoded.deleted_files.contains(&(2, 55)));
        assert!(decoded.deleted_files.contains(&(2, 56)));
        assert_eq!(decoded.compact_pointers.len(), 1);
        assert_eq!(decoded.compact_pointers[0].0, 3);
        assert_eq!(decoded.compact_pointers[0].1, b"cursor-key");
    }

    #[test]
    fn unknown_tag_is_corruption() {
        // Tag 42 is not defined.
        let mut bytes = Vec::new();
        put_varint32(&mut bytes, 42);
        assert!(VersionEdit::decode(&bytes).is_err());
    }

    #[test]
    fn truncated_payload_is_corruption() {
        let mut e = VersionEdit::new();
        e.set_last_sequence(1234);
        let mut bytes = e.encode();
        bytes.pop(); // truncate the varint payload
        assert!(VersionEdit::decode(&bytes).is_err());
    }
}
