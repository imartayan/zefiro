//! Known divergences from libzstd (NOTES/DIVERGENCES.md), pinned so that a
//! change on either side shows up.

use compare::*;
use rust_zstd_decoder::Decoder;

/// libzstd runs `ZSTD_decompressSequencesLong` for blocks of more than 8
/// sequences when the frame already produced over 16 MiB and the offset table
/// has many cells with over 22 extra bits. That variant checks the end of the
/// bitstream before executing its last 8 sequences, ours (the regular body)
/// after each one: a block with both a bad stream end and an execution error
/// in those sequences gets different codes. Not ported (decision of 2026-10-28).
#[test]
fn long_offset_decoder_checks_the_stream_end_before_the_last_sequences() {
    // 9 RLE blocks of 2 MB (one-shot does not limit them), then a block of 12
    // sequences of 3 bytes with offset 8388605 (code 23, no extra bits set).
    let sequence_fields: Vec<_> = (0..12).map(|_| (0, 23)).chain([(0, 1)]).collect();
    let sequences = rle_sequences(12, (0, 23, 0), &sequence_fields);
    let compressed = [raw_literals(b""), sequences].concat();
    let mut blocks: Vec<_> = (0..9)
        .map(|_| block(false, RLE_BLOCK, 2_000_000, b"a"))
        .collect();
    blocks.push(block(true, COMPRESSED_BLOCK, compressed.len(), &compressed));
    let input = frame(WINDOW_128K, false, &blocks, &[]);
    // Room for 10 sequences: the 11th does not fit, and the stream has one bit too many.
    let capacity = 18_000_000 + 30;
    let theirs = Libzstd::new().decompress(&input, capacity);
    let ours = decompress_ours(&mut Decoder::new(), &input, capacity);
    assert!(
        matches!(theirs, Outcome::Err(Code::CorruptionDetected)),
        "{theirs:?}"
    );
    assert!(
        matches!(ours, Outcome::Err(Code::DstSizeTooSmall)),
        "{ours:?}"
    );
    // With a stream that ends exactly the variants agree.
    let sequences = rle_sequences(12, (0, 23, 0), &sequence_fields[..12]);
    let compressed = [raw_literals(b""), sequences].concat();
    blocks.pop();
    blocks.push(block(true, COMPRESSED_BLOCK, compressed.len(), &compressed));
    let input = frame(WINDOW_128K, false, &blocks, &[]);
    for capacity in [18_000_036, 18_000_030] {
        let theirs = Libzstd::new().decompress(&input, capacity);
        let ours = decompress_ours(&mut Decoder::new(), &input, capacity);
        assert_agree(&ours, &theirs, "exact stream end");
    }
}
