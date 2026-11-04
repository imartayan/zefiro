#![cfg(feature = "std")]

//! `decompress` and `Reader` on hand-made frames (no oracle needed).

use std::io::{BufReader, Cursor, ErrorKind, Read};

use zefiro::{Decoder, Error, FrameInfo, Reader, decompress, frame_info};

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
        assert_eq!(decompress(&input, expected.len(), 1), Ok(expected.clone()));
        assert_eq!(decompress(&input, usize::MAX, 1), Ok(expected.clone()));
        assert_eq!(
            decompress(&input, expected.len() - 1, 1),
            Err(Error::DstSizeTooSmall)
        );
        assert_eq!(decompress(&input, 0, 1), Err(Error::DstSizeTooSmall));
    }
}

#[test]
fn decompress_empty_and_broken_inputs() {
    assert_eq!(decompress(&[], 0, 1), Ok(vec![]));
    assert_eq!(decompress(&skippable(b""), 0, 1), Ok(vec![]));
    let good = frame(&data(300), 300, false);
    assert_eq!(
        decompress(&good[..good.len() - 1], 1000, 1),
        Err(Error::SrcSizeWrong)
    );
    assert_eq!(decompress(&good[..9], 1000, 1), Err(Error::SrcSizeWrong));
    assert_eq!(
        decompress(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 1000, 1),
        Err(Error::PrefixUnknown)
    );
    let legacy_v07 = [0x27, 0xB5, 0x2F, 0xFD, 0, 0, 0, 0, 0, 0];
    assert_eq!(
        decompress(&legacy_v07, 1000, 1),
        Err(Error::LegacyFormatUnsupported(7))
    );
    assert_eq!(
        Error::LegacyFormatUnsupported(7).to_string(),
        "unsupported legacy zstd format (v0.7)"
    );
    let sized = frame(&data(300), 300, true);
    assert_eq!(
        decompress(&sized[..sized.len() - 1], 1000, 1),
        Err(Error::SrcSizeWrong)
    );
    // A frame stating a size it does not have.
    let mut lying = sized;
    lying[5..9].copy_from_slice(&400u32.to_le_bytes());
    assert_eq!(decompress(&lying, 1000, 1), Err(Error::CorruptionDetected));
    // A huge stated size is refused before anything is allocated.
    let mut huge = vec![0x28, 0xB5, 0x2F, 0xFD, 0xE0];
    huge.extend_from_slice(&(1u64 << 40).to_le_bytes());
    huge.extend_from_slice(&block(true, 0, 0, &[]));
    assert_eq!(decompress(&huge, 1 << 20, 1), Err(Error::DstSizeTooSmall));
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
    let mut reader = Reader::new(Cursor::new(input));
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
    let legacy_error = read_all(&[0x27, 0xB5, 0x2F, 0xFD, 0, 0, 0, 0, 0, 0], 16).unwrap_err();
    assert_eq!(legacy_error.kind(), ErrorKind::InvalidData);
    assert_eq!(
        legacy_error.get_ref().unwrap().to_string(),
        "unsupported legacy zstd format (v0.7)"
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

#[test]
fn reader_with_decoder_applies_the_window_limit() {
    // Window descriptor 0x58: 1 << 21 bytes.
    let big_window = [&MAGIC[..], &[0, 0x58], &block(true, 0, 3, b"abc")].concat();
    let reader = |log| {
        let mut decoder = Decoder::new();
        decoder.set_window_log_max(log);
        Reader::with_decoder(decoder, Cursor::new(&big_window))
    };
    let mut output = Vec::new();
    reader(21).read_to_end(&mut output).unwrap();
    assert_eq!(output, b"abc");
    let error = reader(20).read_to_end(&mut output).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert_eq!(
        error.get_ref().unwrap().to_string(),
        Error::WindowTooLarge.to_string()
    );
}

#[test]
fn reader_without_a_dictionary_refuses_a_frame_that_needs_one() {
    // Dictionary ID flag 1 (one byte), ID 7.
    let frame = [&MAGIC[..], &[0x01, 0x38, 7], &block(true, 0, 0, b"")].concat();
    let error = read_all(&frame, 16).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert_eq!(
        error.get_ref().unwrap().to_string(),
        Error::DictionaryWrong.to_string()
    );
}

#[test]
fn reader_with_decoder_abandons_a_stream_in_progress() {
    let input = frame(&data(3000), 1000, false);
    let mut decoder = Decoder::new();
    decoder
        .decompress_stream(&input[..50], &mut [0; 10])
        .unwrap();
    let mut reader = Reader::with_decoder(decoder, Cursor::new(&input));
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    assert_eq!(output, data(3000));
}

#[test]
fn frame_info_inspects_frames_and_skippable_frames() {
    let d = data(3000);
    let sized = frame(&d, 1000, true);
    let info = frame_info(&sized).unwrap();
    assert_eq!(
        info,
        FrameInfo {
            compressed_len: sized.len(),
            content_len: Some(3000),
            max_uncompressed_len: 3000,
            is_skippable: false,
        }
    );

    let unsized_frame = frame(&d, 1000, false);
    let info = frame_info(&unsized_frame).unwrap();
    assert_eq!(info.compressed_len, unsized_frame.len());
    assert_eq!(info.content_len, None);
    assert!(info.max_uncompressed_len >= 3000);
    assert!(!info.is_skippable);

    let skip = skippable(b"hello world");
    let info = frame_info(&skip).unwrap();
    assert_eq!(
        info,
        FrameInfo {
            compressed_len: skip.len(),
            content_len: Some(0),
            max_uncompressed_len: 0,
            is_skippable: true,
        }
    );

    // Multi-frame iteration via frame_info.
    let multi = [sized.clone(), skip.clone(), unsized_frame.clone()].concat();
    let mut rest = &multi[..];
    let mut frames = Vec::new();
    while !rest.is_empty() {
        let info = frame_info(rest).unwrap();
        frames.push(info);
        rest = &rest[info.compressed_len..];
    }
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].compressed_len, sized.len());
    assert_eq!(frames[1].compressed_len, skip.len());
    assert_eq!(frames[2].compressed_len, unsized_frame.len());
}

#[test]
fn decompress_into_vec_and_uninit() {
    let d = data(15_000);
    let input = frame(&d, 5000, true);
    let mut decoder = Decoder::new();

    // 1. Fresh vector with zero capacity.
    let mut out = Vec::new();
    let written = decoder.decompress_into_vec(&input, &mut out).unwrap();
    assert_eq!(written, d.len());
    assert_eq!(out, d);

    // 2. Pre-allocated vector reused without reallocation.
    let cap_before = out.capacity();
    let written = decoder.decompress_into_vec(&input, &mut out).unwrap();
    assert_eq!(written, d.len());
    assert_eq!(out, d);
    assert_eq!(out.capacity(), cap_before);

    // 3. Decompress into uninitialized slice.
    let mut uninit_buf = vec![core::mem::MaybeUninit::<u8>::uninit(); d.len() + 32];
    let written = decoder
        .decompress_into_uninit(&input, &mut uninit_buf)
        .unwrap();
    assert_eq!(written, d.len());
    // SAFETY: exactly `written` bytes were initialized by decompress_into_uninit.
    let init_slice =
        unsafe { core::slice::from_raw_parts(uninit_buf.as_ptr().cast::<u8>(), written) };
    assert_eq!(init_slice, &d[..]);

    // 4. Vector with insufficient capacity fails safely and leaves length 0.
    let mut small_out = Vec::with_capacity(10);
    let err = decoder.decompress_into_uninit(&input, small_out.spare_capacity_mut());
    assert_eq!(err, Err(Error::DstSizeTooSmall));
    assert_eq!(small_out.len(), 0);
}

#[test]
fn decoder_reset_cleans_streaming_state_and_preserves_dictionaries() {
    let d1 = data(4000);
    let d2 = data(2000);
    let frame1 = frame(&d1, 1000, false);
    let frame2 = frame(&d2, 1000, true);

    let mut decoder = Decoder::new();
    // Partially decode frame1.
    let mut piece = [0u8; 500];
    let progress = decoder.decompress_stream(&frame1, &mut piece).unwrap();
    assert!(!progress.frame_done);
    assert!(progress.written > 0);

    // Reset decoder and decode frame2 cleanly.
    decoder.reset();
    let mut out = vec![0u8; 2000];
    let progress = decoder.decompress_stream(&frame2, &mut out).unwrap();
    assert!(progress.frame_done);
    assert_eq!(&out[..progress.written], &d2[..]);
}

#[test]
fn verify_checksum_bypass() {
    use std::hash::Hasher;
    use twox_hash::XxHash64;

    let d = data(1000);
    let mut hasher = XxHash64::with_seed(0);
    hasher.write(&d);
    let checksum = (hasher.finish() as u32).to_le_bytes();

    // Frame with checksum flag (descriptor byte 0xA4: 4-byte size (bit 7-6) + single segment (bit 5) + checksum (bit 2)).
    let header = [&MAGIC[..], &[0xA4], &(d.len() as u32).to_le_bytes()].concat();
    let blk = block(true, 0, d.len(), &d);
    let mut input = [header, blk, checksum.to_vec()].concat();

    let mut decoder = Decoder::new();
    let mut out = vec![0u8; d.len()];

    // Normal decompression with valid checksum succeeds.
    assert_eq!(decoder.decompress_into(&input, &mut out), Ok(d.len()));

    // Corrupt the checksum byte.
    let last = input.len() - 1;
    input[last] ^= 0xFF;

    // Default verify_checksum: true returns ChecksumWrong.
    assert_eq!(
        decoder.decompress_into(&input, &mut out),
        Err(Error::ChecksumWrong)
    );

    // Streaming mode with verify_checksum: true fails with ChecksumWrong.
    decoder.reset();
    let mut stream_out = vec![0u8; d.len()];
    assert_eq!(
        decoder.decompress_stream(&input, &mut stream_out),
        Err(Error::ChecksumWrong)
    );

    // Bypassing checksum verification succeeds in one-shot mode.
    decoder.set_verify_checksum(false);
    assert_eq!(decoder.decompress_into(&input, &mut out), Ok(d.len()));
    assert_eq!(out, d);

    // Bypassing checksum verification succeeds in streaming mode.
    decoder.reset();
    let mut stream_out = vec![0u8; d.len()];
    let progress = decoder.decompress_stream(&input, &mut stream_out).unwrap();
    assert!(progress.frame_done);
    assert_eq!(&stream_out[..progress.written], &d[..]);
}
