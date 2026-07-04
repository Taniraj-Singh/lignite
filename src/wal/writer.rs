//! The log record writer.

use crate::util::coding::encode_fixed32;
use crate::util::crc32c;
use crate::wal::{RecordType, BLOCK_SIZE, HEADER_SIZE};

/// Appends length-delimited, checksummed records to an in-memory log buffer.
///
/// The buffer is a growable `Vec<u8>` standing in for the log file; the database
/// hands the finished bytes to its storage layer. Block alignment is tracked so
/// records never straddle a block boundary except as explicit fragments.
pub struct LogWriter {
    dest: Vec<u8>,
    block_offset: usize,
}

impl LogWriter {
    /// Create an empty writer.
    pub fn new() -> Self {
        LogWriter {
            dest: Vec::new(),
            block_offset: 0,
        }
    }

    /// Resume writing into an existing (block-aligned) buffer.
    pub fn from_existing(dest: Vec<u8>) -> Self {
        let block_offset = dest.len() % BLOCK_SIZE;
        LogWriter { dest, block_offset }
    }

    /// Current encoded length.
    pub fn len(&self) -> usize {
        self.dest.len()
    }

    /// Whether nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.dest.is_empty()
    }

    /// Consume the writer, yielding the log bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.dest
    }

    /// Borrow the log bytes so far.
    pub fn as_bytes(&self) -> &[u8] {
        &self.dest
    }

    /// Append one logical record, fragmenting it across blocks as needed.
    pub fn add_record(&mut self, data: &[u8]) {
        let mut left = data;
        let mut begin = true;
        loop {
            let leftover = BLOCK_SIZE - self.block_offset;
            debug_assert!(leftover < BLOCK_SIZE || self.block_offset == 0);
            if leftover < HEADER_SIZE {
                // Not enough room for even a header: pad and start a new block.
                if leftover > 0 {
                    let padded = self.dest.len() + leftover;
                    self.dest.resize(padded, 0);
                }
                self.block_offset = 0;
            }

            let available = BLOCK_SIZE - self.block_offset - HEADER_SIZE;
            let fragment_len = left.len().min(available);
            let end = fragment_len == left.len();
            let record_type = match (begin, end) {
                (true, true) => RecordType::Full,
                (true, false) => RecordType::First,
                (false, true) => RecordType::Last,
                (false, false) => RecordType::Middle,
            };
            self.emit_physical_record(record_type, &left[..fragment_len]);
            left = &left[fragment_len..];
            begin = false;
            if left.is_empty() {
                break;
            }
        }
    }

    fn emit_physical_record(&mut self, record_type: RecordType, data: &[u8]) {
        debug_assert!(data.len() <= 0xffff);
        debug_assert!(self.block_offset + HEADER_SIZE + data.len() <= BLOCK_SIZE);

        let type_byte = record_type as u8;
        let crc = crc32c::mask(crc32c::extend(crc32c::value(&[type_byte]), data));

        encode_fixed32(&mut self.dest, crc);
        let len = data.len() as u16;
        self.dest.push((len & 0xff) as u8);
        self.dest.push((len >> 8) as u8);
        self.dest.push(type_byte);
        self.dest.extend_from_slice(data);

        self.block_offset += HEADER_SIZE + data.len();
    }
}

impl Default for LogWriter {
    fn default() -> Self {
        LogWriter::new()
    }
}
