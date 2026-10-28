//! `decompress_stream` against `ZSTD_decompressStream`: same output, verdict and calls
//! (bytes consumed and written, frame done) for any input and output chunking, intact,
//! truncated and corrupted, with and without ring wraps and window limits.

use std::io::{BufReader, ErrorKind, Read};

use compare::*;
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestRunner;
use rust_zstd_decoder::{Error, Reader, decompress};

/// Cases of the randomized tests: many in release builds.
const CASES: u32 = if cfg!(debug_assertions) { 3000 } else { 40_000 };

struct Base {
    name: String,
    input: Vec<u8>,
}

fn bases() -> Vec<Base> {
    let mut bases = Vec::new();
    let mut push = |name: String, input: Vec<u8>| bases.push(Base { name, input });
    let parameter_sets = [
        ("level 3", Params::level(3)),
        (
            "checksum, no size",
            Params {
                checksum: true,
                content_size: false,
                ..Params::level(1)
            },
        ),
        (
            "window 10",
            Params {
                window_log: 10,
                content_size: false,
                ..Params::level(9)
            },
        ),
        (
            "window 12, size, checksum",
            Params {
                window_log: 12,
                checksum: true,
                ..Params::level(19)
            },
        ),
        (
            "window 11, small blocks",
            Params {
                window_log: 11,
                target_cblock_size: 1500,
                content_size: false,
                ..Params::level(5)
            },
        ),
    ];
    for sample in test_corpus() {
        for (name, params) in &parameter_sets {
            push(
                format!("{} {name}", sample.name),
                compress(&sample.data, params),
            );
        }
    }
    let mixed = test_corpus()
        .into_iter()
        .find(|s| s.name == "mixed-262144")
        .unwrap()
        .data;
    let steps: Vec<(usize, bool)> = (0..60).map(|i| (100 + i * 9, i % 4 != 0)).collect();
    push(
        "flushed blocks".into(),
        compress_stream(&mixed[..20_000], &Params::level(3), &steps),
    );
    push(
        "flushed blocks, window 10".into(),
        compress_stream(
            &mixed[..40_000],
            &Params {
                window_log: 10,
                ..Params::level(3)
            },
            &steps,
        ),
    );
    // Several frames with skippable frames in between and around.
    let mut multi = skippable_frame(b"", 0);
    for (i, sample) in test_corpus().iter().enumerate().filter(|(i, _)| i % 3 == 0) {
        let params = Params {
            checksum: i % 2 == 0,
            content_size: i % 4 != 0,
            ..Params::level(1 + i as i32 % 5)
        };
        multi.extend(compress(
            &sample.data[..sample.data.len().min(9000)],
            &params,
        ));
        multi.extend(skippable_frame(
            &sample.data[..sample.data.len() / 4],
            i as u8 % 16,
        ));
    }
    push("multi-frame".into(), multi);
    bases
}

/// Input and output chunk sizes for an input of `len` bytes: single bytes only for small
/// ones.
fn plans(len: usize) -> Vec<(Vec<usize>, Vec<usize>)> {
    let small = len <= 3000;
    let mut plans = vec![
        (vec![1 << 20], vec![1 << 20]),
        (vec![4096], vec![100]),
        (vec![64, 1, 7], vec![5000, 1 << 17, 3]),
        (vec![7], vec![1 << 18]),
        (vec![1 << 18], vec![61]),
    ];
    if small {
        plans.extend([
            (vec![1], vec![1]),
            (vec![1], vec![1000]),
            (vec![3, 1, 2], vec![1, 2, 3, 1 << 17]),
            (vec![100], vec![1]),
        ]);
    }
    plans
}

#[test]
fn valid_streams_agree_for_fixed_chunkings() {
    for base in bases() {
        for (in_sizes, out_sizes) in plans(base.input.len()) {
            let what = format!("{} (in {in_sizes:?}, out {out_sizes:?})", base.name);
            let run = compare_stream(&base.input, &in_sizes, &out_sizes, 0, &what);
            assert!(
                matches!(run.outcome, Outcome::Ok { .. }),
                "{what}: {:?}",
                run.outcome
            );
        }
    }
}

#[test]
fn output_is_the_original_data() {
    for sample in test_corpus() {
        for window_log in [0, 10, 14] {
            for content_size in [true, false] {
                let params = Params {
                    window_log,
                    content_size,
                    checksum: true,
                    ..Params::level(3)
                };
                let frame = compress(&sample.data, &params);
                for (in_sizes, out_sizes) in plans(frame.len()).into_iter().take(3) {
                    let run = compare_stream(&frame, &in_sizes, &out_sizes, 0, &sample.name);
                    let Outcome::Ok { output, .. } = run.outcome else {
                        panic!("{}: {:?}", sample.name, run.outcome);
                    };
                    assert!(output == sample.data, "{}: wrong output", sample.name);
                }
            }
        }
    }
}

/// The files of `corpus/` (when present) at several levels and windows, with the chunkings
/// of a file copy (128 KiB) and odd ones.
#[test]
fn corpus_files_agree() {
    // A prefix in debug builds; the windows of 2^17 and 2^18 make the ring wrap anyway.
    let prefix = if cfg!(debug_assertions) {
        2 << 20
    } else {
        usize::MAX
    };
    for mut sample in corpus().into_iter().filter(|s| s.data.len() > 300_000) {
        sample.data.truncate(prefix);
        for (level, window_log, content_size) in
            [(1, 0, true), (3, 18, false), (9, 17, false), (19, 0, true)]
        {
            let params = Params {
                window_log,
                content_size,
                checksum: level == 9,
                ..Params::level(level)
            };
            let frame = compress(&sample.data, &params);
            for (in_sizes, out_sizes) in [
                (vec![1 << 17], vec![1 << 17]),
                (vec![1 << 20], vec![1 << 20]),
                (vec![30_011, 77], vec![65_537, 1 << 17, 5000]),
            ] {
                let what = format!("{} level {level} window {window_log}", sample.name);
                let run = compare_stream(&frame, &in_sizes, &out_sizes, 0, &what);
                let Outcome::Ok { output, .. } = run.outcome else {
                    panic!("{what}: {:?}", run.outcome);
                };
                assert!(output == sample.data, "{what}: wrong output");
            }
        }
    }
}

#[test]
fn every_prefix_of_small_frames() {
    for sample in test_corpus()
        .iter()
        .filter(|s| (1..=4096).contains(&s.data.len()))
    {
        for params in [
            Params::level(3),
            Params {
                checksum: true,
                content_size: false,
                ..Params::level(1)
            },
        ] {
            let frame = [compress(&sample.data, &params), skippable_frame(b"xyz", 3)].concat();
            let step = (frame.len() / 150).max(1);
            for end in (0..=frame.len()).filter(|end| end % step == 0 || *end + 20 > frame.len()) {
                for (in_sizes, out_sizes) in [(vec![1 << 20], vec![1 << 20]), (vec![3], vec![2])] {
                    let what = format!("{} prefix {end}", sample.name);
                    compare_stream(&frame[..end], &in_sizes, &out_sizes, 0, &what);
                }
            }
        }
    }
}

#[test]
fn window_limits_agree() {
    let data = test_corpus()
        .into_iter()
        .find(|s| s.name == "text-262144")
        .unwrap()
        .data;
    for window_log in [10, 13, 17, 18] {
        for content_size in [false, true] {
            let params = Params {
                window_log,
                content_size,
                ..Params::level(3)
            };
            let frame = compress(&data, &params);
            for limit in [0, 10, 12, 13, 16, 17, 18, 20, 27, 31] {
                // The single-pass shortcut (output room for a frame of known size) does
                // not check the window.
                for out_len in [1 << 20, 1000] {
                    let what = format!("window {window_log} size {content_size} limit {limit}");
                    let run = compare_stream(&frame, &[1 << 20], &[out_len], limit, &what);
                    let shortcut = content_size && out_len > data.len();
                    let expect_error = limit != 0 && (limit as i32) < window_log && !shortcut;
                    assert_eq!(
                        matches!(run.outcome, Outcome::Err(Code::WindowTooLarge)),
                        expect_error,
                        "{what}: {:?}",
                        run.outcome
                    );
                }
            }
        }
    }
    // Default limit, 128 MiB: a header asking for more is refused before any data.
    for window_descriptor in [0x90, 0x98, 0xA0, 0xA8, 0xC8] {
        let input = frame(
            window_descriptor,
            false,
            &[block(true, RAW_BLOCK, 3, b"abc")],
            &[],
        );
        for limit in [0, 27, 28, 29, 30] {
            let what = format!("descriptor {window_descriptor:#x} limit {limit}");
            compare_stream(&input, &[1 << 20], &[1 << 20], limit, &what);
            compare_stream(&input, &[1], &[1], limit, &what);
        }
    }
}

#[test]
fn crafted_frames_agree() {
    let raw = |last, bytes: &[u8]| block(last, RAW_BLOCK, bytes.len(), bytes);
    let rle = |last, byte: u8, size| block(last, RLE_BLOCK, size, &[byte]);
    let checksum_of = |data: &[u8]| {
        let frame = compress(
            data,
            &Params {
                checksum: true,
                ..Params::level(1)
            },
        );
        frame[frame.len() - 4..].to_vec()
    };
    let big = vec![0x5A; 3000];
    let inputs = [
        // Empty last blocks, with and without checksum: no content size check.
        frame(WINDOW_1K, false, &[raw(true, b"")], &[]),
        frame(WINDOW_1K, true, &[raw(true, b"")], &checksum_of(b"")),
        frame(
            WINDOW_1K,
            true,
            &[raw(false, b"ab"), raw(false, b""), raw(true, b"")],
            &checksum_of(b"ab"),
        ),
        frame(
            WINDOW_1K,
            true,
            &[raw(false, b"ab"), rle(true, b'c', 3)],
            &checksum_of(b"abccc"),
        ),
        frame(WINDOW_1K, true, &[raw(true, b"abc")], &[1, 2, 3, 4]),
        frame(WINDOW_1K, true, &[raw(true, b"abc")], &[1, 2]),
        // Blocks over the block size limit (1 KiB here): raw and RLE.
        frame(WINDOW_1K, false, &[raw(true, &big)], &[]),
        frame(WINDOW_1K, false, &[rle(true, b'x', 3000)], &[]),
        frame(
            WINDOW_1K,
            false,
            &[rle(false, b'x', 1024), rle(true, b'y', 1025)],
            &[],
        ),
        frame(
            WINDOW_128K,
            false,
            &[rle(false, b'x', 100_000), rle(true, b'y', 200_000)],
            &[],
        ),
        frame(
            WINDOW_128K,
            false,
            &[raw(false, &big), raw(true, &big)],
            &[],
        ),
        // Reserved block type, then garbage after a frame.
        [
            &frame(WINDOW_1K, false, &[raw(true, b"abc")], &[])[..],
            &[1, 2, 3],
        ]
        .concat(),
        [
            &frame(WINDOW_1K, false, &[raw(true, b"abc")], &[])[..],
            &[0; 12],
        ]
        .concat(),
        [
            &frame(WINDOW_1K, false, &[raw(true, b"abc")], &[])[..],
            &MAGIC,
        ]
        .concat(),
        [&MAGIC[..], &[0, 0, 7, 0, 0, 0]].concat(),
        // Skippable frames: empty, short, followed by a frame, truncated.
        skippable_frame(b"", 0),
        [
            skippable_frame(b"hello", 15),
            frame(WINDOW_1K, false, &[raw(true, b"abc")], &[]),
        ]
        .concat(),
        skippable_frame(&[7; 1000], 5)[..500].to_vec(),
        vec![0x50, 0x2A, 0x4D, 0x18, 0xFF, 0xFF, 0xFF, 0xFF, 1, 2, 3],
        vec![0x50, 0x2A, 0x4D, 0x18, 0xF8, 0xFF, 0xFF, 0xFF, 1, 2, 3],
        // Bad magic numbers, a reserved header bit, empty input.
        vec![0x28, 0xB5, 0x2F],
        vec![0x28, 0xB5, 0x2F, 0xFC, 0, 0, 0, 0, 0, 0],
        vec![0x28, 0xB5, 0x2F, 0xFD, 0x08, 0, 0, 0, 0, 0],
        vec![],
        vec![0],
    ];
    for (index, input) in inputs.iter().enumerate() {
        for (in_sizes, out_sizes) in plans(100) {
            let what = format!("crafted {index}");
            compare_stream(input, &in_sizes, &out_sizes, 0, &what);
        }
    }
}

/// libzstd keeps its buffers from one frame to the next and shrinks them after 128 frames
/// that need far less, which changes the room a block has (`dstSize_tooSmall` or not).
#[test]
fn buffers_are_kept_then_shrunk_like_libzstd() {
    let big = frame(
        WINDOW_128K,
        false,
        &[
            block(false, RLE_BLOCK, 100_000, b"a"),
            block(true, RLE_BLOCK, 50, b"b"),
        ],
        &[],
    );
    // Single segment, content size 238, last RLE block of 66113 bytes.
    let small_lie = [
        &MAGIC[..],
        &[0x20, 238],
        &block(true, RLE_BLOCK, 66_113, b"x"),
    ]
    .concat();
    let mut verdicts = std::collections::BTreeSet::new();
    for skippable_frames in [0, 1, 100, 126, 127, 128, 129, 130, 200] {
        let mut input = big.clone();
        for _ in 0..skippable_frames {
            input.extend(skippable_frame(b"", 1));
        }
        input.extend_from_slice(&small_lie);
        let what = format!("{skippable_frames} skippable frames");
        let run = compare_stream(&input, &[1 << 20], &[100], 0, &what);
        verdicts.insert(format!("{:?}", run.outcome));
        compare_stream(&input, &[1000], &[1 << 20], 0, &what);
    }
    assert_eq!(verdicts.len(), 2, "{verdicts:?}");
}

/// Single-segment frames with a content size, which is wrong in some of them.
#[test]
fn content_sizes_and_the_single_pass_shortcut() {
    let payload = b"abcdefghij".repeat(5);
    for declared in [0u8, 49, 50, 51, 255] {
        for checksum in [false, true] {
            let descriptor = 0x20 | u8::from(checksum) << 2;
            let tail = if checksum { vec![1, 2, 3, 4] } else { vec![] };
            let blocks = [
                block(false, RAW_BLOCK, 20, &payload[..20]),
                block(true, RAW_BLOCK, 30, &payload[20..]),
            ]
            .concat();
            let input = [&MAGIC[..], &[descriptor, declared], &blocks, &tail].concat();
            for (in_sizes, out_sizes) in plans(100) {
                // Output sizes around the content size: with and without the shortcut.
                for out_len in [1, 49, 50, 51, 1000] {
                    let what = format!("declared {declared} checksum {checksum} out {out_len}");
                    let sizes = out_sizes.iter().map(|_| out_len).collect::<Vec<_>>();
                    compare_stream(&input, &in_sizes, &sizes, 0, &what);
                }
            }
        }
    }
}

fn chunk_sizes() -> impl Strategy<Value = Vec<usize>> {
    prop::collection::vec(prop_oneof![1usize..=8, 1usize..=300, 1usize..=70_000], 1..4)
}

/// A `Reader` over `input` gives libzstd's output or error.
fn check_reader(input: &[u8], buffer_capacity: usize, theirs: &Outcome, what: &str) {
    let mut reader = Reader::new(BufReader::with_capacity(buffer_capacity, input));
    let mut output = Vec::new();
    match (reader.read_to_end(&mut output), theirs) {
        (
            Ok(_),
            Outcome::Ok {
                output: expected, ..
            },
        ) => {
            assert!(output == *expected, "{what}: reader output differs");
        }
        (Err(error), Outcome::Err(Code::Truncated)) => {
            assert_eq!(error.kind(), ErrorKind::UnexpectedEof, "{what}");
        }
        (Err(error), Outcome::Err(code)) => {
            assert_eq!(error.kind(), ErrorKind::InvalidData, "{what}");
            let source = error.get_ref().and_then(|e| e.downcast_ref::<Error>());
            assert_eq!(Code::from(*source.unwrap()), *code, "{what}");
        }
        (result, theirs) => panic!("{what}: reader {result:?}, libzstd {theirs:?}"),
    }
}

fn small_bases() -> Vec<Base> {
    bases()
        .into_iter()
        .filter(|b| b.input.len() < 12_000)
        .collect()
}

fn run_random(corrupt: bool, truncate: bool) {
    let bases = small_bases();
    let strategy = (
        any::<Index>(),
        chunk_sizes(),
        chunk_sizes(),
        prop::collection::vec((any::<Index>(), any::<u8>()), 0..4),
        any::<Index>(),
        0u32..4,
    );
    TestRunner::new(ProptestConfig::with_cases(CASES))
        .run(
            &strategy,
            |(base, in_sizes, out_sizes, mutations, cut, limit)| {
                let base = &bases[base.index(bases.len())];
                let mut input = base.input.clone();
                if corrupt && !input.is_empty() {
                    for (at, byte) in &mutations {
                        let at = at.index(input.len());
                        input[at] = *byte;
                    }
                }
                if truncate && !input.is_empty() {
                    input.truncate(cut.index(input.len() + 1));
                }
                let limit = [0, 0, 10, 12][limit as usize];
                compare_stream(&input, &in_sizes, &out_sizes, limit, &base.name);
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn reader_and_decompress_on_valid_streams() {
    for base in bases() {
        let theirs = stream_theirs(&base.input, &[1 << 20], &[1 << 20], 0).outcome;
        check_reader(&base.input, 1 << 12, &theirs, &base.name);
        check_reader(&base.input, 1, &theirs, &base.name);
        let Outcome::Ok { output, .. } = theirs else {
            panic!("{}: {theirs:?}", base.name);
        };
        assert!(
            decompress(&base.input, output.len()).unwrap() == output,
            "{}",
            base.name
        );
        if !output.is_empty() {
            let error = decompress(&base.input, output.len() - 1).unwrap_err();
            assert_eq!(error, Error::DstSizeTooSmall, "{}", base.name);
        }
    }
}

#[test]
fn random_chunkings_of_valid_streams() {
    run_random(false, false);
}

#[test]
fn random_chunkings_of_truncated_streams() {
    run_random(false, true);
}

#[test]
fn random_chunkings_of_corrupted_streams() {
    run_random(true, false);
}

#[test]
fn random_chunkings_of_corrupted_and_truncated_streams() {
    run_random(true, true);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    /// Random bytes, alone or after the magic number so that they reach frame parsing.
    #[test]
    fn random_bytes_match_libzstd(
        bytes in prop::collection::vec(any::<u8>(), 0..2000),
        with_magic in any::<bool>(),
        in_sizes in chunk_sizes(),
        out_sizes in chunk_sizes(),
    ) {
        let input = if with_magic { [&MAGIC[..], &bytes].concat() } else { bytes };
        compare_stream(&input, &in_sizes, &out_sizes, 0, "random bytes");
    }
}
