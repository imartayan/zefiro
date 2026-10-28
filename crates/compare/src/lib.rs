//! Oracle (C libzstd through `zstd-sys`), corpora and compression helpers for
//! differential tests, fuzzing and benchmarks.

pub mod bench;
mod compress;
mod corpus;
mod craft;
pub mod fuzz;
mod oracle;
mod ours;

pub use compress::{LiteralMode, Params, compress, compress_stream, skippable_frame};
pub use corpus::{Sample, corpus, synthetic_corpus, test_corpus};
pub use craft::*;
pub use fastrand::Rng;
pub use oracle::{Code, Libzstd, Outcome, assert_agree, check_oracle_build};
pub use ours::decompress_ours;
