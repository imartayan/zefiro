//! Blocks decoded by libzstd's prefetching loop (`ZSTD_decompressSequencesLong`): over 16 MiB
//! of history and many long offsets. Fuzzers never get that far, so crafted frames and
//! large libzstd frames, intact, truncated and corrupted, are compared here.

use compare::*;
use rust_zstd_decoder::Decoder;

const LONG_WINDOW_LOG: i32 = 27;
const HISTORY: usize = 17 << 20;
const COPIED: usize = 8 << 20;

fn agree(decoder: &mut Decoder, oracle: &mut Libzstd, input: &[u8], capacity: usize, what: &str) {
    let theirs = oracle.decompress(input, capacity);
    let ours = decompress_ours(decoder, input, capacity);
    assert_agree(&ours, &theirs, &format!("{what} (capacity {capacity})"));
}

// Words of a small vocabulary, then pieces of the first 12 MiB copied from random places.
fn long_distance_data() -> Vec<u8> {
    let mut rng = Rng::with_seed(7);
    let words: Vec<Vec<u8>> = (0..2000)
        .map(|_| (0..rng.usize(3..12)).map(|_| rng.u8(b'a'..=b'z')).collect())
        .collect();
    let mut data = Vec::with_capacity(HISTORY + COPIED);
    while data.len() < HISTORY {
        data.extend_from_slice(&words[rng.usize(..words.len())]);
        data.push(b' ');
    }
    while data.len() < HISTORY + COPIED {
        let len = rng.usize(100..3000);
        let start = rng.usize(..12 << 20);
        data.extend_from_within(start..start + len);
        data.push(rng.u8(..));
    }
    data
}

/// The first 9 blocks make 18 MB of history, then a block of 12 sequences of 3 bytes with
/// offset code 23 (8388605, no extra bit set): libzstd runs its prefetching loop, which
/// checks the stream end before executing the last 8 sequences.
#[test]
fn crafted_block_with_long_offsets() {
    let sequence_fields: Vec<_> = (0..12).map(|_| (0, 23)).chain([(0, 1)]).collect();
    let mut blocks: Vec<_> = (0..9)
        .map(|_| block(false, RLE_BLOCK, 2_000_000, b"a"))
        .collect();
    let mut decoder = Decoder::new();
    let mut oracle = Libzstd::new();
    for (fields, what) in [
        (&sequence_fields[..], "one bit too many"),
        (&sequence_fields[..12], "exact stream end"),
        (&sequence_fields[..11], "missing sequence"),
    ] {
        let sequences = rle_sequences(12, (0, 23, 0), fields);
        let compressed = [raw_literals(b""), sequences].concat();
        blocks.truncate(9);
        blocks.push(block(true, COMPRESSED_BLOCK, compressed.len(), &compressed));
        let input = frame(WINDOW_128K, false, &blocks, &[]);
        for capacity in (18_000_000..18_000_050).chain([20_000_000]) {
            agree(&mut decoder, &mut oracle, &input, capacity, what);
        }
    }
    // One bit too many and no room for the 11th sequence: the stream end comes first.
    let sequences = rle_sequences(12, (0, 23, 0), &sequence_fields);
    let compressed = [raw_literals(b""), sequences].concat();
    blocks.truncate(9);
    blocks.push(block(true, COMPRESSED_BLOCK, compressed.len(), &compressed));
    let input = frame(WINDOW_128K, false, &blocks, &[]);
    let theirs = oracle.decompress(&input, 18_000_030);
    let ours = decompress_ours(&mut decoder, &input, 18_000_030);
    assert!(matches!(theirs, Outcome::Err(Code::CorruptionDetected)));
    assert!(matches!(ours, Outcome::Err(Code::CorruptionDetected)));
}

/// Real frames with window log 27 (`--long=27`), by libzstd: intact, truncated, corrupted.
#[test]
fn large_window_frames() {
    // Compressing 25 MB takes too long in unoptimized builds.
    if cfg!(debug_assertions) {
        return;
    }
    let data = long_distance_data();
    let mut decoder = Decoder::new();
    let mut oracle = Libzstd::new();
    let mut rng = Rng::with_seed(11);
    let settings = [(3, true), (1, false)];
    for (level, long_distance) in settings {
        let what = format!("level {level}, long distance {long_distance}");
        let params = Params {
            window_log: LONG_WINDOW_LOG,
            long_distance,
            checksum: true,
            ..Params::level(level)
        };
        let input = compress(&data, &params);
        let outcome = oracle.decompress(&input, data.len());
        assert!(
            matches!(&outcome, Outcome::Ok { output, .. } if *output == data),
            "{what}: libzstd"
        );
        for capacity in [
            data.len(),
            data.len() + 1000,
            data.len() - 1,
            data.len() - 5000,
        ] {
            agree(&mut decoder, &mut oracle, &input, capacity, &what);
        }
        // Prefixes: in the history, around 16 MiB of output, and the last bytes.
        for cut in (1..40)
            .map(|i| input.len() / 40 * i)
            .chain(input.len() - 8..input.len())
        {
            agree(&mut decoder, &mut oracle, &input[..cut], data.len(), &what);
        }
        // Flipped bytes, mostly past the point where the long offsets start.
        for _ in 0..40 {
            let mut corrupted = input.clone();
            let position = rng.usize(input.len() / 2..input.len());
            corrupted[position] ^= 1 << rng.u32(..8);
            agree(&mut decoder, &mut oracle, &corrupted, data.len(), &what);
            agree(&mut decoder, &mut oracle, &corrupted, data.len() / 2, &what);
        }
    }
}
