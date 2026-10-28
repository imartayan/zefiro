#![no_main]

//! A libzstd frame with a few fuzzed byte changes: same verdict as libzstd.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| compare::fuzz::mutate(data));
