use core::hash::Hasher;

use twox_hash::XxHash64;

use crate::dict::Dictionary;
use crate::error::Error;
use crate::frame::{
    BLOCK_HEADER_SIZE, BlockType, FRAME_HEADER_MIN_SIZE, FRAME_HEADER_PREFIX_SIZE, FrameHeader,
    FrameStart, decompressed_bound, frame_header_size, is_skippable_magic, parse_block_header,
    parse_frame_start, read_le32, skippable_frame_size,
};
use crate::literals::{Literals, LiteralsState};
use crate::sequences::{Ext, Output, Placement, SequencesState, block_output_limit};
use crate::stream::Stream;

const CHECKSUM_SIZE: usize = 4;

/// A reusable decoder: keeps its buffers between calls.
#[derive(Debug, Clone)]
pub struct Decoder {
    // Threads of `decompress_stream`, joined first: they may read the stream's ring.
    #[cfg(feature = "std")]
    pub(crate) streaming: Option<alloc::boxed::Box<crate::parallel::Streaming>>,
    pub(crate) checksum: XxHash64,
    pub(crate) literals: LiteralsState,
    pub(crate) sequences: SequencesState,
    pub(crate) stream: Stream,
    pub(crate) dictionary: Option<Dictionary>,
    pub(crate) verify_checksum: bool,
    #[cfg(feature = "std")]
    pub(crate) threads: usize,
    // Frames below this many compressed bytes stay serial.
    #[cfg(test)]
    pub(crate) gate_bytes: usize,
    // Tests: a block of the streamed frame to hand to the serial code although it is usable.
    #[cfg(test)]
    pub(crate) fallback_at: Option<usize>,
    // Frames that went through the pipeline, and that it redid serially.
    #[cfg(test)]
    pub(crate) pipelined_frames: usize,
    #[cfg(test)]
    pub(crate) redone_frames: usize,
}

// Decodes the compressed blocks of a frame: the serial code, or the pipelines of `parallel`
// (the one-shot one starts from the position of the first block header and ignores `block`).
pub(crate) trait BlockDecoder {
    fn start_frame(&mut self, _sequences: &SequencesState, _start: usize) {}

    fn decode_block(
        &mut self,
        literals: &mut LiteralsState,
        sequences: &mut SequencesState,
        block: &[u8],
        output: Output,
        start: usize,
        history: History<'_>,
    ) -> Result<usize, Error>;

    // Hashes the finished `output[start..start + len]` into `checksum`, in block order. A
    // pipeline may do it later and elsewhere, and gives the state back in `finish_checksum`.
    #[inline(always)]
    fn hash_block(&mut self, checksum: &mut XxHash64, output: Output, start: usize, len: usize) {
        hash_in_place(checksum, output, start, len);
    }

    #[inline(always)]
    fn finish_checksum(&mut self, _checksum: &mut XxHash64) {}
}

// The serial code. Static dispatch, so that it is what it was before the pipeline.
#[derive(Default)]
pub(crate) struct SerialBlocks;

impl BlockDecoder for SerialBlocks {
    #[inline(always)]
    fn decode_block(
        &mut self,
        literals: &mut LiteralsState,
        sequences: &mut SequencesState,
        block: &[u8],
        output: Output,
        start: usize,
        history: History<'_>,
    ) -> Result<usize, Error> {
        decompress_compressed_block(literals, sequences, block, output, start, history)
    }
}

impl Decoder {
    /// A decoder with the default window limit and no dictionary.
    #[must_use]
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "std")]
            streaming: None,
            checksum: XxHash64::with_seed(0),
            literals: LiteralsState::new(),
            sequences: SequencesState::new(),
            stream: Stream::new(),
            dictionary: None,
            verify_checksum: true,
            #[cfg(feature = "std")]
            threads: 1,
            #[cfg(test)]
            gate_bytes: 0,
            #[cfg(test)]
            fallback_at: None,
            #[cfg(test)]
            pipelined_frames: 0,
            #[cfg(test)]
            redone_frames: 0,
        }
    }

    /// Resets the streaming state machine to begin decoding a new frame cleanly.
    ///
    /// Any loaded dictionary and configured window limits are preserved.
    #[inline]
    pub const fn reset(&mut self) {
        self.stream.reset();
    }

    /// Enables or disables checksum verification for frames containing a checksum.
    ///
    /// Defaults to `true`. When disabled, the 4-byte checksum is still consumed
    /// from the input, but hash calculation and validation are skipped.
    pub const fn set_verify_checksum(&mut self, verify: bool) {
        self.verify_checksum = verify;
    }

    /// Sets the threads (this one included) of [`Decoder::decompress_into`] and its variants,
    /// of [`Decoder::decompress_stream`] and of a [`Reader`](crate::Reader) made with this
    /// decoder. The output is the same as with 0 or 1 thread (the default), which is serial.
    ///
    /// A frame of at least a few blocks is decoded by threads spawned for the call, frames in
    /// order. A streamed frame of unknown or large (256 KiB or more) content size is decoded by
    /// threads that last as long as the decoder, which reads the input a few blocks per thread
    /// ahead of the output: see [`Decoder::decompress_stream`] for what that changes. Changing
    /// the threads abandons a stream in progress.
    #[cfg(feature = "std")]
    pub fn set_threads(&mut self, threads: usize) {
        if threads != self.threads {
            self.threads = threads;
            self.streaming = None;
            self.stream.reset();
        }
    }

    /// Decodes every following frame with `dictionary` (`ZSTD_DCtx_loadDictionary`), or
    /// with none for `None`. A frame states the ID of the dictionary it needs, which must be
    /// the one of a formatted dictionary (a raw content one has ID 0, a frame with an ID
    /// needs a dictionary with that ID: `DictionaryWrong`). A stream in the middle of a
    /// frame is abandoned, like after an error.
    pub fn set_dictionary(&mut self, dictionary: Option<&Dictionary>) {
        self.dictionary = dictionary.filter(|d| !d.is_empty()).cloned();
        self.literals.set_dictionary(self.dictionary.as_ref());
        self.sequences.set_dictionary(self.dictionary.as_ref());
        self.sequences.dictionary_end_kept = false;
        self.stream.reset();
    }

    /// Decodes every frame of `input` into uninitialized `output` memory, like
    /// [`Decoder::decompress_into`]. Returns the number of bytes written.
    pub fn decompress_into_uninit(
        &mut self,
        input: &[u8],
        output: &mut [core::mem::MaybeUninit<u8>],
    ) -> Result<usize, Error> {
        #[cfg(feature = "std")]
        if self.threads > 1 {
            return self.decompress_frames::<true>(input, output);
        }
        self.decompress_frames::<false>(input, output)
    }

    // The frames of `input`; `THREADED`: the big ones go through the pipeline. Two copies, so
    // that the serial one is the code it was before threads.
    fn decompress_frames<const THREADED: bool>(
        &mut self,
        input: &[u8],
        output: &mut [core::mem::MaybeUninit<u8>],
    ) -> Result<usize, Error> {
        let mut input = input;
        let mut written = 0;
        let mut frames_done = false;
        while input.len() >= FRAME_HEADER_PREFIX_SIZE {
            if is_skippable_magic(input) {
                input = &input[skippable_frame_size(input)?..];
                continue;
            }
            let output = &mut output[written..];
            #[cfg(feature = "std")]
            let result = if THREADED {
                self.decompress_frame_threaded(input, output)
            } else {
                self.decompress_frame(input, output, &mut SerialBlocks)
            };
            #[cfg(not(feature = "std"))]
            let result = self.decompress_frame(input, output, &mut SerialBlocks);
            match result {
                // Garbage after a good frame is more likely a wrong input size.
                Err(Error::PrefixUnknown | Error::LegacyFormatUnsupported(_)) if frames_done => {
                    return Err(Error::SrcSizeWrong);
                }
                Err(error) => return Err(error),
                Ok((consumed, frame_written)) => {
                    input = &input[consumed..];
                    written += frame_written;
                }
            }
            frames_done = true;
        }
        if input.is_empty() {
            Ok(written)
        } else {
            Err(Error::SrcSizeWrong)
        }
    }

    /// Decodes every frame of `input` into `out` like [`Decoder::decompress_into`], replacing
    /// its content without zero-filling. `out` grows to the sum of the frames' sizes, or of
    /// their bounds for frames that do not state it.
    pub fn decompress_into_vec(
        &mut self,
        input: &[u8],
        out: &mut alloc::vec::Vec<u8>,
    ) -> Result<usize, Error> {
        out.clear();
        // A bound too large to allocate is left to fail as `DstSizeTooSmall`.
        if let Ok(bound) = usize::try_from(decompressed_bound(input)) {
            let _ = out.try_reserve(bound);
        }
        let written = self.decompress_into_uninit(input, out.spare_capacity_mut())?;
        // SAFETY: decompress_into_uninit initialized the first `written` bytes of spare capacity.
        unsafe {
            out.set_len(written);
        }
        Ok(written)
    }

    /// Decodes every frame of `input` into `output` and returns the number of
    /// bytes written, like `ZSTD_decompressDCtx` (skippable frames are skipped,
    /// trailing bytes that are not a whole frame are an error). Bytes of `output`
    /// past the returned length are scratch: wild copies may overwrite them.
    pub fn decompress_into(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize, Error> {
        self.decompress_into_uninit(input, slice_to_uninit_mut(output))
    }

    // `ZSTD_decompressFrame`: returns (input consumed, output written).
    pub(crate) fn decompress_frame<B: BlockDecoder>(
        &mut self,
        input: &[u8],
        output: &mut [core::mem::MaybeUninit<u8>],
        blocks: &mut B,
    ) -> Result<(usize, usize), Error> {
        let output = Output::new(output);
        self.begin_frame();
        if input.len() < FRAME_HEADER_MIN_SIZE + BLOCK_HEADER_SIZE {
            return Err(Error::SrcSizeWrong);
        }
        let header_size = frame_header_size(input)?;
        if input.len() < header_size + BLOCK_HEADER_SIZE {
            return Err(Error::SrcSizeWrong);
        }
        let header = match parse_frame_start(&input[..header_size])? {
            FrameStart::Frame(header) => header,
            FrameStart::Skippable { .. } | FrameStart::NeedMore(_) => {
                return Err(Error::SrcSizeWrong);
            }
        };
        self.check_frame_header(&header)?;
        blocks.start_frame(&self.sequences, header_size);

        let mut input_pos = header_size;
        let mut written = 0;
        loop {
            let block = parse_block_header(&input[input_pos..])?;
            input_pos += BLOCK_HEADER_SIZE;
            let block_input_size = block.input_size();
            if block_input_size > input.len() - input_pos {
                return Err(Error::SrcSizeWrong);
            }
            let block_input = &input[input_pos..input_pos + block_input_size];
            let block_written = match block.block_type {
                BlockType::Compressed => {
                    let ext = self
                        .dictionary
                        .as_ref()
                        .map_or(Ext::Ring(0), |d| Ext::Dict(d.padded()));
                    let history = History {
                        ext,
                        streaming: false,
                        block_size_max: header.block_size_max as usize,
                    };
                    blocks.decode_block(
                        &mut self.literals,
                        &mut self.sequences,
                        block_input,
                        output,
                        written,
                        history,
                    )?
                }
                BlockType::Raw | BlockType::Rle => write_plain_block(
                    block.block_type,
                    block_input,
                    block.size as usize,
                    output,
                    written,
                )?,
            };
            if header.checksum && self.verify_checksum {
                blocks.hash_block(&mut self.checksum, output, written, block_written);
            }
            written += block_written;
            input_pos += block_input_size;
            if block.last {
                break;
            }
        }

        if header.checksum && self.verify_checksum {
            blocks.finish_checksum(&mut self.checksum);
        }
        check_frame_end(
            &header,
            written,
            &input[input_pos..],
            &self.checksum,
            self.verify_checksum,
        )?;
        let checksum_size = if header.checksum { CHECKSUM_SIZE } else { 0 };
        Ok((input_pos + checksum_size, written))
    }
}

// Writes a raw block (`src`) or an RLE one (`src[0]`, `rle_size` times) at `output[start..]`.
// Returns the bytes written.
pub(crate) fn write_plain_block(
    block_type: BlockType,
    src: &[u8],
    rle_size: usize,
    output: Output,
    start: usize,
) -> Result<usize, Error> {
    let size = if block_type == BlockType::Raw {
        src.len()
    } else {
        rle_size
    };
    if size > output.len - start {
        return Err(Error::DstSizeTooSmall);
    }
    // SAFETY: `size` bytes fit in `output` after `start`.
    unsafe {
        let destination = output.base.add(start);
        if block_type == BlockType::Raw {
            core::ptr::copy_nonoverlapping(src.as_ptr(), destination, size);
        } else {
            core::ptr::write_bytes(destination, src[0], size);
        }
    }
    Ok(size)
}

// Hashes `output[start..start + len]`, which a block just wrote.
#[inline(always)]
pub(crate) fn hash_in_place(checksum: &mut XxHash64, output: Output, start: usize, len: usize) {
    // SAFETY: the caller's contract.
    checksum.write(unsafe { core::slice::from_raw_parts(output.base.add(start), len) });
}

// What precedes the block's segment of the output, whether it goes through the ring of
// `decompress_stream`, and the frame's block size limit.
#[derive(Clone, Copy)]
pub(crate) struct History<'a> {
    pub(crate) ext: Ext<'a>,
    pub(crate) streaming: bool,
    pub(crate) block_size_max: usize,
}

// Views a mutable byte slice as uninitialized memory.
#[inline(always)]
pub(crate) const fn slice_to_uninit_mut(slice: &mut [u8]) -> &mut [core::mem::MaybeUninit<u8>] {
    // SAFETY: initialized bytes are always valid `MaybeUninit<u8>`, and layouts are identical.
    unsafe {
        core::slice::from_raw_parts_mut(
            slice.as_mut_ptr().cast::<core::mem::MaybeUninit<u8>>(),
            slice.len(),
        )
    }
}

// `ZSTD_decompressBlock_internal`: the block's output goes to `output` from `start`,
// earlier bytes (then `history.ext`) are the history.
pub(crate) fn decompress_compressed_block(
    literals_state: &mut LiteralsState,
    sequences: &mut SequencesState,
    block: &[u8],
    output: Output,
    start: usize,
    history: History<'_>,
) -> Result<usize, Error> {
    let block_size_max = history.block_size_max;
    if block.len() > block_size_max {
        return Err(Error::SrcSizeWrong);
    }
    let capacity = output.len - start;
    let (literals, literals_size) = literals_state.decode(block, capacity, block_size_max)?;
    let (nb_seq, stream) = sequences.read_header::<true>(&block[literals_size..])?;
    if nb_seq > 0 && capacity == 0 {
        return Err(Error::DstSizeTooSmall);
    }
    let place = block_placement(&literals, start, capacity, history);
    let block_end = sequences.execute_block(nb_seq, stream, &literals, output, &place)?;
    Ok(block_end - start)
}

// Where a block's output goes: `capacity` bytes from `start`, less if libzstd stops earlier.
pub(crate) fn block_placement<'a>(
    literals: &Literals<'_>,
    start: usize,
    capacity: usize,
    history: History<'a>,
) -> Placement<'a> {
    let block_size_max = history.block_size_max;
    let limit = block_output_limit(capacity, literals, block_size_max, history.streaming);
    Placement {
        range: start..start + limit,
        ext: history.ext,
        total_history: start + history.ext.len() + capacity.min(block_size_max),
    }
}

impl Decoder {
    // `ZSTD_decompressBegin_usingDDict`: the tables and offsets to start a frame from.
    pub(crate) fn begin_frame(&mut self) {
        self.literals.start_frame();
        self.sequences.start_frame();
        if self.dictionary.is_some() {
            self.sequences.dictionary_is_cold = !self.sequences.dictionary_end_kept;
            self.sequences.dictionary_end_kept = true;
        } else {
            // libzstd forgets the dictionary end, and keeps `ddictIsCold` as it is.
            self.sequences.dictionary_end_kept = false;
        }
    }

    // The dictionary ID check of `ZSTD_decodeFrameHeader`, and the checksum start.
    pub(crate) fn check_frame_header(&mut self, header: &FrameHeader) -> Result<(), Error> {
        let dictionary_id = self.dictionary.as_ref().map_or(0, Dictionary::id);
        if header.dict_id != 0 && header.dict_id != dictionary_id {
            return Err(Error::DictionaryWrong);
        }
        if header.checksum && self.verify_checksum {
            self.checksum = XxHash64::with_seed(0);
        }
        Ok(())
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

// Content size, then checksum: the order libzstd reports them in.
fn check_frame_end(
    header: &FrameHeader,
    written: usize,
    rest: &[u8],
    checksum: &XxHash64,
    verify_checksum: bool,
) -> Result<(), Error> {
    if header
        .content_size
        .is_some_and(|size| size != written as u64)
    {
        return Err(Error::CorruptionDetected);
    }
    if header.checksum {
        if rest.len() < CHECKSUM_SIZE {
            return Err(Error::ChecksumWrong);
        }
        if verify_checksum && read_le32(rest, 0) != checksum.finish() as u32 {
            return Err(Error::ChecksumWrong);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
