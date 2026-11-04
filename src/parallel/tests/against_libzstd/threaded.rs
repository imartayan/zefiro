//! `decompress_stream` and `Reader` with threads against the serial stream, on libzstd's
//! frames: the same bytes, then the same error, for any chunking of the input and the output.

use alloc::vec;
use std::io::{self, BufRead, Read};

use super::stream::Piece::{Fixed, Whole};
use super::stream::{
    SHAPES, assert_decodes, assert_same_as_serial, assert_same_as_serial_loosely, decoder,
    drive_decoder, drive_reader, drive_stream, reference, shape, source,
};
use super::*;
use crate::Reader;

#[test]
fn libzstd_frames_stream_the_same_threaded_or_not() {
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
                        assert_decodes(None, &frame, &data, 1, &what);
                    }
                }
            }
        }
    }
    assert_eq!(count, 96);
}

#[test]
fn libzstd_frames_with_a_dictionary_stream_the_same_threaded_or_not() {
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
                assert_decodes(Some(&dictionary), &frame, &data, 1, &what);
                // The cold dictionary and the repeat offsets are the second frame's too.
                let twice = [&frame[..], &frame].concat();
                assert_decodes(Some(&dictionary), &twice, &data.repeat(2), 2, &what);
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
        let what = format!("{what}, case {case}");
        let run = assert_same_as_serial(dictionary, input, case, 0, &what);
        ok += usize::from(run.outcome.error.is_none());
        fallbacks += run.fallbacks;
    }
    // Some flips still decode, most do not; blocks are redone serially after the first error.
    assert!(ok > 20 && fallbacks > 500, "{ok} ok, {fallbacks} fallbacks");
}

#[test]
fn corrupted_and_truncated_frames_stream_the_same_threaded_or_not() {
    sweep(None, &[], false, "no dictionary");
    sweep(None, &[], true, "checksum");
}

#[test]
fn corrupted_and_truncated_dictionary_frames_stream_the_same_threaded_or_not() {
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
    // `decompress_stream` (sizes known, input whole, output big enough) stays serial too.
    for (shape, frames) in [
        (shape(Fixed(1 << 16), Fixed(1 << 16)), 5),
        (shape(Whole, Fixed(1 << 21)), 1),
        (shape(Fixed(1000), Fixed(4096)), 5),
        (shape(Fixed(1), Fixed(1 << 16)), 5),
    ] {
        let run = drive_stream(decoder(None, 3, GATE), &input, shape);
        assert_same(&run.outcome.bytes, &expected, "frames in a row");
        assert_eq!(run.outcome.error, None, "{shape:?}");
        assert_eq!((run.fallbacks, run.handovers), (0, 0), "{shape:?}");
        assert_eq!(run.frames, frames, "{shape:?}");
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
        let what = format!("frames in a row, case {case}");
        assert_same_as_serial(None, damaged, case, GATE, &what);
    }
}

// Pieces without a content size, as `pzstd` writes them: a skippable frame, then a frame.
#[test]
fn frames_without_a_content_size_behind_skippable_frames_stream_threaded() {
    let skippable = [&[0x50, 0x2A, 0x4D, 0x18][..], &8u32.to_le_bytes(), &[0; 8]].concat();
    let mut input = Vec::new();
    let mut expected = Vec::new();
    for (seed, level) in [(1, 3), (2, 1), (3, 19)] {
        let data = mixed(seed, 70_000);
        input.extend(&skippable);
        input.extend(compress_with_size(&data, level, 0, seed != 2, false, &[]));
        expected.extend(data);
    }
    assert_decodes(None, &input, &expected, 3, "pzstd-like");
}

// Treeless sections after a table scaled in place, then a block cut or corrupted anywhere.
#[test]
fn crafted_frames_with_tables_to_repeat_stream_the_same_threaded_or_not() {
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
        assert_decodes(None, &frame, &data.repeat(4), 1, "crafted");
        for position in 0..frame.len() {
            for mask in [0x01, 0x10, 0x80] {
                let mut corrupted = frame.clone();
                corrupted[position] ^= mask;
                assert_same_as_serial(None, &corrupted, position + case, 0, "crafted, corrupted");
            }
        }
    }
}

// A reader or a decoder dropped before the end, with jobs in flight, stops its threads.
#[test]
fn a_reader_dropped_mid_frame_stops_its_threads() {
    let data = mixed(5, 300_000);
    let frame = compress(&data, 3, 10, true, &[]);
    for (threads, read) in [(2, 1), (4, 1000), (8, 70_000), (3, 0)] {
        let mut reader = Reader::with_decoder(decoder(None, threads, 0), &frame[..]);
        let mut buf = vec![0; read];
        let read = reader.read(&mut buf).unwrap();
        assert_same(&buf[..read], &data[..read], "dropped");
        drop(reader);
        let mut decoder = decoder(None, threads, 0);
        let progress = decoder
            .decompress_stream(&frame[..50_000], &mut buf)
            .unwrap();
        assert_same(
            &buf[..progress.written],
            &data[..progress.written],
            "dropped",
        );
        drop(decoder);
    }
}

// An input that fails after some bytes: the threaded reader returns what it read ahead, decoded,
// then the error.
#[test]
fn an_input_that_fails_is_reported_after_the_bytes_before_it() {
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
        let flaky = Flaky {
            input: &frame,
            position: 0,
            chunk,
            fail_at,
        };
        let mut reader = Reader::with_decoder(decoder(None, THREADS[index % 4], 0), flaky);
        let (mut bytes, mut buf) = (Vec::new(), [0; 4096]);
        let error = loop {
            match reader.read(&mut buf) {
                Ok(read) => bytes.extend_from_slice(&buf[..read]),
                Err(error) => break error,
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::Other);
        // The input before the failure, cut there.
        let expected = reference(None, &frame[..fail_at]).bytes;
        assert_same(&bytes, &expected, &format!("fail at {fail_at}"));
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
            let threads = THREADS[(at + index) % THREADS.len()];
            let mut decoder = decoder(dictionary.as_ref(), threads, 0);
            decoder.fallback_at = Some(at);
            let what = format!("frame {index}, serial from block {at}");
            let run = if at % 2 == 0 {
                drive_stream(decoder, frame, SHAPES[(at + index) % 4])
            } else {
                drive_reader(decoder, source(frame, 1000), 4096)
            };
            assert_eq!(run.outcome.error, None, "{what}");
            assert_same(&run.outcome.bytes, data, &what);
            taken_over += run.fallbacks;
        }
    }
    assert!(taken_over > 200, "{taken_over}");
}

// Output pieces of 4 MiB over a frame of 6 MiB, input pieces of 64 KiB: the ring wraps a few
// times and each call takes dozens of blocks.
#[test]
fn big_pieces_over_a_large_frame_agree() {
    let data = mixed(13, 6 << 20);
    for (level, checksum) in [(3, true), (1, false)] {
        let frame = compress(&data, level, 0, checksum, &[]);
        for threads in [3, 8] {
            let what = format!("6 MiB, level {level}, {threads} threads");
            let shape = shape(Fixed(64 << 10), Fixed(4 << 20));
            let run = drive_stream(decoder(None, threads, 0), &frame, shape);
            assert_eq!((run.frames, run.fallbacks, run.outcome.error), (1, 0, None));
            assert_same(&run.outcome.bytes, &data, &what);
            let run = drive_reader(decoder(None, threads, 0), source(&frame, 64 << 10), 4 << 20);
            assert_eq!((run.frames, run.fallbacks, run.outcome.error), (1, 0, None));
            assert_same(&run.outcome.bytes, &data, &what);
        }
    }
}

// Frames that stop being fed and start again: a stream abandoned by `reset` or by an error, the
// jobs of the old frame never reach the new one.
#[test]
fn a_reset_or_an_error_abandons_the_frame_in_the_pipeline() {
    let data = mixed(14, 400_000);
    let frame = compress(&data, 3, 10, true, &[]);
    let mut corrupted = frame.clone();
    corrupted[frame.len() / 2] ^= 0x20;
    for threads in [2, 4] {
        let mut decoder = decoder(None, threads, 0);
        let mut buf = vec![0; 1 << 16];
        for (round, shape) in SHAPES.iter().enumerate().take(6) {
            // Half of a frame, then a reset; or a corrupted frame to its error.
            let corrupt = round % 2 == 1;
            let input = if corrupt { &corrupted } else { &frame };
            let stop = if corrupt {
                input.len()
            } else {
                input.len() / 2
            };
            let (mut position, mut failed) = (0, false);
            while !failed && position < stop {
                let end = (position + 20_000).min(stop);
                let result = decoder.decompress_stream(&input[position..end], &mut buf);
                failed = result.is_err();
                position += result.map_or(0, |progress| progress.consumed);
            }
            // Calls with no input go on until the error comes (there is one: the checksum).
            while corrupt && !failed {
                match decoder.decompress_stream(&[], &mut buf) {
                    Ok(progress) => assert!(progress.written > 0, "round {round}: no error"),
                    Err(_) => failed = true,
                }
            }
            if !failed {
                decoder.reset();
            }
            // A whole frame after it.
            let run = drive_decoder(&mut decoder, &frame, *shape);
            assert_eq!(run.outcome.error, None, "round {round}");
            assert_same(&run.outcome.bytes, &data, "after reset");
        }
    }
}

// Raw and RLE blocks among compressed ones: those with no job in flight go from the input to
// the machine, in any piece, and cut or damaged anywhere.
#[test]
fn raw_and_rle_blocks_stream_the_same_threaded_or_not() {
    let mut rng = Rng::with_seed(17);
    let random = |rng: &mut Rng, len| (0..len).map(|_| rng.u8(..)).collect::<Vec<u8>>();
    let data = [
        random(&mut rng, 6000),
        vec![7; 5000],
        text(1, 3, 5000),
        random(&mut rng, 3000),
        vec![9; 3000],
        random(&mut rng, 2500),
    ]
    .concat();
    for (level, window_log, checksum) in [(3, 10, true), (1, 10, false), (3, 0, true)] {
        let frame = compress(&data, level, window_log, checksum, &[]);
        let what = format!("raw and rle, level {level}, windowLog {window_log}");
        assert_decodes(None, &frame, &data, 1, &what);
        for position in (0..frame.len()).step_by(3) {
            assert_same_as_serial_loosely(None, &frame[..position], position, 0, true, &what);
            let mut corrupted = frame.clone();
            corrupted[position] ^= 0x08;
            assert_same_as_serial_loosely(None, &corrupted, position, 0, true, &what);
        }
    }
}
