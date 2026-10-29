//! One test per item of PLAN.md section 4.2: libzstd behaviors that differ
//! from the plain format, each reproduced with a hand-made or found input and
//! compared with libzstd (plus the expected verdict, to prove the quirk).

use std::ffi::{c_int, c_void};

use compare::*;
use zefiro::Decoder;

struct Check {
    oracle: Libzstd,
    decoder: Decoder,
}

impl Check {
    fn new() -> Self {
        Self {
            oracle: Libzstd::new(),
            decoder: Decoder::new(),
        }
    }

    fn outcome(&mut self, input: &[u8], capacity: usize, what: &str) -> Outcome {
        let theirs = self.oracle.decompress(input, capacity);
        let ours = decompress_ours(&mut self.decoder, input, capacity);
        assert_agree(&ours, &theirs, &format!("{what} (capacity {capacity})"));
        theirs
    }

    fn err(&mut self, input: &[u8], capacity: usize, expected: Code, what: &str) {
        let outcome = self.outcome(input, capacity, what);
        assert!(
            matches!(outcome, Outcome::Err(code) if code == expected),
            "{what}: {outcome:?}, expected {expected:?}"
        );
    }

    fn ok(&mut self, input: &[u8], capacity: usize, len: usize, what: &str) -> Vec<u8> {
        match self.outcome(input, capacity, what) {
            Outcome::Ok { output, .. } if output.len() == len => output,
            other => panic!("{what}: {other:?}, expected {len} bytes"),
        }
    }
}

/// A frame with one compressed block holding `literals` and `sequences`.
fn compressed_frame(window: u8, literals: &[u8], sequences: &[u8]) -> Vec<u8> {
    let payload = [literals, sequences].concat();
    frame(
        window,
        false,
        &[block(true, COMPRESSED_BLOCK, payload.len(), &payload)],
        &[],
    )
}

fn raw_block(last: bool, bytes: &[u8]) -> Vec<u8> {
    block(last, RAW_BLOCK, bytes.len(), bytes)
}

fn compressed_block(last: bool, literals: &[u8], sequences: &[u8]) -> Vec<u8> {
    let payload = [literals, sequences].concat();
    block(last, COMPRESSED_BLOCK, payload.len(), &payload)
}

/// Huffman literals sections of libzstd frames: (section, block start, literals payload range).
fn huffman_four_stream_sections(frame: &[u8]) -> Vec<(LiteralsSection, usize)> {
    frame_blocks(frame)
        .into_iter()
        .filter(|(kind, _)| *kind == COMPRESSED_BLOCK)
        .filter_map(|(_, range)| {
            let section = literals_section(&frame[range.clone()])?;
            (section.kind == 2 && section.size_format != 0).then_some((section, range.start))
        })
        .collect()
}

// 4.2-1 and 4.2-2. HUF_selectDecoder is the only source of X1 versus X2 here.
unsafe extern "C" {
    fn HUF_selectDecoder(dst_size: usize, src_size: usize) -> u32;
    fn HUF_decompress4X_hufOnly_wksp(
        table: *mut u32,
        dst: *mut c_void,
        dst_size: usize,
        src: *const c_void,
        src_size: usize,
        wksp: *mut c_void,
        wksp_size: usize,
        flags: c_int,
    ) -> usize;
}

fn c_huffman_accepts(literals: &[u8], regenerated: usize, flags: c_int) -> bool {
    let mut table = vec![0u32; 4097];
    table[0] = 12 * 0x0100_0001;
    let mut workspace = vec![0u64; 2048];
    let mut dst = vec![0u8; regenerated];
    // SAFETY: buffers are valid for the sizes passed, the table is a 12 bit DTable.
    let size = unsafe {
        HUF_decompress4X_hufOnly_wksp(
            table.as_mut_ptr(),
            dst.as_mut_ptr().cast(),
            regenerated,
            literals.as_ptr().cast(),
            literals.len(),
            workspace.as_mut_ptr().cast(),
            workspace.len() * 8,
            flags,
        )
    };
    // SAFETY: pure function.
    unsafe { zstd_sys::ZSTD_isError(size) == 0 }
}

fn bmi2_flag() -> c_int {
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("bmi2") {
        return 1;
    }
    0
}

/// Frames of text with 4-stream Huffman literals, mutated in the streams: some are
/// accepted by libzstd only because its fast path skips the end-of-stream checks.
#[test]
fn huffman_four_streams_fast_path_is_lenient() {
    const DISABLE_FAST: c_int = 32;
    let mut check = Check::new();
    let mut rng = Rng::with_seed(0x4A21);
    let data = test_corpus()
        .into_iter()
        .find(|s| s.name == "text-262144")
        .unwrap()
        .data;
    let params = Params {
        literal_mode: LiteralMode::Huffman,
        ..Params::level(1)
    };
    let (mut lenient, mut tried) = (0, 0);
    for round in 0..40 {
        let start = (round * 5000) % 150_000;
        let frame = compress(&data[start..start + 20_000], &params);
        for (section, block_start) in huffman_four_stream_sections(&frame) {
            let begin = block_start + section.header_size;
            let literals_end = begin + section.compressed;
            for _ in 0..60 {
                let mut mutated = frame.clone();
                // Anywhere in the tree, jump table and streams.
                let at = begin + rng.usize(..section.compressed);
                mutated[at] ^= 1 << (rng.u64(..) % 8);
                let literals = &mutated[begin..literals_end];
                tried += 1;
                let fast = c_huffman_accepts(literals, section.regenerated, bmi2_flag());
                let strict =
                    c_huffman_accepts(literals, section.regenerated, bmi2_flag() | DISABLE_FAST);
                lenient += usize::from(fast && !strict);
                // The whole frame (sequences still match the literal count) has the same verdict.
                check.outcome(&mutated, 20_000, "mutated frame");
            }
        }
    }
    let strict_platform = cfg!(not(target_pointer_width = "64"))
        || (cfg!(target_arch = "x86_64") && bmi2_flag() == 0);
    assert!(
        lenient > 10 || strict_platform,
        "{lenient} lenient of {tried}"
    );
}

/// Both Huffman decoders get selected over the sizes libzstd produces, and mutated
/// blocks (the X2 last-symbol step included) keep the verdict.
#[test]
fn huffman_decoder_selection_and_double_symbol_tables() {
    let mut check = Check::new();
    let mut rng = Rng::with_seed(0x4A22);
    let params = Params {
        literal_mode: LiteralMode::Huffman,
        min_match: 7,
        ..Params::level(3)
    };
    let mut selected = [0usize; 2];
    for round in 0..150 {
        let len = [700, 1500, 3000, 6000, 12_000, 40_000][round % 6];
        let alphabet = [5, 16, 40][round % 3];
        // Skewed symbols: the minimum of three draws.
        let data: Vec<u8> = (0..len)
            .map(|_| (0..3).map(|_| rng.u64(..) % alphabet).min().unwrap() as u8)
            .collect();
        let frame = compress(&data, &params);
        for (section, block_start) in huffman_four_stream_sections(&frame) {
            // SAFETY: pure function.
            let double = unsafe { HUF_selectDecoder(section.regenerated, section.compressed) };
            selected[double as usize] += 1;
            for _ in 0..30 {
                let mut mutated = frame.clone();
                let tail = block_start + section.header_size + section.compressed;
                let at = tail - 1 - rng.usize(..40.min(section.compressed));
                mutated[at] ^= 1 << (rng.u64(..) % 8);
                check.outcome(&mutated, len, "mutated stream tail");
            }
        }
    }
    assert!(selected.iter().all(|&n| n > 10), "{selected:?}");
}

// 4.2-3
#[test]
fn offsets_reach_the_frame_start_not_the_window_or_earlier_frames() {
    let mut check = Check::new();
    // Offset 6 (offset code 3: 5 + 3 bits = 1) after 4 literals.
    let sequences = rle_sequences(1, (4, 3, 0), &[(1, 3)]);
    let second = compressed_frame(WINDOW_128K, &raw_literals(b"abcd"), &sequences);
    let first = frame(WINDOW_128K, false, &[raw_block(true, &[7; 100])], &[]);
    check.err(&second, 100, Code::CorruptionDetected, "alone");
    // The 100 bytes of the first frame are right before, and still out of reach.
    check.err(
        &[first, second].concat(),
        300,
        Code::CorruptionDetected,
        "after another frame",
    );

    // Offset 4 is the frame start: valid, and a repeat of 3 bytes.
    let at_start = rle_sequences(1, (4, 2, 0), &[(3, 2)]);
    let ok = compressed_frame(WINDOW_128K, &raw_literals(b"abcd"), &at_start);
    assert_eq!(check.ok(&ok, 100, 7, "offset 4"), b"abcdabc");
}

#[test]
fn offset_zero_and_a_repeat_that_becomes_zero_are_corruption() {
    let mut check = Check::new();
    let history = raw_block(false, &[9; 16]);
    // Literal length 0, offset code 1 (1 + 1 bit): code 2 (bit 0) is the third repeat offset (8),
    // code 3 (bit 1) the first minus one: 0.
    for (bit, expected) in [(0, None), (1, Some(Code::CorruptionDetected))] {
        let sequences = rle_sequences(1, (0, 1, 0), &[(bit, 1)]);
        let input = frame(
            WINDOW_128K,
            false,
            &[
                history.clone(),
                compressed_block(true, &raw_literals(b""), &sequences),
            ],
            &[],
        );
        match expected {
            None => drop(check.ok(&input, 100, 19, "third repeat offset")),
            Some(code) => check.err(&input, 100, code, "zero offset"),
        }
    }
}

// 4.2-4. Literal length 1, offset code 2 (1 + 2 bits), match length code 52 (65539 + 16 bits).
// The block regenerates more than the 128 KiB limit, which libzstd only enforces when its
// literals sit in the output past the block.
#[test]
fn block_output_limit_depends_on_where_libzstd_puts_literals() {
    let mut check = Check::new();
    let sequences = |count: u8, extra: &[u16]| {
        let fields: Vec<_> = (0..count as usize)
            .flat_map(|i| [(0, 2), (u64::from(extra.get(i).copied().unwrap_or(0)), 16)])
            .collect();
        rle_sequences(count, (1, 2, 52), &fields)
    };
    // 16 sequences: raw literals are read in place from the input (32 bytes follow them),
    // so the capacity is the limit. Literals in the extra buffer (RLE) leave room after
    // the block for them, which caps the block.
    let many = sequences(16, &[]);
    let total = 16 * (1 + 65_539) + 24;
    check.ok(
        &compressed_frame(WINDOW_128K, &raw_literals(&[5; 40]), &many),
        total,
        total,
        "in place",
    );
    check.err(
        &compressed_frame(WINDOW_128K, &raw_literals(&[5; 40]), &many),
        total - 1,
        Code::DstSizeTooSmall,
        "in place, too small",
    );
    check.err(
        &compressed_frame(WINDOW_128K, &rle_literals(5, 40), &many),
        total,
        Code::DstSizeTooSmall,
        "literals after the block",
    );
    // 2 sequences: 131 158 bytes, 54 over 128 KiB + 32. Accepted up to the capacity where
    // libzstd starts to put the (RLE) literals after the block.
    let two = sequences(2, &[40, 0]);
    let input = compressed_frame(WINDOW_128K, &rle_literals(5, 40), &two);
    let room = 131_072 + 64 + 40;
    check.ok(&input, room, 131_158, "no room after the block");
    check.err(
        &input,
        room + 1,
        Code::DstSizeTooSmall,
        "room after the block",
    );
}

// 4.2-5
#[test]
fn content_size_mismatch_comes_before_the_checksum() {
    let mut check = Check::new();
    let header =
        |checksum: bool, size: u8| [&MAGIC[..], &[0x20 | u8::from(checksum) << 2, size]].concat();
    let body = raw_block(true, b"abcd");
    check.err(
        &[header(false, 5), body.clone()].concat(),
        10,
        Code::CorruptionDetected,
        "size 5",
    );
    check.err(
        &[header(true, 3), body.clone(), vec![0; 4]].concat(),
        10,
        Code::CorruptionDetected,
        "size and checksum",
    );
    check.err(
        &[header(true, 4), body.clone(), vec![0; 4]].concat(),
        10,
        Code::ChecksumWrong,
        "checksum",
    );
    for missing in 0..4 {
        check.err(
            &[header(true, 4), body.clone(), vec![0; missing]].concat(),
            10,
            Code::ChecksumWrong,
            "short checksum",
        );
    }
}

// 4.2-6
#[test]
fn frame_header_verdicts() {
    let mut check = Check::new();
    let raw = raw_block(true, b"abc");
    let with = |descriptor: u8, fields: &[u8]| [&MAGIC[..], &[descriptor], fields, &raw].concat();
    check.err(
        &with(0x28, &[0]),
        10,
        Code::FrameParameterUnsupported,
        "reserved bit",
    );
    check.err(
        &with(0, &[22 << 3]),
        10,
        Code::WindowTooLarge,
        "window log 32",
    );
    check.ok(&with(0, &[21 << 3]), 10, 3, "window log 31");
    check.err(
        &with(0x21, &[5, 3]),
        10,
        Code::DictionaryWrong,
        "dictionary id 5",
    );
    check.ok(&with(0x20, &[3]), 10, 3, "no dictionary id");
}

// 4.2-7
#[test]
fn trailing_input_verdicts() {
    let mut check = Check::new();
    let good = frame(WINDOW_1K, false, &[raw_block(true, b"abc")], &[]);
    check.ok(&[], 10, 0, "empty");
    for len in 1..=14 {
        let garbage = vec![0; len];
        // Under 9 bytes there is no room for a header and a block header.
        let alone = if len < 9 {
            Code::SrcSizeWrong
        } else {
            Code::PrefixUnknown
        };
        check.err(&garbage, 10, alone, &format!("{len} garbage bytes"));
        check.err(
            &[good.clone(), garbage].concat(),
            10,
            Code::SrcSizeWrong,
            &format!("{len} bytes after a frame"),
        );
    }
    // Skippable frames need their 8 byte header, and their size must be there.
    let skippable = skippable_frame(b"abcdef", 4);
    for len in 0..skippable.len() {
        check.outcome(&skippable[..len], 10, "truncated skippable frame");
    }
    check.ok(
        &[skippable.clone(), good.clone()].concat(),
        10,
        3,
        "skippable then frame",
    );
    check.ok(&skippable, 10, 0, "skippable only");
}

// 4.2-8
#[test]
fn four_stream_literals_need_six_bytes() {
    let mut check = Check::new();
    // Compressed literals, size format 1 (four streams), regenerated size 5 or 6, compressed size 3.
    for (regenerated, expected) in [
        (5u32, Code::LiteralsHeaderWrong),
        (6, Code::CorruptionDetected),
    ] {
        let header = (2 | 1 << 2 | regenerated << 4 | 3 << 14).to_le_bytes();
        let literals = [&header[..3], &[0x80, 0x11, 0x22][..]].concat();
        check.err(
            &compressed_frame(WINDOW_1K, &literals, &[0]),
            100,
            expected,
            "four streams",
        );
    }
}

// 4.2-9
#[test]
fn sequences_with_no_output_room_are_dst_size_too_small() {
    let mut check = Check::new();
    let input = compressed_frame(WINDOW_1K, &raw_literals(b""), &[1, 0, 0]);
    check.err(&input, 0, Code::DstSizeTooSmall, "capacity 0");
    check.err(&input, 1, Code::CorruptionDetected, "capacity 1");
}

// 4.2-10
#[test]
fn repeated_tables_need_a_previous_block() {
    let mut check = Check::new();
    let history = raw_block(false, &[3; 8]);
    let repeat_all = [1, 0xFC];
    let stream = backward_stream(&[]);
    let repeat = compressed_block(
        true,
        &raw_literals(b""),
        &[&repeat_all[..], &stream].concat(),
    );
    check.err(
        &frame(WINDOW_1K, false, &[history.clone(), repeat.clone()], &[]),
        100,
        Code::CorruptionDetected,
        "no FSE tables",
    );
    // After a block with RLE tables (literal length 0, repeat offset, match 3): fine.
    let first = compressed_block(false, &raw_literals(b""), &rle_sequences(1, (0, 0, 0), &[]));
    check.ok(
        &frame(WINDOW_1K, false, &[history.clone(), first, repeat], &[]),
        100,
        14,
        "repeated FSE tables",
    );
    // Treeless literals (type 3, 3 byte header) without a Huffman table.
    let treeless = [0x03, 0x10, 0x00];
    check.err(
        &compressed_frame(WINDOW_1K, &treeless, &[0]),
        100,
        Code::DictionaryCorrupted,
        "no Huffman table",
    );
}

// 4.2-11 needs the streaming decoder.
#[test]
#[ignore = "phase 4: streaming single-pass shortcut"]
fn streaming_single_pass_shortcut() {}

// 4.2-12
#[test]
fn raw_and_rle_blocks_ignore_the_block_size_limit() {
    let mut check = Check::new();
    let big = vec![b'z'; 200_000];
    check.ok(
        &frame(WINDOW_1K, false, &[raw_block(true, &big)], &[]),
        200_000,
        200_000,
        "raw block",
    );
    check.ok(
        &frame(
            WINDOW_1K,
            false,
            &[block(true, RLE_BLOCK, 200_000, b"z")],
            &[],
        ),
        200_000,
        200_000,
        "rle block",
    );
    // An all-ones content size means unknown, not 2^64 - 1.
    let unknown = [&MAGIC[..], &[0xE0], &[0xFF; 8], &raw_block(true, b"abc")].concat();
    check.ok(&unknown, 10, 3, "unknown content size");
}

// 4.2-13. Weights 12, 11, ..., 1 (a tree of depths 1 to 12, the last symbol implied) with an
// FSE header of table log 6 describing symbols 0..=12: libzstd's workspace allows 0..=11 only.
#[test]
fn huffman_weights_header_is_limited_by_libzstd_workspace() {
    unsafe extern "C" {
        fn FSE_buildCTable_wksp(
            ct: *mut u32,
            norm: *const i16,
            max_symbol: u32,
            table_log: u32,
            wksp: *mut c_void,
            wksp_size: usize,
        ) -> usize;
        fn FSE_writeNCount(
            buffer: *mut c_void,
            size: usize,
            norm: *const i16,
            max_symbol: u32,
            table_log: u32,
        ) -> usize;
        fn FSE_compress_usingCTable(
            dst: *mut c_void,
            dst_size: usize,
            src: *const c_void,
            src_size: usize,
            ct: *const u32,
        ) -> usize;
    }
    let mut check = Check::new();
    let weights: Vec<u8> = (1..=12).rev().collect();
    // 64 states over symbols 1..=12.
    let mut norm = [0i16; 13];
    for (symbol, count) in norm.iter_mut().enumerate().skip(1) {
        *count = if symbol <= 4 { 6 } else { 5 };
    }
    assert_eq!(norm.iter().sum::<i16>(), 64);
    let (mut ct, mut wksp, mut header, mut stream) = (
        vec![0u32; 2048],
        vec![0u32; 4096],
        vec![0u8; 64],
        vec![0u8; 64],
    );
    // SAFETY: buffers are valid for the sizes passed and large enough for table log 6.
    let (header_size, stream_size) = unsafe {
        assert_eq!(
            zstd_sys::ZSTD_isError(FSE_buildCTable_wksp(
                ct.as_mut_ptr(),
                norm.as_ptr(),
                12,
                6,
                wksp.as_mut_ptr().cast(),
                4 * wksp.len()
            )),
            0
        );
        let header_size = FSE_writeNCount(header.as_mut_ptr().cast(), 64, norm.as_ptr(), 12, 6);
        let stream_size = FSE_compress_usingCTable(
            stream.as_mut_ptr().cast(),
            64,
            weights.as_ptr().cast(),
            weights.len(),
            ct.as_ptr(),
        );
        (header_size, stream_size)
    };
    let weights_header = [
        &[(header_size + stream_size) as u8][..],
        &header[..header_size],
        &stream[..stream_size],
    ]
    .concat();
    // 16 times the symbol of depth 1 (code bit 0), then the end mark.
    let literals_stream = backward_stream(&[(0, 16)]);
    let payload = [weights_header, literals_stream].concat();
    let section_header = (2u32 | 16 << 4 | (payload.len() as u32) << 14).to_le_bytes();
    let literals = [&section_header[..3], &payload].concat();
    check.err(
        &compressed_frame(WINDOW_1K, &literals, &[0]),
        100,
        Code::CorruptionDetected,
        "weights over the workspace",
    );
}

// 4.2-14
#[test]
fn literals_size_checks_come_in_libzstd_order() {
    let mut check = Check::new();
    // Window of 1 KiB: block size limit 1024.
    let run = |check: &mut Check, literals: &[u8], capacity: usize| {
        let sequences: &[u8] = &[0];
        let input = compressed_frame(WINDOW_1K, literals, sequences);
        check.outcome(&input, capacity, "literals")
    };
    let code = |outcome: Outcome| match outcome {
        Outcome::Err(code) => code,
        Outcome::Ok { .. } => Code::Other(0),
    };
    // Literals over the limit: corruption; fitting the limit but not the output room: too small.
    assert_eq!(
        code(run(&mut check, &rle_literals(1, 1025), 2000)),
        Code::CorruptionDetected
    );
    assert_eq!(
        code(run(&mut check, &rle_literals(1, 1024), 1000)),
        Code::DstSizeTooSmall
    );
    assert!(matches!(
        run(&mut check, &rle_literals(1, 1024), 1024),
        Outcome::Ok { .. }
    ));
    // A compressed block over the limit is srcSize_wrong before anything else.
    let big = compressed_frame(WINDOW_1K, &raw_literals(&[0; 1100]), &[0]);
    assert_eq!(
        code(check.outcome(&big, 2000, "big block")),
        Code::SrcSizeWrong
    );
}
