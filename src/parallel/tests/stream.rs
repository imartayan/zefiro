//! `decompress_stream` and `Reader` with threads against the serial stream: the same bytes, then
//! the same error, for any chunking of the input and of the output. The reference is the serial
//! `decompress_stream` fed one byte at a time with room for a megabyte: no call writes bytes
//! and then fails, so the bytes it returns before an error are all those decoded before it.

use alloc::boxed::Box;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use std::io::{self, BufRead, BufReader, Read};

use super::*;
use crate::Reader;

// How many bytes one call is given.
#[derive(Debug, Clone, Copy)]
pub(super) enum Piece {
    Fixed(usize),
    // A new size from 1 to 3000 for each call.
    Random,
    Whole,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Shape {
    pub(super) input: Piece,
    pub(super) output: Piece,
}

pub(super) const fn shape(input: Piece, output: Piece) -> Shape {
    Shape { input, output }
}

use Piece::{Fixed, Random, Whole};

pub(super) const SHAPES: [Shape; 7] = [
    shape(Fixed(1), Fixed(1 << 16)),
    shape(Fixed(7), Fixed(7)),
    shape(Fixed(4096), Fixed(1)),
    shape(Fixed(1000), Fixed(4096)),
    shape(Fixed(128 << 10), Fixed(128 << 10)),
    shape(Whole, Fixed(1 << 21)),
    shape(Random, Random),
];
// Whole frames in the input, big pieces, for the inputs that are not swept.
pub(super) const BIG: Shape = shape(Fixed(1 << 20), Fixed(4 << 20));
// The frames that fail in the tests have blocks of 1 KiB (window 10), or crafted ones.
const BLOCK_SIZE_MAX_IN_ERROR_TESTS: usize = 1024;
// Output pieces of one byte are a call each.
pub(super) const SMALL_ONLY: [usize; 2] = [1, 2];

// What a stream gave: its bytes, the error that ended it, and if it stopped between frames.
#[derive(Debug, PartialEq)]
pub(super) struct Outcome {
    pub(super) bytes: Vec<u8>,
    pub(super) error: Option<Error>,
    pub(super) at_frame_boundary: bool,
}

pub(super) struct Run {
    pub(super) outcome: Outcome,
    // Frames that went through the threads, blocks the serial code took over, and times it took
    // over (from a block that failed, or the end of the input inside a block).
    pub(super) frames: usize,
    pub(super) fallbacks: usize,
    pub(super) handovers: usize,
    // The serial code failed on a frame outside the pipeline: its call loses the bytes it wrote.
    pub(super) serial_error: bool,
}

pub(super) fn decoder(dictionary: Option<&Dictionary>, threads: usize, gate: usize) -> Decoder {
    let mut decoder = Decoder::new();
    decoder.set_dictionary(dictionary);
    decoder.set_threads(threads);
    decoder.gate_bytes = gate;
    decoder
}

impl Piece {
    fn next(self, rng: &mut Rng) -> usize {
        match self {
            Self::Fixed(size) => size,
            Self::Random => 1 + rng.usize(..3000),
            Self::Whole => usize::MAX,
        }
    }
}

// `decoder.decompress_stream` over `input` with the pieces of `shape`, to the end: calls go on
// with an empty input until one writes nothing.
pub(super) fn drive_stream(mut decoder: Decoder, input: &[u8], shape: Shape) -> Run {
    drive_decoder(&mut decoder, input, shape)
}

pub(super) fn drive_decoder(decoder: &mut Decoder, input: &[u8], shape: Shape) -> Run {
    let mut rng = Rng::with_seed(input.len() as u64);
    let mut outcome = Outcome {
        bytes: Vec::new(),
        error: None,
        at_frame_boundary: true,
    };
    let (mut position, mut buffer) = (0usize, Vec::new());
    for _ in 0..100_000_000 {
        let end = position
            .saturating_add(shape.input.next(&mut rng))
            .min(input.len());
        let room = shape.output.next(&mut rng).min(1 << 22);
        if buffer.len() < room {
            buffer.resize(room, 0);
        }
        match decoder.decompress_stream(&input[position..end], &mut buffer[..room]) {
            Err(error) => {
                outcome.error = Some(error);
                break;
            }
            Ok(progress) => {
                position += progress.consumed;
                outcome.bytes.extend_from_slice(&buffer[..progress.written]);
                if progress.consumed + progress.written > 0 || progress.frame_done {
                    outcome.at_frame_boundary = progress.frame_done;
                }
                if end == input.len() && progress.consumed + progress.written == 0 {
                    break;
                }
            }
        }
    }
    run(outcome, decoder.streaming.as_deref())
}

fn run(outcome: Outcome, streaming: Option<&Streaming>) -> Run {
    let counters = streaming.map(|s| {
        (
            s.frames,
            s.pipeline.fallbacks,
            s.pipeline.handovers,
            s.serial_errors > 0,
        )
    });
    let (frames, fallbacks, handovers, serial_error) = counters.unwrap_or_default();
    Run {
        outcome,
        frames,
        fallbacks,
        handovers,
        serial_error,
    }
}

// What the serial stream gives, byte by byte.
pub(super) fn reference(dictionary: Option<&Dictionary>, input: &[u8]) -> Outcome {
    reference_with(decoder(dictionary, 1, 0), input)
}

pub(super) fn reference_with(serial: Decoder, input: &[u8]) -> Outcome {
    let run = drive_stream(serial, input, shape(Fixed(1), Fixed(1 << 20)));
    assert_eq!(run.frames, 0);
    run.outcome
}

pub(super) fn source(input: &[u8], chunk: usize) -> Box<dyn BufRead + '_> {
    if chunk == 0 {
        Box::new(input)
    } else {
        Box::new(BufReader::with_capacity(chunk, input))
    }
}

// A `Reader` over `source` with `decoder`, read `read_size` bytes at a time to the end. The
// error is the `Error` inside the `io::Error`, and an end inside a frame is `UnexpectedEof`.
pub(super) fn drive_reader<R: BufRead>(decoder: Decoder, source: R, read_size: usize) -> Run {
    let mut reader = Reader::with_decoder(decoder, source);
    let mut outcome = Outcome {
        bytes: Vec::new(),
        error: None,
        at_frame_boundary: true,
    };
    let mut buffer = vec![0; read_size];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => outcome.bytes.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                outcome.at_frame_boundary = false;
                break;
            }
            Err(error) => {
                let inner = error.get_ref().and_then(|e| e.downcast_ref::<Error>());
                outcome.error = Some(*inner.unwrap_or_else(|| panic!("{error}")));
                break;
            }
        }
    }
    run(outcome, reader.streaming())
}

pub(super) fn assert_matches(run: &Run, reference: &Outcome, what: &str) {
    assert_matches_loosely(run, reference, false, what);
}

// With `loose`, any error is fine if the reference has one. A raw block is taken in pieces, and
// what the serial stream makes of a damaged frame after raw blocks (the room it needs in its
// ring, when the content size is wrong) depends on the size of the pieces: the verdict of the
// threads is that of some chunking, and not always the reference's.
pub(super) fn assert_matches_loosely(run: &Run, reference: &Outcome, loose: bool, what: &str) {
    let what = format!(
        "{what} ({} frames, {} fallbacks, {} handovers)",
        run.frames, run.fallbacks, run.handovers
    );
    if loose {
        assert_eq!(
            run.outcome.error.is_some(),
            reference.error.is_some(),
            "{what}"
        );
    } else {
        assert_eq!(run.outcome.error, reference.error, "{what}");
    }
    if run.serial_error {
        let prefix = &reference.bytes[..run.outcome.bytes.len().min(reference.bytes.len())];
        assert_same(&run.outcome.bytes, prefix, &what);
    } else if run.outcome.error.is_some() {
        // A raw block that fails at its end (the content size is wrong) is delivered byte by
        // byte by the reference, and a whole block fails before it is flushed: it may be missing.
        let prefix = &reference.bytes[..run.outcome.bytes.len().min(reference.bytes.len())];
        assert_same(&run.outcome.bytes, prefix, &what);
        assert!(
            reference.bytes.len() - prefix.len() <= BLOCK_SIZE_MAX_IN_ERROR_TESTS,
            "{what}: blocks missing"
        );
    } else {
        assert_same(&run.outcome.bytes, &reference.bytes, &what);
    }
    // A `Reader` reports the end inside a frame as an error, not by the flag.
    if run.outcome.error.is_none() {
        assert_eq!(
            run.outcome.at_frame_boundary, reference.at_frame_boundary,
            "{what}"
        );
    }
}

// `input` decodes to `expected` through `decompress_stream` in every shape and thread count,
// and through a `Reader`; frames that can go through the threads do, with no block or item left to the
// serial code. `frames`: how many frames `input` holds.
pub(super) fn assert_decodes(
    dictionary: Option<&Dictionary>,
    input: &[u8],
    expected: &[u8],
    frames: usize,
    what: &str,
) {
    // `decompress_stream` decodes a frame whole in the input in one pass when the output holds it.
    let frame_size = expected.len() / frames;
    let shapes = SHAPES.iter().chain(&[BIG]).enumerate();
    for (index, shape) in shapes {
        let threads = THREADS[(index + frames) % THREADS.len()];
        let small_output =
            matches!(shape.output, Fixed(size) if SMALL_ONLY.contains(&size) || size < 8);
        let small_input = matches!(shape.input, Fixed(1));
        if (small_output || small_input) && expected.len() > 50_000 {
            continue;
        }
        let what = format!("{what}, {threads} threads, {shape:?}");
        let run = drive_stream(decoder(dictionary, threads, 0), input, *shape);
        assert_eq!(run.outcome.error, None, "{what}");
        assert_eq!(run.outcome.bytes.len(), expected.len(), "{what}");
        assert_same(&run.outcome.bytes, expected, &what);
        assert_eq!(
            (run.fallbacks, run.handovers),
            (0, 0),
            "{what}: serial code took over"
        );
        let whole_input =
            matches!(shape.input, Whole) || matches!(shape.input, Fixed(n) if n >= input.len());
        let big_output = matches!(shape.output, Fixed(n) if n >= frame_size);
        if !(whole_input && big_output) {
            assert!(
                run.frames >= 1 && run.frames <= frames,
                "{what}: {} frames",
                run.frames
            );
        }
    }
    for (index, (read_size, chunk)) in [
        (1 << 16, 0),
        (4096, 1000),
        (7, 97),
        (1, 5),
        (4 << 20, 1 << 20),
    ]
    .into_iter()
    .enumerate()
    {
        if read_size == 1 && expected.len() > 50_000 {
            continue;
        }
        let threads = THREADS[(index + frames) % THREADS.len()];
        let what =
            format!("{what}, reader, {threads} threads, reads of {read_size}, chunks of {chunk}");
        let run = drive_reader(
            decoder(dictionary, threads, 0),
            source(input, chunk),
            read_size,
        );
        assert_eq!(run.outcome.error, None, "{what}");
        assert_same(&run.outcome.bytes, expected, &what);
        assert_eq!((run.fallbacks, run.handovers), (0, 0), "{what}");
        let one_pass = (chunk == 0 || input.len() <= chunk) && frame_size <= read_size;
        if read_size > 5 && !one_pass {
            assert!(run.frames >= 1 && run.frames <= frames, "{what}");
        }
    }
}

// `input` through a few thread counts and shapes, and a `Reader`, gives the reference's bytes and
// error. A frame decoded in one pass (whole in the input, room for it in the output) never goes
// through the threads, and the serial stream can give another error than byte by byte: then the
// threads must give those of the serial stream with the same shape.
pub(super) fn assert_same_as_serial(
    dictionary: Option<&Dictionary>,
    input: &[u8],
    case: usize,
    gate: usize,
    what: &str,
) -> Run {
    assert_same_as_serial_loosely(dictionary, input, case, gate, false, what)
}

pub(super) fn assert_same_as_serial_loosely(
    dictionary: Option<&Dictionary>,
    input: &[u8],
    case: usize,
    gate: usize,
    loose: bool,
    what: &str,
) -> Run {
    let reference = reference(dictionary, input);
    let mut last = None;
    for step in 0..2 {
        let shape = SHAPES[(case + 3 * step) % SHAPES.len()];
        let threads = THREADS[(case + step) % THREADS.len()];
        let what = format!("{what}, {threads} threads, {shape:?}");
        let run = drive_stream(decoder(dictionary, threads, gate), input, shape);
        if run.frames == 0 {
            let serial = drive_stream(decoder(dictionary, 1, gate), input, shape);
            assert_eq!(run.outcome, serial.outcome, "{what}");
        } else {
            assert_matches_loosely(&run, &reference, loose, &what);
        }
        last = Some(run);
    }
    let (read_size, chunk) = [(1 << 16, 0), (4096, 1000), (7, 97)][case % 3];
    let threads = THREADS[case % THREADS.len()];
    let what =
        format!("{what}, reader, {threads} threads, reads of {read_size}, chunks of {chunk}");
    let run = drive_reader(
        decoder(dictionary, threads, gate),
        source(input, chunk),
        read_size,
    );
    if run.frames == 0 {
        let serial = drive_reader(
            decoder(dictionary, 1, gate),
            source(input, chunk),
            read_size,
        );
        assert_eq!(run.outcome, serial.outcome, "{what}");
    } else {
        assert_matches_loosely(&run, &reference, loose, &what);
    }
    last.unwrap_or(run)
}
