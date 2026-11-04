use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use super::*;
use crate::dict::Dictionary;
use crate::frame::scan_frame;
use crate::testing::{Rng, TestTree, assert_same, huffman_section};

mod stream;

const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

const THREADS: [usize; 4] = [2, 3, 4, 8];

// The output or the error, and the frames that went through the pipeline and that it redid.
struct Run {
    result: Result<Vec<u8>, Error>,
    pipelined: usize,
    redone: usize,
}

fn decode(dictionary: Option<&Dictionary>, input: &[u8], capacity: usize, threads: usize) -> Run {
    decode_gated(dictionary, input, capacity, threads, 0)
}

// Frames under `gate_bytes` compressed bytes stay serial.
fn decode_gated(
    dictionary: Option<&Dictionary>,
    input: &[u8],
    capacity: usize,
    threads: usize,
    gate_bytes: usize,
) -> Run {
    let mut decoder = Decoder::new();
    decoder.set_dictionary(dictionary);
    decoder.set_threads(threads);
    decoder.gate_bytes = gate_bytes;
    let mut output = vec![0xAA; capacity];
    let result = decoder.decompress_into(input, &mut output);
    Run {
        result: result.map(|written| output[..written].to_vec()),
        pipelined: decoder.pipelined_frames,
        redone: decoder.redone_frames,
    }
}

// Every thread count gives the bytes or the error of the serial path, and a frame that
// decodes goes through the threads without being redone; returns that verdict.
fn assert_threaded_is_serial(
    dictionary: Option<&Dictionary>,
    input: &[u8],
    capacity: usize,
    what: &str,
) -> Result<Vec<u8>, Error> {
    let serial = decode(dictionary, input, capacity, 1);
    assert_eq!(serial.pipelined, 0);
    // Miri: one thread count, it is a thousand times slower.
    for threads in &THREADS[..if cfg!(miri) { 1 } else { THREADS.len() }] {
        let threads = *threads;
        let run = decode(dictionary, input, capacity, threads);
        let what = format!("{what}, {threads} threads");
        match (&serial.result, &run.result) {
            (Ok(serial), Ok(threaded)) => assert_same(threaded, serial, &what),
            (serial, threaded) => {
                assert_eq!(threaded.as_ref().err(), serial.as_ref().err(), "{what}");
            }
        }
        if serial.result.is_ok() && !input.is_empty() {
            assert_eq!(run.redone, 0, "{what}: frame redone");
            assert!(run.pipelined > 0, "{what}: serial");
        }
    }
    serial.result
}

fn crafted_frame(blocks: &[Vec<u8>]) -> Vec<u8> {
    // No content size, no checksum, a 1 KiB window.
    let mut frame = [&MAGIC[..], &[0, 0]].concat();
    for (index, block) in blocks.iter().enumerate() {
        let last = u32::from(index + 1 == blocks.len());
        // Compressed block.
        let header = last | 2 << 1 | (block.len() as u32) << 3;
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.extend_from_slice(block);
    }
    frame
}

// A treeless section with four streams after a one-stream table: the table is scaled up in
// place, and a later one-stream section decodes with the scaled table.
#[test]
fn treeless_sections_follow_the_table_scaled_for_four_streams() {
    let mut rng = Rng::with_seed(7);
    for case in 0..20 {
        let tree = TestTree::random(&mut rng, 1 + case % 9);
        let data = tree.random_data(&mut rng, 300);
        let one = tree.encode_stream(&data);
        let four = tree.encode_four_streams(&data);
        // Sections followed by a sequence section of 0 sequences.
        let section = |kind, size_format, payload: &[u8]| {
            [
                huffman_section(kind, size_format, data.len(), payload),
                vec![0],
            ]
            .concat()
        };
        let first = section(2, 0, &[&tree.header[..], &one].concat());
        let blocks = [
            first,
            section(3, 1, &four),
            section(3, 0, &one),
            section(3, 2, &four),
        ];
        let frame = crafted_frame(&blocks);
        let expected = data.repeat(4);
        let verdict = assert_threaded_is_serial(None, &frame, expected.len(), "scaled table");
        assert_same(&verdict.unwrap(), &expected, "scaled table output");
    }
}

// Four blocks of 1 KiB with sequences and a checksum: small enough for Miri.
#[test]
fn a_small_multi_block_frame_decodes_on_threads() {
    let frame = include_bytes!("../../tests/data/threaded-small.zst");
    assert!(scan_frame(frame).unwrap().1 >= 3);
    let verdict = assert_threaded_is_serial(None, frame, 3500, "small frame");
    assert_eq!(verdict.unwrap().len(), 3500);
    for length in (0..frame.len()).step_by(if cfg!(miri) { 97 } else { 1 }) {
        let _ = assert_threaded_is_serial(None, &frame[..length], 3500, "truncated small frame");
    }
}

// The same small frame through `decompress_stream` and `Reader` with threads, whole and cut
// anywhere (the inputs end inside a block).
#[test]
fn a_small_multi_block_frame_streams_on_threads() {
    let frame = include_bytes!("../../tests/data/threaded-small.zst");
    let reference = stream::reference(None, frame);
    assert_eq!((reference.bytes.len(), reference.error), (3500, None));
    stream::assert_decodes(None, frame, &reference.bytes, 1, "small frame");
    for length in (0..frame.len()).step_by(if cfg!(miri) { 211 } else { 1 }) {
        stream::assert_same_as_serial(None, &frame[..length], length, 0, "cut small frame");
    }
}

// One run of the small frame through `decompress_stream` and a `Reader` with threads, in odd
// pieces, whole and cut in a block: small enough for Miri.
#[test]
fn a_small_multi_block_frame_streams_on_threads_in_odd_pieces() {
    use stream::Piece::Fixed;
    let frame = include_bytes!("../../tests/data/threaded-small.zst");
    let reference = stream::reference(None, frame);
    assert_eq!(reference.bytes.len(), 3500);
    let shape = stream::shape(Fixed(97), Fixed(50));
    let run = stream::drive_stream(stream::decoder(None, 2, 0), frame, shape);
    assert_eq!((run.frames, run.fallbacks, run.handovers), (1, 0, 0));
    stream::assert_matches(&run, &reference, "odd pieces");
    let run = stream::drive_reader(stream::decoder(None, 3, 0), &frame[..], 50);
    assert_eq!((run.frames, run.fallbacks, run.handovers), (1, 0, 0));
    stream::assert_matches(&run, &reference, "reader");
    // Output that the content fills exactly: the call that completes the frame writes nothing.
    for read_size in [3500, 1750] {
        let source = stream::source(frame, 97);
        let run = stream::drive_reader(stream::decoder(None, 2, 0), source, read_size);
        assert_eq!(run.frames, 1);
        stream::assert_matches(&run, &reference, "exact reads");
        let shape = stream::shape(Fixed(97), Fixed(read_size));
        let run = stream::drive_stream(stream::decoder(None, 2, 0), frame, shape);
        stream::assert_matches(&run, &reference, "exact output");
    }
    let cut = &frame[..frame.len() * 2 / 3];
    let reference = stream::reference(None, cut);
    let run = stream::drive_stream(stream::decoder(None, 2, 0), cut, shape);
    stream::assert_matches(&run, &reference, "cut");
}

// Found by the fuzz target: planning stopped (the rest of the input is read by the serial code)
// with every planned block executed, and the serial code went on with the tables stage 1 never
// built.
#[test]
fn the_serial_code_going_on_after_the_pipeline_has_the_tables_of_the_last_block() {
    let data =
        &include_bytes!("../../crates/compare/tests/data/parallel-stale-tables-after-planning.bin")
            [3..];
    let mut windowed = Decoder::new();
    windowed.set_window_log_max(20);
    let reference = stream::reference_with(windowed, data);
    assert!(reference.bytes.len() > 40_000, "{}", reference.bytes.len());
    for (index, threads) in THREADS.into_iter().enumerate() {
        for shape in &stream::SHAPES[index..] {
            let mut decoder = stream::decoder(None, threads, 0);
            decoder.set_window_log_max(20);
            let run = stream::drive_stream(decoder, data, *shape);
            stream::assert_matches(&run, &reference, &format!("{threads} threads, {shape:?}"));
        }
    }
}

// Found by the fuzz target: the last literals of a block did not fit after its sequences had
// executed, the repeat offsets of the failed block were kept, and the serial code redid it with them.
#[test]
fn a_block_that_fails_after_its_sequences_leaves_the_repeat_offsets_alone() {
    let data = &include_bytes!(
        "../../crates/compare/tests/data/parallel-repeat-offsets-of-failed-block.bin"
    )[3..];
    let mut windowed = Decoder::new();
    windowed.set_window_log_max(20);
    let reference = stream::reference_with(windowed, data);
    assert_eq!(reference.error, Some(Error::DstSizeTooSmall));
    for (index, threads) in THREADS.into_iter().enumerate() {
        for shape in &stream::SHAPES[index..] {
            let mut decoder = stream::decoder(None, threads, 0);
            decoder.set_window_log_max(20);
            let run = stream::drive_stream(decoder, data, *shape);
            stream::assert_matches(&run, &reference, &format!("{threads} threads, {shape:?}"));
        }
    }
}

// A tree description of the right size that does not build (stage 2 finds out, stage 1 only
// reads its size), defining the table of the treeless sections after it.
#[test]
fn a_tree_that_does_not_build_fails_the_same_with_treeless_sections_after_it() {
    let mut rng = Rng::with_seed(11);
    for case in 0..10 {
        let tree = TestTree::random(&mut rng, 1 + case % 9);
        let data = tree.random_data(&mut rng, 300);
        let one = tree.encode_stream(&data);
        let section = |kind, payload: &[u8]| {
            [huffman_section(kind, 0, data.len(), payload), vec![0]].concat()
        };
        let good = [&tree.header[..], &one].concat();
        // Weights of 15 and 15 for the first two symbols: heavier than any table.
        let mut bad = good.clone();
        bad[1] = 0xFF;
        for (what, blocks) in [
            (
                "first",
                vec![section(2, &bad), section(3, &one), section(3, &one)],
            ),
            (
                "second",
                vec![section(2, &good), section(2, &bad), section(3, &one)],
            ),
            (
                "last",
                vec![section(2, &good), section(3, &one), section(2, &bad)],
            ),
        ] {
            let frame = crafted_frame(&blocks);
            let what = format!("bad tree, {what} block");
            let verdict = assert_threaded_is_serial(None, &frame, 1200, &what);
            assert_eq!(verdict, Err(Error::CorruptionDetected), "{what}");
            // It failed in stage 2, not in stage 1: the frame went through the threads, and was redone.
            let run = decode(None, &frame, 1200, 2);
            assert_eq!((run.pipelined, run.redone), (1, 1), "{what}");
            let reference = stream::reference(None, &frame);
            assert_eq!(reference.error, Some(Error::CorruptionDetected), "{what}");
            for (index, threads) in THREADS.into_iter().enumerate() {
                let shape = stream::SHAPES[(case as usize + index) % stream::SHAPES.len()];
                let run = stream::drive_stream(stream::decoder(None, threads, 0), &frame, shape);
                assert_eq!(run.frames, 1, "{what}, {threads} threads");
                stream::assert_matches(
                    &run,
                    &reference,
                    &format!("{what}, {threads} threads, {shape:?}"),
                );
            }
        }
    }
}

#[cfg(not(any(target_family = "wasm", miri)))]
mod against_libzstd {
    use core::ffi::c_void;

    use zstd_sys::{
        ZSTD_CCtx_loadDictionary, ZSTD_CCtx_setParameter, ZSTD_cParameter, ZSTD_compress2,
        ZSTD_compressBound, ZSTD_createCCtx, ZSTD_freeCCtx, ZSTD_isError,
    };

    use super::*;
    use crate::frame::{
        BLOCK_HEADER_SIZE, BlockType, FrameStart, frame_header_size, parse_block_header,
        parse_frame_start,
    };
    use crate::testing::skewed_bytes;

    mod threaded;

    // Words from a vocabulary with a skewed choice, separated by spaces and punctuation.
    fn text(vocabulary_seed: u64, seed: u64, len: usize) -> Vec<u8> {
        let mut rng = Rng::with_seed(vocabulary_seed);
        let words: Vec<Vec<u8>> = (0..400)
            .map(|_| {
                (0..2 + rng.usize(..8))
                    .map(|_| b'a' + rng.u8(..26))
                    .collect()
            })
            .collect();
        let mut rng = Rng::with_seed(seed);
        let mut data = Vec::new();
        while data.len() < len {
            let pick = rng.usize(..words.len()) * rng.usize(..words.len()) / words.len();
            data.extend_from_slice(&words[pick]);
            data.extend_from_slice(rng.choice([&b" "[..], b" ", b" ", b", ", b".\n"]).unwrap());
        }
        data.truncate(len);
        data
    }

    // Records with fixed fields and a counter: matches at a few repeating offsets.
    fn records(seed: u64, len: usize) -> Vec<u8> {
        let mut rng = Rng::with_seed(seed);
        let mut data = Vec::new();
        let mut counter = rng.u32(..1000);
        while data.len() < len {
            counter += 1 + rng.u32(..3);
            let line = format!(
                "id={counter:08} kind={} flags={:04x} name=item{:03} score={:.2}\n",
                ["alpha", "beta", "gamma"][rng.usize(..3)],
                rng.u16(..) & 0xF0F0,
                rng.u16(..60),
                rng.f64() * 100.0
            );
            data.extend_from_slice(line.as_bytes());
        }
        data.truncate(len);
        data
    }

    fn skewed(seed: u64, len: usize) -> Vec<u8> {
        let mut rng = Rng::with_seed(seed);
        let mut data = Vec::new();
        while data.len() < len {
            data.extend(skewed_bytes(&mut rng));
        }
        data.truncate(len);
        data
    }

    // Segments of the three kinds, so that literal statistics change from block to block.
    fn mixed(seed: u64, len: usize) -> Vec<u8> {
        let mut rng = Rng::with_seed(seed);
        let mut data = Vec::new();
        while data.len() < len {
            let segment = 1000 + rng.usize(..6000);
            let part = match rng.usize(..3) {
                0 => text(1, rng.u64(..), segment),
                1 => records(rng.u64(..), segment),
                _ => skewed(rng.u64(..), segment),
            };
            data.extend(part);
        }
        data.truncate(len);
        data
    }

    const ARCHIVE: &[u8] = include_bytes!("../../tests/data/golden-dictionary.bin");
    // (offset, length) of the trained dictionaries in the archive.
    const TRAINED_2K: (usize, usize) = (31164, 2048);
    const TRAINED_16K: (usize, usize) = (73622, 16384);

    #[derive(Debug, Default)]
    struct Coverage {
        blocks: usize,
        treeless: usize,
        treeless_four_streams: usize,
        // Repeat mode of the literal length, offset and match length tables.
        repeat_tables: [usize; 3],
    }

    fn compress(
        data: &[u8],
        level: i32,
        window_log: i32,
        checksum: bool,
        dictionary: &[u8],
    ) -> Vec<u8> {
        compress_with_size(data, level, window_log, checksum, true, dictionary)
    }

    // `content_size`: the frame header states the size of the data.
    fn compress_with_size(
        data: &[u8],
        level: i32,
        window_log: i32,
        checksum: bool,
        content_size: bool,
        dictionary: &[u8],
    ) -> Vec<u8> {
        // SAFETY: the context is created, used and freed here; buffers are valid for their lengths.
        unsafe {
            let cctx = ZSTD_createCCtx();
            assert!(!cctx.is_null());
            let set = |parameter, value| {
                assert_eq!(
                    ZSTD_isError(ZSTD_CCtx_setParameter(cctx, parameter, value)),
                    0
                );
            };
            set(ZSTD_cParameter::ZSTD_c_compressionLevel, level);
            if window_log > 0 {
                set(ZSTD_cParameter::ZSTD_c_windowLog, window_log);
            }
            set(ZSTD_cParameter::ZSTD_c_checksumFlag, checksum.into());
            set(ZSTD_cParameter::ZSTD_c_contentSizeFlag, content_size.into());
            if !dictionary.is_empty() {
                let loaded =
                    ZSTD_CCtx_loadDictionary(cctx, dictionary.as_ptr().cast(), dictionary.len());
                assert_eq!(ZSTD_isError(loaded), 0);
            }
            let mut out = vec![0u8; ZSTD_compressBound(data.len())];
            let size = ZSTD_compress2(
                cctx,
                out.as_mut_ptr().cast::<c_void>(),
                out.len(),
                data.as_ptr().cast(),
                data.len(),
            );
            assert_eq!(ZSTD_isError(size), 0);
            ZSTD_freeCCtx(cctx);
            out.truncate(size);
            out
        }
    }

    // Counts the sections of `frame` that the tests must see, by reading its headers with the
    // serial states.
    fn scan(frame: &[u8], dictionary: Option<&Dictionary>, coverage: &mut Coverage) {
        let mut decoder = Decoder::new();
        decoder.set_dictionary(dictionary);
        decoder.begin_frame();
        let header_size = frame_header_size(frame).unwrap();
        let FrameStart::Frame(header) = parse_frame_start(&frame[..header_size]).unwrap() else {
            panic!("not a frame");
        };
        let max = header.block_size_max as usize;
        let mut position = header_size;
        loop {
            let block = parse_block_header(&frame[position..]).unwrap();
            position += BLOCK_HEADER_SIZE;
            let body = &frame[position..position + block.input_size()];
            position += block.input_size();
            if block.block_type == BlockType::Compressed {
                coverage.blocks += 1;
                let literals_size = decoder.literals.decode(body, max, max).unwrap().1;
                if body[0] & 3 == 3 {
                    coverage.treeless += 1;
                    coverage.treeless_four_streams += usize::from(body[0] >> 2 & 3 != 0);
                }
                let rest = &body[literals_size..];
                let (nb_seq, _) = decoder.sequences.read_header::<true>(rest).unwrap();
                decoder.sequences.begin_block(nb_seq);
                if nb_seq > 0 {
                    let modes =
                        rest[[1, 2, 3][usize::from(rest[0] >= 128) + usize::from(rest[0] == 255)]];
                    for (count, mode) in coverage.repeat_tables.iter_mut().zip([
                        modes >> 6,
                        modes >> 4 & 3,
                        modes >> 2 & 3,
                    ]) {
                        *count += usize::from(mode == 3);
                    }
                }
            }
            if block.last {
                break;
            }
        }
    }

    fn assert_coverage(coverage: &Coverage) {
        assert!(coverage.blocks > 100, "{coverage:?}");
        assert!(coverage.treeless > 20, "{coverage:?}");
        assert!(coverage.treeless_four_streams > 10, "{coverage:?}");
        assert!(
            coverage.repeat_tables.iter().all(|&count| count > 20),
            "{coverage:?}"
        );
    }

    fn data_sets(len: usize) -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("text", text(1, 2, len)),
            ("records", records(3, len)),
            ("skewed", skewed(4, len)),
            ("mixed", mixed(5, len)),
        ]
    }

    // Capacities around the content size: exact, with room, one short, and half.
    fn check_capacities(dictionary: Option<&Dictionary>, frame: &[u8], size: usize, what: &str) {
        for capacity in [size, size + 5000, size - 1, size / 2] {
            let _ = assert_threaded_is_serial(
                dictionary,
                frame,
                capacity,
                &format!("{what}, capacity {capacity}"),
            );
        }
    }

    #[test]
    fn libzstd_frames_decode_the_same_threaded_or_not() {
        let mut coverage = Coverage::default();
        for (name, data) in data_sets(150_000) {
            for level in [1, 3, 19] {
                for window_log in [0, 10] {
                    for checksum in [false, true] {
                        let what = format!(
                            "{name} level {level} windowLog {window_log} checksum {checksum}"
                        );
                        let frame = compress(&data, level, window_log, checksum, &[]);
                        scan(&frame, None, &mut coverage);
                        let verdict = assert_threaded_is_serial(None, &frame, data.len(), &what);
                        assert_same(&verdict.unwrap(), &data, &what);
                        check_capacities(None, &frame, data.len(), &what);
                    }
                }
            }
        }
        assert_coverage(&coverage);
    }

    #[test]
    fn libzstd_frames_with_a_dictionary_decode_the_same_threaded_or_not() {
        let raw = text(1, 9, 8000);
        let trained = |(offset, length): (usize, usize)| ARCHIVE[offset..offset + length].to_vec();
        // The trained dictionaries start with the repeat offsets 1, 4, 8: use others.
        let mut patched = trained(TRAINED_2K);
        let default_offsets: Vec<u8> = [1u32, 4, 8].iter().flat_map(|o| o.to_le_bytes()).collect();
        let at = patched
            .windows(12)
            .position(|w| w == default_offsets)
            .unwrap();
        for (index, offset) in [100u32, 200, 300].into_iter().enumerate() {
            patched[at + 4 * index..at + 4 * index + 4].copy_from_slice(&offset.to_le_bytes());
        }
        let mut coverage = Coverage::default();
        for (dictionary_name, bytes) in [
            ("raw", raw),
            ("2k", trained(TRAINED_2K)),
            ("16k", trained(TRAINED_16K)),
            ("2k with other repeat offsets", patched),
        ] {
            let dictionary = Dictionary::new(&bytes).unwrap();
            for (name, data) in data_sets(60_000) {
                for level in [3, 19] {
                    for window_log in [0, 10] {
                        let what = format!(
                            "{dictionary_name} {name} level {level} windowLog {window_log}"
                        );
                        let frame = compress(&data, level, window_log, true, &bytes);
                        scan(&frame, Some(&dictionary), &mut coverage);
                        let verdict =
                            assert_threaded_is_serial(Some(&dictionary), &frame, data.len(), &what);
                        assert_same(&verdict.unwrap(), &data, &what);
                        check_capacities(Some(&dictionary), &frame, data.len(), &what);
                        // A second frame: the decoder state after the first, cold flag included.
                        let twice = [&frame[..], &frame].concat();
                        let verdict = assert_threaded_is_serial(
                            Some(&dictionary),
                            &twice,
                            2 * data.len(),
                            &what,
                        );
                        assert_same(&verdict.unwrap(), &data.repeat(2), &what);
                    }
                }
            }
            // Data repeating at each of the dictionary's repeat offsets, which the encoder
            // codes as repeat offsets in the first block.
            for offset in dictionary.entropy().map_or([0; 3], |e| e.repeat_offsets) {
                let mut rng = Rng::with_seed(u64::from(offset));
                let period: Vec<u8> = (0..offset).map(|_| rng.u8(..)).collect();
                let data = period.repeat(3);
                let what = format!("{dictionary_name} period {offset}");
                let frame = compress(&data, 3, 0, false, &bytes);
                let verdict =
                    assert_threaded_is_serial(Some(&dictionary), &frame, data.len(), &what);
                assert_same(&verdict.unwrap(), &data, &what);
            }
        }
        assert!(
            coverage.repeat_tables.iter().all(|&count| count > 20),
            "{coverage:?}"
        );
        assert!(coverage.treeless > 20, "{coverage:?}");
    }

    // Blocks of 128 KiB, the hash running behind the execution: a wrong checksum, and damage in
    // the last block, give the serial verdict at every thread count.
    #[test]
    fn the_checksum_of_a_frame_of_large_blocks_is_checked_on_every_thread_count() {
        let data = mixed(21, 1_500_000);
        let frame = compress(&data, 1, 0, true, &[]);
        assert!(frame.len() > 5 << 17 && scan_frame(&frame).unwrap().1 >= 10);
        let what = "large blocks";
        let verdict = assert_threaded_is_serial(None, &frame, data.len(), what);
        assert_same(&verdict.unwrap(), &data, what);
        for from_end in 1..=4 {
            let mut corrupted = frame.clone();
            let at = corrupted.len() - from_end;
            corrupted[at] ^= 0x10;
            let verdict = assert_threaded_is_serial(None, &corrupted, data.len(), what);
            assert_eq!(verdict, Err(Error::ChecksumWrong));
        }
        for from_end in (5..400).step_by(7) {
            let mut corrupted = frame.clone();
            let at = corrupted.len() - from_end;
            corrupted[at] ^= 0x01;
            let _ = assert_threaded_is_serial(None, &corrupted, data.len(), what);
        }
    }

    // A small multi-block frame: every byte flipped two ways, every truncation.
    fn sweep(dictionary: Option<&Dictionary>, dictionary_bytes: &[u8], checksum: bool, what: &str) {
        let data = mixed(11, 12_000);
        let frame = compress(&data, 3, 10, checksum, dictionary_bytes);
        let mut coverage = Coverage::default();
        scan(&frame, dictionary, &mut coverage);
        assert!(
            coverage.blocks >= 8 && coverage.treeless > 0,
            "{coverage:?}"
        );
        let capacity = data.len() + 64;
        let (mut ok, mut failed) = (0, 0);
        for position in 0..frame.len() {
            for mask in [0x01, 0x80] {
                let mut corrupted = frame.clone();
                corrupted[position] ^= mask;
                let verdict = assert_threaded_is_serial(
                    dictionary,
                    &corrupted,
                    capacity,
                    &format!("{what}, byte {position} ^ {mask:#x}"),
                );
                if verdict.is_ok() {
                    ok += 1;
                } else {
                    failed += 1;
                }
            }
        }
        // Some flips still decode (raw sections, the checksum when disabled), most do not.
        assert!(ok > 5 && failed > 500, "{ok} ok, {failed} failed");
        for length in 0..frame.len() {
            let _ = assert_threaded_is_serial(
                dictionary,
                &frame[..length],
                capacity,
                &format!("{what}, truncated to {length}"),
            );
        }
    }

    #[test]
    fn corrupted_and_truncated_frames_fail_the_same_threaded_or_not() {
        sweep(None, &[], false, "no dictionary");
        sweep(None, &[], true, "checksum");
    }

    #[test]
    fn corrupted_and_truncated_dictionary_frames_fail_the_same_threaded_or_not() {
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

    // Frames of every kind in a row: some under the gate (serial), skippable, tiny, empty, and
    // with raw blocks among compressed ones.
    fn frames_in_a_row() -> (Vec<u8>, Vec<u8>, usize) {
        let mut rng = Rng::with_seed(21);
        let random: Vec<u8> = (0..30_000).map(|_| rng.u8(..)).collect();
        let raw_among_compressed = [text(1, 5, 30_000), random, text(1, 6, 30_000)].concat();
        let skippable = [
            &[0x50, 0x2A, 0x4D, 0x18][..],
            &100u32.to_le_bytes(),
            &[7; 100],
        ]
        .concat();
        let rle_frame = [
            &MAGIC[..],
            &[0, 0],
            &(1u32 | 1 << 1 | 50 << 3).to_le_bytes()[..3],
            &[9],
        ]
        .concat();
        let parts = [
            (
                compress(&text(1, 2, 100_000), 3, 0, false, &[]),
                text(1, 2, 100_000),
            ),
            (skippable, vec![]),
            (
                compress(&records(3, 300), 3, 0, false, &[]),
                records(3, 300),
            ),
            (
                compress(&records(4, 80_000), 1, 0, true, &[]),
                records(4, 80_000),
            ),
            (rle_frame, vec![9; 50]),
            (
                compress(&raw_among_compressed, 3, 10, true, &[]),
                raw_among_compressed,
            ),
            (compress(&[], 3, 0, true, &[]), vec![]),
            (
                compress(&text(1, 7, 100_000), 19, 0, true, &[]),
                text(1, 7, 100_000),
            ),
        ];
        let (input, expected) = parts.into_iter().fold(
            (Vec::new(), Vec::new()),
            |(mut input, mut expected), (frame, data)| {
                input.extend(frame);
                expected.extend(data);
                (input, expected)
            },
        );
        (input, expected, 4)
    }

    #[test]
    fn frames_in_a_row_go_through_the_threads_or_not_by_size() {
        const GATE_BYTES: usize = 2000;
        let (input, expected, gated_frames) = frames_in_a_row();
        for threads in THREADS {
            let run = decode_gated(None, &input, expected.len(), threads, GATE_BYTES);
            assert_same(&run.result.unwrap(), &expected, "frames in a row");
            assert_eq!(
                (run.pipelined, run.redone),
                (gated_frames, 0),
                "{threads} threads"
            );
        }
        // Damaged anywhere, or cut: the serial verdict.
        let capacity = expected.len() + 100;
        let mut cases = Vec::new();
        for position in (0..input.len()).step_by(997) {
            let mut damaged = input.clone();
            damaged[position] ^= 0x40;
            cases.push(damaged);
        }
        for length in (0..input.len()).step_by(1511) {
            cases.push(input[..length].to_vec());
        }
        for (case, damaged) in cases.iter().enumerate() {
            let serial = decode(None, damaged, capacity, 1).result;
            for threads in THREADS {
                let threaded = decode_gated(None, damaged, capacity, threads, GATE_BYTES).result;
                match (&serial, &threaded) {
                    (Ok(serial), Ok(threaded)) => assert_same(threaded, serial, "damaged"),
                    _ => assert_eq!(
                        threaded.as_ref().err(),
                        serial.as_ref().err(),
                        "case {case}"
                    ),
                }
            }
        }
    }

    #[test]
    fn a_reused_decoder_follows_the_threads_setting_frame_after_frame() {
        let sets = data_sets(40_000);
        let frames: Vec<Vec<u8>> = sets
            .iter()
            .zip([(1, 0, false), (3, 10, true), (19, 0, true), (3, 10, false)])
            .map(|((_, data), (level, window_log, checksum))| {
                compress(data, level, window_log, checksum, &[])
            })
            .collect();
        let dictionary_bytes = text(1, 9, 8000);
        let dictionary = Dictionary::new(&dictionary_bytes).unwrap();
        let dictionary_data = records(8, 30_000);
        let dictionary_frame = compress(&dictionary_data, 3, 0, true, &dictionary_bytes);
        let mut decoder = Decoder::new();
        for (round, threads) in [4, 1, 2, 0, 8, 3, 2, 4, 1, 3].into_iter().enumerate() {
            decoder.set_threads(threads);
            for (index, ((name, data), frame)) in sets.iter().zip(&frames).enumerate() {
                let mut output = vec![0; data.len()];
                let written = match (round + index) % 3 {
                    0 => decoder.decompress_into(frame, &mut output).unwrap(),
                    1 => decoder.decompress_into_vec(frame, &mut output).unwrap(),
                    _ => {
                        let mut vec = Vec::new();
                        let written = decoder.decompress_into_vec(frame, &mut vec).unwrap();
                        output = vec;
                        written
                    }
                };
                assert_same(&output[..written], data, &format!("{name}, round {round}"));
            }
            // A dictionary comes and goes between calls.
            decoder.set_dictionary(Some(&dictionary));
            let mut output = vec![0; dictionary_data.len()];
            let written = decoder
                .decompress_into(&dictionary_frame, &mut output)
                .unwrap();
            assert_same(&output[..written], &dictionary_data, "dictionary frame");
            decoder.set_dictionary(None);
        }
        assert_eq!(decoder.redone_frames, 0);
        assert!(
            decoder.pipelined_frames >= 7 * 5,
            "{}",
            decoder.pipelined_frames
        );
    }
}
