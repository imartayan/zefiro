#![no_main]

//! Arbitrary bytes, streamed in chunks whose sizes and window limit come from the first
//! five bytes: ours must agree with libzstd on every call.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| compare::fuzz::stream(data));
