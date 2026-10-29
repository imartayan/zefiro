//! Frames of literal-heavy data (small alphabets, few matches) and hand-made
//! literals sections, compared with libzstd including error codes,
//! truncations, mutations and output capacities.

use std::cell::RefCell;
use std::collections::HashMap;

use compare::{Rng, *};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestRunner;
use zefiro::Decoder;

unsafe extern "C" {
    fn HUF_selectDecoder(dst_size: usize, src_size: usize) -> u32;
}

/// Number of blocks of each literal kind, by (kind, streams, double symbol table).
#[derive(Debug, Default)]
struct Seen {
    raw: usize,
    rle: usize,
    one_stream: usize,
    four_streams: [usize; 2],
    treeless: [usize; 2],
}

struct Checker {
    oracle: Libzstd,
    decoder: Decoder,
}

fn tally(frame: &[u8], seen: &mut Seen) {
    let mut double = false;
    for (kind, range) in frame_blocks(frame) {
        if kind != COMPRESSED_BLOCK {
            continue;
        }
        let section = literals_section(&frame[range]).unwrap();
        // SAFETY: pure function.
        let selects_double =
            |s: &LiteralsSection| unsafe { HUF_selectDecoder(s.regenerated, s.compressed) } == 1;
        match (section.kind, section.size_format) {
            (0, _) => seen.raw += 1,
            (1, _) => seen.rle += 1,
            (2, 0) => {
                seen.one_stream += 1;
                double = false;
            }
            (2, _) => {
                double = selects_double(&section);
                seen.four_streams[usize::from(double)] += 1;
            }
            _ => seen.treeless[usize::from(double)] += 1,
        }
    }
}

impl Checker {
    fn new() -> Self {
        Self {
            oracle: Libzstd::new(),
            decoder: Decoder::new(),
        }
    }

    fn check(&mut self, input: &[u8], capacity: usize, what: &str) {
        let theirs = self.oracle.decompress(input, capacity);
        let ours = decompress_ours(&mut self.decoder, input, capacity);
        assert_agree(
            &ours,
            &theirs,
            &format!("{what} (len {}, capacity {capacity})", input.len()),
        );
    }

    fn check_capacities(&mut self, input: &[u8], expected_len: usize, random: usize, what: &str) {
        let random = random % (expected_len + 10);
        let mut capacities = vec![0, expected_len, expected_len + 1000, random];
        if expected_len > 0 {
            capacities.push(expected_len - 1);
        }
        for capacity in capacities {
            self.check(input, capacity, what);
        }
    }
}

const SIZES: [usize; 16] = [
    1, 5, 20, 64, 100, 200, 255, 256, 300, 600, 1500, 3000, 4096, 6000, 9000, 20_000,
];
/// Alphabets of 50 or more symbols are for large inputs: no repeats, so no sequences.
const SMALL_ALPHABETS: [usize; 6] = [1, 2, 5, 16, 16, 40];
const LARGE_ALPHABETS: [usize; 5] = [50, 100, 100, 150, 200];

fn run_cases<S: Strategy>(cases: u32, strategy: S, test: impl Fn(S::Value)) {
    TestRunner::new(ProptestConfig::with_cases(cases))
        .run(&strategy, |value| {
            test(value);
            Ok(())
        })
        .unwrap();
}

/// Symbols from a small alphabet, flat or geometric, so that trees of every shape appear.
fn literal_data(seed: u64, len: usize, alphabet: usize, geometric: bool, offset: u8) -> Vec<u8> {
    let mut rng = Rng::with_seed(seed);
    (0..len)
        .map(|_| {
            let symbol = if geometric {
                (rng.usize(..alphabet))
                    .min(rng.usize(..alphabet))
                    .min(rng.usize(..alphabet))
            } else {
                rng.usize(..alphabet)
            };
            (symbol as u8).wrapping_add(offset)
        })
        .collect()
}

/// Literal-heavy data (bulk symbols from a seed) and its frame.
fn literal_frame(
    len: impl Strategy<Value = usize>,
    alphabets: &'static [usize],
) -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    let shape = (
        len,
        prop::sample::select(alphabets),
        any::<bool>(),
        any::<u8>(),
        any::<u64>(),
    );
    let params = (
        any::<[bool; 3]>(),
        prop::sample::select(&[0, 0, 1340, 2000][..]),
        prop::sample::select(&[1, 3, 9][..]),
    );
    (shape, params).prop_map(
        |(
            (len, alphabet, geometric, offset, seed),
            ([checksum, content_size, auto], target, level),
        )| {
            let data = literal_data(seed, len, alphabet, geometric, offset);
            let literal_mode = if auto {
                LiteralMode::Auto
            } else {
                LiteralMode::Huffman
            };
            let params = Params {
                checksum,
                content_size,
                min_match: 7,
                literal_mode,
                target_cblock_size: target,
                ..Params::level(level)
            };
            let frame = compress(&data, &params);
            (data, frame)
        },
    )
}

fn size_with_jitter(sizes: &'static [usize]) -> impl Strategy<Value = usize> {
    (prop::sample::select(sizes), 0usize..20).prop_map(|(size, jitter)| size + jitter)
}

const fn edit_byte(byte: &mut u8, operation: u8, operand: u8) {
    *byte = match operation {
        0 => *byte ^ 1 << (operand % 8),
        1 => byte.wrapping_add(1),
        _ => operand,
    };
}

#[test]
fn libzstd_literal_frames_decode() {
    let state = RefCell::new((Checker::new(), Seen::default()));
    // Small frames also check the output against the data, and concatenated frames.
    let body = |((data, frame), random): ((Vec<u8>, Vec<u8>), usize)| {
        let (checker, seen) = &mut *state.borrow_mut();
        tally(&frame, seen);
        checker.check_capacities(&frame, data.len(), random, "frame");
        if data.len() < 100_000 {
            let Outcome::Ok { output, .. } = checker.oracle.decompress(&frame, data.len()) else {
                panic!("libzstd rejects its own frame");
            };
            assert!(output == data, "libzstd output differs from the data");
            let doubled = [frame.clone(), skippable_frame(&[1, 2, 3], 5), frame].concat();
            checker.check_capacities(&doubled, 2 * data.len(), random, "doubled");
        }
    };
    let small = literal_frame(size_with_jitter(&SIZES), &SMALL_ALPHABETS);
    run_cases(1500, (small, any::<usize>()), body);
    // Several blocks: the table of one is repeated by the next.
    let large = literal_frame(130_000usize..330_000, &LARGE_ALPHABETS);
    run_cases(40, (large, any::<usize>()), body);
    let seen = &state.borrow().1;
    // libzstd only writes raw and RLE literals next to sequences: see the synthetic frames.
    assert!(seen.one_stream > 100, "{seen:?}");
    assert!(seen.four_streams.iter().all(|&n| n > 100), "{seen:?}");
    assert!(seen.treeless.iter().all(|&n| n > 0), "{seen:?}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn every_prefix_of_literal_frames_agrees(
        (data, frame) in literal_frame(prop::sample::select(&[100, 250, 700, 1600, 3200][..]), &SMALL_ALPHABETS),
    ) {
        let mut checker = Checker::new();
        let input = [skippable_frame(b"skip", 3), frame.clone(), frame].concat();
        for end in 0..=input.len() {
            checker.check(&input[..end], 2 * data.len() + 10, &format!("prefix {end}"));
            checker.check(&input[..end], data.len(), &format!("prefix {end}"));
        }
    }
}

#[test]
fn mutated_literal_frames_agree() {
    let state = RefCell::new((Checker::new(), HashMap::new()));
    // Class 4 edits the literals header of a compressed block.
    let edit = (0u8..5, any::<Index>(), 0u8..3, any::<u8>());
    let case = (
        literal_frame(size_with_jitter(&SIZES[..10]), &SMALL_ALPHABETS),
        prop::collection::vec(edit, 1..4),
        any::<usize>(),
    );
    run_cases(12_000, case, |((data, mut input), edits, random)| {
        let (checker, errors) = &mut *state.borrow_mut();
        let compressed: Vec<_> = (frame_blocks(&input).into_iter())
            .filter_map(|(kind, range)| (kind == COMPRESSED_BLOCK).then_some(range))
            .collect();
        for (class, at, operation, operand) in edits {
            // Headers and trees first, jump tables and stream ends too.
            let position = match class {
                0 => at.index(input.len()),
                1 => at.index(input.len().min(48)),
                4 => match compressed.get(at.index(compressed.len().max(1))) {
                    Some(range) => range.start + usize::from(operand) % 5.min(range.len()),
                    None => continue,
                },
                _ => at.index(input.len().min(140)),
            };
            edit_byte(&mut input[position], operation, operand);
        }
        if let Outcome::Err(code) = checker.oracle.decompress(&input, data.len() + 100) {
            *errors.entry(format!("{code:?}")).or_insert(0) += 1;
        }
        checker.check_capacities(&input, data.len(), random, "mutated");
    });
    let errors = &state.borrow().1;
    for code in ["CorruptionDetected", "DstSizeTooSmall", "SrcSizeWrong"] {
        assert!(
            errors.get(code).is_some_and(|&n| n > 20),
            "{code}: {errors:?}"
        );
    }
}

fn block_header(last: bool, size: usize) -> [u8; 3] {
    let header = u32::from(last) | 2 << 1 | (size as u32) << 3;
    [header as u8, (header >> 8) as u8, (header >> 16) as u8]
}

fn raw_header(kind: u8, len: usize) -> Vec<u8> {
    match len {
        0..=31 => vec![(len << 3) as u8 | kind],
        32..=4095 => ((len << 4) as u16 | 0x4 | u16::from(kind))
            .to_le_bytes()
            .to_vec(),
        _ => ((len << 4) as u32 | 0xC | u32::from(kind)).to_le_bytes()[..3].to_vec(),
    }
}

/// A compressed literals header with arbitrary sizes in front of arbitrary bytes.
fn random_huffman_section(rng: &mut Rng) -> Vec<u8> {
    let size_format = rng.u64(..) % 4;
    let kind = 2 + u64::from(rng.u64(..).is_multiple_of(4));
    let len = [0, 1, 5, 6, 7, 100, 1000, 20_000, 131_000][rng.usize(..9)];
    let payload: Vec<u8> = (0..rng.usize(..60)).map(|_| rng.u8(..)).collect();
    let (header_size, size_bits) = [(3, 10), (3, 10), (4, 14), (5, 18)][size_format as usize];
    let value =
        kind | size_format << 2 | (len as u64) << 4 | (payload.len() as u64) << (4 + size_bits);
    [&value.to_le_bytes()[..header_size], &payload[..]].concat()
}

/// Blocks of raw, RLE and garbage Huffman literals with no or some sequences,
/// in a frame with a block limit of 128 KiB or 1 KiB.
fn synthetic_frame(rng: &mut Rng) -> (Vec<u8>, usize) {
    let (mut content, mut blocks) = (Vec::new(), Vec::new());
    let count = 1 + rng.u64(..) % 3;
    for index in 0..count {
        let len = [0, 1, 5, 31, 32, 100, 1000, 4095, 4096, 70_000][rng.usize(..10)];
        let section = match rng.u64(..) % 5 {
            0 | 1 => {
                let data: Vec<u8> = (0..len).map(|_| rng.u8(..)).collect();
                content.extend_from_slice(&data);
                [raw_header(0, len), data].concat()
            }
            2 | 3 => {
                let byte = rng.u8(..);
                content.resize(content.len() + len, byte);
                [raw_header(1, len), vec![byte]].concat()
            }
            _ => random_huffman_section(rng),
        };
        let sequences: &[u8] = match rng.u64(..) % 12 {
            0 => &[],
            1 => &[0, 0],
            2 => &[1, 2, 3],
            _ => &[0],
        };
        let payload = [section, sequences.to_vec()].concat();
        blocks.push(
            [
                &block_header(index + 1 == count, payload.len())[..],
                &payload,
            ]
            .concat(),
        );
    }
    let checksum = rng.u64(..).is_multiple_of(2);
    let window_byte = if rng.u64(..).is_multiple_of(3) {
        0x00
    } else {
        0x38
    };
    let mut frame = [
        &MAGIC[..],
        &[u8::from(checksum) << 2, window_byte],
        &blocks.concat(),
    ]
    .concat();
    if checksum {
        let digest = compress(
            &content,
            &Params {
                checksum: true,
                ..Params::level(1)
            },
        );
        frame.extend_from_slice(&digest[digest.len() - 4..]);
    }
    (frame, content.len())
}

#[test]
fn synthetic_literal_frames_agree() {
    let mut checker = Checker::new();
    let mut rng = Rng::with_seed(0x5EED_5EED);
    let (mut accepted, mut errors) = (0, HashMap::new());
    for round in 0..6000 {
        let (frame, len) = synthetic_frame(&mut rng);
        let mut input = frame;
        if round % 2 == 1 {
            let at = rng.usize(..input.len().min(40));
            input[at] ^= 1 << (rng.u64(..) % 8);
        }
        match checker.oracle.decompress(&input, len + 10) {
            Outcome::Ok { .. } => accepted += 1,
            Outcome::Err(code) => *errors.entry(format!("{code:?}")).or_insert(0) += 1,
        }
        checker.check_capacities(&input, len, rng.usize(..), &format!("round {round}"));
    }
    assert!(accepted > 1000, "{accepted} accepted, {errors:?}");
    for code in [
        "CorruptionDetected",
        "LiteralsHeaderWrong",
        "DictionaryCorrupted",
        "SrcSizeWrong",
        "DstSizeTooSmall",
    ] {
        assert!(
            errors.get(code).is_some_and(|&n| n > 10),
            "{code}: {errors:?}"
        );
    }
}
