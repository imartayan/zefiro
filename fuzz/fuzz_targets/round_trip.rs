#![no_main]

//! Data compressed by libzstd with fuzzed parameters decodes to itself.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| compare::fuzz::round_trip(data));
