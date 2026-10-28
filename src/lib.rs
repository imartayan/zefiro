#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;
#[cfg(any(feature = "std", test))]
extern crate std;

mod bits;
mod copy;
mod decoder;
mod error;
mod frame;
mod fse;
mod huf;
mod literals;
mod sequences;
#[cfg(test)]
mod testing;

pub use decoder::Decoder;
pub use error::Error;
pub use frame::frame_content_size;
