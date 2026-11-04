//! The literals section of a compressed block (`ZSTD_decodeLiteralsBlock`),
//! with libzstd's check order. Literals live in a buffer owned by the caller
//! across blocks, or are read in place from the input when raw.

use alloc::vec::Vec;

use crate::copy::WILDCOPY_OVERLENGTH;
use crate::dict::Dictionary;
use crate::error::Error;
use crate::huf::{HufDef, HufTable};

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
    // What `table` is built from.
    def: HufDef,
}

// What a section needs once its header and tree description are read: the literals are written
// from it (stage 2 of the split path).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LiteralsPlan {
    kind: u8,
    header_size: usize,
    pub(crate) len: usize,
    pub(crate) section_size: usize,
    // Compressed: the Huffman streams start here (after the tree description).
    stream_start: usize,
    single_stream: bool,
    // Raw literals read in place from the block.
    in_input: bool,
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
            def: HufDef::default(),
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
        self.def.from_dictionary = true;
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
        let plan = self.plan::<true>(src, block_size_max.min(output_capacity), block_size_max)?;
        let table = active_table(self.dictionary.as_ref(), self.in_dictionary, &self.table);
        plan.fill(Some(table), src, &mut self.buffer)?;
        Ok((plan.literals(src, &self.buffer), plan.section_size))
    }

    // The section's header and tree description (stage 1 of the split path): checks and table
    // state as `ZSTD_decodeLiteralsBlock`, `writable` bytes of room for the literals. With
    // `BUILD` the table is built here, else `def` becomes the description it is built from.
    #[inline(always)]
    pub(crate) fn plan<const BUILD: bool>(
        &mut self,
        src: &[u8],
        writable: usize,
        block_size_max: usize,
    ) -> Result<LiteralsPlan, Error> {
        if src.len() < MIN_LITERALS_SECTION_SIZE {
            return Err(Error::CorruptionDetected);
        }
        let kind = src[0] & 3;
        let size_format = (src[0] >> 2) & 3;
        if kind == COMPRESSED || kind == 3 {
            return self.plan_compressed::<BUILD>(
                src,
                kind == 3,
                size_format,
                writable,
                block_size_max,
            );
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
        let in_input = kind == RAW && header_size + len + WILDCOPY_OVERLENGTH <= src.len();
        if kind == RAW && !in_input && header_size + len > src.len() {
            return Err(Error::CorruptionDetected);
        }
        Ok(LiteralsPlan {
            kind,
            header_size,
            len,
            section_size: header_size + if kind == RAW { len } else { 1 },
            in_input,
            ..LiteralsPlan::default()
        })
    }

    fn plan_compressed<const BUILD: bool>(
        &mut self,
        src: &[u8],
        repeat_table: bool,
        size_format: u8,
        writable: usize,
        block_size_max: usize,
    ) -> Result<LiteralsPlan, Error> {
        if repeat_table && !self.has_table {
            return Err(Error::DictionaryCorrupted);
        }
        let (header_size, len, compressed_len) =
            compressed_header(src, size_format, writable, block_size_max)?;
        let single_stream = size_format == 0;
        let mut tree_size = 0;
        if !repeat_table {
            self.in_dictionary = false;
            let stream = &src[header_size..header_size + compressed_len];
            tree_size = self
                .def
                .read(stream, (!single_stream).then_some(len))
                .map_err(|_| Error::CorruptionDetected)?;
            if BUILD {
                self.table
                    .build(&self.def)
                    .map_err(|_| Error::CorruptionDetected)?;
            }
        } else if !single_stream {
            self.def.scaled = true;
            if BUILD {
                self.table.scale_x1_for_four_streams();
            }
        }
        self.has_table = true;
        Ok(LiteralsPlan {
            kind: COMPRESSED,
            header_size,
            len,
            section_size: header_size + compressed_len,
            stream_start: header_size + tree_size,
            single_stream,
            ..LiteralsPlan::default()
        })
    }

    pub(crate) const fn def(&self) -> &HufDef {
        &self.def
    }

    // What the sections so far left besides the table: a table can be repeated, and it is the
    // dictionary's.
    pub(crate) const fn flags(&self) -> [bool; 2] {
        [self.has_table, self.in_dictionary]
    }

    // Back to the state `flags` and `def` (the last table read or scaled) describe, after
    // sections planned further ahead changed it.
    pub(crate) fn restore(&mut self, flags: [bool; 2], def: &HufDef) {
        [self.has_table, self.in_dictionary] = flags;
        self.def = *def;
        if !self.in_dictionary && !def.from_dictionary {
            let _ = self.table.build(def);
        }
    }
}

impl LiteralsPlan {
    pub(crate) const fn is_compressed(&self) -> bool {
        self.kind == COMPRESSED
    }

    // Writes the literals into `buffer`, unless they are read in place from `src` (the
    // block). Compressed sections need their `table`.
    #[inline(always)]
    pub(crate) fn fill(
        &self,
        table: Option<&HufTable>,
        src: &[u8],
        buffer: &mut Vec<u8>,
    ) -> Result<(), Error> {
        let (start, len) = (self.header_size, self.len);
        match self.kind {
            RAW if self.in_input => {}
            RAW => grow(buffer, len).copy_from_slice(&src[start..start + len]),
            RLE => grow(buffer, len).fill(src[start]),
            _ => {
                let table = table.ok_or(Error::CorruptionDetected)?;
                let stream = &src[self.stream_start..self.section_size];
                let dst = grow(buffer, len);
                let result = if self.single_stream {
                    table.decompress_1x(dst, stream)
                } else {
                    table.decompress_4x(dst, stream)
                };
                result.map_err(|_| Error::CorruptionDetected)?;
            }
        }
        Ok(())
    }

    // The literals once `fill` ran on `buffer`.
    pub(crate) fn literals<'a>(&self, src: &'a [u8], buffer: &'a [u8]) -> Literals<'a> {
        Literals {
            padded: if self.in_input {
                &src[self.header_size..]
            } else {
                buffer
            },
            len: self.len,
            in_input: self.in_input,
        }
    }
}

// The sizes of a compressed section's header, with its checks: regenerated, compressed, and
// the header's own.
#[inline(always)]
fn compressed_header(
    src: &[u8],
    size_format: u8,
    writable: usize,
    block_size_max: usize,
) -> Result<(usize, usize, usize), Error> {
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
    if len > block_size_max {
        return Err(Error::CorruptionDetected);
    }
    if size_format != 0 && len < MIN_LITERALS_FOR_4_STREAMS {
        return Err(Error::LiteralsHeaderWrong);
    }
    if compressed_len + header_size > src.len() {
        return Err(Error::CorruptionDetected);
    }
    if writable < len {
        return Err(Error::DstSizeTooSmall);
    }
    Ok((header_size, len, compressed_len))
}

// The Huffman table of a treeless section: the dictionary's until a table is read.
fn active_table<'a>(
    dictionary: Option<&'a Dictionary>,
    in_dictionary: bool,
    table: &'a HufTable,
) -> &'a HufTable {
    match dictionary {
        Some(dictionary) if in_dictionary => &dictionary.entropy().unwrap().huffman,
        _ => table,
    }
}

// Room for `len` literals and the wild copy slack, by doubling; returns the first `len`. The
// slack is zeroed: wild copies of literals write it to the output, where a corrupt frame can
// read it back as history, and what it holds must not depend on the blocks before.
fn grow(buffer: &mut Vec<u8>, len: usize) -> &mut [u8] {
    let needed = len + WILDCOPY_OVERLENGTH;
    if buffer.len() < needed {
        buffer.resize(needed.max(2 * buffer.len()), 0);
    }
    buffer[len..needed].fill(0);
    &mut buffer[..len]
}

#[cfg(test)]
mod tests;
