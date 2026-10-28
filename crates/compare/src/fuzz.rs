//! Bodies of the fuzz targets, shared with `tests/regressions.rs` which replays
//! saved inputs through the same functions.

use std::cell::RefCell;
use std::sync::Once;

use rust_zstd_decoder::Decoder;

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
