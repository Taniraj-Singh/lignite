//! The log record reader / replayer.

use crate::error::{Error, Result};
use crate::util::coding::decode_fixed32;
use crate::util::crc32c;
use crate::wal::{RecordType, BLOCK_SIZE, HEADER_SIZE};

/// Reassembles logical records from a log buffer, verifying checksums.
pub struct LogReader<'a> {
    src: &'a [u8],
    pos: usize,
    checksum: bool,
}

impl<'a> LogReader<'a> {
    /// Read from `src`; when `checksum` is set every record's CRC is verified.
    pub fn new(src: &'a [u8], checksum: bool) -> Self {
        LogReader {
            src,
            pos: 0,
            checksum,
        }
    }

    /// Read the next logical record.
    ///
    /// Returns `None` at a clean end of log, `Some(Ok(bytes))` for a record, or
    /// `Some(Err(..))` if corruption or an unexpected fragment sequence is found.
    pub fn read_record(&mut self) -> Option<Result<Vec<u8>>> {
        let mut result: Vec<u8> = Vec::new();
        let mut in_fragmented = false;
        loop {
            match self.read_physical_record() {
                PhysicalRecord::Eof => {
                    if in_fragmented {
                        return Some(Err(Error::corruption("log ended mid-record")));
                    }
                    return None;
                }
                PhysicalRecord::Error(e) => return Some(Err(e)),
                PhysicalRecord::Record(RecordType::Full, data) => {
                    if in_fragmented {
                        return Some(Err(Error::corruption("full record inside fragment")));
                    }
                    return Some(Ok(data));
                }
                PhysicalRecord::Record(RecordType::First, data) => {
                    if in_fragmented {
                        return Some(Err(Error::corruption("first record inside fragment")));
                    }
                    result = data;
                    in_fragmented = true;
                }
                PhysicalRecord::Record(RecordType::Middle, data) => {
                    if !in_fragmented {
                        return Some(Err(Error::corruption("middle record without first")));
                    }
                    result.extend_from_slice(&data);
                }
                PhysicalRecord::Record(RecordType::Last, data) => {
                    if !in_fragmented {
                        return Some(Err(Error::corruption("last record without first")));
                    }
                    result.extend_from_slice(&data);
                    return Some(Ok(result));
                }
                PhysicalRecord::Record(RecordType::Zero, _) => {
                    return Some(Err(Error::corruption("unexpected zero-type record")));
                }
            }
        }
    }

    fn read_physical_record(&mut self) -> PhysicalRecord {
        loop {
            let offset_in_block = self.pos % BLOCK_SIZE;
            let block_remaining = BLOCK_SIZE - offset_in_block;
            if block_remaining < HEADER_SIZE {
                // Trailing padding: skip to the next block boundary.
                if self.pos + block_remaining > self.src.len() {
                    return PhysicalRecord::Eof;
                }
                self.pos += block_remaining;
                continue;
            }
            if self.pos + HEADER_SIZE > self.src.len() {
                return PhysicalRecord::Eof;
            }

            let header = &self.src[self.pos..self.pos + HEADER_SIZE];
            let stored_crc = decode_fixed32(header).unwrap_or(0);
            let length = (header[4] as usize) | ((header[5] as usize) << 8);
            let type_byte = header[6];

            // A zero header signals block padding rather than a record.
            if stored_crc == 0 && length == 0 && type_byte == 0 {
                let remaining = BLOCK_SIZE - offset_in_block;
                self.pos += remaining;
                continue;
            }

            let data_start = self.pos + HEADER_SIZE;
            if data_start + length > self.src.len() {
                return PhysicalRecord::Error(Error::corruption("record length exceeds log"));
            }
            let data = self.src[data_start..data_start + length].to_vec();
            self.pos = data_start + length;

            let record_type = match RecordType::from_u8(type_byte) {
                Some(t) => t,
                None => {
                    return PhysicalRecord::Error(Error::corruption(format!(
                        "unknown log record type {type_byte}"
                    )))
                }
            };

            if self.checksum {
                let computed = crc32c::mask(crc32c::extend(crc32c::value(&[type_byte]), &data));
                if computed != stored_crc {
                    return PhysicalRecord::Error(Error::corruption("log record crc mismatch"));
                }
            }

            return PhysicalRecord::Record(record_type, data);
        }
    }
}

enum PhysicalRecord {
    Record(RecordType, Vec<u8>),
    Eof,
    Error(Error),
}
