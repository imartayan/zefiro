//! The sequences section of a compressed block: header and tables
//! (`ZSTD_decodeSeqHeaders`), sequence decoding (`ZSTD_decodeSequence`) and
//! execution (`ZSTD_execSequence`), with libzstd's check order.
//!
//! The three stages are separate: `read_header` builds tables, `SequenceDecoder`
//! yields sequences from the bitstream, `exec_sequence` applies one to the
//! output (`exec_sequence_end` near the end of the output, exact and safe).

use alloc::boxed::Box;
use core::ops::Range;

use branches::prefetch_read_data;

use crate::bits::{BitReader, Status};
use crate::copy::{Kernel, Portable, ShortOffsets, WILDCOPY_OVERLENGTH, copy16};
use crate::dict::Dictionary;
use crate::error::Error;
use crate::fse::*;
use crate::literals::Literals;

const INITIAL_REPEAT_OFFSETS: [u32; 3] = [1, 4, 8];
const LONG_NB_SEQ: usize = 0x7F00;
// Largest symbol count of any of the three alphabets.
const MAX_SYMBOLS: usize = 53;
// Extra bits from which a reload is needed before the literal length
// (libzstd: 57 minus the 26 of the three table logs).
const RELOAD_THRESHOLD_BITS: u8 = 31;

const LL_CELLS: usize = 512;
const OF_CELLS: usize = 256;
const ML_CELLS: usize = 512;

// Share of short offsets (out of 256) from which a block's matches are copied without
// branching on the offset.
const SHORT_OFFSET_SHARE_MIN: usize = 32;

// libzstd runs the prefetching loop for blocks of more than `LOOKAHEAD` sequences, after
// over `PREFETCH_HISTORY_MIN` bytes of history, with at least `LONG_OFFSET_SHARE_MIN` cells
// (out of 256) of the offset table having over `LONG_OFFSET_BITS` extra bits.
const LOOKAHEAD: usize = 8;
const PREFETCH_HISTORY_MIN: usize = 1 << 24;
const LONG_OFFSET_BITS: u8 = 22;
const LONG_OFFSET_SHARE_MIN: usize = 7;
const CACHE_LINE: usize = 64;

const MODE_PREDEFINED: u8 = 0;
const MODE_RLE: u8 = 1;
const MODE_COMPRESSED: u8 = 2;
const MODE_REPEAT: u8 = 3;

// Tables and repeat offsets kept across blocks, reset per frame.
#[derive(Debug, Clone)]
pub(crate) struct SequencesState {
    tables: Box<Tables>,
    repeat_offsets: [u32; 3],
    // Repeat mode is allowed (libzstd's `fseEntropy`).
    has_tables: bool,
    dictionary: Option<Dictionary>,
    // Per table (ll, of, ml): the table is the dictionary's, not copied into `tables`.
    in_dictionary: [bool; 3],
    // libzstd's `ddictIsCold`: the first block after a dictionary was (re)loaded takes the
    // prefetching loop, and whether its `dictEnd` is still the dictionary's end, which a
    // ring restart changes.
    pub(crate) dictionary_is_cold: bool,
    pub(crate) dictionary_end_kept: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Tables {
    ll: FseTable<LL_CELLS>,
    of: FseTable<OF_CELLS>,
    ml: FseTable<ML_CELLS>,
    // Per table (ll, of, ml): the cells are not a copy of the predefined table.
    custom: [bool; 3],
    // Many offset codes of `of` are short (`short_offset_share`).
    short_offsets: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct Sequence {
    literal_length: usize,
    match_length: usize,
    offset: usize,
}

// Output position and literals position, kept by value across sequences.
#[derive(Clone, Copy)]
struct Cursor {
    op: *mut u8,
    lit: *const u8,
}

// History before the segment being written: the previous segment of a ring, of which
// `out[..len]` is the end, or a dictionary, followed by `WILDCOPY_OVERLENGTH` bytes of slack.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Ext<'a> {
    Ring(usize),
    Dict(&'a [u8]),
}

// Where a block's output goes: `out[range]`, after the history `out[..range.start]`, itself
// after `ext`. `total_history` is libzstd's `totalHistorySize`.
#[derive(Debug, Clone)]
pub(crate) struct Placement<'a> {
    pub(crate) range: Range<usize>,
    pub(crate) ext: Ext<'a>,
    pub(crate) total_history: usize,
}

// Bounds of a block's execution (`ZSTD_execSequence`'s `oend`, `oend_w`, `litLimit`).
#[derive(Clone, Copy)]
struct Limits<'a> {
    out_base: *mut u8,
    out_len: usize,
    out_end: usize,
    ext: Ext<'a>,
    // `out_end` minus the wild copy slack.
    fast_end: *mut u8,
    literals: &'a [u8],
    // Start of `literals`, which are followed by the slack.
    lit_base: *const u8,
}

#[derive(Debug)]
struct SequenceDecoder<'a> {
    bits: BitReader<'a>,
    tables: &'a Tables,
    state_ll: usize,
    state_of: usize,
    state_ml: usize,
    repeat_offsets: [usize; 3],
}

impl Tables {
    fn boxed() -> Box<Self> {
        Box::new(Self {
            ll: FseTable::EMPTY,
            of: FseTable::EMPTY,
            ml: FseTable::EMPTY,
            custom: [true; 3],
            short_offsets: false,
        })
    }

    // The tables of a dictionary (`ZSTD_loadDEntropy`): offsets, match lengths, then literal
    // lengths. Returns them and the bytes used.
    pub(crate) fn read_dictionary(src: &[u8]) -> Result<(Box<Self>, usize), Error> {
        let mut tables = Self::boxed();
        let mut custom = true;
        let spec = |max_symbol, max_log| TableSpec {
            mode: MODE_COMPRESSED,
            max_symbol,
            max_log,
        };
        let mut used = build_table(
            &mut tables.of,
            &mut custom,
            &OF_DEFAULT_TABLE,
            spec(OF_MAX_SYMBOL, OF_TABLE_LOG_MAX),
            (&OF_BASE, &OF_EXTRA_BITS),
            src,
            false,
        )
        .map_err(|_| Error::DictionaryCorrupted)?;
        used += build_table(
            &mut tables.ml,
            &mut custom,
            &ML_DEFAULT_TABLE,
            spec(ML_MAX_SYMBOL, ML_TABLE_LOG_MAX),
            (&ML_BASE, &ML_EXTRA_BITS),
            &src[used..],
            false,
        )
        .map_err(|_| Error::DictionaryCorrupted)?;
        used += build_table(
            &mut tables.ll,
            &mut custom,
            &LL_DEFAULT_TABLE,
            spec(LL_MAX_SYMBOL, LL_TABLE_LOG_MAX),
            (&LL_BASE, &LL_EXTRA_BITS),
            &src[used..],
            false,
        )
        .map_err(|_| Error::DictionaryCorrupted)?;
        tables.short_offsets = short_offset_share(&tables.of) >= SHORT_OFFSET_SHARE_MIN;
        Ok((tables, used))
    }
}

impl SequencesState {
    pub(crate) fn new() -> Self {
        Self {
            tables: Tables::boxed(),
            repeat_offsets: INITIAL_REPEAT_OFFSETS,
            has_tables: false,
            dictionary: None,
            in_dictionary: [false; 3],
            dictionary_is_cold: false,
            dictionary_end_kept: false,
        }
    }

    pub(crate) fn set_dictionary(&mut self, dictionary: Option<&Dictionary>) {
        self.dictionary = dictionary.cloned();
    }

    // A frame starts from the dictionary's tables and repeat offsets, if it has any.
    pub(crate) fn start_frame(&mut self) {
        let entropy = self.dictionary.as_ref().and_then(Dictionary::entropy);
        self.has_tables = entropy.is_some();
        self.repeat_offsets = entropy.map_or(INITIAL_REPEAT_OFFSETS, |e| e.repeat_offsets);
        self.in_dictionary = [entropy.is_some(); 3];
        if entropy.is_some() {
            self.tables.custom = [true; 3];
        }
    }

    // Copies the dictionary's tables that blocks still use as they are.
    fn copy_dictionary_tables(&mut self) {
        let entropy = self
            .dictionary
            .as_ref()
            .and_then(Dictionary::entropy)
            .unwrap();
        let (tables, dictionary) = (&mut *self.tables, &entropy.sequences);
        let [ll, of, ml] = self.in_dictionary;
        if ll {
            tables.ll.copy_from(&dictionary.ll);
        }
        if of {
            tables.of.copy_from(&dictionary.of);
            tables.short_offsets = dictionary.short_offsets;
        }
        if ml {
            tables.ml.copy_from(&dictionary.ml);
        }
        self.in_dictionary = [false; 3];
    }

    // `ZSTD_decodeSeqHeaders`: reads the sequence count and builds the tables.
    // Returns the count and the bitstream that follows.
    pub(crate) fn read_header<'a>(&mut self, src: &'a [u8]) -> Result<(usize, &'a [u8]), Error> {
        let (&first, mut rest) = src.split_first().ok_or(Error::SrcSizeWrong)?;
        let nb_seq = match first {
            0..=0x7F => usize::from(first),
            0xFF => {
                let count = rest.get(..2).ok_or(Error::SrcSizeWrong)?;
                rest = &rest[2..];
                usize::from(u16::from_le_bytes([count[0], count[1]])) + LONG_NB_SEQ
            }
            _ => {
                let (&low, tail) = rest.split_first().ok_or(Error::SrcSizeWrong)?;
                rest = tail;
                (usize::from(first - 0x80) << 8) + usize::from(low)
            }
        };
        if nb_seq == 0 {
            return if rest.is_empty() {
                Ok((0, rest))
            } else {
                Err(Error::CorruptionDetected)
            };
        }

        let (&modes, mut rest) = rest.split_first().ok_or(Error::SrcSizeWrong)?;
        if modes & 3 != 0 {
            return Err(Error::CorruptionDetected);
        }
        let has_tables = self.has_tables;
        let mode_of = [modes >> 6, (modes >> 4) & 3, (modes >> 2) & 3];
        for (in_dictionary, mode) in self.in_dictionary.iter_mut().zip(mode_of) {
            *in_dictionary &= mode == MODE_REPEAT;
        }
        let tables = &mut *self.tables;
        let [ll_custom, of_custom, ml_custom] = &mut tables.custom;
        let used = build_table(
            &mut tables.ll,
            ll_custom,
            &LL_DEFAULT_TABLE,
            TableSpec {
                mode: modes >> 6,
                max_symbol: LL_MAX_SYMBOL,
                max_log: LL_TABLE_LOG_MAX,
            },
            (&LL_BASE, &LL_EXTRA_BITS),
            rest,
            has_tables,
        )?;
        rest = &rest[used..];
        let used = build_table(
            &mut tables.of,
            of_custom,
            &OF_DEFAULT_TABLE,
            TableSpec {
                mode: (modes >> 4) & 3,
                max_symbol: OF_MAX_SYMBOL,
                max_log: OF_TABLE_LOG_MAX,
            },
            (&OF_BASE, &OF_EXTRA_BITS),
            rest,
            has_tables,
        )?;
        if (modes >> 4) & 3 != MODE_REPEAT {
            tables.short_offsets = short_offset_share(&tables.of) >= SHORT_OFFSET_SHARE_MIN;
        }
        rest = &rest[used..];
        let used = build_table(
            &mut tables.ml,
            ml_custom,
            &ML_DEFAULT_TABLE,
            TableSpec {
                mode: (modes >> 2) & 3,
                max_symbol: ML_MAX_SYMBOL,
                max_log: ML_TABLE_LOG_MAX,
            },
            (&ML_BASE, &ML_EXTRA_BITS),
            rest,
            has_tables,
        )?;
        if self.in_dictionary != [false; 3] && self.in_dictionary != [true; 3] {
            self.copy_dictionary_tables();
        }
        Ok((nb_seq, &rest[used..]))
    }

    // Decodes and executes the block's sequences from `stream`, then copies the remaining
    // literals. Returns the position after the block's output.
    pub(crate) fn execute_block(
        &mut self,
        nb_seq: usize,
        stream: &[u8],
        literals: &Literals<'_>,
        out: &mut [u8],
        place: &Placement<'_>,
    ) -> Result<usize, Error> {
        let literal_bytes = literals.bytes();
        let (mut op, mut literal_pos) = (place.range.start, 0);
        let cold = core::mem::take(&mut self.dictionary_is_cold);
        if nb_seq > 0 {
            self.has_tables = true;
            let tables: &Tables = match &self.dictionary {
                Some(dictionary) if self.in_dictionary == [true; 3] => {
                    &dictionary.entropy().unwrap().sequences
                }
                _ => &self.tables,
            };
            let prefetch = cold
                || nb_seq > LOOKAHEAD
                    && place.total_history > PREFETCH_HISTORY_MIN
                    && long_offset_share(&tables.of) >= LONG_OFFSET_SHARE_MIN;
            // Blocks reaching the previous ring segment always use the portable kernel.
            (op, literal_pos, self.repeat_offsets) = if place.ext.len() > 0 {
                if prefetch {
                    self.run_sequences::<Portable, true, true>(
                        tables, nb_seq, stream, literals, out, place,
                    )?
                } else {
                    self.run_sequences::<Portable, false, true>(
                        tables, nb_seq, stream, literals, out, place,
                    )?
                }
            } else if prefetch {
                self.run_sequences::<Portable, true, false>(
                    tables, nb_seq, stream, literals, out, place,
                )?
            } else if tables.short_offsets {
                self.run_sequences::<ShortOffsets, false, false>(
                    tables, nb_seq, stream, literals, out, place,
                )?
            } else {
                self.run_sequences::<Portable, false, false>(
                    tables, nb_seq, stream, literals, out, place,
                )?
            };
        }
        let last_literals = &literal_bytes[literal_pos..];
        if last_literals.len() > place.range.end - op {
            return Err(Error::DstSizeTooSmall);
        }
        out[op..op + last_literals.len()].copy_from_slice(last_literals);
        Ok(op + last_literals.len())
    }

    // The block's sequence loop: decodes one sequence ahead of the one executed, or with
    // `PREFETCH` `LOOKAHEAD` ahead (`ZSTD_decompressSequencesLong`), touching the match
    // sources and checking the stream end before the last ones run. With `EXT`, matches may
    // reach the previous ring segment. Returns the output and literals positions and the
    // repeat offsets after it.
    #[inline(never)]
    fn run_sequences<K: Kernel, const PREFETCH: bool, const EXT: bool>(
        &self,
        tables: &Tables,
        nb_seq: usize,
        stream: &[u8],
        literals: &Literals<'_>,
        out: &mut [u8],
        place: &Placement<'_>,
    ) -> Result<(usize, usize, [u32; 3]), Error> {
        let range = &place.range;
        // The unsafe execution below relies on these.
        assert!(range.start <= range.end && range.end <= out.len());
        assert!(literals.padded.len() >= literals.len + WILDCOPY_OVERLENGTH);
        let mut decoder = SequenceDecoder::new(stream, tables, self.repeat_offsets)?;
        // One `as_mut_ptr` call: a second one would invalidate the first pointer.
        let out_base = out.as_mut_ptr();
        let limits = Limits {
            out_base,
            out_len: out.len(),
            out_end: range.end,
            ext: place.ext,
            fast_end: out_base.wrapping_add(range.end.saturating_sub(WILDCOPY_OVERLENGTH)),
            literals: literals.bytes(),
            lit_base: literals.padded.as_ptr(),
        };
        let mut cursor = Cursor {
            op: limits.out_base.wrapping_add(range.start),
            lit: limits.lit_base,
        };
        let kernel = K::default();
        if PREFETCH {
            let mut queue = [Sequence::default(); LOOKAHEAD];
            let mut position = range.start;
            let ahead = nb_seq.min(LOOKAHEAD);
            for (index, slot) in queue.iter_mut().enumerate().take(ahead) {
                let sequence = decoder.next::<false>(index == nb_seq - 1);
                position = prefetch_match(position, sequence, &limits);
                *slot = sequence;
            }
            for index in ahead..nb_seq {
                let sequence = decoder.next::<false>(index == nb_seq - 1);
                let oldest = queue[index % LOOKAHEAD];
                // SAFETY: `limits` and `cursor` describe `out` and `literals`, as `exec_sequence` needs.
                cursor = unsafe { exec_sequence::<K, EXT>(kernel, oldest, cursor, &limits) }?;
                position = prefetch_match(position, sequence, &limits);
                queue[index % LOOKAHEAD] = sequence;
            }
            if !decoder.bits.end_of_stream() {
                return Err(Error::CorruptionDetected);
            }
            for index in nb_seq - ahead..nb_seq {
                // SAFETY: as above.
                cursor = unsafe {
                    exec_sequence::<K, EXT>(kernel, queue[index % LOOKAHEAD], cursor, &limits)
                }?;
            }
        } else {
            // Decoding ahead wins on AArch64 (+3.6%) but spills on x86-64 (15 registers):
            // decoding right before the execution gains 7% there.
            if cfg!(target_arch = "x86_64") {
                let mut remaining = nb_seq;
                while remaining > 1 && decoder.bits.has_room_for_sequence() {
                    let sequence = decoder.next::<true>(false);
                    // SAFETY: `limits` and `cursor` describe `out` and `literals`, as `exec_sequence` needs.
                    cursor = unsafe { exec_sequence::<K, EXT>(kernel, sequence, cursor, &limits) }?;
                    remaining -= 1;
                }
                for remaining in (1..=remaining).rev() {
                    let sequence = decoder.next::<false>(remaining == 1);
                    // SAFETY: as above.
                    cursor = unsafe { exec_sequence::<K, EXT>(kernel, sequence, cursor, &limits) }?;
                }
            } else {
                let mut current = decoder.next::<false>(nb_seq == 1);
                for remaining in (1..nb_seq).rev() {
                    let following = decoder.next::<false>(remaining == 1);
                    // SAFETY: as above.
                    cursor = unsafe { exec_sequence::<K, EXT>(kernel, current, cursor, &limits) }?;
                    current = following;
                }
                // SAFETY: as above.
                cursor = unsafe { exec_sequence::<K, EXT>(kernel, current, cursor, &limits) }?;
            }
            if !decoder.bits.end_of_stream() {
                return Err(Error::CorruptionDetected);
            }
        }
        let op = cursor.op.addr() - limits.out_base.addr();
        let literal_pos = cursor.lit.addr() - limits.lit_base.addr();
        Ok((
            op,
            literal_pos,
            decoder.repeat_offsets.map(|offset| offset as u32),
        ))
    }
}

impl<'a> SequenceDecoder<'a> {
    fn new(stream: &'a [u8], tables: &'a Tables, repeat_offsets: [u32; 3]) -> Result<Self, Error> {
        let mut bits = BitReader::new(stream).ok_or(Error::CorruptionDetected)?;
        let mut init = |table_log: u32| {
            let state = bits.read_bits(table_log) as usize;
            bits.reload();
            state
        };
        let state_ll = init(tables.ll.table_log);
        let state_of = init(tables.of.table_log);
        let state_ml = init(tables.ml.table_log);
        Ok(Self {
            bits,
            tables,
            state_ll,
            state_of,
            state_ml,
            repeat_offsets: repeat_offsets.map(|offset| offset as usize),
        })
    }

    // The last sequence leaves the FSE states alone, as the stream ends with it.
    #[inline(always)]
    fn next<const IN_CONTAINER: bool>(&mut self, last: bool) -> Sequence {
        let tables = self.tables;
        let (ll, ml, of) = if IN_CONTAINER {
            // SAFETY: a state is below `1 << table_log <= N`: initial states have `table_log`
            // bits, and every cell of a built table has `next_state + (1 << nb_bits)` within it.
            unsafe {
                debug_assert!(
                    self.state_ll < LL_CELLS
                        && self.state_ml < ML_CELLS
                        && self.state_of < OF_CELLS
                );
                (
                    tables.ll.entries.get_unchecked(self.state_ll).load_whole(),
                    tables.ml.entries.get_unchecked(self.state_ml).load_whole(),
                    tables.of.entries.get_unchecked(self.state_of).load_whole(),
                )
            }
        } else {
            (
                tables.ll.entries[self.state_ll],
                tables.ml.entries[self.state_ml],
                tables.of.entries[self.state_of],
            )
        };
        let bits = &mut self.bits;
        // Every offset code consumes exactly its extra bits (none: garbage, unused): read them
        // first so that the offset logic, a coin flip on some data, follows the state update.
        let offset_extra = bits.read_bits_fast(u32::from(of.nb_additional_bits)) as usize;
        let mut match_length = ml.base_value as usize;
        if ml.nb_additional_bits > 0 {
            match_length += bits.read_bits_fast(u32::from(ml.nb_additional_bits)) as usize;
        }
        let total_bits = ll.nb_additional_bits + ml.nb_additional_bits + of.nb_additional_bits;
        if total_bits >= RELOAD_THRESHOLD_BITS {
            if IN_CONTAINER {
                // SAFETY: the caller checked `has_room_for_sequence`: this reload is a full one.
                unsafe { bits.reload_unchecked() };
            } else {
                bits.reload();
            }
        }
        let mut literal_length = ll.base_value as usize;
        if ll.nb_additional_bits > 0 {
            literal_length += bits.read_bits_fast(u32::from(ll.nb_additional_bits)) as usize;
        }

        if !last {
            let widths = [
                u32::from(ll.nb_bits),
                u32::from(ml.nb_bits),
                u32::from(of.nb_bits),
            ];
            let [ll_bits, ml_bits, of_bits] = if IN_CONTAINER {
                bits.read_bits_x3_in_container(widths)
            } else {
                bits.read_bits_x3(widths)
            };
            self.state_ll = usize::from(ll.next_state) + ll_bits as usize;
            self.state_ml = usize::from(ml.next_state) + ml_bits as usize;
            self.state_of = usize::from(of.next_state) + of_bits as usize;
            if IN_CONTAINER {
                // SAFETY: as above.
                unsafe { bits.reload_unchecked() };
            // SAFETY: at most 7 bits were consumed after the previous reload, and 63 since.
            } else if unsafe { bits.reload_fast() } == Status::Overflow {
                bits.reload();
            }
        }
        // Constant indices only: dynamic ones would keep the array in memory.
        let [rep0, rep1, rep2] = self.repeat_offsets;
        let offset = if of.nb_additional_bits > 1 {
            let offset = of.base_value as usize + offset_extra;
            self.repeat_offsets = [offset, rep0, rep1];
            offset
        } else {
            let ll_is_zero = ll.base_value == 0;
            if of.nb_additional_bits == 0 {
                if ll_is_zero {
                    self.repeat_offsets = [rep1, rep0, rep2];
                }
                self.repeat_offsets[0]
            } else {
                // Offset codes 1 to 3: second or third repeat offset, or the first minus one.
                let code = of.base_value as usize + usize::from(ll_is_zero) + offset_extra;
                let offset = match code {
                    1 => rep1,
                    2 => rep2,
                    _ => rep0.wrapping_sub(1),
                };
                // A zero offset is invalid: -1 fails the bounds check at execution.
                let offset = if offset == 0 { usize::MAX } else { offset };
                self.repeat_offsets = [offset, rep0, if code == 1 { rep2 } else { rep1 }];
                offset
            }
        };
        Sequence {
            literal_length,
            match_length,
            offset,
        }
    }
}

// Where libzstd stops a block's output (4.2-4): literals stored in the output past
// the block (never when streaming) cap it at `block_size_max` plus the slack, other
// placements leave the capacity. The error when a split block catches up with its
// literals is not modelled.
pub(crate) const fn block_output_limit(
    capacity: usize,
    literals: &Literals<'_>,
    block_size_max: usize,
    streaming: bool,
) -> usize {
    let room_after_block = block_size_max + literals.len + 2 * WILDCOPY_OVERLENGTH;
    let in_output = !streaming && !literals.in_input && capacity > room_after_block;
    if in_output {
        block_size_max + WILDCOPY_OVERLENGTH
    } else {
        capacity
    }
}

#[derive(Clone, Copy)]
struct TableSpec {
    mode: u8,
    max_symbol: u32,
    max_log: u32,
}

// `ZSTD_getOffsetInfo`: cells of the offset table with over `LONG_OFFSET_BITS` extra bits,
// scaled to 256 cells.
fn long_offset_share(table: &FseTable<OF_CELLS>) -> usize {
    let cells = &table.entries[..1 << table.table_log];
    let long = cells
        .iter()
        .filter(|cell| cell.nb_additional_bits > LONG_OFFSET_BITS)
        .count();
    long << (OF_TABLE_LOG_MAX - table.table_log)
}

// `ZSTD_prefetchMatch`: prefetches the match source of `sequence` (the history is the frame's
// output, an offset past its start is invalid and skipped). Returns the position after it.
#[inline(always)]
fn prefetch_match(position: usize, sequence: Sequence, limits: &Limits<'_>) -> usize {
    let position = position.wrapping_add(sequence.literal_length);
    if sequence.offset <= position {
        let source = limits.out_base.wrapping_add(position - sequence.offset);
        prefetch_read_data::<u8, 0>(source);
        prefetch_read_data::<u8, 0>(source.wrapping_add(CACHE_LINE));
    }
    position.wrapping_add(sequence.match_length)
}

// Cells of the offset table with codes 2 to 4 (offsets 1 to 28), scaled to 256 cells.
fn short_offset_share(table: &FseTable<OF_CELLS>) -> usize {
    let cells = &table.entries[..1 << table.table_log];
    let short = cells
        .iter()
        .filter(|cell| (2..=4).contains(&cell.nb_additional_bits))
        .count();
    short << (OF_TABLE_LOG_MAX - table.table_log)
}

// `ZSTD_buildSeqTable`: returns the bytes of `src` used.
fn build_table<const N: usize>(
    table: &mut FseTable<N>,
    custom: &mut bool,
    default: &FseTable<N>,
    spec: TableSpec,
    (base, extra_bits): (&[u32], &[u8]),
    src: &[u8],
    has_tables: bool,
) -> Result<usize, Error> {
    match spec.mode {
        MODE_RLE => {
            let symbol = *src.first().ok_or(Error::CorruptionDetected)?;
            if u32::from(symbol) > spec.max_symbol {
                return Err(Error::CorruptionDetected);
            }
            table.set_rle(base[usize::from(symbol)], extra_bits[usize::from(symbol)]);
            *custom = true;
            Ok(1)
        }
        MODE_PREDEFINED => {
            if *custom {
                table.copy_from(default);
                *custom = false;
            }
            Ok(0)
        }
        MODE_COMPRESSED => {
            let mut norm = [0i16; MAX_SYMBOLS];
            let norm = &mut norm[..=spec.max_symbol as usize];
            let ncount = read_ncount(norm, src)?;
            if ncount.table_log > spec.max_log {
                return Err(Error::CorruptionDetected);
            }
            table.build(
                &norm[..=ncount.max_symbol as usize],
                ncount.table_log,
                base,
                extra_bits,
            );
            *custom = true;
            Ok(ncount.bytes_read)
        }
        _ if has_tables => Ok(0),
        _ => Err(Error::CorruptionDetected),
    }
}

// `ZSTD_execSequence`: executes one sequence, wild copies when the match and
// the literals leave room for them, else `exec_sequence_end`.
// Safety: `limits` describes the output and literals (`lit_base` followed by
// `WILDCOPY_OVERLENGTH` readable bytes), the cursor is inside them.
#[inline(always)]
unsafe fn exec_sequence<K: Kernel, const EXT: bool>(
    kernel: K,
    sequence: Sequence,
    cursor: Cursor,
    limits: &Limits<'_>,
) -> Result<Cursor, Error> {
    let Sequence {
        literal_length,
        match_length,
        offset,
    } = sequence;
    let Cursor { mut op, lit } = cursor;
    let literals_end = op.wrapping_add(literal_length);
    // SAFETY: the cursor's literals position never passes the end of the literals.
    let literals_left = unsafe {
        limits
            .lit_base
            .add(limits.literals.len())
            .offset_from_unsigned(lit)
    };
    if literal_length > literals_left || literals_end.wrapping_add(match_length) > limits.fast_end {
        return exec_sequence_end(sequence, cursor, limits);
    }
    // The segment start bounds the history, except in a ring (`EXT`).
    if offset > literals_end.addr() - limits.out_base.addr() {
        return if EXT {
            // SAFETY: the checks above are the ones `exec_sequence_ext` needs.
            unsafe { exec_sequence_ext(sequence, cursor, limits) }
        } else {
            Err(Error::CorruptionDetected)
        };
    }
    // SAFETY: the literals end `literal_length <= literals_left` bytes on, the slack covers
    // the overshoot; the match end is `32` bytes before the output end, so is the overshoot of
    // both copies; the history holds `offset` bytes before `literals_end`; literals and
    // output are distinct allocations.
    unsafe {
        copy16(op, lit);
        if cfg!(target_arch = "x86_64") {
            if literal_length > 16 {
                copy16(op.add(16), lit.add(16));
                if literal_length > 32 {
                    kernel.wildcopy(op.add(32), lit.add(32), literal_length - 32);
                }
            }
        } else {
            copy16(op.add(16), lit.add(16));
            if literal_length > 32 {
                kernel.wildcopy(op.add(32), lit.add(32), literal_length - 32);
            }
        }
        op = literals_end;
        kernel.copy_match(op, offset, match_length);
        Ok(Cursor {
            op: op.add(match_length),
            lit: lit.add(literal_length),
        })
    }
}

// `exec_sequence` for a match starting before the segment, out of line with the cursor by
// value like `exec_sequence_end`, so that the loop keeps its shape. The history before the
// segment is a dictionary (`padded`) or the end of the previous segment of the ring, ahead of
// the output position: wild copy from there when the ring is large enough around it.
// Safety: that of `exec_sequence`, after its checks, and `offset > literals_end - out_base`.
#[cold]
#[inline(never)]
unsafe fn exec_sequence_ext(
    sequence: Sequence,
    cursor: Cursor,
    limits: &Limits<'_>,
) -> Result<Cursor, Error> {
    let Sequence {
        literal_length,
        match_length,
        offset,
    } = sequence;
    let Cursor { op, lit } = cursor;
    let literals_end = op.wrapping_add(literal_length);
    let match_start = literals_end.addr() - limits.out_base.addr();
    let ext_len = limits.ext.len();
    if offset > match_start + ext_len {
        return Err(Error::CorruptionDetected);
    }
    let from_ext = match_length.min(offset - match_start);
    let source = ext_len + match_start - offset;
    let ext_source = match limits.ext {
        Ext::Dict(padded) => padded.as_ptr(),
        Ext::Ring(_) => {
            // History within the slack of the output position may be overwritten by the wild
            // copies, and reads past the ring are out: the exact path.
            let clobbered = source < match_start + WILDCOPY_OVERLENGTH;
            if clobbered || source + from_ext + WILDCOPY_OVERLENGTH > limits.out_len {
                return exec_sequence_end(sequence, cursor, limits);
            }
            limits.out_base.cast_const()
        }
    };
    let kernel = Portable;
    // SAFETY: literals and slack as in `exec_sequence`; the first `from_ext` bytes of the match
    // are the history from `source` on, followed by slack (the dictionary's, or checked above)
    // and, in a ring, at least `WILDCOPY_OVERLENGTH` bytes past the destination so that chunks
    // do not alias their stores; the rest starts at `out_base`, `match_start + from_ext` bytes
    // before its destination (distinct allocations).
    unsafe {
        copy16(op, lit);
        copy16(op.add(16), lit.add(16));
        if literal_length > 32 {
            kernel.wildcopy(op.add(32), lit.add(32), literal_length - 32);
        }
        kernel.wildcopy(literals_end, ext_source.add(source), from_ext);
        if match_length > from_ext {
            let rest = literals_end.add(from_ext);
            kernel.copy_match(rest, match_start + from_ext, match_length - from_ext);
        }
        Ok(Cursor {
            op: literals_end.add(match_length),
            lit: lit.add(literal_length),
        })
    }
}

// `ZSTD_execSequenceEnd` on a cursor: moves it through the safe exact version.
#[cold]
#[inline(never)]
fn exec_sequence_end(
    sequence: Sequence,
    cursor: Cursor,
    limits: &Limits<'_>,
) -> Result<Cursor, Error> {
    // SAFETY: `out_base` and `out_len` come from the `&mut` slice of `run_sequences`, which is not
    // used while this one lives.
    let out = unsafe { core::slice::from_raw_parts_mut(limits.out_base, limits.out_len) };
    let op = cursor.op.addr() - limits.out_base.addr();
    let mut literal_pos = cursor.lit.addr() - limits.lit_base.addr();
    let op = exec_sequence_exact(
        out,
        op,
        limits.out_end,
        sequence,
        limits.literals,
        &mut literal_pos,
        limits.ext,
    )?;
    Ok(Cursor {
        op: limits.out_base.wrapping_add(op),
        lit: limits.lit_base.wrapping_add(literal_pos),
    })
}

// `ZSTD_execSequenceEnd` (checks in libzstd's order): returns the output
// position after the sequence.
fn exec_sequence_exact(
    out: &mut [u8],
    op: usize,
    end: usize,
    sequence: Sequence,
    literals: &[u8],
    literal_pos: &mut usize,
    ext: Ext<'_>,
) -> Result<usize, Error> {
    let Sequence {
        literal_length,
        match_length,
        offset,
    } = sequence;
    if literal_length + match_length > end - op {
        return Err(Error::DstSizeTooSmall);
    }
    if literal_length > literals.len() - *literal_pos {
        return Err(Error::CorruptionDetected);
    }
    let literals_end = op + literal_length;
    out[op..literals_end].copy_from_slice(&literals[*literal_pos..*literal_pos + literal_length]);
    *literal_pos += literal_length;
    copy_match(out, literals_end, offset, match_length, ext)?;
    Ok(literals_end + match_length)
}

// Copies a match to `out[dest..]`. The history is `out[..dest]`, preceded by `ext`. A match
// can span both, and overlap its own output.
fn copy_match(
    out: &mut [u8],
    dest: usize,
    offset: usize,
    match_length: usize,
    ext: Ext<'_>,
) -> Result<(), Error> {
    let ext_len = ext.len();
    if offset > dest + ext_len {
        return Err(Error::CorruptionDetected);
    }
    let (mut dest, mut remaining, mut source) = (dest, match_length, 0);
    if offset > dest {
        let from_previous = remaining.min(offset - dest);
        let start = ext_len - (offset - dest);
        match ext {
            Ext::Ring(_) => out.copy_within(start..start + from_previous, dest),
            Ext::Dict(padded) => {
                out[dest..dest + from_previous]
                    .copy_from_slice(&padded[start..start + from_previous]);
            }
        }
        dest += from_previous;
        remaining -= from_previous;
    } else {
        source = dest - offset;
    }
    // The pattern of `dest - source` bytes repeats, so each pass can copy everything
    // produced so far.
    let gap = dest - source;
    let mut done = 0;
    while done < remaining {
        let chunk = (gap + done).min(remaining - done);
        out.copy_within(source..source + chunk, dest + done);
        done += chunk;
    }
    Ok(())
}

impl Ext<'_> {
    pub(crate) const fn len(self) -> usize {
        match self {
            Self::Ring(len) => len,
            Self::Dict(padded) => padded.len() - WILDCOPY_OVERLENGTH,
        }
    }
}

#[cfg(test)]
mod tests;
