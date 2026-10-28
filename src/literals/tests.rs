use alloc::vec;
use alloc::vec::Vec;

use super::*;
use crate::testing::{Rng, TestTree, assert_same, huffman_section, raw_section, rle_section};

const BLOCK_MAX: usize = 128 << 10;

// The section followed by an empty sequences section, as in a block.
fn decode(state: &mut LiteralsState, section: &[u8]) -> Result<(Vec<u8>, usize), Error> {
    let block = [section, &[0]].concat();
    let (literals, size) = state.decode(&block, BLOCK_MAX, BLOCK_MAX)?;
    Ok((literals.bytes().to_vec(), size))
}

#[test]
fn raw_literals_are_read_in_place_when_32_bytes_follow() {
    let mut state = LiteralsState::new();
    let data: Vec<u8> = (0..100).collect();
    for (extra, in_place) in [(0, false), (31, false), (32, true), (100, true)] {
        let src = [raw_section(&data), vec![7; extra]].concat();
        let (literals, size) = state.decode(&src, BLOCK_MAX, BLOCK_MAX).unwrap();
        assert_eq!((literals.bytes(), size), (&data[..], 102));
        assert_eq!(
            literals.padded.as_ptr() == src[2..].as_ptr(),
            in_place,
            "{extra} bytes after"
        );
        assert!(literals.padded.len() >= 100 + WILDCOPY_OVERLENGTH);
    }
}

#[test]
fn raw_and_rle_size_formats() {
    let mut state = LiteralsState::new();
    let mut rng = Rng::with_seed(5);
    for len in [0, 1, 31, 32, 33, 4095, 4096, 100_000, BLOCK_MAX] {
        let data: Vec<u8> = (0..len).map(|_| rng.u8(..)).collect();
        let section = raw_section(&data);
        let (decoded, size) = decode(&mut state, &section).unwrap();
        assert!(decoded == data && size == section.len(), "raw {len}");
        let section = rle_section(0xAB, len);
        let (decoded, size) = decode(&mut state, &section).unwrap();
        assert!(
            decoded == vec![0xAB; len] && size == section.len(),
            "rle {len}"
        );
    }
}

#[test]
fn huffman_literals_round_trip_in_every_size_format() {
    let mut rng = Rng::with_seed(0xB10C);
    let mut state = LiteralsState::new();
    let (mut formats, mut depths) = ([0; 4], [0; 13]);
    for _ in 0..600 {
        let max_depth = rng.choice([3, 8, 11, 12]).unwrap();
        let tree = TestTree::random(&mut rng, max_depth);
        let size_format = rng.usize(..4) as u8;
        let len = match size_format {
            0 => 1 + rng.usize(..1023),
            1 => 6 + rng.usize(..1017),
            2 => 6 + rng.usize(..16_000),
            _ => 6 + rng.usize(..BLOCK_MAX - 6),
        };
        let data = tree.random_data(&mut rng, len);
        let streams = if size_format == 0 {
            tree.encode_stream(&data)
        } else {
            tree.encode_four_streams(&data)
        };
        let payload = [tree.header.clone(), streams.clone()].concat();
        let limit = [1023, 1023, 16_383, (1 << 18) - 1][size_format as usize];
        if payload.len() > limit {
            continue;
        }
        let section = huffman_section(2, size_format, len, &payload);
        let (decoded, size) = decode(&mut state, &section).unwrap();
        assert!(decoded == data && size == section.len());
        // The same tree repeated, with stale padding after it.
        let repeated = huffman_section(3, size_format, len, &streams);
        let (decoded, size) = decode(&mut state, &[repeated.clone(), vec![9; 3]].concat()).unwrap();
        assert!(decoded == data && size == repeated.len());
        formats[size_format as usize] += 1;
        depths[tree.depth as usize] += 1;
    }
    assert!(formats.iter().all(|&n| n > 40), "{formats:?}");
    assert!(
        depths[12] > 5 && depths[11] > 5 && depths[2..8].iter().all(|&n| n > 0),
        "{depths:?}"
    );
}

#[test]
fn treeless_literals_need_a_table_from_the_same_frame() {
    let mut rng = Rng::with_seed(77);
    let mut state = LiteralsState::new();
    let tree = TestTree::random(&mut rng, 9);
    let data = tree.random_data(&mut rng, 300);
    let streams = tree.encode_four_streams(&data);
    let treeless = huffman_section(3, 1, 300, &streams);
    let first = huffman_section(2, 1, 300, &[tree.header.clone(), streams].concat());
    assert_eq!(
        decode(&mut state, &treeless).unwrap_err(),
        Error::DictionaryCorrupted
    );
    // Raw and RLE blocks do not provide a table.
    decode(&mut state, &raw_section(b"abc")).unwrap();
    assert_eq!(
        decode(&mut state, &treeless).unwrap_err(),
        Error::DictionaryCorrupted
    );
    decode(&mut state, &first).unwrap();
    assert_same(&decode(&mut state, &treeless).unwrap().0, &data, "treeless");
    state.start_frame();
    assert_eq!(
        decode(&mut state, &treeless).unwrap_err(),
        Error::DictionaryCorrupted
    );
}

#[test]
fn checks_come_in_libzstds_order() {
    let mut rng = Rng::with_seed(99);
    let tree = TestTree::random(&mut rng, 8);
    let data = tree.random_data(&mut rng, 50);
    let payload = [tree.header.clone(), tree.encode_four_streams(&data)].concat();
    let section = huffman_section(2, 1, 50, &payload);
    let mut state = LiteralsState::new();
    let mut run = |src: &[u8], capacity: usize, block_max: usize| {
        state
            .decode(src, capacity, block_max)
            .map(|(literals, size)| (literals.len, size))
    };
    assert_eq!(run(&section, 50, BLOCK_MAX), Ok((50, section.len())));
    // Input shorter than any section, then than a compressed header.
    assert_eq!(run(&[0], 100, BLOCK_MAX), Err(Error::CorruptionDetected));
    assert_eq!(
        run(&section[..4], 100, BLOCK_MAX),
        Err(Error::CorruptionDetected)
    );
    // Declared sizes: over the block limit (even with no room), compressed size past the input.
    assert_eq!(run(&section, 10, 49), Err(Error::CorruptionDetected));
    assert_eq!(
        run(&section[..section.len() - 1], 100, BLOCK_MAX),
        Err(Error::CorruptionDetected)
    );
    // Not enough room for the literals comes after the input checks.
    assert_eq!(run(&section, 49, BLOCK_MAX), Err(Error::DstSizeTooSmall));
    assert_eq!(
        run(&section[..section.len() - 1], 49, BLOCK_MAX),
        Err(Error::CorruptionDetected)
    );
    // Four streams for fewer than 6 literals; one stream may have any count.
    let few = huffman_section(2, 1, 5, &payload);
    assert_eq!(run(&few, 100, BLOCK_MAX), Err(Error::LiteralsHeaderWrong));
    let empty = huffman_section(
        2,
        0,
        0,
        &[tree.header.clone(), tree.encode_stream(&[])].concat(),
    );
    assert_eq!(run(&empty, 0, BLOCK_MAX), Ok((0, empty.len())));
    // Raw: a stated size past the input, then past the room.
    let raw = raw_section(&[1; 40]);
    assert_eq!(
        run(&raw[..30], 100, BLOCK_MAX),
        Err(Error::CorruptionDetected)
    );
    assert_eq!(run(&raw, 39, BLOCK_MAX), Err(Error::DstSizeTooSmall));
    assert_eq!(run(&raw[..30], 39, BLOCK_MAX), Err(Error::DstSizeTooSmall));
    // RLE needs its byte, and its 2 or 3 byte headers.
    assert_eq!(
        run(&[0x1 | 0x4, 0x10], 100, BLOCK_MAX),
        Err(Error::CorruptionDetected)
    );
    assert_eq!(
        run(&[0x1 | 0xC, 0x10, 0], 100, BLOCK_MAX),
        Err(Error::CorruptionDetected)
    );
    assert_eq!(
        run(&[0x1 | 0xC | 0x10, 0, 0, 5], 100, BLOCK_MAX),
        Ok((1, 4))
    );
}

#[test]
fn corrupted_streams_are_corruption() {
    let mut rng = Rng::with_seed(123);
    let mut state = LiteralsState::new();
    let mut seen = [0; 2];
    for _ in 0..300 {
        let tree = TestTree::random(&mut rng, 11);
        let data = tree.random_data(&mut rng, 200);
        let mut payload = [tree.header.clone(), tree.encode_four_streams(&data)].concat();
        let section_of = |payload: &[u8]| huffman_section(2, 1, 200, payload);
        assert_same(
            &decode(&mut state, &section_of(&payload)).unwrap().0,
            &data,
            "intact",
        );
        let at = rng.usize(..payload.len());
        payload[at] ^= 1 << rng.usize(..8);
        match decode(&mut state, &section_of(&payload)) {
            Ok(_) => seen[0] += 1,
            Err(error) => {
                assert_eq!(error, Error::CorruptionDetected);
                seen[1] += 1;
            }
        }
    }
    assert!(seen.iter().all(|&n| n > 20), "{seen:?}");
}

#[test]
fn buffer_grows_by_doubling_and_never_shrinks() {
    let mut state = LiteralsState::new();
    let mut sizes = Vec::new();
    for len in [10, 100, 120, 1000, 1500, 500, 60_000, 60_001, 100] {
        decode(&mut state, &rle_section(1, len)).unwrap();
        sizes.push(state.buffer.len());
    }
    assert_eq!(sizes[0], 10 + WILDCOPY_OVERLENGTH);
    assert!(sizes.windows(2).all(|pair| pair[1] >= pair[0]));
    assert!(
        sizes
            .iter()
            .zip([10, 100, 120, 1000, 1500, 500, 60_000, 60_001, 100])
            .all(|(&s, len)| s >= len + 32)
    );
    // Growth is max(needed, 2 * current).
    assert_eq!(sizes[..3], [42, 132, 264]);
}
