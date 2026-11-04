use std::io::{self, BufRead, Read};

use crate::Decoder;

/// Decompresses the zstd frames read from `R`, one after the other like `zstd -d`
/// (skippable frames are skipped).
///
/// With a decoder set to several threads ([`Decoder::set_threads`]), a frame whose content size
/// is unknown or large is decoded by the in-frame pipeline: the input is read ahead by up to
/// a few blocks per thread, and the output is the same as with one thread (the only thing
/// that can differ is when `R` is read, and that a failing `R` is met later).
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
    /// (dictionary, window limit, threads). A stream in progress in `decoder` is abandoned.
    pub const fn with_decoder(mut decoder: Decoder, inner: R) -> Self {
        decoder.reset();
        Self {
            inner,
            decoder,
            at_frame_boundary: true,
        }
    }

    #[cfg(test)]
    pub(crate) fn streaming(&self) -> Option<&crate::parallel::Streaming> {
        self.decoder.streaming.as_deref()
    }
}

impl<R: BufRead> Read for Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let input = match self.inner.fill_buf() {
                Ok(input) => input,
                // What the threads read ahead is decoded before the error is reported.
                Err(error) if self.decoder.threads > 1 => {
                    let progress = self.decoder.decompress_stream(&[], buf)?;
                    return if progress.written > 0 {
                        Ok(progress.written)
                    } else {
                        Err(error)
                    };
                }
                Err(error) => return Err(error),
            };
            let progress = self.decoder.decompress_stream(input, buf)?;
            let at_end_of_input = input.is_empty();
            self.inner.consume(progress.consumed);
            if !at_end_of_input {
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
