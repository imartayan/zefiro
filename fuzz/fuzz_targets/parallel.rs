#![no_main]

//! A thread count byte, a 2 byte output capacity, then arbitrary bytes, one-shot with
//! threads: ours must agree with libzstd.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| compare::fuzz::parallel(data));
