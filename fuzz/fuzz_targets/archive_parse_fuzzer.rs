//! Robustness fuzz target for the archive reader.
//!
//! It feeds arbitrary bytes to `ArchiveReader::parse`. The archive format is a
//! primary interchange surface — backups and cross-instance transfers arrive as
//! opaque byte strings — so the parser must treat every input as hostile: a
//! malformed header, a lying length field, a corrupt checksum, a truncated
//! record, or a bogus compressed body must all resolve to a clean `Err`, never a
//! panic, an over-read, or an unbounded allocation.
//!
//! As a stronger check, any archive that *does* parse is re-serialised and
//! re-parsed; the second parse must yield the same records, exercising the
//! writer/reader round-trip on decoder-reachable inputs.
//!
//! Build/run: `cargo +nightly fuzz run archive_parse_fuzzer`

#![no_main]

use libfuzzer_sys::fuzz_target;
use lignite::archive::{ArchiveReader, ArchiveWriter};

fuzz_target!(|data: &[u8]| {
    if let Ok(records) = ArchiveReader::parse(data) {
        // Re-serialise and re-parse: a decodable archive must round-trip.
        let mut w = ArchiveWriter::new();
        for r in &records {
            w.add(r);
        }
        let bytes = w.finish();
        let again = ArchiveReader::parse(&bytes).expect("re-serialised archive parses");
        assert_eq!(again, records, "archive round-trip mismatch");
    }
});
