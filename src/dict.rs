//! Dictionaries, loaded like `ZSTD_createDDict` (content type auto): a formatted one
//! (magic, ID, entropy tables, repeat offsets, content) or raw content.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::copy::WILDCOPY_OVERLENGTH;
use crate::error::Error;
use crate::frame::read_le32;
use crate::huf::HufTable;
use crate::sequences::Tables;

const MAGIC: u32 = 0xEC30_A437;
const HEADER_SIZE: usize = 8;
const REPEAT_OFFSETS_SIZE: usize = 12;

/// A dictionary for [`Decoder::set_dictionary`](crate::Decoder::set_dictionary), cheap to
/// clone and to share between decoders.
#[derive(Debug, Clone)]
pub struct Dictionary(Arc<Parsed>);

#[derive(Debug)]
struct Parsed {
    id: u32,
    // The whole dictionary: libzstd's `ZSTD_DDict` keeps the entropy header in the history
    // (unlike `ZSTD_decompress_usingDict`, which starts it after the header), then
    // `WILDCOPY_OVERLENGTH` bytes of slack for wild copies.
    padded: Vec<u8>,
    entropy: Option<Entropy>,
}

// What a formatted dictionary starts a frame with.
#[derive(Debug)]
pub(crate) struct Entropy {
    pub(crate) huffman: Box<HufTable>,
    pub(crate) sequences: Box<Tables>,
    pub(crate) repeat_offsets: [u32; 3],
}

impl Dictionary {
    /// Parses `bytes`: input of at least 8 bytes starting with the dictionary magic number
    /// is a formatted dictionary, else it is all content. A formatted dictionary whose
    /// tables or repeat offsets are invalid is `DictionaryCorrupted`. Empty input is a
    /// dictionary with no content, which decodes like no dictionary.
    pub fn new(bytes: &[u8]) -> Result<Self, Error> {
        let (id, entropy) = if bytes.len() < HEADER_SIZE || read_le32(bytes, 0) != MAGIC {
            (0, None)
        } else {
            (read_le32(bytes, 4), Some(read_entropy(bytes)?))
        };
        let mut padded = Vec::with_capacity(bytes.len() + WILDCOPY_OVERLENGTH);
        padded.extend_from_slice(bytes);
        padded.resize(bytes.len() + WILDCOPY_OVERLENGTH, 0);
        Ok(Self(Arc::new(Parsed {
            id,
            padded,
            entropy,
        })))
    }

    pub(crate) fn id(&self) -> u32 {
        self.0.id
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.padded.len() == WILDCOPY_OVERLENGTH
    }

    // The dictionary and its slack.
    pub(crate) fn padded(&self) -> &[u8] {
        &self.0.padded
    }

    pub(crate) fn entropy(&self) -> Option<&Entropy> {
        self.0.entropy.as_ref()
    }
}

// `ZSTD_loadDEntropy`.
fn read_entropy(dict: &[u8]) -> Result<Entropy, Error> {
    if dict.len() <= HEADER_SIZE {
        return Err(Error::DictionaryCorrupted);
    }
    // The Huffman table is always the double-symbol kind.
    let mut huffman = HufTable::boxed();
    let mut used = HEADER_SIZE
        + huffman
            .read_x2(&dict[HEADER_SIZE..])
            .map_err(|_| Error::DictionaryCorrupted)?;
    let (sequences, size) = Tables::read_dictionary(&dict[used..])?;
    used += size;
    if used + REPEAT_OFFSETS_SIZE > dict.len() {
        return Err(Error::DictionaryCorrupted);
    }
    let content_size = dict.len() - used - REPEAT_OFFSETS_SIZE;
    let mut repeat_offsets = [0; 3];
    for (offset, at) in repeat_offsets.iter_mut().zip((used..).step_by(4)) {
        *offset = read_le32(dict, at);
        if *offset == 0 || *offset as usize > content_size {
            return Err(Error::DictionaryCorrupted);
        }
    }
    Ok(Entropy {
        huffman,
        sequences,
        repeat_offsets,
    })
}

#[cfg(test)]
mod tests;
