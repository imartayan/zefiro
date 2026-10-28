//! Huffman literals decoder, a port of libzstd's `huf_decompress.c`: the tree
//! description (`HUF_readStats`), the X1 (one symbol per lookup) and X2 (up to
//! two) tables, 1-stream and 4-stream bodies, and the 4-stream fast path whose
//! stop points and lenient verdicts (PLAN.md 4.2-1, 4.2-2) are reproduced.
//!
//! Errors inside a literals block are all `corruption_detected` for the caller,
//! so this module reports `CorruptionDetected` (or `SrcSizeWrong` and
//! `DstSizeTooSmall` where C does, for easy comparison) and nothing finer.
//! Bounds are checked by indexing; the invariants that make them redundant are
//! noted at the fast loops for the unsafe pass of Phase 2.

use crate::bits::{BitReader, Status};
use crate::error::Error;
use crate::fse;

const TABLE_LOG_MAX: u32 = 12;
const TABLE_SIZE: usize = 1 << TABLE_LOG_MAX;
const RANK_COUNT: usize = TABLE_LOG_MAX as usize + 1;
const SYMBOL_COUNT: usize = 256;
// The fast path needs exactly this table log; trees up to this depth are scaled to it.
const FAST_TABLE_LOG: u32 = 11;
const JUMP_TABLE_SIZE: usize = 6;
const MIN_STREAM_SIZE_FAST: usize = 8;
const STREAMS: usize = 4;

// libzstd's `algoTime`: (table build time, decode time per 256 bytes) for the
// single and double symbol decoders, by compression ratio in sixteenths.
const ALGO_TIME: [[(u32, u32); 2]; 16] = [
    [(0, 0), (1, 1)],
    [(0, 0), (1, 1)],
    [(150, 216), (381, 119)],
    [(170, 205), (514, 112)],
    [(177, 199), (539, 110)],
    [(197, 194), (644, 107)],
    [(221, 192), (735, 107)],
    [(256, 189), (881, 106)],
    [(359, 188), (1167, 109)],
    [(582, 187), (1570, 114)],
    [(688, 187), (1712, 122)],
    [(825, 186), (1965, 136)],
    [(976, 185), (2131, 150)],
    [(1180, 186), (2070, 175)],
    [(1377, 185), (1731, 202)],
    [(1412, 185), (1695, 202)],
];

// nb_bits in the low byte, the symbol in the high one (`HUF_DEltX1`): one `u16`, so that
// runs of equal cells are written with wide stores.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct EltX1(u16);

impl EltX1 {
    fn new(nb_bits: u8, byte: u8) -> Self {
        Self(u16::from(nb_bits) | u16::from(byte) << 8)
    }

    fn nb_bits(self) -> u8 {
        self.0 as u8
    }

    fn byte(self) -> u8 {
        (self.0 >> 8) as u8
    }
}

// `sequence` holds one or two symbols (low byte first), `length` how many.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct EltX2 {
    pub(crate) sequence: u16,
    pub(crate) nb_bits: u8,
    pub(crate) length: u8,
}

// The decoding table kept between blocks (libzstd's `hufTable`): one of the
// two kinds is valid, the other array is stale.
#[derive(Clone)]
pub(crate) struct HufTable {
    is_x2: bool,
    table_log: u32,
    x1: [EltX1; TABLE_SIZE],
    x2: [EltX2; TABLE_SIZE],
}

impl core::fmt::Debug for HufTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HufTable")
            .field("is_x2", &self.is_x2)
            .field("table_log", &self.table_log)
            .finish_non_exhaustive()
    }
}

struct Stats {
    bytes_read: usize,
    nb_symbols: usize,
    table_log: u32,
}

// State of the fast loop, `HUF_DecompressFastArgs` with positions as offsets
// in the input (`ilowest` is 0) and in the output.
struct FastArgs {
    ip: [usize; STREAMS],
    op: [usize; STREAMS],
    bits: [u64; STREAMS],
    iend: [usize; STREAMS],
}

trait Variant {
    type Elt: Copy;
    // The strict 4-stream loop runs while `op4 < dst.len() - STRICT_MARGIN`.
    const STRICT_MARGIN: usize;

    fn table(table: &HufTable) -> &[Self::Elt; TABLE_SIZE];
    // One lookup, writes at `dst[op..]` and returns the number of bytes.
    fn decode_symbol(
        table: &[Self::Elt; TABLE_SIZE],
        log: u32,
        dst: &mut [u8],
        op: usize,
        bits: &mut BitReader<'_>,
    ) -> usize;
    // `HUF_decodeStreamX*`: fills `dst[op..end]` from `bits`, without an end check.
    fn decode_stream(
        table: &[Self::Elt; TABLE_SIZE],
        log: u32,
        dst: &mut [u8],
        op: usize,
        end: usize,
        bits: &mut BitReader<'_>,
    );
    // `HUF_decompress4X*_usingDTable_internal_fast_c_loop`.
    fn fast_loop(table: &[Self::Elt; TABLE_SIZE], src: &[u8], dst: &mut [u8], args: &mut FastArgs);
}

struct X1;
struct X2;

impl HufTable {
    pub(crate) fn boxed() -> alloc::boxed::Box<Self> {
        alloc::boxed::Box::new(Self {
            is_x2: false,
            table_log: 0,
            x1: [EltX1::default(); TABLE_SIZE],
            x2: [EltX2::default(); TABLE_SIZE],
        })
    }

    // `HUF_readDTableX1_wksp`: returns the bytes of the tree description.
    pub(crate) fn read_x1(&mut self, src: &[u8]) -> Result<usize, Error> {
        let mut weights = [0; SYMBOL_COUNT];
        let mut rank_stats = [0; RANK_COUNT];
        let stats = read_stats(&mut weights, &mut rank_stats, src)?;
        let (sorted, rank_start) = sort_by_weight(&weights[..stats.nb_symbols], &rank_stats);

        // A symbol of weight w owns 2^(w-1) cells of the scaled table and reads
        // tree_log + 1 - w bits.
        let table_log = stats.table_log.max(FAST_TABLE_LOG);
        let scale = (table_log - stats.table_log) as usize;
        let mut cells = &mut self.x1[..1 << table_log];
        for w in 1..=stats.table_log as usize {
            let symbols = &sorted[rank_start[w] as usize..rank_start[w + 1] as usize];
            let length = 1 << (w - 1 + scale);
            let nb_bits = (stats.table_log as usize + 1 - w) as u8;
            let (run, rest) = cells.split_at_mut(symbols.len() * length);
            fill_runs(run, symbols, nb_bits, length);
            cells = rest;
        }
        self.is_x2 = false;
        self.table_log = table_log;
        Ok(stats.bytes_read)
    }

    // `HUF_readDTableX2_wksp`: returns the bytes of the tree description.
    pub(crate) fn read_x2(&mut self, src: &[u8]) -> Result<usize, Error> {
        let mut weights = [0; SYMBOL_COUNT];
        let mut rank_stats = [0; RANK_COUNT];
        let stats = read_stats(&mut weights, &mut rank_stats, src)?;
        let tree_log = stats.table_log as usize;
        let table_log = if stats.table_log <= FAST_TABLE_LOG {
            FAST_TABLE_LOG
        } else {
            TABLE_LOG_MAX
        };
        let mut max_weight = tree_log;
        while rank_stats[max_weight] == 0 {
            max_weight -= 1;
        }

        let (sorted, rank_start) = sort_by_weight(&weights[..stats.nb_symbols], &rank_stats);

        // `rank_val[consumed][w]`: first cell of weight w once `consumed` bits are known.
        let mut rank_val = [[0; RANK_COUNT]; TABLE_LOG_MAX as usize];
        let rescale = table_log as usize - tree_log;
        let mut next = 0;
        for w in 1..=max_weight {
            rank_val[0][w] = next;
            next += rank_stats[w] << (w + rescale - 1);
        }
        let min_bits = tree_log + 1 - max_weight;
        let heaviest_first = rank_val[0];
        let rows = rank_val.iter_mut().enumerate();
        for (consumed, row) in rows.take(table_log as usize - min_bits + 1).skip(min_bits) {
            let cells = row[1..=max_weight].iter_mut();
            for (cell, &first) in cells.zip(&heaviest_first[1..=max_weight]) {
                *cell = first >> consumed;
            }
        }

        let fill = Fill {
            table_log,
            sorted: &sorted,
            rank_start: &rank_start,
            rank_val: &rank_val,
            nb_bits_baseline: tree_log + 1,
        };
        fill.run(&mut self.x2, max_weight);
        self.is_x2 = true;
        self.table_log = table_log;
        Ok(stats.bytes_read)
    }

    // `HUF_readDTableX1_wksp` then `HUF_decompress1X1_DCtx_wksp`.
    pub(crate) fn read_and_decompress_1x(
        &mut self,
        dst: &mut [u8],
        src: &[u8],
    ) -> Result<(), Error> {
        let tree_size = self.read_x1(src)?;
        if tree_size >= src.len() {
            return Err(Error::SrcSizeWrong);
        }
        self.decompress_1x(dst, &src[tree_size..])
    }

    // `HUF_decompress4X_hufOnly_wksp`: picks the table kind by `select_decoder`.
    pub(crate) fn read_and_decompress_4x(
        &mut self,
        dst: &mut [u8],
        src: &[u8],
    ) -> Result<(), Error> {
        if dst.is_empty() {
            return Err(Error::DstSizeTooSmall);
        }
        if src.is_empty() {
            return Err(Error::CorruptionDetected);
        }
        let tree_size = if select_decoder(dst.len(), src.len()) {
            self.read_x2(src)?
        } else {
            self.read_x1(src)?
        };
        if tree_size >= src.len() {
            return Err(Error::SrcSizeWrong);
        }
        self.decompress_4x(dst, &src[tree_size..])
    }

    // `HUF_decompress1X_usingDTable`: `src` is one stream, `dst` its exact output.
    pub(crate) fn decompress_1x(&self, dst: &mut [u8], src: &[u8]) -> Result<(), Error> {
        if self.is_x2 {
            decompress_1x::<X2>(self, dst, src)
        } else {
            decompress_1x::<X1>(self, dst, src)
        }
    }

    // `HUF_decompress4X_usingDTable`: `src` is the jump table and four streams.
    pub(crate) fn decompress_4x(&self, dst: &mut [u8], src: &[u8]) -> Result<(), Error> {
        if self.is_x2 {
            decompress_4x::<X2>(self, dst, src)
        } else {
            decompress_4x::<X1>(self, dst, src)
        }
    }
}

// `HUF_selectDecoder`: true for the double-symbol decoder. `dst_size` is at most 128 KiB.
pub(crate) fn select_decoder(dst_size: usize, src_size: usize) -> bool {
    let quantized = if src_size >= dst_size {
        15
    } else {
        src_size * 16 / dst_size
    };
    let decode_256_count = (dst_size >> 8) as u32;
    let time = |(table, decode_256): (u32, u32)| table + decode_256 * decode_256_count;
    let single = time(ALGO_TIME[quantized][0]);
    let mut double = time(ALGO_TIME[quantized][1]);
    // Small advantage to the table using less memory.
    double += double >> 5;
    double < single
}

// `HUF_readStats`: the weights of all symbols but the last (implied by the
// total being a power of two), and how many symbols have each weight.
fn read_stats(
    weights: &mut [u8; SYMBOL_COUNT],
    rank_stats: &mut [u32; RANK_COUNT],
    src: &[u8],
) -> Result<Stats, Error> {
    let first = usize::from(*src.first().ok_or(Error::SrcSizeWrong)?);
    let (header_size, weight_count);
    if first >= 128 {
        // Raw 4-bit weights, at most 128 of them.
        weight_count = first - 127;
        header_size = weight_count.div_ceil(2);
        if header_size + 1 > src.len() {
            return Err(Error::SrcSizeWrong);
        }
        for n in (0..weight_count).step_by(2) {
            weights[n] = src[1 + n / 2] >> 4;
            weights[n + 1] = src[1 + n / 2] & 15;
        }
    } else {
        header_size = first;
        if header_size + 1 > src.len() {
            return Err(Error::SrcSizeWrong);
        }
        // The last weight is implied: at most 255 are decoded.
        weight_count =
            fse::decompress_weights(&mut weights[..SYMBOL_COUNT - 1], &src[1..=header_size])?;
    }

    if weights[..weight_count]
        .iter()
        .any(|&weight| u32::from(weight) > TABLE_LOG_MAX)
    {
        return Err(Error::CorruptionDetected);
    }
    let mut counts = Lanes::default();
    for &weight in &weights[..weight_count] {
        counts.bump(weight);
    }
    let mut weight_total = 0u32;
    for (weight, rank) in rank_stats.iter_mut().enumerate() {
        *rank = u32::from(counts.get(weight as u8));
        weight_total += *rank * ((1 << weight) >> 1);
    }
    if weight_total == 0 {
        return Err(Error::CorruptionDetected);
    }
    let table_log = weight_total.ilog2() + 1;
    if table_log > TABLE_LOG_MAX {
        return Err(Error::CorruptionDetected);
    }
    let rest = (1 << table_log) - weight_total;
    if !rest.is_power_of_two() {
        return Err(Error::CorruptionDetected);
    }
    let last_weight = rest.ilog2() + 1;
    weights[weight_count] = last_weight as u8;
    rank_stats[last_weight as usize] += 1;
    // At least two symbols of weight 1, and an even number of them.
    if rank_stats[1] < 2 || rank_stats[1] & 1 == 1 {
        return Err(Error::CorruptionDetected);
    }
    Ok(Stats {
        bytes_read: header_size + 1,
        nb_symbols: weight_count + 1,
        table_log,
    })
}

// One byte counter per weight below 16, in two registers: counting in memory
// makes runs of one weight wait on store forwarding.
#[derive(Default)]
struct Lanes([u64; 2]);

impl Lanes {
    #[inline(always)]
    fn add(&mut self, weight: u8, count: u64) {
        let word = count << (8 * (weight & 7));
        let (low, high) = if weight & 8 == 0 {
            (word, 0)
        } else {
            (0, word)
        };
        self.0[0] = self.0[0].wrapping_add(low);
        self.0[1] = self.0[1].wrapping_add(high);
    }

    #[inline(always)]
    fn bump(&mut self, weight: u8) {
        const ONE: [[u64; 2]; 16] = {
            let mut one = [[0; 2]; 16];
            let mut weight = 0;
            while weight < 16 {
                one[weight][weight / 8] = 1 << (8 * (weight % 8));
                weight += 1;
            }
            one
        };
        let [low, high] = ONE[usize::from(weight & 15)];
        self.0[0] = self.0[0].wrapping_add(low);
        self.0[1] = self.0[1].wrapping_add(high);
    }

    #[inline(always)]
    fn get(&self, weight: u8) -> u8 {
        let half = if weight & 8 == 0 {
            self.0[0]
        } else {
            self.0[1]
        };
        (half >> (8 * (weight & 7))) as u8
    }
}

// Symbols ordered by weight (weight 0 first) and where each weight starts in
// that order, `rank_start[RANK_COUNT]` being the end.
fn sort_by_weight(
    weights: &[u8],
    rank_stats: &[u32; RANK_COUNT],
) -> ([u8; SYMBOL_COUNT], [u32; RANK_COUNT + 1]) {
    // Next free slot of each weight in a byte lane. A slot reaches 256 only after the
    // last symbol, so the carry into the next lane is never read.
    let mut next_slot = Lanes::default();
    let mut rank_start = [0; RANK_COUNT + 1];
    for (weight, &count) in rank_stats.iter().enumerate() {
        next_slot.add(weight as u8, u64::from(rank_start[weight]));
        rank_start[weight + 1] = rank_start[weight] + count;
    }
    let mut sorted = [0; SYMBOL_COUNT];
    for (symbol, &weight) in weights.iter().enumerate() {
        sorted[usize::from(next_slot.get(weight))] = symbol as u8;
        next_slot.bump(weight);
    }
    (sorted, rank_start)
}

// Writes `length` cells of `nb_bits` and each symbol, in order, starting at `cells`.
fn fill_runs(cells: &mut [EltX1], symbols: &[u8], nb_bits: u8, length: usize) {
    // Short runs are fixed-size arrays, stored as one wide write each.
    fn short_runs<const LENGTH: usize>(cells: &mut [EltX1], symbols: &[u8], nb_bits: u8) {
        let runs = cells.as_chunks_mut::<LENGTH>().0.iter_mut();
        for (run, &byte) in runs.zip(symbols) {
            *run = [EltX1::new(nb_bits, byte); LENGTH];
        }
    }
    match length {
        1 => short_runs::<1>(cells, symbols, nb_bits),
        2 => short_runs::<2>(cells, symbols, nb_bits),
        4 => short_runs::<4>(cells, symbols, nb_bits),
        8 => short_runs::<8>(cells, symbols, nb_bits),
        _ => {
            for (run, &byte) in cells.chunks_exact_mut(length).zip(symbols) {
                for chunk in run.as_chunks_mut::<16>().0 {
                    *chunk = [EltX1::new(nb_bits, byte); 16];
                }
            }
        }
    }
}

// Inputs of `HUF_fillDTableX2`.
struct Fill<'a> {
    table_log: u32,
    sorted: &'a [u8; SYMBOL_COUNT],
    rank_start: &'a [u32; RANK_COUNT + 1],
    rank_val: &'a [[u32; RANK_COUNT]; TABLE_LOG_MAX as usize],
    nb_bits_baseline: usize,
}

impl Fill<'_> {
    // `HUF_fillDTableX2`: heavier symbols first, each with the second symbols that fit after it.
    fn run(&self, table: &mut [EltX2; TABLE_SIZE], max_weight: usize) {
        let table_log = self.table_log as usize;
        let scale_log = self.nb_bits_baseline as isize - table_log as isize;
        let min_bits = self.nb_bits_baseline - max_weight;
        for w in 1..=max_weight {
            let symbols = self.symbols_of(w);
            let nb_bits = self.nb_bits_baseline - w;
            if table_log - nb_bits >= min_bits {
                let length = 1 << (table_log - nb_bits);
                let min_weight = (nb_bits as isize + scale_log).max(1) as usize;
                let mut start = self.rank_val[0][w] as usize;
                for &first in symbols {
                    self.level2(&mut table[start..], nb_bits, min_weight, max_weight, first);
                    start += length;
                }
            } else {
                let at = self.rank_val[0][w] as usize;
                fill_weight(&mut table[at..], symbols, nb_bits, table_log, None);
            }
        }
    }

    // `HUF_fillDTableX2Level2`: the cells after `first`, which consumed `consumed_bits`.
    fn level2(
        &self,
        table: &mut [EltX2],
        consumed_bits: usize,
        min_weight: usize,
        max_weight: usize,
        first: u8,
    ) {
        let rank_val = &self.rank_val[consumed_bits];
        if min_weight > 1 {
            // Combined weights too large for a second symbol: `first` alone.
            let single = EltX2 {
                sequence: u16::from(first),
                nb_bits: consumed_bits as u8,
                length: 1,
            };
            table[..rank_val[min_weight] as usize].fill(single);
        }
        for (w, &at) in rank_val
            .iter()
            .enumerate()
            .take(max_weight + 1)
            .skip(min_weight)
        {
            let nb_bits = self.nb_bits_baseline - w + consumed_bits;
            fill_weight(
                &mut table[at as usize..],
                self.symbols_of(w),
                nb_bits,
                self.table_log as usize,
                Some(first),
            );
        }
    }

    fn symbols_of(&self, weight: usize) -> &[u8] {
        &self.sorted[self.rank_start[weight] as usize..self.rank_start[weight + 1] as usize]
    }
}

// `HUF_fillDTableX2ForWeight`: each symbol takes `2^(table_log - nb_bits)`
// cells, alone or as the second symbol after `first`.
fn fill_weight(
    table: &mut [EltX2],
    symbols: &[u8],
    nb_bits: usize,
    table_log: usize,
    first: Option<u8>,
) {
    let length = 1 << (table_log - nb_bits);
    for (index, &symbol) in symbols.iter().enumerate() {
        let (sequence, count) = match first {
            None => (u16::from(symbol), 1),
            Some(first) => (u16::from(first) + (u16::from(symbol) << 8), 2),
        };
        let elt = EltX2 {
            sequence,
            nb_bits: nb_bits as u8,
            length: count,
        };
        table[index * length..(index + 1) * length].fill(elt);
    }
}

fn decompress_1x<V: Variant>(table: &HufTable, dst: &mut [u8], src: &[u8]) -> Result<(), Error> {
    let mut bits = BitReader::new(src).ok_or(Error::CorruptionDetected)?;
    V::decode_stream(
        V::table(table),
        table.table_log,
        dst,
        0,
        dst.len(),
        &mut bits,
    );
    if bits.end_of_stream() {
        Ok(())
    } else {
        Err(Error::CorruptionDetected)
    }
}

// Fast path when libzstd takes it, otherwise the strict body.
fn decompress_4x<V: Variant>(table: &HufTable, dst: &mut [u8], src: &[u8]) -> Result<(), Error> {
    if decompress_4x_fast::<V>(table, dst, src)? {
        return Ok(());
    }
    decompress_4x_strict::<V>(table, dst, src)
}

fn stream_lengths(src: &[u8]) -> [usize; STREAMS] {
    let le16 = |at: usize| usize::from(u16::from_le_bytes([src[at], src[at + 1]]));
    let (length1, length2, length3) = (le16(0), le16(2), le16(4));
    // Wraps like C's size_t when the jump table overshoots, which the callers reject.
    let length4 = src
        .len()
        .wrapping_sub(length1 + length2 + length3 + JUMP_TABLE_SIZE);
    [length1, length2, length3, length4]
}

// `HUF_decompress4X*_usingDTable_internal_body`: every stream must end exactly.
fn decompress_4x_strict<V: Variant>(
    table: &HufTable,
    dst: &mut [u8],
    src: &[u8],
) -> Result<(), Error> {
    if src.len() < 10 || dst.len() < 6 {
        return Err(Error::CorruptionDetected);
    }
    let (elts, log) = (V::table(table), table.table_log);
    let lengths = stream_lengths(src);
    let segment_size = dst.len().div_ceil(4);
    let segment_starts = [0, segment_size, 2 * segment_size, 3 * segment_size];
    if lengths[3] > src.len() || segment_starts[3] > dst.len() {
        return Err(Error::CorruptionDetected);
    }
    let init = |start: usize, length: usize| {
        BitReader::new(&src[start..start + length]).ok_or(Error::CorruptionDetected)
    };
    let start2 = JUMP_TABLE_SIZE + lengths[0];
    let start3 = start2 + lengths[1];
    let mut bits = [
        init(JUMP_TABLE_SIZE, lengths[0])?,
        init(start2, lengths[1])?,
        init(start3, lengths[2])?,
        init(start3 + lengths[2], lengths[3])?,
    ];

    let mut op = segment_starts;
    if dst.len() - op[3] >= 8 {
        let op_limit = dst.len() - V::STRICT_MARGIN;
        let mut unfinished = true;
        while unfinished && op[3] < op_limit {
            for _ in 0..4 {
                for s in 0..STREAMS {
                    op[s] += V::decode_symbol(elts, log, dst, op[s], &mut bits[s]);
                }
            }
            for reader in &mut bits {
                // SAFETY: at most 4 symbols of at most 12 bits since the last reload (at most 7 bits consumed).
                unfinished &= unsafe { reader.reload_fast() } == Status::Unfinished;
            }
        }
    }
    // X2 streams can outrun their segment, X1 ones cannot.
    if (0..3).any(|s| op[s] > segment_starts[s + 1]) {
        return Err(Error::CorruptionDetected);
    }
    let mut ended = true;
    for s in 0..STREAMS {
        let end = segment_starts.get(s + 1).copied().unwrap_or(dst.len());
        V::decode_stream(elts, log, dst, op[s], end, &mut bits[s]);
        ended &= bits[s].end_of_stream();
    }
    if ended {
        Ok(())
    } else {
        Err(Error::CorruptionDetected)
    }
}

// `HUF_decompress4X*_usingDTable_internal_fast`: `Ok(false)` means "use the
// strict body". No stream is checked to end exactly: each only has to stay
// within its segment and within 8 bytes below its own start.
fn decompress_4x_fast<V: Variant>(
    table: &HufTable,
    dst: &mut [u8],
    src: &[u8],
) -> Result<bool, Error> {
    if dst.is_empty() {
        return Ok(false);
    }
    if src.len() < 10 {
        return Err(Error::CorruptionDetected);
    }
    if table.table_log != FAST_TABLE_LOG {
        return Ok(false);
    }
    let lengths = stream_lengths(src);
    let iend2 = JUMP_TABLE_SIZE + lengths[0];
    let iend3 = iend2 + lengths[1];
    let iend4 = iend3 + lengths[2];
    // Streams under 8 bytes cannot seed a 64-bit container.
    if lengths.iter().any(|&length| length < MIN_STREAM_SIZE_FAST) {
        return Ok(false);
    }
    if lengths[3] > src.len() {
        return Err(Error::CorruptionDetected);
    }
    let segment_size = dst.len().div_ceil(4);
    let op = [0, segment_size, 2 * segment_size, 3 * segment_size];
    if op[3] >= dst.len() {
        return Ok(false);
    }
    let ip = [iend2 - 8, iend3 - 8, iend4 - 8, src.len() - 8];
    let mut args = FastArgs {
        ip,
        op,
        bits: ip.map(|at| init_fast_bits(src, at)),
        iend: [JUMP_TABLE_SIZE, iend2, iend3, iend4],
    };
    let elts = V::table(table);
    V::fast_loop(elts, src, dst, &mut args);

    // Finish each stream with the checked reader, down to the start of the input.
    let mut segment_end = 0;
    for s in 0..STREAMS {
        segment_end = if segment_size <= dst.len() - segment_end {
            segment_end + segment_size
        } else {
            dst.len()
        };
        // `HUF_initRemainingDStream`
        if args.op[s] > segment_end || args.ip[s] + 8 < args.iend[s] {
            return Err(Error::CorruptionDetected);
        }
        let mut bits = BitReader::resume(src, args.ip[s], args.bits[s].trailing_zeros());
        V::decode_stream(
            elts,
            FAST_TABLE_LOG,
            dst,
            args.op[s],
            segment_end,
            &mut bits,
        );
    }
    Ok(true)
}

// `HUF_initFastDStream`: the 8 bytes at `at`, a set bit below the stream's
// last bit, and the unused bits of the last byte shifted out.
fn init_fast_bits(src: &[u8], at: usize) -> u64 {
    let last_byte = src[at + 7];
    let consumed = if last_byte == 0 {
        0
    } else {
        last_byte.leading_zeros() + 1
    };
    (read_u64(src, at) | 1) << consumed
}

#[inline(always)]
fn read_u64(src: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(src[at..at + 8].try_into().unwrap())
}

// Refill of one fast-loop stream: move back by the whole bytes consumed.
// Safety: `*ip - consumed / 8 + 8 <= src.len()`, with no underflow.
#[inline(always)]
unsafe fn reload_fast_stream(src: &[u8], ip: &mut usize, bits: &mut u64) {
    let consumed = bits.trailing_zeros();
    *ip -= (consumed >> 3) as usize;
    // SAFETY: the caller's.
    let word = unsafe { src.as_ptr().add(*ip).cast::<u64>().read_unaligned() };
    *bits = (u64::from_le(word) | 1) << (consumed & 7);
}

// Streams after the first never sit below the one before; the loop stops otherwise.
fn streams_crossed(ip: &[usize; STREAMS]) -> bool {
    (1..STREAMS).any(|s| ip[s] < ip[s - 1])
}

impl Variant for X1 {
    type Elt = EltX1;
    const STRICT_MARGIN: usize = 3;

    fn table(table: &HufTable) -> &[EltX1; TABLE_SIZE] {
        &table.x1
    }

    #[inline(always)]
    fn decode_symbol(
        table: &[EltX1; TABLE_SIZE],
        log: u32,
        dst: &mut [u8],
        op: usize,
        bits: &mut BitReader<'_>,
    ) -> usize {
        let elt = table[bits.look_bits_fast(log) as usize];
        bits.skip(u32::from(elt.nb_bits()));
        dst[op] = elt.byte();
        1
    }

    fn decode_stream(
        table: &[EltX1; TABLE_SIZE],
        log: u32,
        dst: &mut [u8],
        mut op: usize,
        end: usize,
        bits: &mut BitReader<'_>,
    ) {
        if end - op > 3 {
            while bits.reload() == Status::Unfinished && op + 3 < end {
                // Shifting the bits aside keeps the consumed count out of the lookup chain.
                let mut unread = bits.unread();
                let mut consumed = 0;
                for byte in &mut dst[op..op + 4] {
                    let elt = table[(unread >> (64 - log)) as usize];
                    unread <<= elt.nb_bits() & 0x3F;
                    consumed += u32::from(elt.nb_bits());
                    *byte = elt.byte();
                }
                bits.skip(consumed);
                op += 4;
            }
        } else {
            bits.reload();
        }
        // The stream holds no more data to load: the rest comes from the container.
        while op < end {
            op += Self::decode_symbol(table, log, dst, op, bits);
        }
    }

    // Invariants of the loop (C asserts them): each round writes 5 bytes in
    // every segment, `rounds` keeps op[3] below the end and the segments are
    // as long as the last one; ip[s] >= ip[0] >= 7 * rounds, and a round
    // consumes at most 55 bits per stream.
    fn fast_loop(table: &[EltX1; TABLE_SIZE], src: &[u8], dst: &mut [u8], args: &mut FastArgs) {
        // Locals, not `args` fields: stores to `dst` would force the state to memory.
        let (mut ip, mut op, mut bits) = (args.ip, args.op, args.bits);
        let dst_ptr = dst.as_mut_ptr();
        loop {
            let rounds = ((dst.len() - op[3]) / 5).min(ip[0] / 7);
            let op_limit = op[3] + rounds * 5;
            if op[3] == op_limit || streams_crossed(&ip) {
                break;
            }
            loop {
                for symbol in 0..5 {
                    for s in 0..STREAMS {
                        let elt = table[(bits[s] >> 53) as usize];
                        bits[s] <<= elt.nb_bits() & 0x3F;
                        // SAFETY: op[s] <= op[3] and op[3] + 5 <= dst.len() at the start of a round.
                        unsafe { *dst_ptr.add(op[s] + symbol) = elt.byte() };
                    }
                }
                for s in 0..STREAMS {
                    op[s] += 5;
                    // SAFETY: ip[s] >= ip[0] >= 7 * rounds_left, ip[s] + 8 <= src.len().
                    unsafe { reload_fast_stream(src, &mut ip[s], &mut bits[s]) };
                }
                if op[3] >= op_limit {
                    break;
                }
            }
        }
        (args.ip, args.op, args.bits) = (ip, op, bits);
    }
}

impl Variant for X2 {
    type Elt = EltX2;
    const STRICT_MARGIN: usize = 7;

    fn table(table: &HufTable) -> &[EltX2; TABLE_SIZE] {
        &table.x2
    }

    #[inline(always)]
    fn decode_symbol(
        table: &[EltX2; TABLE_SIZE],
        log: u32,
        dst: &mut [u8],
        op: usize,
        bits: &mut BitReader<'_>,
    ) -> usize {
        let elt = table[bits.look_bits_fast(log) as usize];
        dst[op..op + 2].copy_from_slice(&elt.sequence.to_le_bytes());
        bits.skip(u32::from(elt.nb_bits));
        usize::from(elt.length)
    }

    fn decode_stream(
        table: &[EltX2; TABLE_SIZE],
        log: u32,
        dst: &mut [u8],
        mut op: usize,
        end: usize,
        bits: &mut BitReader<'_>,
    ) {
        if end - op >= 8 {
            // Up to 10 bytes per reload with a table log of 11, else up to 8.
            let (symbols, room) = if log <= FAST_TABLE_LOG {
                (5, 10)
            } else {
                (4, 8)
            };
            while bits.reload() == Status::Unfinished && op + room <= end {
                for _ in 0..symbols {
                    op += Self::decode_symbol(table, log, dst, op, bits);
                }
            }
        } else {
            bits.reload();
        }
        // Closer to the end: up to 2 bytes at a time.
        if end - op >= 2 {
            while bits.reload() == Status::Unfinished && op + 2 <= end {
                op += Self::decode_symbol(table, log, dst, op, bits);
            }
            while op + 2 <= end {
                op += Self::decode_symbol(table, log, dst, op, bits);
            }
        }
        if op < end {
            // `HUF_decodeLastSymbolX2`: one byte left, but a pair entry still
            // consumes the bits of both symbols (clamped, PLAN.md 4.2-2).
            let elt = table[bits.look_bits_fast(log) as usize];
            dst[op] = elt.sequence as u8;
            if elt.length == 1 {
                bits.skip(u32::from(elt.nb_bits));
            } else {
                bits.skip_clamped(u32::from(elt.nb_bits));
            }
        }
    }

    // Same invariants as X1 with up to 10 bytes per round and stream; each
    // stream is bounded by the start of the next segment (`oend`).
    fn fast_loop(table: &[EltX2; TABLE_SIZE], src: &[u8], dst: &mut [u8], args: &mut FastArgs) {
        let (mut ip, mut op, mut bits) = (args.ip, args.op, args.bits);
        let dst_ptr = dst.as_mut_ptr();
        let oend = [op[1], op[2], op[3], dst.len()];
        loop {
            // Rounds that cannot run out of input or of room in any segment.
            let mut rounds = ip[0] / 7;
            for s in 0..STREAMS {
                rounds = rounds.min((oend[s] - op[s]) / 10);
            }
            let op_limit = op[3] + rounds * 5;
            if op[3] == op_limit || streams_crossed(&ip) {
                break;
            }
            loop {
                // The last stream is decoded between the reloads to ease register pressure.
                for _ in 0..5 {
                    for s in 0..3 {
                        // SAFETY: a round leaves 10 bytes below `oend[s]` for 5 cells of up to 2.
                        unsafe { fast_symbol_x2(table, dst_ptr, &mut op[s], &mut bits[s]) };
                    }
                }
                // SAFETY: as above.
                unsafe { fast_symbol_x2(table, dst_ptr, &mut op[3], &mut bits[3]) };
                for s in 0..STREAMS {
                    // SAFETY: as above.
                    unsafe { fast_symbol_x2(table, dst_ptr, &mut op[3], &mut bits[3]) };
                    // SAFETY: ip[s] >= ip[0] >= 7 * rounds_left, ip[s] + 8 <= src.len().
                    unsafe { reload_fast_stream(src, &mut ip[s], &mut bits[s]) };
                }
                if op[3] >= op_limit {
                    break;
                }
            }
        }
        (args.ip, args.op, args.bits) = (ip, op, bits);
    }
}

// Safety: `*op + 2` is within the buffer at `dst`.
#[inline(always)]
unsafe fn fast_symbol_x2(
    table: &[EltX2; TABLE_SIZE],
    dst: *mut u8,
    op: &mut usize,
    bits: &mut u64,
) {
    let elt = table[(*bits >> 53) as usize];
    // SAFETY: the caller's.
    unsafe {
        dst.add(*op)
            .cast::<[u8; 2]>()
            .write_unaligned(elt.sequence.to_le_bytes());
    }
    *bits <<= elt.nb_bits & 0x3F;
    *op += usize::from(elt.length);
}

#[cfg(test)]
mod tests;
