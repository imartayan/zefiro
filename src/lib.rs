#![no_std]
#![cfg_attr(feature = "std", doc = include_str!("../README.md"))]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::redundant_pub_crate)]

extern crate alloc;
#[cfg(any(feature = "std", test))]
extern crate std;

use alloc::vec;
use alloc::vec::Vec;

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
mod reader;
mod sequences;
mod stream;
#[cfg(test)]
mod testing;

pub use decoder::Decoder;
pub use dict::Dictionary;
pub use error::Error;
pub use frame::frame_content_size;
#[cfg(feature = "std")]
pub use reader::Reader;
pub use stream::Progress;

use frame::find_frame_size;

const VEC_LEN_START: usize = 1 << 16;

/// Decompresses every frame of `input` (skippable frames are skipped) into a `Vec` of at
/// most `max_len` bytes, else `DstSizeTooSmall`.
///
/// When every frame states its content size, the output is allocated at the sum of the
/// sizes and decoded like `ZSTD_decompress` ([`Decoder::decompress_into`]), so a size
/// above `max_len` is refused before allocating. Otherwise the frames are decoded like
/// [`Decoder::decompress_stream`] into a growing `Vec`, and input ending inside a frame is
/// `SrcSizeWrong`.
pub fn decompress(input: &[u8], max_len: usize) -> Result<Vec<u8>, Error> {
    let mut decoder = Decoder::new();
    if let Some(size) = total_content_size(input) {
        if size > max_len as u64 {
            return Err(Error::DstSizeTooSmall);
        }
        let mut output = vec![0; size as usize];
        let written = decoder.decompress_into(input, &mut output)?;
        output.truncate(written);
        return Ok(output);
    }
    let (mut input, mut output, mut len, mut frame_done) = (input, Vec::new(), 0, true);
    loop {
        if len == output.len() {
            // One byte more than `max_len` is room enough to see that it is exceeded.
            output.resize(
                (len * 2).max(VEC_LEN_START).min(max_len.saturating_add(1)),
                0,
            );
        }
        let progress = decoder.decompress_stream(input, &mut output[len..])?;
        len += progress.written;
        input = &input[progress.consumed..];
        if len > max_len {
            return Err(Error::DstSizeTooSmall);
        }
        if progress.consumed == 0 && progress.written == 0 {
            break;
        }
        frame_done = progress.frame_done;
    }
    if !frame_done || !input.is_empty() {
        return Err(Error::SrcSizeWrong);
    }
    output.truncate(len);
    Ok(output)
}

// The sum of the content sizes of the frames, if all are known and the input is
// made of whole frames.
fn total_content_size(mut input: &[u8]) -> Option<u64> {
    let mut total = 0u64;
    while !input.is_empty() {
        total = total.checked_add(frame_content_size(input).ok()??)?;
        input = &input[find_frame_size(input).ok()?..];
    }
    Some(total)
}
