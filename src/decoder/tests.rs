use alloc::vec;
use alloc::vec::Vec;

use super::*;
use crate::testing::{Rng, TestTree, huffman_section, raw_section, rle_section};

const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const RAW: u32 = 0;
const RLE: u32 = 1;
const COMPRESSED: u32 = 2;

fn block(last: bool, kind: u32, size: usize, payload: &[u8]) -> Vec<u8> {
    let header = u32::from(last) | kind << 1 | (size as u32) << 3;
    [&header.to_le_bytes()[..3], payload].concat()
}

// Single-segment frame with a 1-byte content size.
fn frame(content_size: u8, checksum: bool, blocks: &[Vec<u8>], tail: &[u8]) -> Vec<u8> {
    let descriptor = 0x20 | u8::from(checksum) << 2;
    [
        &MAGIC[..],
        &[descriptor, content_size],
        &blocks.concat(),
        tail,
    ]
    .concat()
}

fn decode(input: &[u8], capacity: usize) -> Result<Vec<u8>, Error> {
    let mut output = vec![0xAA; capacity];
    let written = Decoder::new().decompress_into(input, &mut output)?;
    output.truncate(written);
    Ok(output)
}

fn checksum_of(data: &[u8]) -> [u8; 4] {
    let mut hasher = XxHash64::with_seed(0);
    hasher.write(data);
    (hasher.finish() as u32).to_le_bytes()
}

#[test]
fn raw_and_rle_blocks() {
    let input = frame(
        8,
        false,
        &[
            block(false, RAW, 3, b"abc"),
            block(false, RLE, 4, b"z"),
            block(true, RAW, 1, b"!"),
        ],
        &[],
    );
    assert_eq!(decode(&input, 8).unwrap(), b"abczzzz!");
    assert_eq!(decode(&input, 100).unwrap(), b"abczzzz!");
    assert_eq!(decode(&input, 7), Err(Error::DstSizeTooSmall));
    assert_eq!(decode(&input, 0), Err(Error::DstSizeTooSmall));
}

#[test]
fn empty_inputs_and_outputs() {
    assert_eq!(decode(&[], 0), Ok(vec![]));
    assert_eq!(
        decode(&frame(0, false, &[block(true, RAW, 0, &[])], &[]), 0),
        Ok(vec![])
    );
    assert_eq!(
        decode(&frame(0, false, &[block(true, RLE, 0, b"x")], &[]), 0),
        Ok(vec![])
    );
}

#[test]
fn checksum_is_verified_after_the_content_size() {
    let good = frame(
        3,
        true,
        &[block(true, RAW, 3, b"abc")],
        &checksum_of(b"abc"),
    );
    assert_eq!(decode(&good, 3).unwrap(), b"abc");
    let mut bad = good.clone();
    *bad.last_mut().unwrap() ^= 1;
    assert_eq!(decode(&bad, 3), Err(Error::ChecksumWrong));
    for cut in 1..=4 {
        assert_eq!(
            decode(&good[..good.len() - cut], 3),
            Err(Error::ChecksumWrong),
            "cut {cut}"
        );
    }
    // Wrong content size and wrong checksum: the size is reported.
    let both = frame(4, true, &[block(true, RAW, 3, b"abc")], &[0; 4]);
    assert_eq!(decode(&both, 3), Err(Error::CorruptionDetected));
}

#[test]
fn checksum_covers_every_block() {
    let blocks = [block(false, RAW, 40, &[7; 40]), block(true, RLE, 60, b"q")];
    let content = [&[7u8; 40][..], &[b'q'; 60]].concat();
    let input = frame(100, true, &blocks, &checksum_of(&content));
    assert_eq!(decode(&input, 100).unwrap(), content);
}

#[test]
fn frames_and_skippable_frames_concatenate() {
    let skippable = [
        &0x184D_2A53u32.to_le_bytes()[..],
        &2u32.to_le_bytes(),
        &[1, 2],
    ]
    .concat();
    let a = frame(2, false, &[block(true, RAW, 2, b"ab")], &[]);
    let b = frame(2, false, &[block(true, RLE, 2, b"c")], &[]);
    let input = [&skippable[..], &a, &skippable, &skippable, &b, &skippable].concat();
    assert_eq!(decode(&input, 4).unwrap(), b"abcc");
    assert_eq!(decode(&input, 3), Err(Error::DstSizeTooSmall));
    assert_eq!(decode(&skippable, 0), Ok(vec![]));
}

#[test]
fn trailing_bytes() {
    let a = frame(2, false, &[block(true, RAW, 2, b"ab")], &[]);
    // Fewer than 5 bytes cannot start a frame.
    assert_eq!(
        decode(&[a.clone(), vec![0; 4]].concat(), 2),
        Err(Error::SrcSizeWrong)
    );
    // After a good frame a bad magic is a size error, not a prefix error.
    assert_eq!(
        decode(&[a, vec![0; 9]].concat(), 2),
        Err(Error::SrcSizeWrong)
    );
    assert_eq!(decode(&[0; 9], 0), Err(Error::PrefixUnknown));
    assert_eq!(decode(&[0; 8], 0), Err(Error::SrcSizeWrong));
    assert_eq!(decode(&[0; 4], 0), Err(Error::SrcSizeWrong));
    // Truncated skippable frame.
    let skippable = [
        &0x184D_2A50u32.to_le_bytes()[..],
        &5u32.to_le_bytes(),
        &[1, 2],
    ]
    .concat();
    assert_eq!(decode(&skippable, 0), Err(Error::SrcSizeWrong));
    assert_eq!(decode(&skippable[..6], 0), Err(Error::SrcSizeWrong));
}

#[test]
fn malformed_frames() {
    let raw = block(true, RAW, 2, b"ab");
    // Header length and content size.
    assert_eq!(
        decode(&frame(3, false, core::slice::from_ref(&raw), &[]), 8),
        Err(Error::CorruptionDetected)
    );
    assert_eq!(
        decode(&frame(1, false, core::slice::from_ref(&raw), &[]), 8),
        Err(Error::CorruptionDetected)
    );
    // Reserved bit, dictionary id, reserved block type.
    assert_eq!(
        decode(&[&MAGIC[..], &[0x28, 2], &raw].concat(), 8),
        Err(Error::FrameParameterUnsupported)
    );
    assert_eq!(
        decode(&[&MAGIC[..], &[0x21, 7, 2], &raw].concat(), 8),
        Err(Error::DictionaryWrong)
    );
    assert_eq!(
        decode(&frame(2, false, &[block(true, 3, 0, &[])], &[]), 8),
        Err(Error::CorruptionDetected)
    );
    // Window too large.
    assert_eq!(
        decode(&[&MAGIC[..], &[0, 0xB0], &raw].concat(), 8),
        Err(Error::WindowTooLarge)
    );
    // Missing last block, block longer than the input, missing RLE byte.
    assert_eq!(
        decode(&frame(2, false, &[block(false, RAW, 2, b"ab")], &[]), 8),
        Err(Error::SrcSizeWrong)
    );
    assert_eq!(
        decode(&frame(2, false, &[block(true, RAW, 9, b"ab")], &[]), 8),
        Err(Error::SrcSizeWrong)
    );
    assert_eq!(
        decode(&frame(2, false, &[block(true, RLE, 2, &[])], &[]), 8),
        Err(Error::SrcSizeWrong)
    );
    // Too short for a header plus a block header.
    assert_eq!(
        decode(&[&MAGIC[..], &[0x20, 0, 1, 0]].concat(), 8),
        Err(Error::SrcSizeWrong)
    );
}

// Frame without content size and a window (so block size limit) of 1 KiB.
fn windowed_frame(checksum: bool, blocks: &[Vec<u8>]) -> Vec<u8> {
    [
        &MAGIC[..],
        &[u8::from(checksum) << 2, 0x00],
        &blocks.concat(),
    ]
    .concat()
}

fn compressed(last: bool, literals: &[u8], sequences: &[u8]) -> Vec<u8> {
    let payload = [literals, sequences].concat();
    block(last, COMPRESSED, payload.len(), &payload)
}

#[test]
fn compressed_blocks_without_sequences_are_their_literals() {
    let mut rng = Rng::with_seed(11);
    let tree = TestTree::random(&mut rng, 10);
    let data = tree.random_data(&mut rng, 300);
    let streams = tree.encode_four_streams(&data);
    let huffman = huffman_section(2, 1, 300, &[tree.header.clone(), streams.clone()].concat());
    let repeated = huffman_section(3, 1, 300, &streams);
    let input = windowed_frame(
        true,
        &[
            compressed(false, &raw_section(b"hello "), &[0]),
            compressed(false, &rle_section(b'!', 5), &[0]),
            compressed(false, &huffman, &[0]),
            // The table of the previous block, repeated.
            compressed(true, &repeated, &[0]),
        ],
    );
    let expected = [&b"hello !!!!!"[..], &data, &data].concat();
    let input = [input, checksum_of(&expected).to_vec()].concat();
    assert_eq!(decode(&input, expected.len()).unwrap(), expected);
    assert_eq!(
        decode(&input, expected.len() - 1),
        Err(Error::DstSizeTooSmall)
    );
    // The next frame starts without a table.
    let treeless = windowed_frame(false, &[compressed(true, &repeated, &[0])]);
    let mut decoder = Decoder::new();
    let mut output = vec![0; 1000];
    assert!(decoder.decompress_into(&input, &mut output).is_ok());
    assert_eq!(
        decoder.decompress_into(&treeless, &mut output),
        Err(Error::DictionaryCorrupted)
    );
}

#[test]
fn compressed_block_sequences_section_checks() {
    let literals = raw_section(b"abc");
    let run = |sequences: &[u8]| {
        decode(
            &windowed_frame(false, &[compressed(true, &literals, sequences)]),
            100,
        )
    };
    assert_eq!(run(&[0]).unwrap(), b"abc");
    // Nothing after the literals, bytes after a zero count.
    assert_eq!(run(&[]), Err(Error::SrcSizeWrong));
    assert_eq!(run(&[0, 0]), Err(Error::CorruptionDetected));
    // One sequence with a stream that has no end mark.
    assert_eq!(run(&[1, 0, 0]), Err(Error::CorruptionDetected));
    assert_eq!(run(&[1]), Err(Error::SrcSizeWrong));
}

#[test]
fn compressed_block_limits() {
    // A compressed block over the block size limit (here the 1 KiB window)
    // is `srcSize_wrong`, whatever it holds; raw literals bigger than the output room are too small.
    let big = raw_section(&[7; 1100]);
    let run = |literals: &[u8], capacity: usize| {
        decode(
            &windowed_frame(false, &[compressed(true, literals, &[0])]),
            capacity,
        )
    };
    assert_eq!(run(&big, 2000), Err(Error::SrcSizeWrong));
    let fits = raw_section(&[7; 1000]);
    assert_eq!(run(&fits, 1000).unwrap(), vec![7; 1000]);
    assert_eq!(run(&fits, 999), Err(Error::DstSizeTooSmall));
    // Literals past the block limit within a block that is not.
    let rle = rle_section(7, 1025);
    assert_eq!(run(&rle, 2000), Err(Error::CorruptionDetected));
}

#[test]
fn rle_and_raw_blocks_may_exceed_the_block_size_limit() {
    // libzstd checks that against the window only when streaming.
    let size = 1 << 20;
    let input = frame(0, false, &[block(true, RLE, size, b"x")], &[]);
    let input = [&input[..4], &[0x00, 0x00], &input[6..]].concat();
    assert_eq!(decode(&input, size).unwrap(), vec![b'x'; size]);
}

#[test]
fn decoder_is_reusable() {
    let mut decoder = Decoder::new();
    let with_checksum = frame(
        3,
        true,
        &[block(true, RAW, 3, b"abc")],
        &checksum_of(b"abc"),
    );
    let without = frame(3, false, &[block(true, RAW, 3, b"xyz")], &[]);
    let mut output = [0; 3];
    for input in [&with_checksum, &without, &with_checksum] {
        assert_eq!(decoder.decompress_into(input, &mut output), Ok(3));
    }
    assert_eq!(&output, b"abc");
}

fn decode_vec(input: &[u8], mut output: Vec<u8>) -> Result<Vec<u8>, Error> {
    Decoder::new().decompress_into_vec(input, &mut output)?;
    Ok(output)
}

#[test]
fn vec_output_grows_for_every_frame() {
    let a = frame(2, false, &[block(true, RAW, 2, b"ab")], &[]);
    let b = frame(2, false, &[block(true, RLE, 2, b"c")], &[]);
    let unsized_frame = windowed_frame(false, &[block(true, RAW, 3, b"xyz")]);
    let input = [&a[..], &b, &unsized_frame].concat();
    assert_eq!(decode_vec(&input, Vec::new()).unwrap(), b"abccxyz");
    // A reused `Vec` with some capacity, and content to replace.
    assert_eq!(
        decode_vec(&input, Vec::with_capacity(1)).unwrap(),
        b"abccxyz"
    );
    assert_eq!(decode_vec(&a, b"old content".to_vec()).unwrap(), b"ab");
}

#[test]
fn vec_output_ignores_a_content_size_above_the_blocks() {
    // 8-byte content size of `u64::MAX - 1` (`u64::MAX` is "unknown"), one raw block of 2 bytes.
    let header = [&MAGIC[..], &[0xC0, 0x00], &(u64::MAX - 1).to_le_bytes()].concat();
    let input = [&header[..], &block(true, RAW, 2, b"ab")].concat();
    let mut output = Vec::new();
    let result = Decoder::new().decompress_into_vec(&input, &mut output);
    assert_eq!(result, Err(Error::CorruptionDetected));
    assert!(output.capacity() < 1 << 20);
}

// A decoder with threads can move to another thread, and be shared.
#[test]
fn a_decoder_and_a_reader_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Decoder>();
    #[cfg(feature = "std")]
    assert_send_sync::<crate::Reader<&[u8]>>();
}
