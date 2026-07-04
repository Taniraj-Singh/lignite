//! The write-ahead log.
//!
//! Before a batch is applied to the memtable it is appended here, so that a crash
//! can be recovered by replaying the log into a fresh memtable. The physical
//! format is LevelDB's: the log is a sequence of 32 KiB blocks, and each block
//! holds one or more *physical records*:
//!
//! ```text
//! record := checksum(u32) length(u16) type(u8) data[length]
//! ```
//!
//! A logical record that does not fit in the remainder of a block is split into
//! `First`/`Middle`/`Last` fragments; one that fits becomes a single `Full`
//! record. When fewer than seven bytes remain in a block they are zero-padded and
//! the next record starts in a fresh block. Each physical record's checksum
//! covers its type byte and payload, so torn or bit-rotted tails are detected on
//! replay.

pub mod reader;
pub mod writer;

pub use reader::LogReader;
pub use writer::LogWriter;

/// Size of a physical log block.
pub const BLOCK_SIZE: usize = 32 * 1024;

/// Bytes of per-record header: `checksum(4) + length(2) + type(1)`.
pub const HEADER_SIZE: usize = 4 + 2 + 1;

/// Fragment role of a physical record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordType {
    /// Zero byte: only seen as block padding, never a real record.
    Zero = 0,
    /// A complete logical record.
    Full = 1,
    /// The first fragment of a split record.
    First = 2,
    /// An interior fragment.
    Middle = 3,
    /// The final fragment.
    Last = 4,
}

impl RecordType {
    /// Map a raw type byte to a [`RecordType`].
    pub fn from_u8(v: u8) -> Option<RecordType> {
        match v {
            0 => Some(RecordType::Zero),
            1 => Some(RecordType::Full),
            2 => Some(RecordType::First),
            3 => Some(RecordType::Middle),
            4 => Some(RecordType::Last),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::reader::LogReader;
    use super::writer::LogWriter;
    use super::BLOCK_SIZE;

    #[test]
    fn roundtrip_small_records() {
        let mut w = LogWriter::new();
        let records: Vec<Vec<u8>> = vec![
            b"hello".to_vec(),
            b"".to_vec(),
            b"a slightly longer record".to_vec(),
            vec![7u8; 300],
        ];
        for r in &records {
            w.add_record(r);
        }
        let bytes = w.into_bytes();
        let mut reader = LogReader::new(&bytes, true);
        let mut got = Vec::new();
        while let Some(res) = reader.read_record() {
            got.push(res.unwrap());
        }
        assert_eq!(got, records);
    }

    #[test]
    fn record_spanning_multiple_blocks() {
        let mut w = LogWriter::new();
        // Much larger than a block, forcing First/Middle/Last fragmentation.
        let big = vec![0xabu8; BLOCK_SIZE * 3 + 1234];
        w.add_record(b"before");
        w.add_record(&big);
        w.add_record(b"after");
        let bytes = w.into_bytes();
        let mut reader = LogReader::new(&bytes, true);
        assert_eq!(reader.read_record().unwrap().unwrap(), b"before");
        assert_eq!(reader.read_record().unwrap().unwrap(), big);
        assert_eq!(reader.read_record().unwrap().unwrap(), b"after");
        assert!(reader.read_record().is_none());
    }

    #[test]
    fn corruption_is_detected() {
        let mut w = LogWriter::new();
        w.add_record(b"important data");
        let mut bytes = w.into_bytes();
        // Corrupt a payload byte (byte 10 is inside the first record's data).
        bytes[10] ^= 0xff;
        let mut reader = LogReader::new(&bytes, true);
        assert!(reader.read_record().unwrap().is_err());
    }

    #[test]
    fn many_records_boundary_fill() {
        let mut w = LogWriter::new();
        // Records sized to repeatedly land near block boundaries.
        let mut expected = Vec::new();
        for i in 0..500u32 {
            let r = vec![(i & 0xff) as u8; (i as usize % 200) + 1];
            w.add_record(&r);
            expected.push(r);
        }
        let bytes = w.into_bytes();
        let mut reader = LogReader::new(&bytes, true);
        let mut got = Vec::new();
        while let Some(res) = reader.read_record() {
            got.push(res.unwrap());
        }
        assert_eq!(got, expected);
    }
}
