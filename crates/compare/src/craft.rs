//! Hand-made frames and blocks for tests that need exact bytes.

pub const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
pub const RAW_BLOCK: u32 = 0;
pub const RLE_BLOCK: u32 = 1;
pub const COMPRESSED_BLOCK: u32 = 2;
/// Window descriptor of a 128 KiB window, and of a 1 KiB one.
pub const WINDOW_128K: u8 = 0x38;
pub const WINDOW_1K: u8 = 0x00;
/// Sequence section modes byte with the three tables in RLE mode.
const ALL_RLE: u8 = 0x54;

/// A frame without content size: descriptor (checksum flag only), window
/// descriptor, `blocks`, then `tail` (the checksum bytes if any).
#[must_use]
pub fn frame(window_descriptor: u8, checksum: bool, blocks: &[Vec<u8>], tail: &[u8]) -> Vec<u8> {
    [
        &MAGIC[..],
        &[u8::from(checksum) << 2, window_descriptor],
        &blocks.concat(),
        tail,
    ]
    .concat()
}

/// A block header and `payload`; `size` is the header's size field (the
/// regenerated size for RLE blocks).
#[must_use]
pub fn block(last: bool, kind: u32, size: usize, payload: &[u8]) -> Vec<u8> {
    let header = u32::from(last) | kind << 1 | (size as u32) << 3;
    [&header.to_le_bytes()[..3], payload].concat()
}

/// A literals section of `kind` (0 raw, 1 RLE) with a regenerated size of `len`.
fn literals_header(kind: u8, len: usize) -> Vec<u8> {
    match len {
        0..=31 => vec![(len << 3) as u8 | kind],
        32..=4095 => ((len << 4) as u16 | 0x4 | u16::from(kind))
            .to_le_bytes()
            .to_vec(),
        _ => ((len << 4) as u32 | 0xC | u32::from(kind)).to_le_bytes()[..3].to_vec(),
    }
}

#[must_use]
pub fn raw_literals(bytes: &[u8]) -> Vec<u8> {
    [literals_header(0, bytes.len()), bytes.to_vec()].concat()
}

#[must_use]
pub fn rle_literals(byte: u8, len: usize) -> Vec<u8> {
    [literals_header(1, len), vec![byte]].concat()
}

/// A backward bitstream: the end mark, then `fields` (value, bit count) in
/// reading order, most significant bit first, padded to whole bytes.
#[must_use]
pub fn backward_stream(fields: &[(u64, u32)]) -> Vec<u8> {
    let mut bits = vec![true];
    for &(value, count) in fields {
        bits.extend((0..count).rev().map(|bit| value >> bit & 1 == 1));
    }
    let padding = (8 - bits.len() % 8) % 8;
    let bits = [vec![false; padding], bits].concat();
    let bytes = bits
        .chunks(8)
        .map(|chunk| chunk.iter().fold(0, |byte, &bit| byte << 1 | u8::from(bit)));
    bytes.rev().collect()
}

/// A sequences section of `count` sequences whose three tables are RLE with
/// the given symbols (literal length, offset, match length code): the states
/// need no bits, only the extra bits in `fields` (reading order, per sequence:
/// offset, match length, literal length).
#[must_use]
pub fn rle_sequences(count: u8, (ll, of, ml): (u8, u8, u8), fields: &[(u64, u32)]) -> Vec<u8> {
    [&[count, ALL_RLE, ll, of, ml][..], &backward_stream(fields)].concat()
}

/// Header and sizes of a literals section; `compressed` is the size of the
/// payload after the header for Huffman kinds (2, 3) and 0 otherwise.
#[derive(Debug, Clone, Copy)]
pub struct LiteralsSection {
    pub kind: u8,
    pub size_format: u8,
    pub header_size: usize,
    pub regenerated: usize,
    pub compressed: usize,
}

/// `None` when the block is too short for what its header says.
#[must_use]
pub fn literals_section(block: &[u8]) -> Option<LiteralsSection> {
    let le = |bytes: usize| {
        let mut word = [0; 8];
        word[..bytes].copy_from_slice(block.get(..bytes)?);
        Some(u64::from_le_bytes(word))
    };
    let (kind, size_format) = (block.first()? & 3, block.first()? >> 2 & 3);
    let (header_size, regenerated, compressed) = match (kind, size_format) {
        (0 | 1, 0 | 2) => (1, usize::from(block[0] >> 3), 0),
        (0 | 1, 1) => (2, (le(2)? >> 4) as usize, 0),
        (0 | 1, _) => (3, (le(3)? >> 4) as usize, 0),
        (_, 0 | 1) => (
            3,
            (le(4)? >> 4 & 0x3FF) as usize,
            (le(4)? >> 14 & 0x3FF) as usize,
        ),
        (_, 2) => (4, (le(4)? >> 4 & 0x3FFF) as usize, (le(4)? >> 18) as usize),
        _ => (5, (le(5)? >> 4 & 0x3FFFF) as usize, (le(5)? >> 22) as usize),
    };
    let payload = match kind {
        0 => regenerated,
        1 => 1,
        _ => compressed,
    };
    (block.len() >= header_size + payload && (kind < 2 || block.len() >= 5)).then_some(
        LiteralsSection {
            kind,
            size_format,
            header_size,
            regenerated,
            compressed,
        },
    )
}

/// (block type, payload range) of each block of a well formed frame without dictionary ID.
#[must_use]
pub fn frame_blocks(frame: &[u8]) -> Vec<(u32, std::ops::Range<usize>)> {
    let descriptor = frame[4];
    let single_segment = descriptor >> 5 & 1 == 1;
    let header_size = 5
        + usize::from(!single_segment)
        + [0, 1, 2, 4][usize::from(descriptor & 3)]
        + [0, 2, 4, 8][usize::from(descriptor >> 6)]
        + usize::from(single_segment && descriptor >> 6 == 0);
    let (mut at, mut blocks) = (header_size, Vec::new());
    loop {
        let header = u32::from_le_bytes([frame[at], frame[at + 1], frame[at + 2], 0]);
        let (last, kind, size) = (header & 1 == 1, header >> 1 & 3, (header >> 3) as usize);
        let payload = if kind == RLE_BLOCK { 1 } else { size };
        blocks.push((kind, at + 3..at + 3 + payload));
        at += 3 + payload;
        if last {
            return blocks;
        }
    }
}
