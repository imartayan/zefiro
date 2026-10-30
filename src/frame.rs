//! Frame and block headers, with libzstd's check order and error codes.

use crate::error::Error;

const MAGIC: u32 = 0xFD2F_B528;
const LEGACY_PREFIX: u32 = 0xFD2F_B500;
const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;
const SKIPPABLE_MAGIC_MASK: u32 = 0xFFFF_FFF0;
const WINDOW_LOG_MIN: u32 = 10;
const WINDOW_LOG_MAX: u32 = 31;
const DICT_ID_SIZES: [usize; 4] = [0, 1, 2, 4];
const CONTENT_SIZE_SIZES: [usize; 4] = [0, 2, 4, 8];
const CONTENT_SIZE_UNKNOWN: u64 = u64::MAX;

pub(crate) const BLOCK_SIZE_MAX: u32 = 1 << 17;
pub(crate) const BLOCK_HEADER_SIZE: usize = 3;
// Bytes needed to know the size of the frame header.
pub(crate) const FRAME_HEADER_PREFIX_SIZE: usize = 5;
pub(crate) const FRAME_HEADER_MIN_SIZE: usize = 6;
pub(crate) const SKIPPABLE_HEADER_SIZE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameHeader {
    pub(crate) header_size: usize,
    // `None` when absent (or the reserved all-ones value, as libzstd).
    pub(crate) content_size: Option<u64>,
    pub(crate) window_size: u64,
    pub(crate) block_size_max: u32,
    pub(crate) dict_id: u32,
    pub(crate) checksum: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameStart {
    Frame(FrameHeader),
    Skippable { content_size: u32 },
    // The input is too short: this many bytes are needed.
    NeedMore(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockType {
    Raw,
    Rle,
    Compressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockHeader {
    pub(crate) last: bool,
    pub(crate) block_type: BlockType,
    // Regenerated size for RLE, size in the input otherwise.
    pub(crate) size: u32,
}

impl BlockHeader {
    pub(crate) const fn input_size(self) -> usize {
        match self.block_type {
            BlockType::Rle => 1,
            _ => self.size as usize,
        }
    }
}

/// Describes the frame at the start of `src` like `ZSTD_getFrameContentSize`.
/// `Ok(None)` is an unknown size; a skippable frame has size 0.
pub fn frame_content_size(src: &[u8]) -> Result<Option<u64>, Error> {
    match parse_frame_start(src)? {
        FrameStart::Frame(header) => Ok(header.content_size),
        FrameStart::Skippable { .. } => Ok(Some(0)),
        FrameStart::NeedMore(_) => Err(Error::SrcSizeWrong),
    }
}

pub(crate) const fn legacy_version(magic: u32) -> Option<u8> {
    if magic & 0xFFFF_FF00 != LEGACY_PREFIX {
        return None;
    }
    match (magic & 0xFF) as u8 {
        0x1E => Some(1),
        b @ 0x22..=0x27 => Some(b - 0x20),
        _ => None,
    }
}

// `ZSTD_getFrameHeader`: a zstd frame header, a skippable frame header, or the
// number of bytes missing. `src` may extend past the header.
pub(crate) fn parse_frame_start(src: &[u8]) -> Result<FrameStart, Error> {
    if src.len() < FRAME_HEADER_PREFIX_SIZE {
        // Reject early what cannot become a known magic number.
        let known = |magic: u32, mask: u32| {
            let mut bytes = magic.to_le_bytes();
            let seen = src.len().min(4);
            bytes[..seen].copy_from_slice(&src[..seen]);
            u32::from_le_bytes(bytes) & mask == magic & mask
        };
        if !src.is_empty()
            && !known(MAGIC, u32::MAX)
            && !known(SKIPPABLE_MAGIC, SKIPPABLE_MAGIC_MASK)
        {
            return Err(Error::PrefixUnknown);
        }
        return Ok(FrameStart::NeedMore(FRAME_HEADER_PREFIX_SIZE));
    }
    let magic = read_le32(src, 0);
    if magic != MAGIC {
        if magic & SKIPPABLE_MAGIC_MASK == SKIPPABLE_MAGIC {
            if src.len() < SKIPPABLE_HEADER_SIZE {
                return Ok(FrameStart::NeedMore(SKIPPABLE_HEADER_SIZE));
            }
            return Ok(FrameStart::Skippable {
                content_size: read_le32(src, 4),
            });
        }
        if let Some(version) = legacy_version(magic) {
            return Err(Error::LegacyFormatUnsupported(version));
        }
        return Err(Error::PrefixUnknown);
    }

    let header_size = frame_header_size(src)?;
    if src.len() < header_size {
        return Ok(FrameStart::NeedMore(header_size));
    }
    let descriptor = src[4];
    if descriptor & 0x08 != 0 {
        return Err(Error::FrameParameterUnsupported);
    }
    let single_segment = descriptor >> 5 & 1 != 0;
    let mut pos = FRAME_HEADER_PREFIX_SIZE;
    let mut window_size = 0;
    if !single_segment {
        let window_byte = src[pos];
        pos += 1;
        let window_log = u32::from(window_byte >> 3) + WINDOW_LOG_MIN;
        if window_log > WINDOW_LOG_MAX {
            return Err(Error::WindowTooLarge);
        }
        window_size = 1u64 << window_log;
        window_size += (window_size >> 3) * u64::from(window_byte & 7);
    }
    let dict_id_size = DICT_ID_SIZES[usize::from(descriptor & 3)];
    let dict_id = read_le(src, pos, dict_id_size) as u32;
    pos += dict_id_size;
    let content_size_code = usize::from(descriptor >> 6);
    let mut content_size = read_le(src, pos, CONTENT_SIZE_SIZES[content_size_code]);
    match content_size_code {
        0 if !single_segment => content_size = CONTENT_SIZE_UNKNOWN,
        0 => content_size = u64::from(src[pos]),
        1 => content_size += 256,
        _ => {}
    }
    if single_segment {
        window_size = content_size;
    }
    Ok(FrameStart::Frame(FrameHeader {
        header_size,
        content_size: (content_size != CONTENT_SIZE_UNKNOWN).then_some(content_size),
        window_size,
        block_size_max: window_size.min(u64::from(BLOCK_SIZE_MAX)) as u32,
        dict_id,
        checksum: descriptor >> 2 & 1 != 0,
    }))
}

// `ZSTD_frameHeaderSize`: from the first 5 bytes, magic not checked.
pub(crate) fn frame_header_size(src: &[u8]) -> Result<usize, Error> {
    let Some(&descriptor) = src.get(FRAME_HEADER_PREFIX_SIZE - 1) else {
        return Err(Error::SrcSizeWrong);
    };
    let single_segment = descriptor >> 5 & 1;
    let content_size_code = usize::from(descriptor >> 6);
    Ok(FRAME_HEADER_PREFIX_SIZE
        + usize::from(single_segment ^ 1)
        + DICT_ID_SIZES[usize::from(descriptor & 3)]
        + CONTENT_SIZE_SIZES[content_size_code]
        + usize::from(single_segment == 1 && content_size_code == 0))
}

// `readSkippableFrameSize`: whole frame size, header included.
pub(crate) fn skippable_frame_size(src: &[u8]) -> Result<usize, Error> {
    if src.len() < SKIPPABLE_HEADER_SIZE {
        return Err(Error::SrcSizeWrong);
    }
    let content_size = read_le32(src, 4);
    if content_size
        .checked_add(SKIPPABLE_HEADER_SIZE as u32)
        .is_none()
    {
        return Err(Error::FrameParameterUnsupported);
    }
    let size = SKIPPABLE_HEADER_SIZE + content_size as usize;
    if size > src.len() {
        return Err(Error::SrcSizeWrong);
    }
    Ok(size)
}

pub(crate) fn is_skippable_magic(src: &[u8]) -> bool {
    src.len() >= 4 && read_le32(src, 0) & SKIPPABLE_MAGIC_MASK == SKIPPABLE_MAGIC
}

/// Information about a single zstd or skippable frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    /// Exact compressed byte length of this frame (headers, blocks, and checksum).
    pub compressed_len: usize,
    /// Exact uncompressed byte length if recorded in the frame header.
    pub content_len: Option<u64>,
    /// Upper bound on uncompressed bytes (content_len if known, else sum of block capacities).
    pub max_uncompressed_len: u64,
    /// Whether this is an RFC 8878 skippable frame.
    pub is_skippable: bool,
}

/// Inspects the frame starting at `src` without decompressing it.
pub fn frame_info(src: &[u8]) -> Result<FrameInfo, Error> {
    if is_skippable_magic(src) {
        let compressed_len = skippable_frame_size(src)?;
        return Ok(FrameInfo {
            compressed_len,
            content_len: Some(0),
            max_uncompressed_len: 0,
            is_skippable: true,
        });
    }
    let FrameStart::Frame(header) = parse_frame_start(src)? else {
        return Err(Error::SrcSizeWrong);
    };
    let mut pos = header.header_size;
    let mut max_uncompressed_len = 0u64;
    loop {
        let block = parse_block_header(&src[pos..])?;
        pos += BLOCK_HEADER_SIZE + block.input_size();
        if pos > src.len() {
            return Err(Error::SrcSizeWrong);
        }
        let block_max = match block.block_type {
            BlockType::Raw | BlockType::Rle => u64::from(block.size),
            BlockType::Compressed => u64::from(header.block_size_max),
        };
        max_uncompressed_len = max_uncompressed_len.saturating_add(block_max);
        if block.last {
            break;
        }
    }
    if header.checksum {
        if src.len() - pos < 4 {
            return Err(Error::SrcSizeWrong);
        }
        pos += 4;
    }
    Ok(FrameInfo {
        compressed_len: pos,
        content_len: header.content_size,
        max_uncompressed_len: header.content_size.unwrap_or(max_uncompressed_len),
        is_skippable: false,
    })
}

// `ZSTD_findFrameCompressedSize`: size of the (skippable) frame at the start of `src`,
// from its block headers.
pub(crate) fn find_frame_size(src: &[u8]) -> Result<usize, Error> {
    frame_info(src).map(|info| info.compressed_len)
}

// `ZSTD_getcBlockSize`.
pub(crate) const fn parse_block_header(src: &[u8]) -> Result<BlockHeader, Error> {
    let Some(&[b0, b1, b2]) = src.first_chunk::<BLOCK_HEADER_SIZE>() else {
        return Err(Error::SrcSizeWrong);
    };
    let header = u32::from_le_bytes([b0, b1, b2, 0]);
    let block_type = match header >> 1 & 3 {
        0 => BlockType::Raw,
        1 => BlockType::Rle,
        2 => BlockType::Compressed,
        _ => return Err(Error::CorruptionDetected),
    };
    Ok(BlockHeader {
        last: header & 1 != 0,
        block_type,
        size: header >> 3,
    })
}

pub(crate) fn read_le32(src: &[u8], at: usize) -> u32 {
    read_le(src, at, 4) as u32
}

// `size` <= 8.
fn read_le(src: &[u8], at: usize, size: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes[..size].copy_from_slice(&src[at..at + size]);
    u64::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests;
