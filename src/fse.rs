//! FSE normalized counts and decoding tables. One entry type serves the
//! sequence tables (libzstd's `ZSTD_seqSymbol`) and the Huffman weight tables
//! (identity `base`, no extra bits).

#![allow(clippy::unreadable_literal)] // libzstd's tables as published

use crate::bits::{BitReader, Status};
use crate::error::Error;

const MIN_TABLE_LOG: i32 = 5;
const NCOUNT_TABLE_LOG_MAX: i32 = 15;
const MAX_SYMBOLS: usize = 256;
const WEIGHTS_TABLE_LOG_MAX: u32 = 6;
// Largest table of any use (the sequence tables).
const SPREAD_MAX: usize = 512;
const IDENTITY: [u32; MAX_SYMBOLS] = {
    let mut identity = [0; MAX_SYMBOLS];
    let mut symbol = 0;
    while symbol < MAX_SYMBOLS {
        identity[symbol] = symbol as u32;
        symbol += 1;
    }
    identity
};

pub(crate) const LL_MAX_SYMBOL: u32 = 35;
pub(crate) const ML_MAX_SYMBOL: u32 = 52;
pub(crate) const OF_MAX_SYMBOL: u32 = 31;
pub(crate) const LL_TABLE_LOG_MAX: u32 = 9;
pub(crate) const ML_TABLE_LOG_MAX: u32 = 9;
pub(crate) const OF_TABLE_LOG_MAX: u32 = 8;

pub(crate) const LL_BASE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    0x80, 0x100, 0x200, 0x400, 0x800, 0x1000, 0x2000, 0x4000, 0x8000, 0x10000,
];
pub(crate) const LL_EXTRA_BITS: [u8; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
pub(crate) const ML_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 0x83, 0x103, 0x203,
    0x403, 0x803, 0x1003, 0x2003, 0x4003, 0x8003, 0x10003,
];
pub(crate) const ML_EXTRA_BITS: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];
pub(crate) const OF_BASE: [u32; 32] = [
    0, 1, 1, 5, 0xD, 0x1D, 0x3D, 0x7D, 0xFD, 0x1FD, 0x3FD, 0x7FD, 0xFFD, 0x1FFD, 0x3FFD, 0x7FFD,
    0xFFFD, 0x1FFFD, 0x3FFFD, 0x7FFFD, 0xFFFFD, 0x1FFFFD, 0x3FFFFD, 0x7FFFFD, 0xFFFFFD, 0x1FFFFFD,
    0x3FFFFFD, 0x7FFFFFD, 0xFFFFFFD, 0x1FFFFFFD, 0x3FFFFFFD, 0x7FFFFFFD,
];
pub(crate) const OF_EXTRA_BITS: [u8; 32] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 30, 31,
];

const LL_DEFAULT_NORM: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
const ML_DEFAULT_NORM: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
const OF_DEFAULT_NORM: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

pub(crate) static LL_DEFAULT_TABLE: FseTable<512> =
    FseTable::from_norm(&LL_DEFAULT_NORM, 6, &LL_BASE, &LL_EXTRA_BITS);
pub(crate) static ML_DEFAULT_TABLE: FseTable<512> =
    FseTable::from_norm(&ML_DEFAULT_NORM, 6, &ML_BASE, &ML_EXTRA_BITS);
pub(crate) static OF_DEFAULT_TABLE: FseTable<256> =
    FseTable::from_norm(&OF_DEFAULT_NORM, 5, &OF_BASE, &OF_EXTRA_BITS);

// A decoding table cell: after reading `nb_bits`, the next state is
// `next_state + bits` and the decoded value is `base_value + extra bits`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C, align(8))]
pub(crate) struct SeqSymbol {
    pub(crate) next_state: u16,
    pub(crate) nb_additional_bits: u8,
    pub(crate) nb_bits: u8,
    pub(crate) base_value: u32,
}

impl SeqSymbol {
    // The cell read as one 64-bit word, the fields taken apart by shifts: they stay in one register
    // instead of four, which spills less in the sequence loop.
    #[inline(always)]
    pub(crate) fn load_whole(self) -> Self {
        if cfg!(target_endian = "little") {
            // SAFETY: `SeqSymbol` is `repr(C)`, 8 bytes without padding.
            let word = unsafe { core::mem::transmute::<Self, u64>(self) };
            Self {
                next_state: word as u16,
                nb_additional_bits: (word >> 16) as u8,
                nb_bits: (word >> 24) as u8,
                base_value: (word >> 32) as u32,
            }
        } else {
            self
        }
    }
}

// `1 << table_log` cells used out of `N`.
#[derive(Debug, Clone)]
pub(crate) struct FseTable<const N: usize> {
    pub(crate) table_log: u32,
    pub(crate) entries: [SeqSymbol; N],
}

// Result of [`read_ncount`]: `max_symbol` is the last symbol described.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NCount {
    pub(crate) table_log: u32,
    pub(crate) max_symbol: u32,
    pub(crate) bytes_read: usize,
}

impl<const N: usize> FseTable<N> {
    pub(crate) const EMPTY: Self = Self {
        table_log: 0,
        entries: [SeqSymbol {
            next_state: 0,
            nb_additional_bits: 0,
            nb_bits: 0,
            base_value: 0,
        }; N],
    };

    pub(crate) const fn from_norm(
        norm: &[i16],
        table_log: u32,
        base: &[u32],
        extra: &[u8],
    ) -> Self {
        let mut table = Self::EMPTY;
        table.build(norm, table_log, base, extra);
        table
    }

    // Builds the table of a valid distribution (`read_ncount` output, summing
    // to `1 << table_log`, `table_log` at most log2(N)). `base` and `extra`
    // give each symbol's baseline and extra bits.
    pub(crate) const fn build(&mut self, norm: &[i16], table_log: u32, base: &[u32], extra: &[u8]) {
        let table_size = 1usize << table_log;
        assert!(table_size <= N && N <= SPREAD_MAX && norm.len() <= MAX_SYMBOLS);
        let mut symbol_next = [0u16; MAX_SYMBOLS];
        // Cells at the end of the table hold the symbols of count -1.
        let mut high_threshold = table_size - 1;
        let mut symbol = 0;
        while symbol < norm.len() {
            if norm[symbol] == -1 {
                self.entries[high_threshold].base_value = symbol as u32;
                high_threshold = high_threshold.wrapping_sub(1);
                symbol_next[symbol] = 1;
            } else {
                symbol_next[symbol] = norm[symbol] as u16;
            }
            symbol += 1;
        }

        let step = (table_size >> 1) + (table_size >> 3) + 3;
        let mask = table_size - 1;
        if high_threshold == mask {
            // Symbols in order, 8 bytes at a time (the next symbol overwrites the excess),
            // then scattered two per iteration: no branch depends on the counts.
            let mut spread = [0u8; SPREAD_MAX + 8];
            let (mut at, mut symbol) = (0, 0);
            while symbol < norm.len() {
                let mut offset = 0;
                while offset < norm[symbol] as usize || offset == 0 {
                    let (_, rest) = spread.split_at_mut(at + offset);
                    *rest.first_chunk_mut::<8>().unwrap() = [symbol as u8; 8];
                    offset += 8;
                }
                at += norm[symbol] as usize;
                symbol += 1;
            }
            let mut position = 0;
            let mut cell = 0;
            while cell < table_size {
                self.entries[position].base_value = spread[cell] as u32;
                self.entries[(position + step) & mask].base_value = spread[cell + 1] as u32;
                position = (position + 2 * step) & mask;
                cell += 2;
            }
        } else {
            let mut position = 0;
            let mut symbol = 0;
            while symbol < norm.len() {
                let mut placed = 0;
                while placed < norm[symbol] {
                    self.entries[position].base_value = symbol as u32;
                    position = (position + step) & mask;
                    while position > high_threshold {
                        position = (position + step) & mask;
                    }
                    placed += 1;
                }
                symbol += 1;
            }
        }

        let mut state = 0;
        while state < table_size {
            let symbol = self.entries[state].base_value as usize;
            let next_state = symbol_next[symbol] as u32;
            symbol_next[symbol] += 1;
            let nb_bits = table_log - (u32::BITS - 1 - next_state.leading_zeros());
            self.entries[state] = SeqSymbol {
                next_state: ((next_state << nb_bits) - table_size as u32) as u16,
                nb_additional_bits: extra[symbol],
                nb_bits: nb_bits as u8,
                base_value: base[symbol],
            };
            state += 1;
        }
        self.table_log = table_log;
    }

    // Copies a table of the same capacity: only the cells in use.
    pub(crate) fn copy_from(&mut self, other: &Self) {
        let cells = 1 << other.table_log;
        self.table_log = other.table_log;
        self.entries[..cells].copy_from_slice(&other.entries[..cells]);
    }

    // Single-cell table for RLE mode: always the same symbol, no state bits.
    pub(crate) fn set_rle(&mut self, base_value: u32, nb_additional_bits: u8) {
        self.table_log = 0;
        self.entries[0] = SeqSymbol {
            next_state: 0,
            nb_additional_bits,
            nb_bits: 0,
            base_value,
        };
    }
}

// `FSE_readNCount`: parses a normalized count header into `norm` (its length
// is the largest symbol count allowed). Every failure is `CorruptionDetected`,
// which is what libzstd's callers report.
pub(crate) fn read_ncount(norm: &mut [i16], src: &[u8]) -> Result<NCount, Error> {
    if src.len() < 8 {
        // The parser reads 4 bytes at a time: run it on a zero-padded copy.
        let mut padded = [0; 8];
        padded[..src.len()].copy_from_slice(src);
        let ncount = read_ncount(norm, &padded)?;
        return if ncount.bytes_read > src.len() {
            Err(Error::CorruptionDetected)
        } else {
            Ok(ncount)
        };
    }
    norm.fill(0);
    let len = src.len();
    let symbol_limit = norm.len() as u32;
    let read_le32 = |at: usize| u32::from_le_bytes(src[at..at + 4].try_into().unwrap());
    let mut ip = 0;
    let mut bit_stream = read_le32(0);
    let mut nb_bits = (bit_stream & 0xF) as i32 + MIN_TABLE_LOG;
    if nb_bits > NCOUNT_TABLE_LOG_MAX {
        return Err(Error::CorruptionDetected);
    }
    bit_stream >>= 4;
    let mut bit_count = 4i32;
    let table_log = nb_bits as u32;
    let mut remaining = (1i32 << nb_bits) + 1;
    let mut threshold = 1i32 << nb_bits;
    nb_bits += 1;
    let mut symbol = 0u32;
    let mut previous_is_zero = false;

    // Moves `ip` forward by the whole bytes consumed, keeping the 4-byte window
    // inside `src`; the bit count wraps like C's int arithmetic masked by 31.
    let advance = |ip: &mut usize, bit_count: &mut i32| {
        if *ip + 7 <= len || *ip + (*bit_count >> 3) as usize + 4 <= len {
            *ip += (*bit_count >> 3) as usize;
            *bit_count &= 7;
        } else {
            *bit_count -= 8 * (len - 4 - *ip) as i32;
            *bit_count &= 31;
            *ip = len - 4;
        }
    };

    loop {
        if previous_is_zero {
            // Runs of zero counts: 2-bit repeat codes, 0b11 meaning "3 more".
            let count_repeats = |bit_stream: u32| (!bit_stream | 0x8000_0000).trailing_zeros() >> 1;
            let mut repeats = count_repeats(bit_stream);
            while repeats >= 12 {
                symbol += 36;
                if ip + 7 <= len {
                    ip += 3;
                } else {
                    bit_count -= 8 * (len as i32 - 7 - ip as i32);
                    bit_count &= 31;
                    ip = len - 4;
                }
                bit_stream = read_le32(ip) >> bit_count;
                repeats = count_repeats(bit_stream);
            }
            symbol += 3 * repeats;
            bit_stream >>= 2 * repeats;
            bit_count += 2 * repeats as i32;
            symbol += bit_stream & 3;
            bit_count += 2;
            if symbol >= symbol_limit {
                break;
            }
            advance(&mut ip, &mut bit_count);
            bit_stream = read_le32(ip) >> bit_count;
        }

        let max = (2 * threshold - 1) - remaining;
        let low_mask = threshold as u32 - 1;
        let mut count;
        if (bit_stream & low_mask) < max as u32 {
            count = (bit_stream & low_mask) as i32;
            bit_count += nb_bits - 1;
        } else {
            count = (bit_stream & (2 * threshold as u32 - 1)) as i32;
            if count >= threshold {
                count -= max;
            }
            bit_count += nb_bits;
        }
        count -= 1;
        remaining -= count.abs();
        norm[symbol as usize] = count as i16;
        symbol += 1;
        previous_is_zero = count == 0;

        if remaining < threshold {
            if remaining <= 1 {
                break;
            }
            nb_bits = (u32::BITS - (remaining as u32).leading_zeros()) as i32;
            threshold = 1 << (nb_bits - 1);
        }
        if symbol >= symbol_limit {
            break;
        }
        advance(&mut ip, &mut bit_count);
        bit_stream = read_le32(ip) >> bit_count;
    }

    if remaining != 1 || symbol > symbol_limit || bit_count > 32 {
        return Err(Error::CorruptionDetected);
    }
    Ok(NCount {
        table_log,
        max_symbol: symbol - 1,
        bytes_read: ip + ((bit_count + 7) >> 3) as usize,
    })
}

// Words of libzstd's FSE workspace. Huffman weights get one sized for table log 6
// and symbols 0..=11 (`HUF_READ_STATS_WORKSPACE_SIZE_U32`): a header describing
// more is rejected even if the stream never uses the extra symbols.
const fn workspace_words(table_log: u32, max_symbol: u32) -> u32 {
    (1 << table_log) + (2 * (max_symbol + 1) + (1 << table_log) + 8).div_ceil(4)
}

// `FSE_decompress_wksp` for byte symbols with a table log of at most 6, as
// used for Huffman weights. Returns the number of symbols written to `dst`.
pub(crate) fn decompress_weights(dst: &mut [u8], src: &[u8]) -> Result<usize, Error> {
    let mut norm = [0i16; MAX_SYMBOLS];
    let ncount = read_ncount(&mut norm, src)?;
    if ncount.table_log > WEIGHTS_TABLE_LOG_MAX
        || workspace_words(ncount.table_log, ncount.max_symbol)
            > workspace_words(WEIGHTS_TABLE_LOG_MAX, 11)
    {
        return Err(Error::CorruptionDetected);
    }
    let mut table = FseTable::<{ 1 << WEIGHTS_TABLE_LOG_MAX }>::EMPTY;
    let counts = &norm[..=ncount.max_symbol as usize];
    table.build(counts, ncount.table_log, &IDENTITY, &[0; MAX_SYMBOLS]);
    let mut bits = BitReader::new(&src[ncount.bytes_read..]).ok_or(Error::CorruptionDetected)?;

    let init_state = |bits: &mut BitReader<'_>| {
        let state = bits.read_bits(ncount.table_log) as usize;
        bits.reload();
        state
    };
    let decode = |state: &mut usize, bits: &mut BitReader<'_>| {
        let cell = table.entries[*state];
        *state = usize::from(cell.next_state) + bits.read_bits(u32::from(cell.nb_bits)) as usize;
        cell.base_value as u8
    };
    let mut state1 = init_state(&mut bits);
    let mut state2 = init_state(&mut bits);
    if bits.reload() == Status::Overflow {
        return Err(Error::CorruptionDetected);
    }

    let mut op = 0;
    while bits.reload() == Status::Unfinished && op + 3 < dst.len() {
        dst[op] = decode(&mut state1, &mut bits);
        dst[op + 1] = decode(&mut state2, &mut bits);
        dst[op + 2] = decode(&mut state1, &mut bits);
        dst[op + 3] = decode(&mut state2, &mut bits);
        op += 4;
    }
    // Past the last reload the two states drain alternately; the stream ends on overflow.
    loop {
        if op + 2 > dst.len() {
            return Err(Error::DstSizeTooSmall);
        }
        dst[op] = decode(&mut state1, &mut bits);
        op += 1;
        if bits.reload() == Status::Overflow {
            dst[op] = decode(&mut state2, &mut bits);
            return Ok(op + 1);
        }
        if op + 2 > dst.len() {
            return Err(Error::DstSizeTooSmall);
        }
        dst[op] = decode(&mut state2, &mut bits);
        op += 1;
        if bits.reload() == Status::Overflow {
            dst[op] = decode(&mut state1, &mut bits);
            return Ok(op + 1);
        }
    }
}

#[cfg(test)]
mod tests;
