//! Oracle (C libzstd through `zstd-sys`), corpora and compression helpers for
//! differential tests, fuzzing and benchmarks.

pub mod bench;
mod compress;
mod corpus;
mod craft;
mod dictionary;
pub mod fuzz;
mod oracle;
mod ours;
mod stream;

pub use compress::{
    LiteralMode, Params, compress, compress_stream, compress_with_dictionary, skippable_frame,
    train_dictionary,
};
pub use corpus::{Sample, corpus, synthetic_corpus, test_corpus};
pub use craft::*;
pub use dictionary::compare_dictionary;
pub use fastrand::Rng;
pub use oracle::{Code, Libzstd, Outcome, assert_agree, check_oracle_build};
pub use ours::decompress_ours;
pub use stream::{
    Run, Step, compare_stream, compare_stream_dictionary, run_stream, stream_ours,
    stream_ours_dictionary, stream_theirs, stream_theirs_dictionary,
};
