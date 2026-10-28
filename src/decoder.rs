use core::hash::Hasher;

use twox_hash::XxHash64;

use crate::error::Error;
use crate::frame::{
    BLOCK_HEADER_SIZE, BlockType, FRAME_HEADER_MIN_SIZE, FRAME_HEADER_PREFIX_SIZE, FrameHeader,
    FrameStart, frame_header_size, is_skippable_magic, parse_block_header, parse_frame_start,
    read_le32, skippable_frame_size,
};
use crate::literals::LiteralsState;
use crate::sequences::{SequencesState, block_output_limit};

const CHECKSUM_SIZE: usize = 4;

/// A reusable decoder: keeps its buffers between calls.
#[derive(Debug, Clone)]
pub struct Decoder {
    checksum: XxHash64,
    literals: LiteralsState,
    sequences: SequencesState,
}

impl Decoder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            checksum: XxHash64::with_seed(0),
            literals: LiteralsState::new(),
            sequences: SequencesState::new(),
        }
    }

    /// Decodes every frame of `input` into `output` and returns the number of
    /// bytes written, like `ZSTD_decompressDCtx` (skippable frames are skipped,
    /// trailing bytes that are not a whole frame are an error). Bytes of `output`
    /// past the returned length are scratch: wild copies may overwrite them.
    pub fn decompress_into(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize, Error> {
        let mut input = input;
        let mut written = 0;
        let mut frames_done = false;
        while input.len() >= FRAME_HEADER_PREFIX_SIZE {
            if is_skippable_magic(input) {
                input = &input[skippable_frame_size(input)?..];
                continue;
            }
            match self.decompress_frame(input, &mut output[written..]) {
                // Garbage after a good frame is more likely a wrong input size.
                Err(Error::PrefixUnknown) if frames_done => return Err(Error::SrcSizeWrong),
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

    // `ZSTD_decompressFrame`: returns (input consumed, output written).
    fn decompress_frame(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize), Error> {
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
        // Phase 5 compares with the id of the loaded dictionary.
        if header.dict_id != 0 {
            return Err(Error::DictionaryWrong);
        }
        if header.checksum {
            self.checksum = XxHash64::with_seed(0);
        }
        self.literals.start_frame();
        self.sequences.start_frame();

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
            let block_output = &mut output[written..];
            let block_written = match block.block_type {
                BlockType::Compressed => {
                    let max = header.block_size_max as usize;
                    self.decompress_compressed_block(block_input, output, written, max)?
                }
                BlockType::Raw => {
                    let target = block_output
                        .get_mut(..block_input_size)
                        .ok_or(Error::DstSizeTooSmall)?;
                    target.copy_from_slice(block_input);
                    block_input_size
                }
                BlockType::Rle => {
                    let size = block.size as usize;
                    let target = block_output.get_mut(..size).ok_or(Error::DstSizeTooSmall)?;
                    target.fill(block_input[0]);
                    size
                }
            };
            if header.checksum {
                self.checksum
                    .write(&output[written..written + block_written]);
            }
            written += block_written;
            input_pos += block_input_size;
            if block.last {
                break;
            }
        }

        check_frame_end(&header, written, &input[input_pos..], &self.checksum)?;
        let checksum_size = if header.checksum { CHECKSUM_SIZE } else { 0 };
        Ok((input_pos + checksum_size, written))
    }

    // `ZSTD_decompressBlock_internal`: the block's output goes to `output`
    // from `start`, earlier bytes are the frame's history.
    fn decompress_compressed_block(
        &mut self,
        block: &[u8],
        output: &mut [u8],
        start: usize,
        block_size_max: usize,
    ) -> Result<usize, Error> {
        if block.len() > block_size_max {
            return Err(Error::SrcSizeWrong);
        }
        let capacity = output.len() - start;
        let (literals, literals_size) = self.literals.decode(block, capacity, block_size_max)?;
        let (nb_seq, stream) = self.sequences.read_header(&block[literals_size..])?;
        if nb_seq > 0 && capacity == 0 {
            return Err(Error::DstSizeTooSmall);
        }
        let end = start + block_output_limit(capacity, &literals, block_size_max);
        let block_end =
            self.sequences
                .execute_block(nb_seq, stream, &literals, output, start..end)?;
        Ok(block_end - start)
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
) -> Result<(), Error> {
    if header
        .content_size
        .is_some_and(|size| size != written as u64)
    {
        return Err(Error::CorruptionDetected);
    }
    if header.checksum
        && (rest.len() < CHECKSUM_SIZE || read_le32(rest, 0) != checksum.finish() as u32)
    {
        return Err(Error::ChecksumWrong);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
