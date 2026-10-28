#![no_main]

//! A dictionary and a frame (arbitrary, or made with the dictionary and mutated), decoded
//! one-shot and streamed: ours must agree with libzstd, loading included.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| compare::fuzz::dict(data));
