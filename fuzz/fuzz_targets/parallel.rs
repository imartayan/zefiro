#![no_main]

//! A byte (threads, piece sizes), a 2 byte output capacity, then arbitrary bytes: one-shot with
//! threads must agree with libzstd, and `decompress_stream` and `Reader` with threads, fed in
//! pieces, with the serial stream.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| compare::fuzz::parallel(data));
