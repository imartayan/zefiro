use alloc::vec;
use alloc::vec::Vec;

use super::*;

const PREDEFINED: u8 = 0;
const RLE: u8 = 1;
const COMPRESSED: u8 = 2;
const REPEAT: u8 = 3;

fn modes(ll: u8, of: u8, ml: u8) -> u8 {
    ll << 6 | of << 4 | ml << 2
}

fn literals(bytes: &[u8]) -> (Vec<u8>, usize) {
    ([bytes, &[0; WILDCOPY_OVERLENGTH]].concat(), bytes.len())
}

fn header(src: &[u8]) -> Result<(usize, usize), Error> {
    SequencesState::new()
        .read_header(src)
        .map(|(nb_seq, stream)| (nb_seq, stream.len()))
}

// Backward stream of fields given in reading order (value, bit count):
// the end mark, then the fields, most significant bit first.
fn stream(fields: &[(u64, u32)]) -> Vec<u8> {
    let mut bits = vec![true];
    for &(value, count) in fields {
        bits.extend((0..count).rev().map(|bit| value >> bit & 1 == 1));
    }
    let padding = (8 - bits.len() % 8) % 8;
    let bits = [vec![false; padding], bits].concat();
    bits.chunks(8)
        .map(|chunk| chunk.iter().fold(0, |byte, &bit| byte << 1 | u8::from(bit)))
        .rev()
        .collect()
}

// A block of `count` sequences whose three tables are RLE with the given
// symbols (no state bits): returns the header and stream, as `read_header` expects.
fn rle_block(count: u8, (ll, of, ml): (u8, u8, u8), fields: &[(u64, u32)]) -> Vec<u8> {
    [
        &[count, modes(RLE, RLE, RLE), ll, of, ml][..],
        &stream(fields),
    ]
    .concat()
}

fn run(
    state: &mut SequencesState,
    block: &[u8],
    literals: &[u8],
    history: &[u8],
    capacity: usize,
) -> Result<Vec<u8>, Error> {
    let (padded, len) = self::literals(literals);
    let literals = Literals {
        padded: &padded,
        len,
        in_input: false,
    };
    let (nb_seq, stream) = state.read_header(block)?;
    let mut out = [history, &vec![0; capacity]].concat();
    let end = history.len() + capacity;
    let place = Placement {
        range: history.len()..end,
        ext: Ext::Ring(0),
        total_history: 0,
    };
    let block_end = state.execute_block(nb_seq, stream, &literals, &mut out, &place)?;
    Ok(out[history.len()..block_end].to_vec())
}

#[test]
fn header_sequence_counts() {
    assert_eq!(header(&[0]), Ok((0, 0)));
    assert_eq!(header(&[127, 0, 9]), Ok((127, 1)));
    assert_eq!(header(&[0x80, 5, 0]), Ok((5, 0)));
    assert_eq!(header(&[0x81, 0, 0]), Ok((256, 0)));
    assert_eq!(header(&[0xFE, 0xFF, 0]), Ok((0x7EFF, 0)));
    assert_eq!(header(&[0xFF, 0, 0, 0]), Ok((0x7F00, 0)));
    assert_eq!(header(&[0xFF, 0xFF, 0xFF, 0]), Ok((0x17EFF, 0)));
}

#[test]
fn header_errors_in_libzstd_order() {
    assert_eq!(header(&[]), Err(Error::SrcSizeWrong));
    assert_eq!(header(&[0x80]), Err(Error::SrcSizeWrong));
    assert_eq!(header(&[0xFF, 1]), Err(Error::SrcSizeWrong));
    // Bytes after a zero count; a count without the modes byte.
    assert_eq!(header(&[0, 0]), Err(Error::CorruptionDetected));
    assert_eq!(header(&[3]), Err(Error::SrcSizeWrong));
    // Reserved bits of the modes byte come before any table.
    assert_eq!(header(&[3, 1]), Err(Error::CorruptionDetected));
    assert_eq!(header(&[3, 0x80 | 3]), Err(Error::CorruptionDetected));
    // Repeat with no table, RLE without a symbol or with a symbol too large.
    assert_eq!(
        header(&[3, modes(REPEAT, PREDEFINED, PREDEFINED)]),
        Err(Error::CorruptionDetected)
    );
    assert_eq!(
        header(&[3, modes(PREDEFINED, PREDEFINED, RLE)]),
        Err(Error::CorruptionDetected)
    );
    assert_eq!(
        header(&[3, modes(RLE, PREDEFINED, PREDEFINED), 36]),
        Err(Error::CorruptionDetected)
    );
    assert_eq!(
        header(&[3, modes(RLE, PREDEFINED, PREDEFINED), 35]),
        Ok((3, 0))
    );
    assert_eq!(
        header(&[3, modes(PREDEFINED, RLE, PREDEFINED), 32]),
        Err(Error::CorruptionDetected)
    );
    assert_eq!(
        header(&[3, modes(PREDEFINED, PREDEFINED, RLE), 53]),
        Err(Error::CorruptionDetected)
    );
    // A truncated FSE header; a table log of 16 (above 9) is too large.
    assert_eq!(
        header(&[3, modes(COMPRESSED, PREDEFINED, PREDEFINED)]),
        Err(Error::CorruptionDetected)
    );
    assert_eq!(
        header(&[3, modes(COMPRESSED, PREDEFINED, PREDEFINED), 0x0B, 0, 0, 0]),
        Err(Error::CorruptionDetected)
    );
}

#[test]
fn repeat_mode_needs_a_block_that_started_decoding() {
    let mut state = SequencesState::new();
    let repeat = [1, modes(REPEAT, REPEAT, REPEAT), 1];
    assert_eq!(
        state.read_header(&repeat).err(),
        Some(Error::CorruptionDetected)
    );
    // A block with sequences sets the flag even if it then fails.
    let all_predefined = [1, 0, 0];
    let (nb_seq, stream) = state.read_header(&all_predefined).unwrap();
    let (padded, len) = literals(b"");
    let literals = Literals {
        padded: &padded,
        len,
        in_input: false,
    };
    let mut out = [0; 10];
    let place = Placement {
        range: 0..10,
        ext: Ext::Ring(0),
        total_history: 0,
    };
    assert!(
        state
            .execute_block(nb_seq, stream, &literals, &mut out, &place)
            .is_err()
    );
    assert!(state.read_header(&repeat).is_ok());
    state.start_frame();
    assert_eq!(
        state.read_header(&repeat).err(),
        Some(Error::CorruptionDetected)
    );
}

#[test]
fn repeat_offsets_follow_the_literal_length_and_persist() {
    // Literal length code 0, offset code 0, match length code 0 (3 bytes): every
    // sequence repeats an offset, the second one when there are no literals.
    let block = rle_block(3, (0, 0, 0), &[]);
    let mut state = SequencesState::new();
    let history = b"0123456789";
    let out = run(&mut state, &block, b"", history, 20).unwrap();
    // Offsets 4, 1, 4: "678", then "888", then "8" + "888"[..2] shifted by 4.
    assert_eq!(out, b"678888888");
    assert_eq!(state.repeat_offsets, [4, 1, 8]);
    // Next frame starts again from 1, 4, 8.
    state.start_frame();
    assert_eq!(state.repeat_offsets, [1, 4, 8]);
}

// Two sequences using extra bits of every field: the block, its literals and the output.
fn offset_codes_block() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    // Literal length code 16 (16 + 1 bit), offset code 3 (5 + 3 bits), match
    // length code 32 (35 + 1 bit). Fields are read in the order offset, match, literal.
    let block = rle_block(
        2,
        (16, 3, 32),
        &[(3, 3), (1, 1), (0, 1), (2, 3), (0, 1), (1, 1)],
    );
    let data: Vec<u8> = (0..40).collect();
    let (mut expected, mut position) = (Vec::new(), 0);
    for (literal_length, match_length, offset) in [(16, 36, 8), (17, 35, 7)] {
        expected.extend_from_slice(&data[position..position + literal_length]);
        position += literal_length;
        for _ in 0..match_length {
            expected.push(expected[expected.len() - offset]);
        }
    }
    expected.extend_from_slice(&data[position..]);
    (block, data, expected)
}

#[test]
fn offset_codes_and_extra_bits() {
    let (block, data, expected) = offset_codes_block();
    let out = run(&mut SequencesState::new(), &block, &data, b"", 200).unwrap();
    assert_eq!(out, expected);
}

#[test]
fn output_is_the_same_whether_sequences_take_the_wild_copy_path() {
    let (block, data, expected) = offset_codes_block();
    let history = b"abcdefgh";
    for capacity in expected.len()..expected.len() + 80 {
        let out = run(&mut SequencesState::new(), &block, &data, history, capacity);
        assert_eq!(out.as_deref(), Ok(&expected[..]), "capacity {capacity}");
    }
    let too_small = run(
        &mut SequencesState::new(),
        &block,
        &data,
        history,
        expected.len() - 1,
    );
    assert_eq!(too_small, Err(Error::DstSizeTooSmall));
}

#[test]
fn execution_checks_in_libzstd_order() {
    let ok = |literal_length, match_length, offset| Sequence {
        literal_length,
        match_length,
        offset,
    };
    let literals = *b"abcdefgh";
    let mut position = 0;
    let mut run = |out: &mut [u8], op, end, sequence| {
        exec_sequence_exact(
            out,
            op,
            end,
            sequence,
            &literals,
            &mut position,
            Ext::Ring(0),
        )
    };
    let mut out = [0u8; 64];
    out[..4].copy_from_slice(b"wxyz");
    // Output too small beats a literal overrun, which beats a bad offset.
    assert_eq!(
        run(&mut out, 4, 10, ok(20, 3, usize::MAX / 2)),
        Err(Error::DstSizeTooSmall)
    );
    assert_eq!(
        run(&mut out, 4, 40, ok(20, 3, usize::MAX / 2)),
        Err(Error::CorruptionDetected)
    );
    assert_eq!(
        run(&mut out, 4, 40, ok(2, 3, 7)),
        Err(Error::CorruptionDetected)
    );
    // The offset may reach the start of the frame's output, not further.
    assert_eq!(run(&mut out, 4, 40, ok(0, 3, 4)), Ok(7));
    assert_eq!(&out[..7], b"wxyzwxy");
    // The window overlaps itself: offset 1 repeats a byte, offset 3 a pattern.
    assert_eq!(run(&mut out, 7, 40, ok(0, 5, 1)), Ok(12));
    assert_eq!(&out[..12], b"wxyzwxyyyyyy");
    assert_eq!(run(&mut out, 12, 40, ok(1, 7, 3)), Ok(20));
    assert_eq!(&out[12..20], b"cyycyycy");
}

#[test]
fn matches_reach_the_previous_ring_segment() {
    // Offsets 4, 1, 4 with no literals, as in the test above; the history is the end of
    // the previous segment, at a distance from the new one that takes the wild copy (16 or
    // more, and room for its slack after the history) or the exact path.
    let block = rle_block(3, (0, 0, 0), &[]);
    let mut state = SequencesState::new();
    let (nb_seq, stream) = state.read_header(&block).unwrap();
    let (padded, len) = literals(b"");
    let literals = Literals {
        padded: &padded,
        len,
        in_input: false,
    };
    for (size, ext_len) in [
        (70, 40),
        (200, 100),
        (200, 196),
        (200, 18),
        (200, 35),
        (200, 36),
    ] {
        for end in 9..=size {
            let mut ring = vec![b'.'; size];
            ring[ext_len - 4..ext_len].copy_from_slice(b"6789");
            let place = Placement {
                range: 0..end,
                ext: Ext::Ring(ext_len),
                total_history: 0,
            };
            let block_end = state.execute_block(nb_seq, stream, &literals, &mut ring, &place);
            assert_eq!(block_end, Ok(9), "size {size} ext {ext_len} end {end}");
            assert_eq!(
                &ring[..9],
                b"678888888",
                "size {size} ext {ext_len} end {end}"
            );
            state.repeat_offsets = INITIAL_REPEAT_OFFSETS;
        }
    }
    // Four bytes back from the start of a segment, with 3 bytes of the previous one.
    let place = Placement {
        range: 0..70,
        ext: Ext::Ring(3),
        total_history: 0,
    };
    let result = state.execute_block(nb_seq, stream, &literals, &mut [0; 70], &place);
    assert_eq!(result, Err(Error::CorruptionDetected));
}

#[test]
fn matches_reach_a_dictionary() {
    // The test above with the history in a dictionary (with its slack), for output ends
    // that take the wild copy and the exact path.
    let block = rle_block(3, (0, 0, 0), &[]);
    let mut state = SequencesState::new();
    let (nb_seq, stream) = state.read_header(&block).unwrap();
    let (padded, len) = literals(b"");
    let literals = Literals {
        padded: &padded,
        len,
        in_input: false,
    };
    let dictionary = |content: &[u8]| [content, &[0xAA; WILDCOPY_OVERLENGTH]].concat();
    let history = dictionary(&[[b'.'; 36].as_slice(), b"6789"].concat());
    for end in 9..=70 {
        let mut out = [b'.'; 70];
        let place = Placement {
            range: 0..end,
            ext: Ext::Dict(&history),
            total_history: 0,
        };
        let block_end = state.execute_block(nb_seq, stream, &literals, &mut out, &place);
        assert_eq!(block_end, Ok(9), "end {end}");
        assert_eq!(&out[..9], b"678888888", "end {end}");
        state.repeat_offsets = INITIAL_REPEAT_OFFSETS;
    }
    let short = dictionary(b"789");
    let place = Placement {
        range: 0..70,
        ext: Ext::Dict(&short),
        total_history: 0,
    };
    let result = state.execute_block(nb_seq, stream, &literals, &mut [0; 70], &place);
    assert_eq!(result, Err(Error::CorruptionDetected));
}

#[test]
fn bitstream_must_end_exactly() {
    // One sequence needs no bits; any extra bit in the stream is corruption.
    let ok = rle_block(1, (0, 0, 0), &[]);
    let mut state = SequencesState::new();
    assert_eq!(run(&mut state, &ok, b"", b"abcd", 10).unwrap(), b"abc");
    let extra_bit = rle_block(1, (0, 0, 0), &[(0, 1)]);
    assert_eq!(
        run(&mut state, &extra_bit, b"", b"abc", 10),
        Err(Error::CorruptionDetected)
    );
    // No end mark.
    let no_mark = [1, modes(RLE, RLE, RLE), 0, 0, 0, 0];
    assert_eq!(
        run(&mut state, &no_mark, b"", b"abc", 10),
        Err(Error::CorruptionDetected)
    );
}

#[test]
fn last_literals_must_fit() {
    let block = [0u8];
    let mut state = SequencesState::new();
    assert_eq!(run(&mut state, &block, b"hello", b"", 5).unwrap(), b"hello");
    assert_eq!(
        run(&mut state, &block, b"hello", b"", 4),
        Err(Error::DstSizeTooSmall)
    );
}

#[test]
fn output_limit_follows_the_literals_placement() {
    let block_max = 1 << 17;
    let limit = |capacity, len, in_input, streaming| {
        let (padded, _) = literals(&[]);
        block_output_limit(
            capacity,
            &Literals {
                padded: &padded,
                len,
                in_input,
            },
            block_max,
            streaming,
        )
    };
    let room = block_max + 64;
    // Room after the block for the literals: they go there and cap the block.
    assert_eq!(limit(room + 1000 + 1, 1000, false, false), block_max + 32);
    assert_eq!(limit(room + 1000, 1000, false, false), room + 1000);
    // Raw literals read in place, or little room: the capacity.
    assert_eq!(limit(1 << 20, 1000, true, false), 1 << 20);
    assert_eq!(limit(1000, 10, false, false), 1000);
    assert_eq!(limit(block_max, 70_000, false, false), block_max);
    // Streaming never puts literals after the block.
    assert_eq!(limit(room + 1000 + 1, 1000, false, true), room + 1000 + 1);
}

#[test]
fn short_offsets_are_flagged_by_the_offset_table_and_kept_by_repeat_mode() {
    let flag = |state: &mut SequencesState, block: &[u8]| {
        state.read_header(block).unwrap();
        state.tables.short_offsets
    };
    let mut state = SequencesState::new();
    // RLE of offset code 3 (offsets 1 to 28 only), then repeated.
    assert!(flag(&mut state, &[1, modes(RLE, RLE, RLE), 0, 3, 0, 0x80]));
    state.has_tables = true;
    assert!(flag(&mut state, &[1, modes(REPEAT, REPEAT, REPEAT), 0x80]));
    // The predefined offset table has 24 of 256 cells in codes 2 to 4.
    assert!(!flag(
        &mut state,
        &[1, modes(RLE, PREDEFINED, RLE), 0, 0, 0x80]
    ));
}

// Output (or error) and repeat offsets of one of the two sequence loops.
fn run_loop<const PREFETCH: bool>(
    block: &[u8],
    literals: &[u8],
    history: &[u8],
    capacity: usize,
) -> Result<(Vec<u8>, [u32; 3]), Error> {
    let mut state = SequencesState::new();
    let (padded, len) = self::literals(literals);
    let literals = Literals {
        padded: &padded,
        len,
        in_input: false,
    };
    let (nb_seq, stream) = state.read_header(block)?;
    let mut out = [history, &vec![0; capacity]].concat();
    let end = history.len() + capacity;
    let place = Placement {
        range: history.len()..end,
        ext: Ext::Ring(0),
        total_history: 0,
    };
    let tables = &*state.tables;
    let (op, _, repeat_offsets) = state.run_sequences::<Portable, PREFETCH, false>(
        tables, nb_seq, stream, &literals, &mut out, &place,
    )?;
    Ok((out[history.len()..op].to_vec(), repeat_offsets))
}

#[test]
fn prefetch_loop_agrees_with_the_regular_loop_except_for_the_stream_end() {
    // Literal length 2, match length 3, offset code 3 (3 extra bits): 12 sequences.
    let fields: Vec<_> = (0..12).map(|index| (index % 8, 3)).collect();
    let literals: Vec<u8> = (0..40).collect();
    let history = *b"0123456789abcdef";
    for count in [1, 3, 8, 9, 12] {
        let block = rle_block(count, (2, 3, 0), &fields[..count as usize]);
        for capacity in 0..80 {
            let regular = run_loop::<false>(&block, &literals, &history, capacity);
            let prefetch = run_loop::<true>(&block, &literals, &history, capacity);
            assert_eq!(regular, prefetch, "{count} sequences, capacity {capacity}");
        }
        for history_len in 0..history.len() {
            let regular = run_loop::<false>(&block, &literals, &history[..history_len], 80);
            let prefetch = run_loop::<true>(&block, &literals, &history[..history_len], 80);
            assert_eq!(
                regular, prefetch,
                "{count} sequences, history {history_len}"
            );
        }
    }
    // A stream with one bit too many: the prefetch loop reports it before it executes the
    // last 8 sequences, so it hides an output overflow there (libzstd does the same).
    let mut extra = fields;
    extra.push((0, 1));
    let block = rle_block(12, (2, 3, 0), &extra);
    assert_eq!(
        run_loop::<false>(&block, &literals, &history, 12 * 5 - 1),
        Err(Error::DstSizeTooSmall)
    );
    assert_eq!(
        run_loop::<true>(&block, &literals, &history, 12 * 5 - 1),
        Err(Error::CorruptionDetected)
    );
    // Before the 4th sequence only: the error comes from the executed ones first.
    assert_eq!(
        run_loop::<true>(&block, &literals, &history, 3 * 5 - 1),
        Err(Error::DstSizeTooSmall)
    );
}

#[test]
fn prefetch_loop_is_chosen_like_libzstd() {
    let mut state = SequencesState::new();
    let predefined = [9, modes(PREDEFINED, PREDEFINED, PREDEFINED), 0];
    state.read_header(&predefined).unwrap();
    // Predefined offsets: codes 23 to 28 are 6 of 32 cells, 48 out of 256.
    assert_eq!(long_offset_share(&state.tables.of), 48);
    let rle_long = [1, modes(PREDEFINED, RLE, PREDEFINED), 23];
    let mut state = SequencesState::new();
    state.read_header(&rle_long).unwrap();
    assert_eq!(long_offset_share(&state.tables.of), 256);
    let rle_short = [1, modes(PREDEFINED, RLE, PREDEFINED), 22];
    state.read_header(&rle_short).unwrap();
    assert_eq!(long_offset_share(&state.tables.of), 0);
}
