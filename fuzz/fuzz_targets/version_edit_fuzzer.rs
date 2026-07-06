//! Robustness fuzz target for the MANIFEST `VersionEdit` decoder.
//!
//! `VersionEdit::decode` parses the tagged-field record format the MANIFEST is
//! built from: varint tags, varint scalars, and varint-length-prefixed keys for
//! compaction pointers and per-file key bounds. Recovery replays these from disk,
//! so the decoder consumes untrusted bytes and must treat every input as hostile:
//! a truncated field, a bogus tag, or a lying length prefix must resolve to a
//! clean `Err`, never a panic, an over-read, or an unbounded allocation.
//!
//! This entry point was previously unfuzzed.
//!
//! Build/run: `cargo +nightly fuzz run version_edit_fuzzer`

#![no_main]

use libfuzzer_sys::fuzz_target;
use lignite::version::VersionEdit;

fuzz_target!(|data: &[u8]| {
    if let Ok(edit) = VersionEdit::decode(data) {
        // Touch the decoded collections so the work is not optimised away.
        let _ = edit.new_files.len();
        let _ = edit.deleted_files.len();
        let _ = edit.compact_pointers.len();
        let _ = edit.comparator.as_deref();
    }
});
