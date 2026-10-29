use std::io::{self, BufRead, Read};

use crate::Decoder;

/// Decompresses the zstd frames read from `R`, one after the other like `zstd -d`
/// (skippable frames are skipped).
///
/// Input ending inside a frame is `ErrorKind::UnexpectedEof`, invalid input `ErrorKind::InvalidData`.
#[derive(Debug)]
pub struct Reader<R: BufRead> {
    inner: R,
    decoder: Decoder,
    // No frame is in progress: the input may end here.
    at_frame_boundary: bool,
}

impl<R: BufRead> Reader<R> {
    /// A reader of the frames in `inner`, with the default window limit (128 MiB).
    pub fn new(inner: R) -> Self {
        Self::with_decoder(Decoder::new(), inner)
    }

    /// A reader of the frames in `inner` that uses the settings of `decoder`
    /// (dictionary, window limit). A stream in progress in `decoder` is abandoned.
    pub const fn with_decoder(mut decoder: Decoder, inner: R) -> Self {
        decoder.stream.reset();
        Self {
            inner,
            decoder,
            at_frame_boundary: true,
        }
    }
}

impl<R: BufRead> Read for Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let input = self.inner.fill_buf()?;
            let progress = self.decoder.decompress_stream(input, buf)?;
            let at_end_of_input = input.is_empty();
            self.inner.consume(progress.consumed);
            if progress.consumed > 0 || progress.written > 0 {
                self.at_frame_boundary = progress.frame_done;
            }
            if progress.written > 0 {
                return Ok(progress.written);
            }
            if at_end_of_input {
                return if self.at_frame_boundary {
                    Ok(0)
                } else {
                    Err(io::ErrorKind::UnexpectedEof.into())
                };
            }
        }
    }
}
