//! The literals section of a compressed block (`ZSTD_decodeLiteralsBlock`),
//! with libzstd's check order. Literals live in a buffer owned by the caller
//! across blocks, or are read in place from the input when raw.

use alloc::vec::Vec;

use crate::copy::WILDCOPY_OVERLENGTH;
use crate::dict::Dictionary;
use crate::error::Error;
use crate::huf::HufTable;

const MIN_LITERALS_SECTION_SIZE: usize = 2;
const MIN_COMPRESSED_HEADER_SIZE: usize = 5;
const MIN_LITERALS_FOR_4_STREAMS: usize = 6;

const RAW: u8 = 0;
const RLE: u8 = 1;
const COMPRESSED: u8 = 2;

// Literals of a block: `padded` holds them first, then at least
// `WILDCOPY_OVERLENGTH` readable bytes of anything.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Literals<'a> {
    pub(crate) padded: &'a [u8],
    pub(crate) len: usize,
    // Read in place from the block (raw literals with room after them).
    pub(crate) in_input: bool,
}

// Literal buffer and Huffman table, reused across blocks and frames.
#[derive(Debug, Clone)]
pub(crate) struct LiteralsState {
    buffer: Vec<u8>,
    table: alloc::boxed::Box<HufTable>,
    // A Huffman table from this frame can be repeated (libzstd's `litEntropy`).
    has_table: bool,
    dictionary: Option<Dictionary>,
    // The table to repeat is the dictionary's, not copied into `table`.
    in_dictionary: bool,
}

impl<'a> Literals<'a> {
    pub(crate) fn bytes(&self) -> &'a [u8] {
        &self.padded[..self.len]
    }
}

impl LiteralsState {
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            table: HufTable::boxed(),
            has_table: false,
            dictionary: None,
            in_dictionary: false,
        }
    }

    pub(crate) fn set_dictionary(&mut self, dictionary: Option<&Dictionary>) {
        self.dictionary = dictionary.cloned();
    }

    // A frame starts from the dictionary's Huffman table, if it has one.
    pub(crate) fn start_frame(&mut self) {
        self.in_dictionary = self
            .dictionary
            .as_ref()
            .is_some_and(|d| d.entropy().is_some());
        self.has_table = self.in_dictionary;
    }

    // Decodes the section at the start of `src`. `output_capacity` is the room
    // left for the block's output, `block_size_max` the frame's block limit.
    // Returns the literals and the section's size.
    pub(crate) fn decode<'a>(
        &'a mut self,
        src: &'a [u8],
        output_capacity: usize,
        block_size_max: usize,
    ) -> Result<(Literals<'a>, usize), Error> {
        if src.len() < MIN_LITERALS_SECTION_SIZE {
            return Err(Error::CorruptionDetected);
        }
        let kind = src[0] & 3;
        let size_format = (src[0] >> 2) & 3;
        let writable = block_size_max.min(output_capacity);
        if kind == COMPRESSED || kind == 3 {
            return self.decode_compressed(src, kind == 3, size_format, writable, block_size_max);
        }

        // Raw and RLE: 1, 2 or 3 bytes of header, the size in the bits above the 4 first.
        let le24 = |at: usize| {
            src.get(at..at + 3)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], 0]))
        };
        let (header_size, len) = match size_format {
            1 => (2, u32::from(u16::from_le_bytes([src[0], src[1]])) >> 4),
            3 => (3, le24(0).ok_or(Error::CorruptionDetected)? >> 4),
            _ => (1, u32::from(src[0] >> 3)),
        };
        let len = len as usize;
        if kind == RLE && src.len() <= header_size {
            return Err(Error::CorruptionDetected);
        }
        if len > block_size_max {
            return Err(Error::CorruptionDetected);
        }
        if writable < len {
            return Err(Error::DstSizeTooSmall);
        }
        if kind == RAW {
            if header_size + len + WILDCOPY_OVERLENGTH <= src.len() {
                return Ok((
                    Literals {
                        padded: &src[header_size..],
                        len,
                        in_input: true,
                    },
                    header_size + len,
                ));
            }
            if header_size + len > src.len() {
                return Err(Error::CorruptionDetected);
            }
            grow(&mut self.buffer, len).copy_from_slice(&src[header_size..header_size + len]);
            Ok((
                Literals {
                    padded: &self.buffer,
                    len,
                    in_input: false,
                },
                header_size + len,
            ))
        } else {
            grow(&mut self.buffer, len).fill(src[header_size]);
            Ok((
                Literals {
                    padded: &self.buffer,
                    len,
                    in_input: false,
                },
                header_size + 1,
            ))
        }
    }

    fn decode_compressed<'a>(
        &'a mut self,
        src: &'a [u8],
        repeat_table: bool,
        size_format: u8,
        writable: usize,
        block_size_max: usize,
    ) -> Result<(Literals<'a>, usize), Error> {
        if repeat_table && !self.has_table {
            return Err(Error::DictionaryCorrupted);
        }
        if src.len() < MIN_COMPRESSED_HEADER_SIZE {
            return Err(Error::CorruptionDetected);
        }
        // Sizes in 10, 14 or 18 bits each (regenerated, then compressed), 3 to 5 bytes in all.
        let header = u32::from_le_bytes([src[0], src[1], src[2], src[3]]);
        let (header_size, len, compressed_len) = match size_format {
            0 | 1 => (3, (header >> 4) & 0x3FF, (header >> 14) & 0x3FF),
            2 => (4, (header >> 4) & 0x3FFF, header >> 18),
            _ => (
                5,
                (header >> 4) & 0x3FFFF,
                (header >> 22) + (u32::from(src[4]) << 10),
            ),
        };
        let (len, compressed_len) = (len as usize, compressed_len as usize);
        let single_stream = size_format == 0;
        if len > block_size_max {
            return Err(Error::CorruptionDetected);
        }
        if !single_stream && len < MIN_LITERALS_FOR_4_STREAMS {
            return Err(Error::LiteralsHeaderWrong);
        }
        if compressed_len + header_size > src.len() {
            return Err(Error::CorruptionDetected);
        }
        if writable < len {
            return Err(Error::DstSizeTooSmall);
        }

        let dst = grow(&mut self.buffer, len);
        let stream = &src[header_size..header_size + compressed_len];
        let result = if repeat_table {
            let table = match &self.dictionary {
                Some(dictionary) if self.in_dictionary => &dictionary.entropy().unwrap().huffman,
                _ => &self.table,
            };
            if single_stream {
                table.decompress_1x(dst, stream)
            } else {
                table.decompress_4x(dst, stream)
            }
        } else {
            self.in_dictionary = false;
            if single_stream {
                self.table.read_and_decompress_1x(dst, stream)
            } else {
                self.table.read_and_decompress_4x(dst, stream)
            }
        };
        result.map_err(|_| Error::CorruptionDetected)?;
        self.has_table = true;
        Ok((
            Literals {
                padded: &self.buffer,
                len,
                in_input: false,
            },
            header_size + compressed_len,
        ))
    }
}

// Room for `len` literals and the wild copy slack, by doubling; returns the first `len`.
fn grow(buffer: &mut Vec<u8>, len: usize) -> &mut [u8] {
    let needed = len + WILDCOPY_OVERLENGTH;
    if buffer.len() < needed {
        buffer.resize(needed.max(2 * buffer.len()), 0);
    }
    &mut buffer[..len]
}

#[cfg(test)]
mod tests;
