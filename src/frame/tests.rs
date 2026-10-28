use alloc::vec::Vec;

use super::*;

fn with_magic(rest: &[u8]) -> Vec<u8> {
    [&MAGIC.to_le_bytes()[..], rest].concat()
}

fn frame_header(src: &[u8]) -> FrameHeader {
    match parse_frame_start(src) {
        Ok(FrameStart::Frame(header)) => header,
        other => panic!("{other:?}"),
    }
}

#[test]
fn window_size_from_the_window_byte() {
    // Descriptor 0: window byte, no dictionary, no content size, no checksum.
    for (window_byte, expected) in [
        (0x00, 1 << 10),
        (0x07, (1 << 10) + 7 * (1 << 7)),
        (0x08, 1 << 11),
        (0x5A, (1 << 21) + 2 * (1 << 18)),
        (0xA8, 1 << 31),
        (0xAF, (1 << 31) + 7 * (1 << 28)),
    ] {
        let header = frame_header(&with_magic(&[0x00, window_byte]));
        assert_eq!(header.window_size, expected, "window byte {window_byte:#x}");
        assert_eq!(header.header_size, 6);
        assert_eq!(header.content_size, None);
        assert_eq!(header.block_size_max, expected.min(1 << 17) as u32);
    }
    assert_eq!(
        parse_frame_start(&with_magic(&[0x00, 0xB0])),
        Err(Error::WindowTooLarge)
    );
}

#[test]
fn content_size_field_sizes_and_offsets() {
    // (descriptor, fields after it, header size, content size, window size)
    type Case<'a> = (u8, &'a [u8], usize, Option<u64>, u64);
    let cases: [Case<'_>; 7] = [
        (0x20, &[200], 6, Some(200), 200),
        (0x60, &[0x34, 0x12], 7, Some(0x1234 + 256), 0x1234 + 256),
        (0xA0, &[1, 2, 3, 4], 9, Some(0x0403_0201), 0x0403_0201),
        (
            0xE0,
            &[1, 0, 0, 0, 0, 0, 0, 1],
            13,
            Some(0x0100_0000_0000_0001),
            0x0100_0000_0000_0001,
        ),
        (0x40, &[0, 0x34, 0x12], 8, Some(0x1234 + 256), 1 << 10),
        (0xC0, &[0, 5, 0, 0, 0, 0, 0, 0, 0], 14, Some(5), 1 << 10),
        // All ones is libzstd's "unknown".
        (
            0xC0,
            &[0, 255, 255, 255, 255, 255, 255, 255, 255],
            14,
            None,
            1 << 10,
        ),
    ];
    for (descriptor, fields, header_size, content_size, window_size) in cases {
        let header = frame_header(&with_magic(&[&[descriptor], fields, &[0; 3]].concat()));
        assert_eq!(
            header.header_size, header_size,
            "descriptor {descriptor:#x}"
        );
        assert_eq!(
            header.content_size, content_size,
            "descriptor {descriptor:#x}"
        );
        assert_eq!(
            header.window_size, window_size,
            "descriptor {descriptor:#x}"
        );
    }
}

#[test]
fn dictionary_id_and_checksum_flags() {
    let header = frame_header(&with_magic(&[0x07, 0x00, 0xAA, 0xBB, 0xCC, 0xDD]));
    assert_eq!(
        (header.dict_id, header.checksum, header.header_size),
        (0xDDCC_BBAA, true, 10)
    );
    let header = frame_header(&with_magic(&[0x02, 0x00, 0xAA, 0xBB]));
    assert_eq!(
        (header.dict_id, header.checksum, header.header_size),
        (0xBBAA, false, 8)
    );
    let header = frame_header(&with_magic(&[0x21, 0x07, 0x00]));
    assert_eq!((header.dict_id, header.header_size), (7, 7));
}

#[test]
fn short_and_invalid_inputs() {
    assert_eq!(parse_frame_start(&[]), Ok(FrameStart::NeedMore(5)));
    assert_eq!(parse_frame_start(&[0x28]), Ok(FrameStart::NeedMore(5)));
    assert_eq!(
        parse_frame_start(&[0x28, 0xB5, 0x2F, 0xFD]),
        Ok(FrameStart::NeedMore(5))
    );
    assert_eq!(parse_frame_start(&[0x29]), Err(Error::PrefixUnknown));
    assert_eq!(
        parse_frame_start(&[0x28, 0xB5, 0x2F, 0xFC]),
        Err(Error::PrefixUnknown)
    );
    assert_eq!(
        parse_frame_start(&[0x50, 0x2A, 0x4D, 0x18]),
        Ok(FrameStart::NeedMore(5))
    );
    assert_eq!(parse_frame_start(&[0x5F]), Ok(FrameStart::NeedMore(5)));
    assert_eq!(parse_frame_start(&[0x60]), Err(Error::PrefixUnknown));
    // Magic fine, header longer than the input.
    assert_eq!(
        parse_frame_start(&with_magic(&[0xE0, 0, 0])),
        Ok(FrameStart::NeedMore(13))
    );
    assert_eq!(
        parse_frame_start(&with_magic(&[0x00])),
        Ok(FrameStart::NeedMore(6))
    );
    // Reserved bit comes before the window check, bit 4 is ignored.
    assert_eq!(
        parse_frame_start(&with_magic(&[0x08, 0xFF])),
        Err(Error::FrameParameterUnsupported)
    );
    assert!(parse_frame_start(&with_magic(&[0x10, 0x00])).is_ok());
    assert_eq!(
        parse_frame_start(&[0, 0, 0, 0, 0, 0, 0]),
        Err(Error::PrefixUnknown)
    );
    // Not checked by `frame_header_size`, which reads the descriptor only.
    assert_eq!(frame_header_size(&[0, 0, 0, 0, 0xE3]), Ok(5 + 4 + 8));
    assert_eq!(frame_header_size(&[0; 4]), Err(Error::SrcSizeWrong));
}

#[test]
fn skippable_frames() {
    let mut frame = 0x184D_2A5Fu32.to_le_bytes().to_vec();
    assert_eq!(parse_frame_start(&frame), Ok(FrameStart::NeedMore(5)));
    frame.push(0);
    assert_eq!(parse_frame_start(&frame), Ok(FrameStart::NeedMore(8)));
    frame.truncate(4);
    frame.extend_from_slice(&3u32.to_le_bytes());
    assert_eq!(
        parse_frame_start(&frame),
        Ok(FrameStart::Skippable { content_size: 3 })
    );
    assert_eq!(skippable_frame_size(&frame), Err(Error::SrcSizeWrong));
    frame.extend_from_slice(&[1, 2, 3]);
    assert_eq!(skippable_frame_size(&frame), Ok(11));
    frame.push(9);
    assert_eq!(skippable_frame_size(&frame), Ok(11));
    assert_eq!(frame_content_size(&frame), Ok(Some(0)));
    assert!(is_skippable_magic(&frame));
    assert!(!is_skippable_magic(&frame[1..]));
    assert!(!is_skippable_magic(&frame[..3]));
    // 0xFFFF_FFF8 + 8 overflows u32.
    let mut huge = frame[..4].to_vec();
    huge.extend_from_slice(&0xFFFF_FFF8u32.to_le_bytes());
    assert_eq!(
        skippable_frame_size(&huge),
        Err(Error::FrameParameterUnsupported)
    );
    huge[4..].copy_from_slice(&0xFFFF_FFF7u32.to_le_bytes());
    assert_eq!(skippable_frame_size(&huge), Err(Error::SrcSizeWrong));
}

#[test]
fn content_size_api() {
    assert_eq!(frame_content_size(&with_magic(&[0x20, 9])), Ok(Some(9)));
    assert_eq!(frame_content_size(&with_magic(&[0x00, 0x00])), Ok(None));
    assert_eq!(frame_content_size(&[]), Err(Error::SrcSizeWrong));
    assert_eq!(
        frame_content_size(&[1, 2, 3, 4, 5, 6]),
        Err(Error::PrefixUnknown)
    );
    assert_eq!(
        frame_content_size(&with_magic(&[0x20])),
        Err(Error::SrcSizeWrong)
    );
}

#[test]
fn block_headers() {
    let header = |value: u32| parse_block_header(&value.to_le_bytes()[..3]);
    assert_eq!(
        header(1 | 5 << 3),
        Ok(BlockHeader {
            last: true,
            block_type: BlockType::Raw,
            size: 5
        })
    );
    let rle = header(1 << 1 | 0x1F_FFFF << 3).unwrap();
    assert_eq!(
        (rle.last, rle.block_type, rle.size, rle.input_size()),
        (false, BlockType::Rle, 0x1F_FFFF, 1)
    );
    let compressed = header(2 << 1 | 1 | 100 << 3).unwrap();
    assert_eq!(
        (compressed.block_type, compressed.input_size()),
        (BlockType::Compressed, 100)
    );
    assert_eq!(header(3 << 1), Err(Error::CorruptionDetected));
    assert_eq!(parse_block_header(&[0, 0]), Err(Error::SrcSizeWrong));
}

#[cfg(not(target_family = "wasm"))]
mod against_libzstd {
    use zstd_sys::{
        ZSTD_ErrorCode, ZSTD_FrameHeader, ZSTD_FrameType_e, ZSTD_getErrorCode, ZSTD_getFrameHeader,
        ZSTD_isError,
    };

    use super::*;

    fn c_parse(src: &[u8]) -> Result<FrameStart, Error> {
        let mut c_header = ZSTD_FrameHeader {
            frameContentSize: 0,
            windowSize: 0,
            blockSizeMax: 0,
            frameType: ZSTD_FrameType_e::ZSTD_frame,
            headerSize: 0,
            dictID: 0,
            checksumFlag: 0,
            _reserved1: 0,
            _reserved2: 0,
        };
        // SAFETY: the header struct is valid and src is valid for its length.
        let result =
            unsafe { ZSTD_getFrameHeader(&raw mut c_header, src.as_ptr().cast(), src.len()) };
        // SAFETY: pure functions of the result.
        let (is_error, code) = unsafe { (ZSTD_isError(result) != 0, ZSTD_getErrorCode(result)) };
        if is_error {
            return Err(match code {
                ZSTD_ErrorCode::ZSTD_error_prefix_unknown => Error::PrefixUnknown,
                ZSTD_ErrorCode::ZSTD_error_frameParameter_unsupported => {
                    Error::FrameParameterUnsupported
                }
                ZSTD_ErrorCode::ZSTD_error_frameParameter_windowTooLarge => Error::WindowTooLarge,
                other => panic!("{other:?}"),
            });
        }
        if result > 0 {
            return Ok(FrameStart::NeedMore(result));
        }
        if c_header.frameType == ZSTD_FrameType_e::ZSTD_skippableFrame {
            return Ok(FrameStart::Skippable {
                content_size: c_header.frameContentSize as u32,
            });
        }
        Ok(FrameStart::Frame(FrameHeader {
            header_size: c_header.headerSize as usize,
            content_size: (c_header.frameContentSize != u64::MAX)
                .then_some(c_header.frameContentSize),
            window_size: c_header.windowSize,
            block_size_max: c_header.blockSizeMax,
            dict_id: c_header.dictID,
            checksum: c_header.checksumFlag != 0,
        }))
    }

    #[test]
    fn frame_start_matches_libzstd_on_random_prefixes() {
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut kinds = [0; 4];
        for _ in 0..200_000 {
            let len = (next() % 20) as usize;
            let mut src: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            match next() % 8 {
                0..=4 => {
                    let magic = if next() % 5 == 0 {
                        0x184D_2A50 + (next() % 16) as u32
                    } else {
                        MAGIC
                    };
                    let bytes = magic.to_le_bytes();
                    let keep = src.len().min(4);
                    src[..keep].copy_from_slice(&bytes[..keep]);
                    if next() % 2 == 0 && src.len() > 4 {
                        src[4] &= 0xF7;
                    }
                    if next() % 3 == 0 && src.len() > 5 {
                        src[5] &= 0x7F;
                    }
                }
                5 if !src.is_empty() => src[0] = [0x28, 0x50, 0x2A, 0x4D][next() as usize % 4],
                _ => {}
            }
            let (ours, theirs) = (parse_frame_start(&src), c_parse(&src));
            assert_eq!(ours, theirs, "{src:02X?}");
            kinds[match ours {
                Ok(FrameStart::Frame(_)) => 0,
                Ok(FrameStart::Skippable { .. }) => 1,
                Ok(FrameStart::NeedMore(_)) => 2,
                Err(_) => 3,
            }] += 1;
        }
        assert!(kinds.iter().all(|&n| n > 5000), "{kinds:?}");
    }
}
