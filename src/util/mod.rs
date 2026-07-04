//! Low-level utilities shared across the storage engine: integer coding,
//! checksums, hashing and a small deterministic pseudo-random generator.

pub mod coding;
pub mod crc32c;
pub mod hash;
pub mod histogram;
pub mod random;

pub use coding::{
    decode_fixed32, decode_fixed64, decode_varint32, decode_varint64, encode_fixed32,
    encode_fixed64, put_length_prefixed_slice, put_varint32, put_varint64, take_length_prefixed_slice,
    varint_length,
};
pub use crc32c::{mask, unmask, value as crc32c};
pub use hash::hash;
pub use histogram::Histogram;
pub use random::Random;
