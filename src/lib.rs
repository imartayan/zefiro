#![no_std]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(feature = "std", doc = include_str!("../README.md"))]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::redundant_pub_crate)]
// The staged decoding code is for the threaded pipeline, which needs std.
#![cfg_attr(not(feature = "std"), allow(dead_code))]

extern crate alloc;
#[cfg(any(feature = "std", test))]
extern crate std;

mod bits;
mod copy;
mod decoder;
mod dict;
mod error;
mod frame;
mod fse;
mod huf;
mod literals;
#[cfg(feature = "std")]
mod parallel;
#[cfg(feature = "std")]
mod reader;
mod sequences;
mod stream;
#[cfg(test)]
mod testing;

pub use decoder::Decoder;
pub use dict::Dictionary;
pub use error::Error;
#[cfg(feature = "std")]
pub use reader::Reader;
pub use stream::Progress;
