//! Robustness fuzz target for the write-ahead-log reader.
//!
//! It replays arbitrary bytes through `LogReader`, which must decode any input
//! without panicking: a corrupt or truncated record must surface as an `Err` (or
//! a clean end of log), never a crash or hang.
//!
//! Build/run: `cargo +nightly fuzz run wal_parse_fuzzer`

#![no_main]

use libfuzzer_sys::fuzz_target;
use lignite::wal::LogReader;

fuzz_target!(|data: &[u8]| {
    let mut reader = LogReader::new(data, true);
    let mut budget: u32 = 200_000;
    while budget > 0 {
        match reader.read_record() {
            Some(Ok(_record)) => {}
            _ => break, // Err or clean EOF: stop.
        }
        budget -= 1;
    }
});
