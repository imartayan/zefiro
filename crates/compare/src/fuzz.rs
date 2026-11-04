//! Bodies of the fuzz targets, shared with `tests/regressions.rs` which replays
//! saved inputs through the same functions.

use std::cell::RefCell;
use std::io::{self, BufRead, BufReader, Read};
use std::sync::Once;

use zefiro::{Decoder, Reader};

use crate::{
    Code, Libzstd, LiteralMode, Outcome, Params, assert_agree, check_oracle_build,
    compare_dictionary, compare_stream, compare_stream_dictionary, compress,
    compress_with_dictionary, decompress_ours,
};

const MAX_DATA: usize = 1 << 17;
const LEVELS: [i32; 9] = [-7, -1, 1, 3, 5, 9, 12, 19, 22];

thread_local! {
    static CONTEXT: RefCell<(Libzstd, Decoder)> = RefCell::new((Libzstd::new(), Decoder::new()));
}

fn check_build_once() {
    static CHECK: Once = Once::new();
    CHECK.call_once(check_oracle_build);
}

/// Compares our one-shot verdict with libzstd's at `capacity`.
fn compare_one_shot(input: &[u8], capacity: usize, what: &str) -> Outcome {
    CONTEXT.with(|context| {
        let (oracle, decoder) = &mut *context.borrow_mut();
        let theirs = oracle.decompress(input, capacity);
        assert_agree(&decompress_ours(decoder, input, capacity), &theirs, what);
        theirs
    })
}

/// Arbitrary bytes after a 2 byte output capacity.
pub fn decompress(data: &[u8]) {
    check_build_once();
    let Some((capacity, input)) = data.split_first_chunk::<2>() else {
        return;
    };
    compare_one_shot(
        input,
        usize::from(u16::from_le_bytes(*capacity)),
        "decompress",
    );
}

/// 1 byte (2 to 4 threads, and the sizes of the pieces of input and output), then as `decompress`.
///
/// With threads, the verdict and the output are still libzstd's (the gate is 0 under
/// `cfg(fuzzing)`: every frame takes the pipeline), and `decompress_stream` and a `Reader` with
/// threads give the bytes and the error of the serial stream, for pieces of other sizes (from
/// the capacity).
pub fn parallel(data: &[u8]) {
    check_build_once();
    let Some((&threads, data)) = data.split_first() else {
        return;
    };
    let Some((capacity, input)) = data.split_first_chunk::<2>() else {
        return;
    };
    let capacity = usize::from(u16::from_le_bytes(*capacity));
    let theirs = compare_one_shot(input, capacity, "parallel, serial");
    let threads_count = 2 + usize::from(threads % 3);
    let mut decoder = Decoder::new();
    decoder.set_threads(threads_count);
    let ours = decompress_ours(&mut decoder, input, capacity);
    assert_agree(&ours, &theirs, "parallel");
    let pieces = [1, 7, 97, 1000, 4096, 1 << 17];
    let shape = (
        pieces[usize::from(threads / 3) % pieces.len()],
        pieces[capacity % pieces.len()],
        // Seed 0 is exact pieces.
        capacity | 1,
    );
    streams_agree(input, threads_count, shape);
}

/// What a stream of frames gave: bytes, the end of the input in a frame, the error.
#[derive(Debug, PartialEq)]
struct Streamed {
    bytes: Vec<u8>,
    incomplete: bool,
    error: Option<String>,
    // The output reached the budget: the stream was cut there.
    cut: bool,
}

// Frames of RLE blocks are gigabytes of output: streams stop after a few megabytes.
const STREAM_BUDGET: usize = 1 << 22;

fn limited_decoder(threads: usize) -> Decoder {
    let mut decoder = Decoder::new();
    // Windows are small, so that the ring wraps and the frames allocate little.
    decoder.set_window_log_max(20);
    decoder.set_threads(threads);
    decoder
}

/// `decompress_stream` over `input` in pieces of `in_piece` bytes (varying from there by
/// `seed`) with room for `out_piece` bytes, then with an empty input until it writes nothing.
fn stream_pieces(
    threads: usize,
    input: &[u8],
    (in_piece, out_piece, seed): (usize, usize, usize),
) -> Streamed {
    let mut decoder = limited_decoder(threads);
    let mut result = Streamed {
        bytes: Vec::new(),
        incomplete: false,
        error: None,
        cut: false,
    };
    let (mut position, mut state) = (0usize, seed as u64);
    let mut buffer = vec![0; 2 * out_piece];
    while result.bytes.len() < STREAM_BUDGET {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        // From 1 to twice the piece, or the piece itself for seed 0.
        let vary = |piece: usize| {
            if seed == 0 {
                piece
            } else {
                1 + (state >> 33) as usize % (2 * piece - 1)
            }
        };
        let end = (position + vary(in_piece)).min(input.len());
        let room = vary(out_piece).min(buffer.len());
        match decoder.decompress_stream(&input[position..end], &mut buffer[..room]) {
            Err(error) => {
                result.error = Some(error.to_string());
                return result;
            }
            Ok(progress) => {
                position += progress.consumed;
                result.bytes.extend_from_slice(&buffer[..progress.written]);
                if progress.consumed + progress.written > 0 || progress.frame_done {
                    result.incomplete = !progress.frame_done;
                }
                if end == input.len() && progress.consumed + progress.written == 0 {
                    return result;
                }
            }
        }
    }
    result.cut = true;
    result
}

/// A `Reader` with `threads` over `input` in chunks of `chunk` bytes (0: one chunk), read
/// `read_size` bytes at a time.
fn read_pieces(threads: usize, input: &[u8], read_size: usize, chunk: usize) -> Streamed {
    let source: Box<dyn BufRead + '_> = if chunk == 0 {
        Box::new(input)
    } else {
        Box::new(BufReader::with_capacity(chunk, input))
    };
    let mut reader = Reader::with_decoder(limited_decoder(threads), source);
    let mut result = Streamed {
        bytes: Vec::new(),
        incomplete: false,
        error: None,
        cut: false,
    };
    let mut buffer = vec![0; read_size];
    while result.bytes.len() < STREAM_BUDGET {
        match reader.read(&mut buffer) {
            Ok(0) => return result,
            Ok(read) => result.bytes.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                result.incomplete = true;
                return result;
            }
            Err(error) => {
                result.error = Some(error.to_string());
                return result;
            }
        }
    }
    result.cut = true;
    result
}

// `threaded` has the bytes and the error of the serial stream byte by byte, which loses no byte
// before an error, or those of the serial stream with the same pieces (a frame decoded in one
// pass is not decoded as a stream), or stops earlier in the bytes when the serial code fails
// on a frame that is not in the pipeline (the call that fails loses what it wrote).
fn assert_streams_agree(
    threaded: &Streamed,
    lossless: &Streamed,
    same_pieces: &Streamed,
    raw_blocks: bool,
    what: &str,
) {
    if threaded == lossless || threaded == same_pieces {
        return;
    }
    // Raw blocks are taken in pieces, and a damaged frame after them can read ring memory
    // that the pieces left in another state: its bytes, and whether it fails, depend on the
    // size of the pieces in the serial stream too (with pieces of 1 and 1000 bytes, 46989 bytes
    // and an error, or 55223 bytes and none). Only what is valid for both is compared.
    if raw_blocks && (lossless.error.is_some() || same_pieces.error.is_some()) {
        return;
    }
    let common = threaded.bytes.len().min(lossless.bytes.len());
    assert!(
        threaded.bytes[..common] == lossless.bytes[..common],
        "{what}: bytes differ"
    );
    if threaded.cut || lossless.cut {
        return;
    }
    assert_eq!(threaded.error, lossless.error, "{what}");
    assert!(
        threaded.bytes.len() <= lossless.bytes.len(),
        "{what}: {} bytes, {} expected",
        threaded.bytes.len(),
        lossless.bytes.len()
    );
}

/// If a frame of `input` (as far as its headers parse) has a raw block of more than one byte.
fn has_raw_blocks(mut input: &[u8]) -> bool {
    while let Some(magic) = input.first_chunk::<4>() {
        if magic[0] & 0xF0 == 0x50 && magic[1..] == [0x2A, 0x4D, 0x18] {
            let Some(size) = input
                .get(4..8)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            else {
                return false;
            };
            input = input.get(8 + size as usize..).unwrap_or_default();
            continue;
        }
        let Some(&descriptor) = input.get(4) else {
            return false;
        };
        let single = descriptor >> 5 & 1;
        let content_size = match descriptor >> 6 {
            0 => single,
            flag => [0, 2, 4, 8][usize::from(flag)],
        };
        let header = 5
            + usize::from(1 - single)
            + [0, 1, 2, 4][usize::from(descriptor & 3)]
            + usize::from(content_size);
        let mut position = header;
        loop {
            let Some(&[b0, b1, b2]) = input
                .get(position..position + 3)
                .and_then(|b| b.first_chunk())
            else {
                return false;
            };
            let word = u32::from_le_bytes([b0, b1, b2, 0]);
            let size = (word >> 3) as usize;
            match word >> 1 & 3 {
                0 if size > 1 => return true,
                0 | 2 => position += 3 + size,
                1 => position += 4,
                _ => return false,
            }
            if word & 1 != 0 {
                break;
            }
        }
        input = input
            .get(position + 4 * usize::from(descriptor >> 2 & 1)..)
            .unwrap_or_default();
    }
    false
}

/// `decompress_stream` with `threads` over `input` in pieces of `shape` (input, output, seed),
/// and a `Reader`, agree with the serial stream.
fn streams_agree(input: &[u8], threads: usize, shape: (usize, usize, usize)) {
    let raw_blocks = has_raw_blocks(input);
    let lossless = stream_pieces(1, input, (1, 1 << 20, 0));
    let same_pieces = stream_pieces(1, input, shape);
    let threaded = stream_pieces(threads, input, shape);
    assert_streams_agree(
        &threaded,
        &lossless,
        &same_pieces,
        raw_blocks,
        &format!("stream, {threads} threads, {shape:?}"),
    );
    let (read_size, chunk) = (1 + shape.2 % 5000, [0, 5, 97, 1000, 4096][shape.0 % 5]);
    let same_reads = read_pieces(1, input, read_size, chunk);
    let threaded = read_pieces(threads, input, read_size, chunk);
    assert_streams_agree(
        &threaded,
        &lossless,
        &same_reads,
        raw_blocks,
        &format!("reader, {threads} threads, {read_size}, {chunk}"),
    );
}

/// 5 bytes (input and output chunk sizes, window limit), then arbitrary bytes streamed
/// through both decoders: every call must agree.
pub fn stream(data: &[u8]) {
    check_build_once();
    let Some((&[in_low, in_high, out_low, out_high, window], input)) =
        data.split_first_chunk::<5>()
    else {
        return;
    };
    // 1 to 4096 bytes, mostly small.
    let chunk = |low, high| {
        let value = u16::from_le_bytes([low, high]);
        1 + (usize::from(value >> 4) & ((1 << (value & 15)) - 1))
    };
    // The default limit, or 2^10 to 2^28 (a bigger window allocates gigabytes).
    let window_log = if window < 128 {
        0
    } else {
        10 + u32::from(window % 19)
    };
    compare_stream(
        input,
        &[chunk(in_low, in_high)],
        &[chunk(out_low, out_high)],
        window_log,
        "stream",
    );
}

/// 4 bytes of parameters, then data.
fn split_params(data: &[u8]) -> Option<(Params, &[u8])> {
    let (bytes, rest) = data.split_first_chunk::<4>()?;
    let [level, flags, window, match_length] = *bytes;
    let params = Params {
        level: LEVELS[usize::from(level) % LEVELS.len()],
        checksum: flags & 1 != 0,
        content_size: flags & 2 != 0,
        long_distance: flags & 4 != 0,
        literal_mode: [LiteralMode::Auto, LiteralMode::Huffman, LiteralMode::Raw]
            [usize::from(flags >> 3) % 3],
        target_cblock_size: if flags & 0x40 != 0 { 1500 } else { 0 },
        window_log: if flags & 0x80 != 0 {
            10 + i32::from(window % 8)
        } else {
            0
        },
        min_match: if match_length & 0x80 != 0 {
            3 + i32::from(match_length % 5)
        } else {
            0
        },
    };
    Some((params, rest))
}

/// Turns fuzz bytes into data with matches: small literal runs and copies
/// from earlier output, nearby or far.
fn expand(recipe: &[u8]) -> Vec<u8> {
    let (mut out, mut at) = (Vec::new(), 0);
    while at < recipe.len() && out.len() < MAX_DATA {
        let byte = recipe[at];
        at += 1;
        if byte < 0x80 || out.is_empty() {
            let end = (at + usize::from(byte & 7) + 1).min(recipe.len());
            out.extend_from_slice(&recipe[at..end]);
            at = end;
        } else {
            let distance = usize::from(recipe.get(at).copied().unwrap_or(0))
                * if byte & 0x40 != 0 { 251 } else { 1 };
            at += 1;
            let start = out.len() - 1 - distance.min(out.len() - 1);
            for i in 0..3 + usize::from(byte & 0x3F) * 3 {
                out.push(out[start + i]);
            }
        }
    }
    out
}

/// Data compressed by libzstd decodes to itself at the exact capacity and
/// reports `DstSizeTooSmall` one byte below, as libzstd does.
pub fn round_trip(data: &[u8]) {
    check_build_once();
    let Some((params, rest)) = split_params(data) else {
        return;
    };
    let payload = expand(rest);
    let frame = compress(&payload, &params);
    for capacity in [payload.len(), payload.len() + 7] {
        match compare_one_shot(&frame, capacity, "round trip") {
            Outcome::Ok { output, .. } => assert!(output == payload, "round trip: wrong output"),
            Outcome::Err(code) => panic!("round trip: libzstd rejects its own frame: {code:?}"),
        }
    }
    if !payload.is_empty() {
        let outcome = compare_one_shot(&frame, payload.len() - 1, "round trip, too small");
        assert!(matches!(outcome, Outcome::Err(Code::DstSizeTooSmall)));
    }
}

/// A frame from libzstd with a few bytes changed: 4 bytes of parameters, a
/// mutation count, that many (position, new byte) pairs, then data.
pub fn mutate(data: &[u8]) {
    check_build_once();
    let Some((params, rest)) = split_params(data) else {
        return;
    };
    let Some((&count, rest)) = rest.split_first() else {
        return;
    };
    let count = usize::from(count % 8);
    if rest.len() < 3 * count {
        return;
    }
    let (mutations, recipe) = rest.split_at(3 * count);
    let payload = expand(recipe);
    let mut frame = compress(&payload, &params);
    for mutation in mutations.as_chunks::<3>().0 {
        let at = usize::from(u16::from_le_bytes([mutation[0], mutation[1]])) % frame.len();
        frame[at] = mutation[2];
    }
    for capacity in [payload.len(), payload.len() + 1000, payload.len() / 2] {
        compare_one_shot(&frame, capacity, "mutate");
    }
}

/// 5 bytes (dictionary length, selector, flags, extra), then the dictionary and:
/// - flags bit 0 clear: arbitrary frame bytes, decoded with the dictionary one-shot and
///   streamed;
/// - flags bit 0 set: `extra % 8` (position, new byte) pairs, then data compressed with the
///   dictionary (level and window from the selector, ID written if flags bit 1), with those
///   bytes of the frame changed, decoded both ways.
pub fn dict(data: &[u8]) {
    check_build_once();
    let Some((&[low, high, selector, flags, extra], rest)) = data.split_first_chunk::<5>() else {
        return;
    };
    let generate = flags & 1 != 0;
    let count = if generate { usize::from(extra % 8) } else { 0 };
    if rest.len() < 3 * count {
        return;
    }
    let (mutations, rest) = rest.split_at(3 * count);
    let (dictionary, rest) =
        rest.split_at(usize::from(u16::from_le_bytes([low, high])).min(rest.len()));
    let (input, payload_len) = if generate {
        let params = Params {
            level: LEVELS[usize::from(selector) % LEVELS.len()],
            checksum: flags & 4 != 0,
            content_size: flags & 8 != 0,
            window_log: if flags & 0x10 != 0 {
                10 + i32::from(selector >> 4 & 7)
            } else {
                0
            },
            ..Params::level(3)
        };
        let payload = expand(rest);
        // Dictionaries are mostly content: ours and the encoder's checks differ in places.
        let Some(mut frame) =
            compress_with_dictionary(&payload, &params, dictionary, flags & 2 != 0)
        else {
            return;
        };
        for mutation in mutations.as_chunks::<3>().0 {
            let at = usize::from(u16::from_le_bytes([mutation[0], mutation[1]])) % frame.len();
            frame[at] = mutation[2];
        }
        (frame, payload.len())
    } else {
        (
            rest.to_vec(),
            usize::from(u16::from_le_bytes([selector, extra])),
        )
    };
    for capacity in [payload_len, payload_len + 1000, payload_len / 2] {
        if compare_dictionary(dictionary, &input, capacity, "dict").is_none() {
            return;
        }
    }
    let chunk = |byte: u8| 1usize << (byte % 13);
    let window_log = if flags & 0x20 != 0 {
        10 + u32::from(selector % 10)
    } else {
        0
    };
    compare_stream_dictionary(
        dictionary,
        &input,
        &[chunk(selector)],
        &[chunk(extra)],
        window_log,
        "dict stream",
    );
}
