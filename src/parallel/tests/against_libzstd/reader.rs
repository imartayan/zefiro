//! A `Reader` with threads against the one without: the same bytes, in the same reads, then the
//! same error, for any chunking of the input and size of the reads.

use alloc::boxed::Box;
use std::io::{self, BufRead, BufReader, Read};

use super::*;
use crate::Reader;

// (read size, input chunk size, 0 for the whole input in one chunk).
const SHAPES: [(usize, usize); 4] = [(1 << 16, 0), (4096, 1000), (7, 97), (1, 5)];
// Reads of 4 MiB over chunks of 1 MiB, for the inputs that are not swept.
const BIG_READS: (usize, usize) = (4 << 20, 1 << 20);
// Smaller reads and chunks than these never hold a frame header: nothing goes through threads.
const SHAPE_WITHOUT_THREADS: (usize, usize) = SHAPES[3];

#[derive(Debug, PartialEq)]
struct Outcome {
    bytes: Vec<u8>,
    // Sizes of the reads that returned data.
    reads: Vec<usize>,
    // The error that ended the stream: its kind and the `Error` inside.
    error: Option<(io::ErrorKind, Option<Error>)>,
}

struct Run {
    outcome: Outcome,
    // Frames that went through the threads, and blocks the serial code took over.
    frames: usize,
    fallbacks: usize,
}

fn source(input: &[u8], chunk: usize) -> Box<dyn BufRead + '_> {
    if chunk == 0 {
        Box::new(input)
    } else {
        Box::new(BufReader::with_capacity(chunk, input))
    }
}

fn read_all(mut reader: impl Read, read_size: usize) -> Outcome {
    let mut outcome = Outcome {
        bytes: Vec::new(),
        reads: Vec::new(),
        error: None,
    };
    let mut buf = vec![0; read_size];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return outcome,
            Ok(read) => {
                outcome.bytes.extend_from_slice(&buf[..read]);
                outcome.reads.push(read);
            }
            Err(error) => {
                let inner = error.get_ref().and_then(|e| e.downcast_ref::<Error>());
                outcome.error = Some((error.kind(), inner.copied()));
                return outcome;
            }
        }
    }
}

// Frames under `gate` bytes of content stay serial; the serial code takes over the block
// `fallback_at` although it is fine.
fn run<R: BufRead>(
    input: R,
    dictionary: Option<&Dictionary>,
    threads: usize,
    read_size: usize,
    gate: usize,
    fallback_at: Option<usize>,
) -> Run {
    let mut decoder = Decoder::new();
    decoder.set_dictionary(dictionary);
    decoder.set_threads(threads);
    decoder.gate_bytes = gate;
    let mut reader = Reader::with_decoder(decoder, input);
    if let Some(streaming) = reader.streaming_mut() {
        streaming.pipeline.fallback_at = fallback_at;
    }
    let outcome = read_all(&mut reader, read_size);
    let (frames, fallbacks) = reader
        .streaming_mut()
        .map_or((0, 0), |s| (s.frames, s.pipeline.fallbacks));
    Run {
        outcome,
        frames,
        fallbacks,
    }
}

// The serial reader and the one with `threads` agree; returns the threaded run.
fn assert_threaded_is_serial(
    input: &[u8],
    dictionary: Option<&Dictionary>,
    threads: usize,
    (read_size, chunk): (usize, usize),
    gate: usize,
    what: &str,
) -> Run {
    let what = format!("{what}, {threads} threads, reads of {read_size}, chunks of {chunk}");
    let serial = run(source(input, chunk), dictionary, 1, read_size, gate, None);
    assert_eq!(serial.frames, 0, "{what}");
    let threaded = run(
        source(input, chunk),
        dictionary,
        threads,
        read_size,
        gate,
        None,
    );
    let what = format!(
        "{what} ({} frames, {} fallbacks)",
        threaded.frames, threaded.fallbacks
    );
    assert_eq!(threaded.outcome.error, serial.outcome.error, "{what}");
    assert_same(&threaded.outcome.bytes, &serial.outcome.bytes, &what);
    assert_eq!(threaded.outcome.reads, serial.outcome.reads, "{what}");
    threaded
}

// A valid input decodes to `expected` in every shape and thread count, all frames that can go
// through the threads do, with no block left to the serial code.
fn assert_reads(
    input: &[u8],
    dictionary: Option<&Dictionary>,
    expected: &[u8],
    frames: usize,
    what: &str,
) {
    // `decompress_stream` decodes a frame whole in the input in one pass when the reads are big.
    let frame_size = expected.len() / frames;
    for (index, shape) in SHAPES.iter().chain(&[BIG_READS]).enumerate() {
        let threads = THREADS[(index + frames) % THREADS.len()];
        // Reads of one byte: a few thousand bytes are enough.
        if shape.0 == 1 && expected.len() > 50_000 {
            continue;
        }
        let run = assert_threaded_is_serial(input, dictionary, threads, *shape, 0, what);
        assert_eq!(run.outcome.error, None, "{what}");
        assert_same(&run.outcome.bytes, expected, what);
        assert_eq!(run.fallbacks, 0, "{what}, {shape:?}");
        let one_pass = (shape.1 == 0 || input.len() <= shape.1) && frame_size <= shape.0;
        if *shape != SHAPE_WITHOUT_THREADS && !one_pass {
            assert!(run.frames >= 1 && run.frames <= frames, "{what}, {shape:?}");
        }
    }
}

#[test]
fn libzstd_frames_read_the_same_threaded_or_not() {
    let mut count = 0;
    for (name, data) in data_sets(100_000) {
        for level in [1, 3, 19] {
            for window_log in [0, 10] {
                for checksum in [false, true] {
                    for content_size in [false, true] {
                        let what = format!(
                            "{name} level {level} windowLog {window_log} checksum {checksum} size {content_size}"
                        );
                        let frame = compress_with_size(
                            &data,
                            level,
                            window_log,
                            checksum,
                            content_size,
                            &[],
                        );
                        count += 1;
                        assert_reads(&frame, None, &data, 1, &what);
                    }
                }
            }
        }
    }
    assert_eq!(count, 96);
}

#[test]
fn libzstd_frames_with_a_dictionary_read_the_same_threaded_or_not() {
    let raw = text(1, 9, 8000);
    let trained = |(offset, length): (usize, usize)| ARCHIVE[offset..offset + length].to_vec();
    for (dictionary_name, bytes) in [
        ("raw", raw),
        ("2k", trained(TRAINED_2K)),
        ("16k", trained(TRAINED_16K)),
    ] {
        let dictionary = Dictionary::new(&bytes).unwrap();
        for (name, data) in data_sets(40_000) {
            for (level, window_log) in [(3, 0), (19, 10)] {
                let what = format!("{dictionary_name} {name} level {level} windowLog {window_log}");
                let frame = compress(&data, level, window_log, true, &bytes);
                assert_reads(&frame, Some(&dictionary), &data, 1, &what);
                // The cold dictionary and the repeat offsets are the second frame's too.
                let twice = [&frame[..], &frame].concat();
                assert_reads(&twice, Some(&dictionary), &data.repeat(2), 2, &what);
            }
        }
    }
}

// A small multi-block frame: every byte flipped two ways, every truncation.
fn sweep(dictionary: Option<&Dictionary>, dictionary_bytes: &[u8], checksum: bool, what: &str) {
    let data = mixed(11, 12_000);
    let frame = compress(&data, 3, 10, checksum, dictionary_bytes);
    let mut cases = Vec::new();
    for position in 0..frame.len() {
        for mask in [0x01, 0x80] {
            let mut corrupted = frame.clone();
            corrupted[position] ^= mask;
            cases.push(corrupted);
        }
    }
    cases.extend((0..frame.len()).map(|length| frame[..length].to_vec()));
    let (mut ok, mut fallbacks) = (0, 0);
    for (case, input) in cases.iter().enumerate() {
        for shape in [SHAPES[1], SHAPES[case % SHAPES.len()]] {
            let threads = THREADS[(case + shape.0) % THREADS.len()];
            let what = format!("{what}, case {case}");
            let run = assert_threaded_is_serial(input, dictionary, threads, shape, 0, &what);
            ok += usize::from(run.outcome.error.is_none());
            fallbacks += run.fallbacks;
        }
    }
    // Some flips still decode, most do not; blocks are redone serially after the first error.
    assert!(ok > 20 && fallbacks > 500, "{ok} ok, {fallbacks} fallbacks");
}

#[test]
fn corrupted_and_truncated_frames_read_the_same_threaded_or_not() {
    sweep(None, &[], false, "no dictionary");
    sweep(None, &[], true, "checksum");
}

#[test]
fn corrupted_and_truncated_dictionary_frames_read_the_same_threaded_or_not() {
    let bytes = ARCHIVE[TRAINED_2K.0..TRAINED_2K.0 + TRAINED_2K.1].to_vec();
    sweep(
        Some(&Dictionary::new(&bytes).unwrap()),
        &bytes,
        true,
        "trained dictionary",
    );
    let raw = text(1, 9, 3000);
    sweep(
        Some(&Dictionary::new(&raw).unwrap()),
        &raw,
        false,
        "raw dictionary",
    );
}

// Frames of every kind in a row, as `frames_in_a_row`: gated or not by content size.
#[test]
fn frames_in_a_row_go_through_the_threads_or_not_by_size() {
    const GATE: usize = 2000;
    let (input, expected, _) = frames_in_a_row();
    // Five frames pass the gate, the empty frame and the small one do not. The one-pass case of
    // `decompress_stream` (sizes known, input whole, reads big enough) stays serial too.
    for (shape, frames) in [((1 << 16, 0), 5), ((1 << 21, 0), 1), ((4096, 1000), 0)] {
        let run = assert_threaded_is_serial(&input, None, 3, shape, GATE, "frames in a row");
        assert_same(&run.outcome.bytes, &expected, "frames in a row");
        assert_eq!(run.outcome.error, None, "{shape:?}");
        assert_eq!(run.fallbacks, 0);
        // A header can straddle two chunks, and then the frame is serial.
        if frames == 0 {
            assert!((3..=5).contains(&run.frames), "{shape:?}: {}", run.frames);
        } else {
            assert_eq!(run.frames, frames, "{shape:?}");
        }
    }
    // Damaged anywhere, or cut.
    let mut cases = Vec::new();
    for position in (0..input.len()).step_by(997) {
        let mut damaged = input.clone();
        damaged[position] ^= 0x40;
        cases.push(damaged);
    }
    cases.extend(
        (0..input.len())
            .step_by(1511)
            .map(|length| input[..length].to_vec()),
    );
    for (case, damaged) in cases.iter().enumerate() {
        for (index, shape) in SHAPES[..3].iter().enumerate() {
            let threads = THREADS[(case + index) % THREADS.len()];
            let what = format!("frames in a row, case {case}");
            assert_threaded_is_serial(damaged, None, threads, *shape, GATE, &what);
        }
    }
}

// Pieces without a content size, as `pzstd` writes them: a skippable frame, then a frame.
#[test]
fn frames_without_a_content_size_behind_skippable_frames_read_threaded() {
    let skippable = [&[0x50, 0x2A, 0x4D, 0x18][..], &8u32.to_le_bytes(), &[0; 8]].concat();
    let mut input = Vec::new();
    let mut expected = Vec::new();
    for (seed, level) in [(1, 3), (2, 1), (3, 19)] {
        let data = mixed(seed, 70_000);
        input.extend(&skippable);
        input.extend(compress_with_size(&data, level, 0, seed != 2, false, &[]));
        expected.extend(data);
    }
    assert_reads(&input, None, &expected, 3, "pzstd-like");
}

// Treeless sections after a table scaled in place, then a block cut or corrupted anywhere.
#[test]
fn crafted_frames_with_tables_to_repeat_read_the_same_threaded_or_not() {
    let mut rng = Rng::with_seed(9);
    for case in 0..6usize {
        let tree = TestTree::random(&mut rng, 1 + case as u32 % 9);
        let data = tree.random_data(&mut rng, 300);
        let one = tree.encode_stream(&data);
        let four = tree.encode_four_streams(&data);
        let section = |kind, size_format, payload: &[u8]| {
            [
                huffman_section(kind, size_format, data.len(), payload),
                vec![0],
            ]
            .concat()
        };
        let blocks = [
            section(2, 0, &[&tree.header[..], &one].concat()),
            section(3, 1, &four),
            section(3, 0, &one),
            section(3, 2, &four),
        ];
        let frame = crafted_frame(&blocks);
        assert_reads(&frame, None, &data.repeat(4), 1, "crafted");
        for position in 0..frame.len() {
            for mask in [0x01, 0x10, 0x80] {
                let mut corrupted = frame.clone();
                corrupted[position] ^= mask;
                let shape = SHAPES[(position + case) % 3];
                let threads = THREADS[position % THREADS.len()];
                assert_threaded_is_serial(
                    &corrupted,
                    None,
                    threads,
                    shape,
                    0,
                    "crafted, corrupted",
                );
            }
        }
    }
}

// A reader dropped before the end, with jobs in flight, stops its threads.
#[test]
fn a_reader_dropped_mid_frame_stops_its_threads() {
    let data = mixed(5, 300_000);
    let frame = compress(&data, 3, 10, true, &[]);
    for (threads, read) in [(2, 1), (4, 1000), (8, 70_000), (3, 0)] {
        let mut decoder = Decoder::new();
        decoder.set_threads(threads);
        let mut reader = Reader::with_decoder(decoder, &frame[..]);
        let mut buf = vec![0; read];
        let read = reader.read(&mut buf).unwrap();
        assert_same(&buf[..read], &data[..read], "dropped");
        drop(reader);
    }
}

// An input that fails after some bytes: the threaded reader reports the same error after the
// same bytes (what it read ahead is decoded first).
#[test]
fn an_input_that_fails_is_reported_after_the_same_bytes() {
    struct Flaky<'a> {
        input: &'a [u8],
        position: usize,
        chunk: usize,
        fail_at: usize,
    }
    impl Read for Flaky<'_> {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            unreachable!()
        }
    }
    impl BufRead for Flaky<'_> {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            if self.position >= self.fail_at {
                return Err(io::Error::other("flaky"));
            }
            let end = (self.position + self.chunk)
                .min(self.fail_at)
                .min(self.input.len());
            Ok(&self.input[self.position..end])
        }
        fn consume(&mut self, amount: usize) {
            self.position += amount;
        }
    }
    let data = mixed(6, 100_000);
    let frame = compress(&data, 3, 10, true, &[]);
    for (index, fail_at) in (1..frame.len()).step_by(frame.len() / 41).enumerate() {
        let chunk = [97, 1000, 5000][index % 3];
        let flaky = || Flaky {
            input: &frame,
            position: 0,
            chunk,
            fail_at,
        };
        let serial = run(flaky(), None, 1, 4096, 0, None).outcome;
        let threaded = run(flaky(), None, THREADS[index % 4], 4096, 0, None).outcome;
        assert_eq!(serial.error.map(|e| e.0), Some(io::ErrorKind::Other));
        assert_eq!(threaded.error.map(|e| e.0), Some(io::ErrorKind::Other));
        assert_same(
            &threaded.bytes,
            &serial.bytes,
            &format!("fail at {fail_at}"),
        );
    }
}

// The serial code can take over at any block, with the states of the blocks before it: tables
// to repeat, scaled tables, dictionary tables and offsets.
#[test]
fn the_serial_code_taking_over_at_any_block_gives_the_same_output() {
    let mut frames = Vec::new();
    let bytes = ARCHIVE[TRAINED_2K.0..TRAINED_2K.0 + TRAINED_2K.1].to_vec();
    let raw = text(1, 9, 3000);
    for (dictionary_bytes, checksum) in
        [(vec![], false), (vec![], true), (bytes, true), (raw, false)]
    {
        let dictionary =
            (!dictionary_bytes.is_empty()).then(|| Dictionary::new(&dictionary_bytes).unwrap());
        let data = mixed(11, 12_000);
        let frame = compress(&data, 3, 10, checksum, &dictionary_bytes);
        frames.push((frame, dictionary, data));
    }
    for (name, data) in data_sets(30_000) {
        let _ = name;
        for level in [1, 19] {
            frames.push((compress(&data, level, 10, true, &[]), None, data.clone()));
        }
    }
    let mut rng = Rng::with_seed(3);
    let tree = TestTree::random(&mut rng, 5);
    let data = tree.random_data(&mut rng, 300);
    let (one, four) = (tree.encode_stream(&data), tree.encode_four_streams(&data));
    let section = |kind, size_format, payload: &[u8]| {
        [
            huffman_section(kind, size_format, data.len(), payload),
            vec![0],
        ]
        .concat()
    };
    let blocks = [
        section(2, 0, &[&tree.header[..], &one].concat()),
        section(3, 1, &four),
        section(3, 0, &one),
        section(3, 2, &four),
    ];
    frames.push((crafted_frame(&blocks), None, data.repeat(4)));
    let mut taken_over = 0;
    for (index, (frame, dictionary, data)) in frames.iter().enumerate() {
        for at in 0..40 {
            let shape = [(4096, 0), SHAPES[1], SHAPES[2]][(at + index) % 3];
            let threads = THREADS[(at + index) % THREADS.len()];
            let run = run(
                source(frame, shape.1),
                dictionary.as_ref(),
                threads,
                shape.0,
                0,
                Some(at),
            );
            let what = format!("frame {index}, serial from block {at}");
            assert_eq!(run.outcome.error, None, "{what}");
            assert_same(&run.outcome.bytes, data, &what);
            taken_over += run.fallbacks;
        }
    }
    assert!(taken_over > 200, "{taken_over}");
}

// Reads of 4 MiB over a frame of 6 MiB, chunks as a `BufReader` of 64 KiB gives: the ring wraps
// a few times and each read takes dozens of blocks.
#[test]
fn big_reads_over_a_large_frame_agree() {
    let data = mixed(13, 6 << 20);
    for (level, checksum) in [(3, true), (1, false)] {
        let frame = compress(&data, level, 0, checksum, &[]);
        for (threads, chunk) in [(3, 64 << 10), (8, 0)] {
            let run =
                assert_threaded_is_serial(&frame, None, threads, (4 << 20, chunk), 0, "6 MiB");
            assert_eq!((run.frames, run.fallbacks, run.outcome.error), (1, 0, None));
            assert_same(&run.outcome.bytes, &data, "6 MiB");
        }
    }
}
