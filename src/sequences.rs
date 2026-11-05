//! The sequences section of a compressed block: header and tables
//! (`ZSTD_decodeSeqHeaders`), sequence decoding (`ZSTD_decodeSequence`) and
//! execution (`ZSTD_execSequence`), with libzstd's check order.
//!
//! The three stages are separate: `read_header` builds tables, `SequenceDecoder`
//! yields sequences from the bitstream, `exec_sequence` applies one to the
//! output (`exec_sequence_end` near the end of the output, exact and safe).

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::mem;
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
    // What `tables` are built from.
    defs: TableDefs,
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

// How stage 1 of the split path knows a table: not built, only what it is built from.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TableDef {
    kind: TableKind,
    // `Counts`: the normalized counts of the symbols up to `max_symbol`.
    norm: [i16; MAX_SYMBOLS],
    max_symbol: u8,
    table_log: u8,
}

#[derive(Debug, Clone, Copy)]
enum TableKind {
    Dictionary,
    Predefined,
    Rle(u8),
    Counts,
}

// The definitions of the literal lengths, offsets and match lengths tables.
pub(crate) type TableDefs = [TableDef; 3];

#[derive(Debug, Clone, Copy, Default)]
struct Sequence {
    literal_length: usize,
    match_length: usize,
    offset: usize,
}

// A sequence as stage 2 leaves it: the offset is relative to the repeat offsets the block starts
// with (see `REL_BIAS`), and `from` (0 to 3) is in the top two bits of the literal length.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RawSequence {
    literal_length_from: u32,
    match_length: u32,
    offset: u32,
}

// Stage 2 tracks the repeat offsets as `from << 32 | offset`: for `from` below 3 the block's
// starting repeat offset `from` plus `offset - REL_BIAS` (they only decrease, by one per
// sequence, so never borrow into `from`), for `REL_NONE` the offset itself.
const REL_BIAS: u32 = 1 << 31;
const REL_START: [u64; 3] = [
    REL_BIAS as u64,
    1 << 32 | REL_BIAS as u64,
    2 << 32 | REL_BIAS as u64,
];
const REL_NONE: u64 = 3 << 32;
const REL_LITERAL_MASK: u32 = (1 << 30) - 1;

// The sequences of a block decoded from the bitstream (stage 2 of the split path), whether
// the stream ended exactly, and the repeat offsets after them.
#[derive(Debug, Default)]
pub(crate) struct DecodedSequences {
    raw: Vec<RawSequence>,
    ended: bool,
    end: [u64; 3],
}

// Where `run_sequences` takes its sequences from: the bitstream, or a decoded buffer.
trait Source {
    fn next<const IN_CONTAINER: bool>(&mut self, last: bool) -> Sequence;
    fn has_room_for_sequence(&self) -> bool;
    // The sequences were the whole stream.
    fn ended(&self) -> bool;
    fn repeat_offsets(&self) -> [u32; 3];
}

// A decoded buffer, with the offsets resolved against the repeat offsets the block starts with
// (minus `REL_BIAS`, followed by 0 for `REL_NONE`).
struct Buffered<'a> {
    next: *const RawSequence,
    decoded: &'a DecodedSequences,
    base: [u32; 4],
}

// A sequence as `decode_fields` reads it: the offset code is `of_base` plus `offset_extra`
// (garbage when there are no extra bits).
struct Fields {
    literal_length: usize,
    match_length: usize,
    offset_extra: usize,
    of_extra_bits: u8,
    of_base: u32,
    ll_is_zero: bool,
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

// The output buffer as raw memory: a pipeline hashes the finished part of it on another thread
// while later blocks are written, so no `&mut` may cover it. Wild copies may write up to the end.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Output {
    pub(crate) base: *mut u8,
    pub(crate) len: usize,
}

impl Output {
    pub(crate) const fn new(slice: &mut [mem::MaybeUninit<u8>]) -> Self {
        Self {
            base: slice.as_mut_ptr().cast(),
            len: slice.len(),
        }
    }
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

impl Default for TableDef {
    fn default() -> Self {
        Self {
            kind: TableKind::Dictionary,
            norm: [0; MAX_SYMBOLS],
            max_symbol: 0,
            table_log: 0,
        }
    }
}

impl Tables {
    pub(crate) fn boxed() -> Box<Self> {
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
        let (mut used, mut def) = (0, TableDef::default());
        for (index, max_symbol, max_log) in [
            (1, OF_MAX_SYMBOL, OF_TABLE_LOG_MAX),
            (2, ML_MAX_SYMBOL, ML_TABLE_LOG_MAX),
            (0, LL_MAX_SYMBOL, LL_TABLE_LOG_MAX),
        ] {
            let spec = TableSpec {
                mode: MODE_COMPRESSED,
                max_symbol,
                max_log,
            };
            used += describe_table(spec, &src[used..], &mut def)
                .map_err(|_| Error::DictionaryCorrupted)?;
            tables.build(index, &def, None)?;
        }
        Ok((tables, used))
    }

    // Builds table `index` (ll, of, ml) as `def` defines it; `dictionary` has the tables that
    // `TableKind::Dictionary` stands for.
    #[inline(always)]
    pub(crate) fn build(
        &mut self,
        index: usize,
        def: &TableDef,
        dictionary: Option<&Self>,
    ) -> Result<(), Error> {
        let custom = &mut self.custom[index];
        match index {
            0 => build_from_def(
                &mut self.ll,
                custom,
                def,
                dictionary.map(|tables| &tables.ll),
                &LL_DEFAULT_TABLE,
                (&LL_BASE, &LL_EXTRA_BITS),
            ),
            1 => {
                build_from_def(
                    &mut self.of,
                    custom,
                    def,
                    dictionary.map(|tables| &tables.of),
                    &OF_DEFAULT_TABLE,
                    (&OF_BASE, &OF_EXTRA_BITS),
                )?;
                self.short_offsets = short_offset_share(&self.of) >= SHORT_OFFSET_SHARE_MIN;
                Ok(())
            }
            _ => build_from_def(
                &mut self.ml,
                custom,
                def,
                dictionary.map(|tables| &tables.ml),
                &ML_DEFAULT_TABLE,
                (&ML_BASE, &ML_EXTRA_BITS),
            ),
        }
    }

    // Builds the tables `defs` define (stage 2 of the split path), whatever they held.
    pub(crate) fn build_all(
        &mut self,
        defs: &TableDefs,
        dictionary: Option<&Dictionary>,
    ) -> Result<(), Error> {
        let dictionary = dictionary
            .and_then(Dictionary::entropy)
            .map(|entropy| &*entropy.sequences);
        self.custom = [true; 3];
        for (index, def) in defs.iter().enumerate() {
            self.build(index, def, dictionary)?;
        }
        Ok(())
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
            defs: TableDefs::default(),
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
        for def in &mut self.defs {
            def.kind = TableKind::Dictionary;
        }
        if entropy.is_some() {
            self.tables.custom = [true; 3];
        }
    }

    // `ZSTD_decodeSeqHeaders`: reads the sequence count and the table modes. Returns the count and
    // the bitstream that follows. With `BUILD` the tables are built here, else `defs` become the
    // definitions of the block's tables, which stage 2 builds.
    #[inline(never)]
    pub(crate) fn read_header<'a, const BUILD: bool>(
        &mut self,
        src: &'a [u8],
    ) -> Result<(usize, &'a [u8]), Error> {
        let (nb_seq, rest) = read_count(src)?;
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
        rest = self.read_table::<0, BUILD>(modes, rest)?;
        rest = self.read_table::<1, BUILD>(modes, rest)?;
        rest = self.read_table::<2, BUILD>(modes, rest)?;
        if self.in_dictionary != [true; 3] {
            // Repeated tables that are the dictionary's are used from `tables` from now on.
            for index in 0..3 {
                if BUILD && self.in_dictionary[index] {
                    let dictionary = dictionary_tables(self.dictionary.as_ref());
                    self.tables.build(index, &self.defs[index], dictionary)?;
                }
            }
            self.in_dictionary = [false; 3];
        }
        Ok((nb_seq, rest))
    }

    // The table `INDEX` (ll, of, ml) of a block, its mode in `modes`: returns what follows it in
    // `src`.
    #[inline(always)]
    fn read_table<'a, const INDEX: usize, const BUILD: bool>(
        &mut self,
        modes: u8,
        src: &'a [u8],
    ) -> Result<&'a [u8], Error> {
        const LIMITS: [(u32, u32); 3] = [
            (LL_MAX_SYMBOL, LL_TABLE_LOG_MAX),
            (OF_MAX_SYMBOL, OF_TABLE_LOG_MAX),
            (ML_MAX_SYMBOL, ML_TABLE_LOG_MAX),
        ];
        let mode = (modes >> (6 - 2 * INDEX)) & 3;
        self.in_dictionary[INDEX] &= mode == MODE_REPEAT;
        if mode == MODE_REPEAT {
            return if self.has_tables {
                Ok(src)
            } else {
                Err(Error::CorruptionDetected)
            };
        }
        let (max_symbol, max_log) = LIMITS[INDEX];
        let spec = TableSpec {
            mode,
            max_symbol,
            max_log,
        };
        let used = describe_table(spec, src, &mut self.defs[INDEX])?;
        if BUILD {
            let dictionary = dictionary_tables(self.dictionary.as_ref());
            self.tables.build(INDEX, &self.defs[INDEX], dictionary)?;
        }
        Ok(&src[used..])
    }

    pub(crate) const fn repeat_offsets(&self) -> [u32; 3] {
        self.repeat_offsets
    }

    pub(crate) const fn dictionary(&self) -> Option<&Dictionary> {
        self.dictionary.as_ref()
    }

    // What the blocks so far left besides the tables: repeat mode allowed, the dictionary's
    // tables in use (all or none), the dictionary still cold.
    pub(crate) const fn flags(&self) -> [bool; 5] {
        let [ll, of, ml] = self.in_dictionary;
        [self.has_tables, ll, of, ml, self.dictionary_is_cold]
    }

    pub(crate) const fn defs(&self) -> &TableDefs {
        &self.defs
    }

    // Back to the state `flags`, `repeat_offsets` and `defs` (the tables of the last block)
    // describe, after blocks planned further ahead changed it.
    pub(crate) fn restore(&mut self, flags: [bool; 5], repeat_offsets: [u32; 3], defs: &TableDefs) {
        let [has_tables, ll, of, ml, cold] = flags;
        (self.has_tables, self.in_dictionary, self.dictionary_is_cold) =
            (has_tables, [ll, of, ml], cold);
        self.repeat_offsets = repeat_offsets;
        self.defs = *defs;
        if self.in_dictionary != [true; 3] {
            let _ = self.tables.build_all(defs, self.dictionary.as_ref());
        }
    }

    // What a block decides in stream order before its sequences run: repeat modes are allowed
    // from the first sequence on, and the cold dictionary flag is used up. Returns that flag.
    pub(crate) fn begin_block(&mut self, nb_seq: usize) -> bool {
        self.has_tables |= nb_seq > 0;
        mem::take(&mut self.dictionary_is_cold)
    }

    // The tables the block's sequences decode with: the dictionary's until one is read.
    pub(crate) fn block_tables(&self) -> &Tables {
        match &self.dictionary {
            Some(dictionary) if self.in_dictionary == [true; 3] => {
                &dictionary.entropy().unwrap().sequences
            }
            _ => &self.tables,
        }
    }

    // Decodes and executes the block's sequences from `stream`, then copies the remaining
    // literals. Returns the position after the block's output.
    pub(crate) fn execute_block(
        &mut self,
        nb_seq: usize,
        stream: &[u8],
        literals: &Literals<'_>,
        out: Output,
        place: &Placement<'_>,
    ) -> Result<usize, Error> {
        let cold = self.begin_block(nb_seq);
        let (mut op, mut literal_pos) = (place.range.start, 0);
        if nb_seq > 0 {
            let tables = self.block_tables();
            let repeat_offsets = self.repeat_offsets;
            let decoder = || SequenceDecoder::new(stream, tables, repeat_offsets);
            (op, literal_pos, self.repeat_offsets) =
                run_block(decoder, tables, cold, nb_seq, literals, out, place)?;
        }
        finish_block(literals, literal_pos, op, out, place)
    }
}

impl DecodedSequences {
    pub(crate) fn clear(&mut self) {
        self.raw.clear();
    }

    // Stage 2: decodes the `nb_seq` sequences of `stream`, offsets relative to the block's start.
    pub(crate) fn decode(
        &mut self,
        tables: &Tables,
        stream: &[u8],
        nb_seq: usize,
    ) -> Result<(), Error> {
        let mut decoder = SequenceDecoder::new(stream, tables, [0; 3])?;
        self.raw.clear();
        self.raw.reserve(nb_seq);
        let mut offsets = REL_START;
        let out = self.raw.as_mut_ptr();
        // The fast phase of the x86 sequence loop while the stream has room, then the exact code.
        let mut index = 0;
        while index + 1 < nb_seq && decoder.has_room_for_sequence() {
            let sequence = relative_sequence(&mut offsets, &decoder.decode_fields::<true>(false));
            // SAFETY: `nb_seq` sequences are reserved.
            unsafe { out.add(index).write(sequence) };
            index += 1;
        }
        for index in index..nb_seq {
            let fields = decoder.decode_fields::<false>(index + 1 == nb_seq);
            let sequence = relative_sequence(&mut offsets, &fields);
            // SAFETY: as above.
            unsafe { out.add(index).write(sequence) };
        }
        // SAFETY: the `nb_seq` sequences are written.
        unsafe { self.raw.set_len(nb_seq) };
        self.end = offsets;
        self.ended = decoder.bits.end_of_stream();
        Ok(())
    }
}

// Stage 3: resolves the offsets of `decoded` against `repeat_offsets` and executes it, then
// copies the remaining literals. `tables` are the block's, `cold` is `begin_block`'s. Returns
// the position after the block's output.
pub(crate) fn execute_decoded(
    decoded: &DecodedSequences,
    tables: Option<&Tables>,
    cold: bool,
    repeat_offsets: &mut [u32; 3],
    literals: &Literals<'_>,
    out: Output,
    place: &Placement<'_>,
) -> Result<usize, Error> {
    let (mut op, mut literal_pos) = (place.range.start, 0);
    if let Some(tables) = tables.filter(|_| !decoded.raw.is_empty()) {
        let [rep0, rep1, rep2] = repeat_offsets.map(|offset| offset.wrapping_sub(REL_BIAS));
        let source = || {
            Ok(Buffered {
                next: decoded.raw.as_ptr(),
                decoded,
                base: [rep0, rep1, rep2, 0],
            })
        };
        (op, literal_pos, *repeat_offsets) = run_block(
            source,
            tables,
            cold,
            decoded.raw.len(),
            literals,
            out,
            place,
        )?;
    }
    finish_block(literals, literal_pos, op, out, place)
}

// Picks the loop of a block (`ZSTD_decompressSequences*`) and runs it. Returns the output and
// literals positions and the repeat offsets after it.
fn run_block<S: Source>(
    source: impl FnOnce() -> Result<S, Error>,
    tables: &Tables,
    cold: bool,
    nb_seq: usize,
    literals: &Literals<'_>,
    out: Output,
    place: &Placement<'_>,
) -> Result<(usize, usize, [u32; 3]), Error> {
    let prefetch = cold
        || nb_seq > LOOKAHEAD
            && place.total_history > PREFETCH_HISTORY_MIN
            && long_offset_share(&tables.of) >= LONG_OFFSET_SHARE_MIN;
    // Blocks reaching the previous ring segment always use the portable kernel.
    if place.ext.len() > 0 {
        if prefetch {
            run_sequences::<S, Portable, true, true>(source, nb_seq, literals, out, place)
        } else {
            run_sequences::<S, Portable, false, true>(source, nb_seq, literals, out, place)
        }
    } else if prefetch {
        run_sequences::<S, Portable, true, false>(source, nb_seq, literals, out, place)
    } else if tables.short_offsets {
        run_sequences::<S, ShortOffsets, false, false>(source, nb_seq, literals, out, place)
    } else {
        run_sequences::<S, Portable, false, false>(source, nb_seq, literals, out, place)
    }
}

// Copies the literals left after the last sequence. Returns the position after them.
fn finish_block(
    literals: &Literals<'_>,
    literal_pos: usize,
    op: usize,
    out: Output,
    place: &Placement<'_>,
) -> Result<usize, Error> {
    let last_literals = &literals.bytes()[literal_pos..];
    if last_literals.len() > place.range.end - op {
        return Err(Error::DstSizeTooSmall);
    }
    // SAFETY: op + last_literals.len() <= place.range.end <= out.len.
    unsafe {
        core::ptr::copy_nonoverlapping(
            last_literals.as_ptr(),
            out.base.add(op),
            last_literals.len(),
        );
    }
    Ok(op + last_literals.len())
}

// The block's sequence loop: takes one sequence ahead of the one executed, or with
// `PREFETCH` `LOOKAHEAD` ahead (`ZSTD_decompressSequencesLong`), touching the match
// sources and checking the stream end before the last ones run. With `EXT`, matches may
// reach the previous ring segment. Returns the output and literals positions and the
// repeat offsets after it.
#[inline(never)]
fn run_sequences<S: Source, K: Kernel, const PREFETCH: bool, const EXT: bool>(
    source: impl FnOnce() -> Result<S, Error>,
    nb_seq: usize,
    literals: &Literals<'_>,
    out: Output,
    place: &Placement<'_>,
) -> Result<(usize, usize, [u32; 3]), Error> {
    let range = &place.range;
    // The unsafe execution below relies on these.
    assert!((range.start..=out.len).contains(&range.end));
    assert!(literals.padded.len() >= literals.len + WILDCOPY_OVERLENGTH);
    let mut source = source()?;
    let out_base = out.base;
    let limits = Limits {
        out_base,
        out_len: out.len,
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
            let sequence = source.next::<false>(index == nb_seq - 1);
            position = prefetch_match(position, sequence, &limits);
            *slot = sequence;
        }
        for index in ahead..nb_seq {
            let sequence = source.next::<false>(index == nb_seq - 1);
            let oldest = queue[index % LOOKAHEAD];
            // SAFETY: `limits` and `cursor` describe `out` and `literals`, as `exec_sequence` needs.
            cursor = unsafe { exec_sequence::<K, EXT>(kernel, oldest, cursor, &limits) }?;
            position = prefetch_match(position, sequence, &limits);
            queue[index % LOOKAHEAD] = sequence;
        }
        if !source.ended() {
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
            while remaining > 1 && source.has_room_for_sequence() {
                let sequence = source.next::<true>(false);
                // SAFETY: `limits` and `cursor` describe `out` and `literals`, as `exec_sequence` needs.
                cursor = unsafe { exec_sequence::<K, EXT>(kernel, sequence, cursor, &limits) }?;
                remaining -= 1;
            }
            for remaining in (1..=remaining).rev() {
                let sequence = source.next::<false>(remaining == 1);
                // SAFETY: as above.
                cursor = unsafe { exec_sequence::<K, EXT>(kernel, sequence, cursor, &limits) }?;
            }
        } else {
            let mut current = source.next::<false>(nb_seq == 1);
            for remaining in (1..nb_seq).rev() {
                let following = source.next::<false>(remaining == 1);
                // SAFETY: as above.
                cursor = unsafe { exec_sequence::<K, EXT>(kernel, current, cursor, &limits) }?;
                current = following;
            }
            // SAFETY: as above.
            cursor = unsafe { exec_sequence::<K, EXT>(kernel, current, cursor, &limits) }?;
        }
        if !source.ended() {
            return Err(Error::CorruptionDetected);
        }
    }
    let op = cursor.op.addr() - limits.out_base.addr();
    let literal_pos = cursor.lit.addr() - limits.lit_base.addr();
    Ok((op, literal_pos, source.repeat_offsets()))
}

impl<'a> SequenceDecoder<'a> {
    #[inline(always)]
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
        let fields = self.decode_fields::<IN_CONTAINER>(last);
        let Fields {
            literal_length,
            match_length,
            offset_extra,
            of_extra_bits,
            of_base,
            ll_is_zero,
        } = fields;
        // Constant indices only: dynamic ones would keep the array in memory.
        let [rep0, rep1, rep2] = self.repeat_offsets;
        let offset = if of_extra_bits > 1 {
            let offset = of_base as usize + offset_extra;
            self.repeat_offsets = [offset, rep0, rep1];
            offset
        } else if of_extra_bits == 0 {
            if ll_is_zero {
                self.repeat_offsets = [rep1, rep0, rep2];
            }
            self.repeat_offsets[0]
        } else {
            // Offset codes 1 to 3: second or third repeat offset, or the first minus one.
            let code = of_base as usize + usize::from(ll_is_zero) + offset_extra;
            let offset = match code {
                1 => rep1,
                2 => rep2,
                _ => rep0.wrapping_sub(1),
            };
            // A zero offset is invalid: -1 fails the bounds check at execution.
            let offset = if offset == 0 { usize::MAX } else { offset };
            self.repeat_offsets = [offset, rep0, if code == 1 { rep2 } else { rep1 }];
            offset
        };
        Sequence {
            literal_length,
            match_length,
            offset,
        }
    }

    // What the bitstream gives for a sequence, and the FSE states after it.
    #[inline(always)]
    fn decode_fields<const IN_CONTAINER: bool>(&mut self, last: bool) -> Fields {
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
        Fields {
            literal_length,
            match_length,
            offset_extra,
            of_extra_bits: of.nb_additional_bits,
            of_base: of.base_value,
            ll_is_zero: ll.base_value == 0,
        }
    }
}

impl Source for SequenceDecoder<'_> {
    #[inline(always)]
    fn next<const IN_CONTAINER: bool>(&mut self, last: bool) -> Sequence {
        Self::next::<IN_CONTAINER>(self, last)
    }

    #[inline(always)]
    fn has_room_for_sequence(&self) -> bool {
        self.bits.has_room_for_sequence()
    }

    #[inline(always)]
    fn ended(&self) -> bool {
        self.bits.end_of_stream()
    }

    fn repeat_offsets(&self) -> [u32; 3] {
        self.repeat_offsets.map(|offset| offset as u32)
    }
}

impl Source for Buffered<'_> {
    #[inline(always)]
    fn next<const IN_CONTAINER: bool>(&mut self, _last: bool) -> Sequence {
        // SAFETY: `run_sequences` takes `decoded.raw.len()` sequences, no more.
        let raw = unsafe { self.next.read() };
        self.next = self.next.wrapping_add(1);
        let offset = resolve_relative(
            &self.base,
            u64::from(raw.literal_length_from >> 30) << 32 | u64::from(raw.offset),
        ) as usize;
        Sequence {
            literal_length: (raw.literal_length_from & REL_LITERAL_MASK) as usize,
            match_length: raw.match_length as usize,
            // A zero offset is invalid: -1 fails the bounds check at execution.
            offset: if offset == 0 { usize::MAX } else { offset },
        }
    }

    #[inline(always)]
    fn has_room_for_sequence(&self) -> bool {
        true
    }

    #[inline(always)]
    fn ended(&self) -> bool {
        self.decoded.ended
    }

    fn repeat_offsets(&self) -> [u32; 3] {
        self.decoded
            .end
            .map(|offset| resolve_relative(&self.base, offset))
    }
}

// The offset a relative `offset` (`from << 32 | offset`) stands for, `base` being the starting
// repeat offsets minus `REL_BIAS`, then 0.
#[inline(always)]
const fn resolve_relative(base: &[u32; 4], offset: u64) -> u32 {
    base[(offset >> 32) as usize & 3].wrapping_add(offset as u32)
}

// The sequence `fields` as stage 2 stores it, updating the relative repeat `offsets` as
// `SequenceDecoder::next` updates the real ones.
#[inline(always)]
fn relative_sequence(offsets: &mut [u64; 3], fields: &Fields) -> RawSequence {
    let [rep0, rep1, rep2] = *offsets;
    let offset = if fields.of_extra_bits > 1 {
        let offset = REL_NONE | u64::from(fields.of_base + fields.offset_extra as u32);
        *offsets = [offset, rep0, rep1];
        offset
    } else if fields.of_extra_bits == 0 {
        if fields.ll_is_zero {
            *offsets = [rep1, rep0, rep2];
        }
        offsets[0]
    } else {
        // Offset codes 1 to 3: second or third repeat offset, or the first minus one.
        let code = fields.of_base as usize + usize::from(fields.ll_is_zero) + fields.offset_extra;
        let offset = match code {
            1 => rep1,
            2 => rep2,
            _ => rep0.wrapping_sub(1),
        };
        *offsets = [offset, rep0, if code == 1 { rep2 } else { rep1 }];
        offset
    };
    RawSequence {
        literal_length_from: fields.literal_length as u32 | ((offset >> 32) as u32) << 30,
        match_length: fields.match_length as u32,
        offset: offset as u32,
    }
}

// Where libzstd stops a block's output: literals stored in the output past
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

// The sequence count at the start of a sequences section, and the rest.
#[inline(always)]
fn read_count(src: &[u8]) -> Result<(usize, &[u8]), Error> {
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
    Ok((nb_seq, rest))
}

// The tables of the dictionary, if it has any.
fn dictionary_tables(dictionary: Option<&Dictionary>) -> Option<&Tables> {
    dictionary
        .and_then(Dictionary::entropy)
        .map(|entropy| &*entropy.sequences)
}

// Sets `def` to the table read from `src` (not a repeat) and returns the bytes of `src` used
// (`ZSTD_buildSeqTable` without the build).
fn describe_table(spec: TableSpec, src: &[u8], def: &mut TableDef) -> Result<usize, Error> {
    match spec.mode {
        MODE_RLE => {
            let symbol = *src.first().ok_or(Error::CorruptionDetected)?;
            if u32::from(symbol) > spec.max_symbol {
                return Err(Error::CorruptionDetected);
            }
            def.kind = TableKind::Rle(symbol);
            Ok(1)
        }
        MODE_PREDEFINED => {
            def.kind = TableKind::Predefined;
            Ok(0)
        }
        _ => {
            let ncount = read_ncount(&mut def.norm[..=spec.max_symbol as usize], src)?;
            if ncount.table_log > spec.max_log {
                return Err(Error::CorruptionDetected);
            }
            def.kind = TableKind::Counts;
            def.max_symbol = ncount.max_symbol as u8;
            def.table_log = ncount.table_log as u8;
            Ok(ncount.bytes_read)
        }
    }
}

// Builds `table` as `def` defines it. `custom`: the cells are not a copy of the predefined
// table, which is then not copied again.
fn build_from_def<const N: usize>(
    table: &mut FseTable<N>,
    custom: &mut bool,
    def: &TableDef,
    dictionary: Option<&FseTable<N>>,
    default: &FseTable<N>,
    (base, extra_bits): (&[u32], &[u8]),
) -> Result<(), Error> {
    let was_custom = mem::replace(custom, true);
    match def.kind {
        TableKind::Dictionary => table.copy_from(dictionary.ok_or(Error::CorruptionDetected)?),
        TableKind::Predefined => {
            *custom = false;
            if was_custom {
                table.copy_from(default);
            }
        }
        TableKind::Rle(symbol) => {
            table.set_rle(base[usize::from(symbol)], extra_bits[usize::from(symbol)]);
        }
        TableKind::Counts => table.build(
            &def.norm[..=usize::from(def.max_symbol)],
            u32::from(def.table_log),
            base,
            extra_bits,
        ),
    }
    Ok(())
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
    // The segment start bounds the history, except in a ring (`EXT`), where all that is out of
    // the fast path is one cold call (two calls in the loop slow it by 20%).
    let before_segment = offset > literals_end.addr() - limits.out_base.addr();
    if EXT {
        if literal_length > literals_left
            || literals_end.wrapping_add(match_length) > limits.fast_end
            || before_segment
        {
            // SAFETY: the checks of `exec_sequence_cold` are the ones it needs.
            return unsafe { exec_sequence_cold(sequence, cursor, limits) };
        }
    } else {
        if literal_length > literals_left
            || literals_end.wrapping_add(match_length) > limits.fast_end
        {
            return exec_sequence_end(sequence, cursor, limits);
        }
        if before_segment {
            return Err(Error::CorruptionDetected);
        }
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

// What `exec_sequence` leaves out of line, in one place so that the loop has a single cold
// call: a sequence near the end of the output or of the literals (the exact path), or, in a
// ring, a match starting before the segment.
// Safety: that of `exec_sequence`, in a ring when the offset is past the segment start.
#[cold]
#[inline(never)]
unsafe fn exec_sequence_cold(
    sequence: Sequence,
    cursor: Cursor,
    limits: &Limits<'_>,
) -> Result<Cursor, Error> {
    let literals_end = cursor.op.wrapping_add(sequence.literal_length);
    // SAFETY: the cursor's literals position never passes the end of the literals.
    let literals_left = unsafe {
        limits
            .lit_base
            .add(limits.literals.len())
            .offset_from_unsigned(cursor.lit)
    };
    if sequence.literal_length > literals_left
        || literals_end.wrapping_add(sequence.match_length) > limits.fast_end
    {
        return exec_sequence_end(sequence, cursor, limits);
    }
    // SAFETY: the checks above are the ones `exec_sequence_ext` needs.
    unsafe { exec_sequence_ext(sequence, cursor, limits) }
}

// `exec_sequence` for a match starting before the segment, out of line with the cursor by
// value like `exec_sequence_end`, so that the loop keeps its shape. The history before the
// segment is a dictionary (`padded`) or the end of the previous segment of the ring, ahead of
// the output position: wild copy from there when the ring is large enough around it.
// Safety: that of `exec_sequence`, after its checks, and `offset > literals_end - out_base`.
#[inline(always)]
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
    let op = cursor.op.addr() - limits.out_base.addr();
    let mut literal_pos = cursor.lit.addr() - limits.lit_base.addr();
    let op = exec_sequence_exact(
        limits.out_base,
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
    out_base: *mut u8,
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
    // SAFETY: op + literal_length <= end, bounds of the output allocation.
    unsafe {
        core::ptr::copy_nonoverlapping(
            literals[*literal_pos..].as_ptr(),
            out_base.add(op),
            literal_length,
        );
    }
    *literal_pos += literal_length;
    copy_match(out_base, literals_end, offset, match_length, ext)?;
    Ok(literals_end + match_length)
}

// Copies a match to `out_base.add(dest)`. The history is preceding bytes in `out_base`,
// preceded by `ext`. A match can span both, and overlap its own output.
fn copy_match(
    out_base: *mut u8,
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
            Ext::Ring(_) => {
                // SAFETY: start and dest are within the ring buffer; ranges may overlap.
                unsafe {
                    core::ptr::copy(out_base.add(start), out_base.add(dest), from_previous);
                }
            }
            Ext::Dict(padded) => {
                // SAFETY: source is within the padded dictionary; dest in output.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        padded[start..].as_ptr(),
                        out_base.add(dest),
                        from_previous,
                    );
                }
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
        // SAFETY: source + chunk <= dest + done <= dest + remaining; within output.
        unsafe {
            core::ptr::copy(out_base.add(source), out_base.add(dest + done), chunk);
        }
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
