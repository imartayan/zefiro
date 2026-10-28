#![no_main]

//! Arbitrary bytes, one-shot, into a buffer whose size comes from the first
//! two bytes: ours must agree with libzstd.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| compare::fuzz::decompress(data));
