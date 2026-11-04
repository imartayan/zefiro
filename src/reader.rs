use alloc::boxed::Box;
use std::io::{self, BufRead, Read};

use crate::Decoder;
use crate::parallel::Streaming;

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
    // With several threads. First, so that the workers, which may read the decoder's ring,
    // are joined before it is freed.
    streaming: Option<Box<Streaming>>,
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
    pub fn with_decoder(mut decoder: Decoder, inner: R) -> Self {
        decoder.stream.reset();
        let streaming = (decoder.threads > 1).then(|| Box::new(Streaming::new(decoder.threads)));
        Self {
            streaming,
            inner,
            decoder,
            at_frame_boundary: true,
        }
    }

    #[cfg(test)]
    pub(crate) fn streaming_mut(&mut self) -> Option<&mut Streaming> {
        self.streaming.as_deref_mut()
    }
}

impl<R: BufRead> Read for Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut written = 0;
        loop {
            // The input read ahead for a frame in the pipeline, piece by piece.
            if let Some(streaming) = self.streaming.as_mut()
                && let Some((progress, returns)) =
                    streaming.step(&mut self.inner, &mut self.decoder, buf, written)?
            {
                written += progress.written;
                if progress.consumed > 0 || progress.written > 0 {
                    self.at_frame_boundary = progress.frame_done;
                }
                if written > 0 && returns {
                    return Ok(written);
                }
                continue;
            }
            let input = self.inner.fill_buf()?;
            if written == 0
                && let Some(streaming) = self.streaming.as_mut()
                && let Some(header_size) =
                    streaming.start_frame(&mut self.decoder, input, buf.len())?
            {
                self.inner.consume(header_size);
                self.at_frame_boundary = false;
                continue;
            }
            let progress = self.decoder.decompress_stream(input, &mut buf[written..])?;
            let at_end_of_input = input.is_empty();
            self.inner.consume(progress.consumed);
            if progress.consumed > 0 || progress.written > 0 {
                self.at_frame_boundary = progress.frame_done;
            }
            written += progress.written;
            if written > 0 {
                return Ok(written);
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
