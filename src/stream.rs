//! `ZSTD_decompressStream`: the same stages, buffers and quirks. Blocks are decoded into a
//! ring (libzstd's size and wrap rule) and flushed to the caller's output; when the frame
//! is whole in the input and fits the output it is decoded in one pass.

use alloc::vec;
use alloc::vec::Vec;
use core::hash::Hasher;
use core::{mem, ptr};

use crate::copy::WILDCOPY_OVERLENGTH;
use crate::decoder::{BlockDecoder, Decoder, History, SerialBlocks};
use crate::error::Error;
use crate::frame::{
    BLOCK_HEADER_SIZE, BlockType, FrameHeader, FrameStart, find_frame_size, parse_block_header,
    parse_frame_start, read_le32,
};
use crate::sequences::{Ext, Output};

pub(crate) const FRAME_HEADER_SIZE_MAX: usize = 18;
const CHECKSUM_SIZE: usize = 4;
const WINDOW_SIZE_MIN: u64 = 1 << 10;
const WINDOW_LOG_MIN: u32 = 10;
const WINDOW_LOG_MAX: u32 = 31;
const WINDOW_LOG_DEFAULT: u32 = 27;
// libzstd shrinks its buffers once they were over `OVERSIZED_FACTOR` times too large for
// `OVERSIZED_FRAMES_MAX` frames in a row.
const OVERSIZED_FACTOR: u64 = 3;
const OVERSIZED_FRAMES_MAX: u32 = 128;

/// What one [`Decoder::decompress_stream`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Bytes of the input used. Once a frame is complete but its output not fully
    /// written, the last input byte is held back and must be passed again.
    pub consumed: usize,
    /// Bytes written to the output.
    pub written: usize,
    /// A frame is complete and all its output written.
    pub frame_done: bool,
}

// `ZSTD_DStream`'s `streamStage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Init,
    LoadHeader,
    Read,
    Load,
    Flush,
}

// `ZSTD_DCtx`'s `stage`, what the next `expected` input bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    BlockHeader,
    Block,
    LastBlock,
    Checksum,
    SkipFrame,
    End,
}

#[derive(Debug, Clone)]
pub(crate) struct Stream {
    phase: Phase,
    stage: Stage,
    expected: usize,
    max_window_size: u64,
    header: [u8; FRAME_HEADER_SIZE_MAX],
    header_len: usize,
    // Frame being decoded (`fParams`).
    content_size: Option<u64>,
    block_size_max: usize,
    checksum: bool,
    decoded_size: u64,
    block_type: BlockType,
    rle_size: usize,
    // A block arriving in pieces is gathered in `block`, `block_loaded` bytes so far.
    block: Vec<u8>,
    block_loaded: usize,
    // `ring[..ring_len]`: `out_start..out_end` is decoded and not yet flushed. The segment
    // being written starts at 0; `ext_len` bytes of the previous one are history. Like
    // libzstd's buffers, `block_buffer_len` and `ring_len` only grow, except when they are
    // oversized for long (a verdict depends on the room left in the ring).
    ring: Vec<u8>,
    ring_len: usize,
    block_buffer_len: usize,
    oversized_frames: u32,
    out_start: usize,
    out_end: usize,
    segment_end: usize,
    ext_len: usize,
    // The history before the segment is the dictionary, until the ring restarts.
    ext_dict: bool,
    // The last input byte of a finished frame is kept until its output is flushed.
    hostage: bool,
}

impl Stream {
    pub(crate) const fn new() -> Self {
        Self {
            phase: Phase::Init,
            stage: Stage::End,
            expected: 0,
            max_window_size: 1 << WINDOW_LOG_DEFAULT,
            header: [0; FRAME_HEADER_SIZE_MAX],
            header_len: 0,
            content_size: None,
            block_size_max: 0,
            checksum: false,
            decoded_size: 0,
            block_type: BlockType::Raw,
            rle_size: 0,
            block: Vec::new(),
            block_loaded: 0,
            ring: Vec::new(),
            ring_len: 0,
            block_buffer_len: 0,
            oversized_frames: 0,
            out_start: 0,
            out_end: 0,
            segment_end: 0,
            ext_len: 0,
            ext_dict: false,
            hostage: false,
        }
    }

    pub(crate) const fn reset(&mut self) {
        self.phase = Phase::Init;
    }

    // Between frames: the next call starts with a frame header.
    #[cfg(feature = "std")]
    pub(crate) const fn at_frame_start(&self) -> bool {
        matches!(self.phase, Phase::Init)
    }

    // The part of a frame header held so far, if the stream is at a frame header.
    #[cfg(feature = "std")]
    pub(crate) fn header_held(&self) -> Option<&[u8]> {
        match self.phase {
            Phase::Init => Some(&[]),
            Phase::LoadHeader => Some(&self.header[..self.header_len]),
            _ => None,
        }
    }

    // Decoded bytes wait for room in the caller's output.
    #[cfg(feature = "std")]
    pub(crate) const fn is_flushing(&self) -> bool {
        matches!(self.phase, Phase::Flush)
    }

    #[cfg(feature = "std")]
    pub(crate) const fn block_size_max(&self) -> usize {
        self.block_size_max
    }

    // The window check and buffer sizing of `zdss_loadHeader` (a skippable frame has no
    // window and no blocks).
    fn begin_frame(&mut self, header: &FrameHeader, has_dictionary: bool) -> Result<(), Error> {
        let window_size = header.window_size.max(WINDOW_SIZE_MIN);
        if window_size > self.max_window_size {
            return Err(Error::WindowTooLarge);
        }
        let block_size_max = header.block_size_max as usize;
        self.adjust_buffers(window_size, block_size_max, header.content_size)?;
        self.content_size = header.content_size;
        self.block_size_max = block_size_max;
        self.checksum = header.checksum;
        self.decoded_size = 0;
        self.segment_end = 0;
        self.ext_len = 0;
        self.ext_dict = has_dictionary;
        self.stage = Stage::BlockHeader;
        self.expected = BLOCK_HEADER_SIZE;
        Ok(())
    }

    fn begin_skippable_frame(&mut self, content_size: u32) -> Result<(), Error> {
        self.adjust_buffers(WINDOW_SIZE_MIN, 0, Some(u64::from(content_size)))?;
        self.content_size = Some(u64::from(content_size));
        self.block_size_max = 0;
        self.stage = Stage::SkipFrame;
        self.expected = content_size as usize;
        Ok(())
    }

    // The ring as raw memory: a pipeline may hash decoded blocks while later ones are written,
    // so a `&mut` must not cover the whole ring.
    const fn output(&mut self) -> Output {
        Output {
            base: self.ring.as_mut_ptr(),
            len: self.ring_len,
        }
    }

    // `ZSTD_decodingBufferSize_internal` and the (re)allocation rule.
    fn adjust_buffers(
        &mut self,
        window_size: u64,
        block_size_max: usize,
        content_size: Option<u64>,
    ) -> Result<(), Error> {
        let block_len = block_size_max.max(CHECKSUM_SIZE);
        let ring_size = window_size + 2 * (block_size_max + WILDCOPY_OVERLENGTH) as u64;
        let ring_len = usize::try_from(ring_size.min(content_size.unwrap_or(u64::MAX)))
            .map_err(|_| Error::WindowTooLarge)?;
        let oversized = self.block_buffer_len as u64 + self.ring_len as u64
            >= (block_len as u64 + ring_len as u64) * OVERSIZED_FACTOR;
        self.oversized_frames = if oversized {
            self.oversized_frames + 1
        } else {
            0
        };
        if self.block_buffer_len < block_len
            || self.ring_len < ring_len
            || self.oversized_frames >= OVERSIZED_FRAMES_MAX
        {
            (self.block_buffer_len, self.ring_len) = (block_len, ring_len);
            if self.block.len() < block_len {
                self.block = vec![0; block_len];
            }
            if self.ring.len() < ring_len {
                self.ring = vec![0; ring_len];
            }
        }
        Ok(())
    }

    // `ZSTD_nextSrcSizeToDecompressWithInputSize`: raw blocks can be taken in pieces.
    fn next_input_size(&self, available: usize) -> usize {
        let is_block = matches!(self.stage, Stage::Block | Stage::LastBlock);
        if is_block && self.block_type == BlockType::Raw {
            available.clamp(1, self.expected)
        } else {
            self.expected
        }
    }
}

impl Decoder {
    /// Decodes a piece of a stream of frames like `ZSTD_decompressStream` with
    /// `ZSTD_inBuffer { input, 0 }` and `ZSTD_outBuffer { output, 0 }`: same output, errors,
    /// and input and output positions after every call, for any chunking.
    ///
    /// A frame whose content size is known and whole in `input` is decoded in one pass
    /// when `output` can hold it all (with the checks of [`Decoder::decompress_into`]).
    /// Frames with a window above the limit of [`Decoder::set_window_log_max`] are
    /// rejected. After an error the stream restarts at the next frame header.
    ///
    /// With several threads ([`Decoder::set_threads`]) the output and the error are the serial
    /// ones, but the positions are not: a call takes input ahead (what it copied counts as
    /// consumed) and writes what is decoded by then, so output may lag behind the input
    /// consumed. After the last input, call with an empty `input` until a call writes nothing
    /// (one that completes a frame may consume and write nothing). Bytes decoded before an
    /// error come out in calls before the one reporting it. How a damaged frame fails after raw
    /// blocks may differ from a serial run, as it does between chunkings (raw blocks are taken
    /// in pieces).
    pub fn decompress_stream(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<Progress, Error> {
        #[cfg(feature = "std")]
        if self.threads > 1 {
            return self.decompress_stream_threaded(input, output);
        }
        self.decompress_stream_with(input, output, &mut SerialBlocks)
    }

    // `decompress_stream` with `blocks` decoding the compressed blocks.
    pub(crate) fn decompress_stream_with<B: BlockDecoder>(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        blocks: &mut B,
    ) -> Result<Progress, Error> {
        let result = self.stream_calls(input, output, blocks);
        if result.is_err() {
            self.stream.reset();
        }
        result
    }

    /// Sets the largest window `decompress_stream` accepts to `1 << log` bytes
    /// (`ZSTD_d_windowLogMax`): 0 is the default, 27 (128 MiB), other values are
    /// clamped to 10..=31.
    pub fn set_window_log_max(&mut self, log: u32) {
        let log = if log == 0 {
            WINDOW_LOG_DEFAULT
        } else {
            log.clamp(WINDOW_LOG_MIN, WINDOW_LOG_MAX)
        };
        self.stream.max_window_size = 1 << log;
    }

    fn stream_calls<B: BlockDecoder>(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        blocks: &mut B,
    ) -> Result<Progress, Error> {
        let (mut ip, mut op) = (0, 0);
        loop {
            match self.stream.phase {
                Phase::Init => {
                    let stream = &mut self.stream;
                    (stream.header_len, stream.block_loaded) = (0, 0);
                    (stream.out_start, stream.out_end) = (0, 0);
                    stream.hostage = false;
                    stream.phase = Phase::LoadHeader;
                }
                Phase::LoadHeader => {
                    let stream = &mut self.stream;
                    let start = match parse_frame_start(&stream.header[..stream.header_len])? {
                        FrameStart::NeedMore(size) => {
                            let to_load = size - stream.header_len;
                            let loaded = to_load.min(input.len() - ip);
                            let at = stream.header_len;
                            stream.header[at..at + loaded].copy_from_slice(&input[ip..ip + loaded]);
                            stream.header_len += loaded;
                            ip += loaded;
                            if loaded < to_load {
                                // Early errors: a prefix that cannot become a magic number.
                                parse_frame_start(&stream.header[..stream.header_len])?;
                                return Ok(Progress {
                                    consumed: input.len(),
                                    written: 0,
                                    frame_done: false,
                                });
                            }
                            continue;
                        }
                        start => start,
                    };
                    if let FrameStart::Frame(header) = start
                        && header
                            .content_size
                            .is_some_and(|size| output.len() as u64 >= size)
                        && let Ok(frame_size) = find_frame_size(input)
                    {
                        let written = self.decompress_into(&input[..frame_size], output)?;
                        self.stream.expected = 0;
                        self.stream.phase = Phase::Init;
                        return Ok(Progress {
                            consumed: frame_size,
                            written,
                            frame_done: true,
                        });
                    }
                    self.begin_frame();
                    match start {
                        FrameStart::Frame(header) => {
                            self.check_frame_header(&header)?;
                            self.stream
                                .begin_frame(&header, self.dictionary.is_some())?;
                        }
                        FrameStart::Skippable { content_size } => {
                            self.stream.begin_skippable_frame(content_size)?;
                        }
                        FrameStart::NeedMore(_) => unreachable!(),
                    }
                    self.stream.phase = Phase::Read;
                }
                Phase::Read => {
                    let available = input.len() - ip;
                    let needed = self.stream.next_input_size(available);
                    if needed == 0 {
                        self.stream.phase = Phase::Init;
                        break;
                    }
                    if available >= needed {
                        self.decompress_continue_stream(&input[ip..ip + needed], blocks)?;
                        ip += needed;
                    } else if available == 0 {
                        break;
                    } else {
                        self.stream.phase = Phase::Load;
                    }
                }
                Phase::Load => {
                    let stream = &mut self.stream;
                    let skipping = stream.stage == Stage::SkipFrame;
                    let to_load = stream.expected - stream.block_loaded;
                    let loaded = to_load.min(input.len() - ip);
                    if !skipping {
                        let at = stream.block_loaded;
                        stream.block[at..at + loaded].copy_from_slice(&input[ip..ip + loaded]);
                    }
                    ip += loaded;
                    stream.block_loaded += loaded;
                    if loaded < to_load {
                        break;
                    }
                    let block = mem::take(&mut stream.block);
                    stream.block_loaded = 0;
                    let size = if skipping { 0 } else { stream.expected };
                    let result = self.decompress_continue_stream(&block[..size], blocks);
                    self.stream.block = block;
                    result?;
                }
                Phase::Flush => {
                    let stream = &mut self.stream;
                    let pending = stream.out_end - stream.out_start;
                    let flushed = pending.min(output.len() - op);
                    let from = stream.out_start;
                    output[op..op + flushed].copy_from_slice(&stream.ring[from..from + flushed]);
                    op += flushed;
                    stream.out_start += flushed;
                    if flushed < pending {
                        break;
                    }
                    stream.phase = Phase::Read;
                    // Start over at the beginning of the ring when no block fits after.
                    let ring_is_small =
                        (stream.ring_len as u64) < stream.content_size.unwrap_or(u64::MAX);
                    if ring_is_small && stream.out_start + stream.block_size_max > stream.ring_len {
                        // What was decoded must be hashed before it is overwritten.
                        blocks.finish_checksum(&mut self.checksum);
                        (stream.out_start, stream.out_end) = (0, 0);
                    }
                }
            }
        }

        let stream = &mut self.stream;
        if stream.expected == 0 {
            if stream.out_end == stream.out_start {
                if stream.hostage {
                    if ip >= input.len() {
                        stream.phase = Phase::Read;
                        return Ok(Progress {
                            consumed: ip,
                            written: op,
                            frame_done: false,
                        });
                    }
                    ip += 1;
                }
                return Ok(Progress {
                    consumed: ip,
                    written: op,
                    frame_done: true,
                });
            }
            if !stream.hostage {
                ip -= 1;
                stream.hostage = true;
            }
        }
        Ok(Progress {
            consumed: ip,
            written: op,
            frame_done: false,
        })
    }

    // `ZSTD_decompressContinueStream` for the buffered mode.
    fn decompress_continue_stream<B: BlockDecoder>(
        &mut self,
        src: &[u8],
        blocks: &mut B,
    ) -> Result<(), Error> {
        let skipping = self.stream.stage == Stage::SkipFrame;
        let capacity = if skipping {
            0
        } else {
            self.stream.ring_len - self.stream.out_start
        };
        let decoded = self.decompress_continue(src, capacity, blocks)?;
        let stream = &mut self.stream;
        if decoded == 0 && !skipping {
            stream.phase = Phase::Read;
        } else {
            stream.out_end = stream.out_start + decoded;
            stream.phase = Phase::Flush;
        }
        Ok(())
    }

    // `ZSTD_decompressContinue`: consumes exactly the `expected` bytes of `src` and
    // decodes into the ring from `out_start`, where `capacity` bytes are left.
    fn decompress_continue<B: BlockDecoder>(
        &mut self,
        src: &[u8],
        capacity: usize,
        blocks: &mut B,
    ) -> Result<usize, Error> {
        let Self {
            literals,
            sequences,
            checksum,
            stream,
            dictionary,
            verify_checksum,
            ..
        } = self;
        // `ZSTD_checkContinuity`: after a restart the previous segment becomes history.
        if capacity > 0 && stream.out_start != stream.segment_end {
            stream.ext_len = stream.segment_end;
            stream.ext_dict = false;
            sequences.dictionary_end_kept = false;
            stream.segment_end = stream.out_start;
        }
        match stream.stage {
            Stage::BlockHeader => {
                let block = parse_block_header(src)?;
                if block.input_size() > stream.block_size_max {
                    return Err(Error::CorruptionDetected);
                }
                stream.expected = block.input_size();
                stream.block_type = block.block_type;
                stream.rle_size = block.size as usize;
                if stream.expected > 0 {
                    stream.stage = if block.last {
                        Stage::LastBlock
                    } else {
                        Stage::Block
                    };
                } else if !block.last {
                    stream.expected = BLOCK_HEADER_SIZE;
                } else {
                    (stream.stage, stream.expected) = frame_end(stream.checksum);
                }
                Ok(0)
            }
            Stage::Block | Stage::LastBlock => {
                let start = stream.out_start;
                let decoded = match stream.block_type {
                    BlockType::Compressed => {
                        stream.expected = 0;
                        let ext = match dictionary {
                            Some(dictionary) if stream.ext_dict => Ext::Dict(dictionary.padded()),
                            _ => Ext::Ring(stream.ext_len),
                        };
                        blocks.decode_block(
                            literals,
                            sequences,
                            src,
                            stream.output(),
                            start,
                            History {
                                ext,
                                streaming: true,
                                block_size_max: stream.block_size_max,
                            },
                        )?
                    }
                    BlockType::Raw => {
                        if src.len() > capacity {
                            return Err(Error::DstSizeTooSmall);
                        }
                        // SAFETY: `start + capacity` is the end of the ring.
                        unsafe {
                            ptr::copy_nonoverlapping(
                                src.as_ptr(),
                                stream.ring.as_mut_ptr().add(start),
                                src.len(),
                            );
                        }
                        stream.expected -= src.len();
                        src.len()
                    }
                    BlockType::Rle => {
                        if stream.rle_size > capacity {
                            return Err(Error::DstSizeTooSmall);
                        }
                        // SAFETY: as above.
                        unsafe {
                            ptr::write_bytes(
                                stream.ring.as_mut_ptr().add(start),
                                src[0],
                                stream.rle_size,
                            );
                        }
                        stream.expected = 0;
                        stream.rle_size
                    }
                };
                if decoded > stream.block_size_max {
                    return Err(Error::CorruptionDetected);
                }
                stream.decoded_size += decoded as u64;
                if stream.checksum && *verify_checksum {
                    blocks.hash_block(checksum, stream.output(), start, decoded);
                }
                stream.segment_end = start + decoded;
                if stream.expected > 0 {
                    return Ok(decoded);
                }
                if stream.stage == Stage::LastBlock {
                    if stream
                        .content_size
                        .is_some_and(|size| size != stream.decoded_size)
                    {
                        return Err(Error::CorruptionDetected);
                    }
                    (stream.stage, stream.expected) = frame_end(stream.checksum);
                } else {
                    (stream.stage, stream.expected) = (Stage::BlockHeader, BLOCK_HEADER_SIZE);
                }
                Ok(decoded)
            }
            Stage::Checksum => {
                if stream.checksum && *verify_checksum {
                    blocks.finish_checksum(checksum);
                    if read_le32(src, 0) != checksum.finish() as u32 {
                        return Err(Error::ChecksumWrong);
                    }
                }
                (stream.stage, stream.expected) = (Stage::End, 0);
                Ok(0)
            }
            Stage::SkipFrame | Stage::End => {
                (stream.stage, stream.expected) = (Stage::End, 0);
                Ok(0)
            }
        }
    }
}

// The stage after the last block.
const fn frame_end(checksum: bool) -> (Stage, usize) {
    if checksum {
        (Stage::Checksum, CHECKSUM_SIZE)
    } else {
        (Stage::End, 0)
    }
}

#[cfg(test)]
mod tests;
