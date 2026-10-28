#![cfg(feature = "std")]

//! `decompress` and `Reader` on hand-made frames (no oracle needed).

use std::io::{BufReader, Cursor, ErrorKind, Read};

use rust_zstd_decoder::{Error, Reader, decompress};

const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

fn block(last: bool, kind: u32, size: usize, payload: &[u8]) -> Vec<u8> {
    let header = u32::from(last) | kind << 1 | (size as u32) << 3;
    [&header.to_le_bytes()[..3], payload].concat()
}

// Raw blocks of at most `chunk` bytes. With `known_size`, a single-segment frame with a
// 4-byte content size, else a frame with a 128 KiB window and no size.
fn frame(data: &[u8], chunk: usize, known_size: bool) -> Vec<u8> {
    let header = if known_size {
        [&MAGIC[..], &[0xA0], &(data.len() as u32).to_le_bytes()].concat()
    } else {
        [&MAGIC[..], &[0, 0x38]].concat()
    };
    let pieces: Vec<_> = data.chunks(chunk).collect();
    let blocks = pieces
        .iter()
        .enumerate()
        .map(|(i, piece)| block(i + 1 == pieces.len(), 0, piece.len(), piece));
    [header, blocks.flatten().collect()].concat()
}

fn skippable(payload: &[u8]) -> Vec<u8> {
    [
        &[0x52, 0x2A, 0x4D, 0x18][..],
        &(payload.len() as u32).to_le_bytes(),
        payload,
    ]
    .concat()
}

fn read_all(input: &[u8], buffer_capacity: usize) -> std::io::Result<Vec<u8>> {
    let mut reader = Reader::new(BufReader::with_capacity(
        buffer_capacity,
        Cursor::new(input),
    ));
    let mut output = Vec::new();
    reader.read_to_end(&mut output).map(|_| output)
}

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

#[test]
fn decompress_with_known_and_unknown_sizes() {
    let (a, b) = (data(300), data(200_000));
    for known_size in [true, false] {
        let input = [
            frame(&a, 100, known_size),
            skippable(b"skip me"),
            frame(&b, 60_000, known_size),
        ]
        .concat();
        let expected = [&a[..], &b[..]].concat();
        assert_eq!(decompress(&input, expected.len()), Ok(expected.clone()));
        assert_eq!(decompress(&input, usize::MAX), Ok(expected.clone()));
        assert_eq!(
            decompress(&input, expected.len() - 1),
            Err(Error::DstSizeTooSmall)
        );
        assert_eq!(decompress(&input, 0), Err(Error::DstSizeTooSmall));
    }
}

#[test]
fn decompress_empty_and_broken_inputs() {
    assert_eq!(decompress(&[], 0), Ok(vec![]));
    assert_eq!(decompress(&skippable(b""), 0), Ok(vec![]));
    let good = frame(&data(300), 300, false);
    assert_eq!(
        decompress(&good[..good.len() - 1], 1000),
        Err(Error::SrcSizeWrong)
    );
    assert_eq!(decompress(&good[..9], 1000), Err(Error::SrcSizeWrong));
    assert_eq!(
        decompress(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 1000),
        Err(Error::PrefixUnknown)
    );
    let sized = frame(&data(300), 300, true);
    assert_eq!(
        decompress(&sized[..sized.len() - 1], 1000),
        Err(Error::SrcSizeWrong)
    );
    // A frame stating a size it does not have.
    let mut lying = sized.clone();
    lying[5..9].copy_from_slice(&400u32.to_le_bytes());
    assert_eq!(decompress(&lying, 1000), Err(Error::CorruptionDetected));
    // A huge stated size is refused before anything is allocated.
    let mut huge = vec![0x28, 0xB5, 0x2F, 0xFD, 0xE0];
    huge.extend_from_slice(&(1u64 << 40).to_le_bytes());
    huge.extend_from_slice(&block(true, 0, 0, &[]));
    assert_eq!(decompress(&huge, 1 << 20), Err(Error::DstSizeTooSmall));
}

#[test]
fn reader_reads_all_frames_through_any_buffer() {
    let (a, b) = (data(5000), data(70_000));
    let input = [
        skippable(b"x"),
        frame(&a, 700, false),
        frame(&b, 128 << 10, true),
        skippable(&[]),
        frame(&a, 1000, true),
    ]
    .concat();
    let expected = [&a[..], &b[..], &a[..]].concat();
    for capacity in [1, 7, 4096, 1 << 20] {
        assert_eq!(
            read_all(&input, capacity).unwrap(),
            expected,
            "buffer {capacity}"
        );
    }
    // Small reads.
    let mut reader = Reader::new(Cursor::new(input.clone()));
    let mut output = Vec::new();
    let mut piece = [0; 13];
    loop {
        let n = reader.read(&mut piece).unwrap();
        if n == 0 {
            break;
        }
        output.extend_from_slice(&piece[..n]);
    }
    assert_eq!(output, expected);
    assert_eq!(reader.read(&mut piece).unwrap(), 0);
    assert_eq!(reader.read(&mut []).unwrap(), 0);
}

#[test]
fn reader_reports_truncated_and_invalid_input() {
    assert_eq!(read_all(&[], 16).unwrap(), b"");
    let good = frame(&data(3000), 1000, false);
    for end in [1, 4, 6, 9, 20, good.len() - 1] {
        let error = read_all(&good[..end], 16).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnexpectedEof, "prefix {end}");
    }
    let error = read_all(&[b"not zstd".as_slice(), &[0; 8]].concat(), 16).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert_eq!(
        error.get_ref().unwrap().to_string(),
        Error::PrefixUnknown.to_string()
    );
    let trailing = [good.clone(), MAGIC[..2].to_vec()].concat();
    assert_eq!(
        read_all(&trailing, 16).unwrap_err().kind(),
        ErrorKind::UnexpectedEof
    );
    let padded = [good.clone(), vec![0; 3]].concat();
    assert_eq!(
        read_all(&padded, 16).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    // Output before the error is delivered.
    let broken = [good, MAGIC.to_vec(), vec![0x08, 0, 0]].concat();
    let mut reader = Reader::new(Cursor::new(broken));
    let mut output = vec![0; 10_000];
    assert_eq!(reader.read(&mut output).unwrap(), 3000);
}
