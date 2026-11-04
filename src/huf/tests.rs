use alloc::vec;
use alloc::vec::Vec;

use super::*;
use crate::testing::{Rng, TestTree, assert_same, miri_scaled};

impl HufTable {
    fn read_x1(&mut self, src: &[u8]) -> Result<usize, Error> {
        self.read_x1_at_least(src, FAST_TABLE_LOG)
    }

    fn read_table(&mut self, src: &[u8], four_streams_len: Option<usize>) -> Result<usize, Error> {
        let mut def = HufDef::default();
        let size = def.read(src, four_streams_len)?;
        self.build(&def)?;
        Ok(size)
    }

    fn read_table_1x(&mut self, src: &[u8]) -> Result<usize, Error> {
        self.read_table(src, None)
    }

    fn read_table_4x(&mut self, dst_len: usize, src: &[u8]) -> Result<usize, Error> {
        self.read_table(src, Some(dst_len))
    }
}

// Raw header for the weights 1, 1, 2, 3, 3 (the implied sixth is 3): table log 4.
const DIRECT_WEIGHTS: [u8; 4] = [0x84, 0x11, 0x23, 0x30];

#[test]
fn read_stats_direct_weights() {
    let mut weights = [0; SYMBOL_COUNT];
    let mut rank_stats = [0; RANK_COUNT];
    let stats = read_stats(&mut weights, &mut rank_stats, &DIRECT_WEIGHTS).unwrap();
    assert_eq!(
        (stats.bytes_read, stats.nb_symbols, stats.table_log),
        (4, 6, 4)
    );
    assert_eq!(weights[..6], [1, 1, 2, 3, 3, 3]);
    assert_eq!(rank_stats[..5], [0, 2, 1, 3, 0]);
}

#[test]
fn read_stats_rejects_bad_trees() {
    let mut weights = [0; SYMBOL_COUNT];
    let mut rank_stats = [0; RANK_COUNT];
    let mut read = |src: &[u8]| read_stats(&mut weights, &mut rank_stats, src).map(|s| s.table_log);
    assert_eq!(read(&[]).unwrap_err(), Error::SrcSizeWrong);
    // Promises 5 weights (3 bytes), carries 1.
    assert_eq!(read(&[0x84, 0x11]).unwrap_err(), Error::SrcSizeWrong);
    // Weight 13 exceeds the maximum table log.
    assert!(read(&[0x83, 0xD1, 0x10]).is_err());
    // All weights zero.
    assert!(read(&[0x83, 0x00, 0x00]).is_err());
    // A lone weight 2: no pair of weight 1.
    assert!(read(&[0x80, 0x20]).is_err());
    // Weights 1, 2, 2 leave 3 for the last one, not a power of two.
    assert!(read(&[0x82, 0x12, 0x20]).is_err());
    // A lone weight 1 plus the implied one is the smallest tree.
    assert_eq!(read(&[0x80, 0x10]).unwrap(), 1);
}

#[test]
fn sort_by_weight_orders_by_weight_then_symbol_up_to_a_full_alphabet() {
    // 256 symbols of one weight fill the byte counters of their lane.
    let mut rank_stats = [0; RANK_COUNT];
    rank_stats[1] = 256;
    let (sorted, rank_start) = sort_by_weight(&[1; SYMBOL_COUNT], &rank_stats);
    assert_eq!(sorted.map(usize::from), core::array::from_fn(|i| i));
    assert_eq!((rank_start[1], rank_start[2]), (0, 256));
    // Mixed weights, zeros first.
    let weights = [2, 0, 1, 2, 1, 0, 3, 1];
    let mut rank_stats = [0; RANK_COUNT];
    for &weight in &weights {
        rank_stats[usize::from(weight)] += 1;
    }
    let (sorted, rank_start) = sort_by_weight(&weights, &rank_stats);
    assert_eq!(sorted[..8], [1, 5, 2, 4, 7, 0, 3, 6]);
    assert_eq!(rank_start[..5], [0, 2, 5, 7, 8]);
}

#[test]
fn select_decoder_follows_the_timing_table() {
    // Nearly incompressible: the cheaper table to build wins.
    assert!(!select_decoder(1000, 900));
    assert!(!select_decoder(256, 100));
    // Large and half-compressed: double symbols win.
    assert!(select_decoder(4096, 2048));
}

#[test]
fn x1_table_of_a_known_tree() {
    let mut table = HufTable::boxed();
    assert_eq!(table.read_x1(&DIRECT_WEIGHTS).unwrap(), 4);
    // Rescaled from depth 4 to 11: weights 1,1,2,3,3,3 become 8,8,9,10,10,10.
    assert_eq!(table.table_log, FAST_TABLE_LOG);
    let cells = |symbol: u8| {
        table.x1[..1 << FAST_TABLE_LOG]
            .iter()
            .filter(|e| e.byte() == symbol)
            .count()
    };
    assert_eq!(
        [0, 1, 2, 3, 4, 5].map(cells),
        [128, 128, 256, 512, 512, 512]
    );
    assert_eq!(table.x1[0], EltX1::new(4, 0));
}

#[test]
fn single_stream_x1_table_is_small_and_scales_to_the_fast_table() {
    let mut rng = Rng::with_seed(0x51_17);
    for _ in 0..miri_scaled(200) {
        let max_depth = rng.choice([2, 5, 9, 11, 12]).unwrap();
        let tree = TestTree::random(&mut rng, max_depth);
        let (mut small, mut scaled) = (HufTable::boxed(), HufTable::boxed());
        assert_eq!(
            small.read_x1_at_least(&tree.header, 0),
            Ok(tree.header.len())
        );
        assert_eq!(scaled.read_x1(&tree.header), Ok(tree.header.len()));
        assert_eq!(small.table_log, tree.depth);
        small.scale_x1_for_four_streams();
        assert_eq!(small.table_log, scaled.table_log);
        let cells = 1 << scaled.table_log;
        assert_eq!(
            small.x1[..cells],
            scaled.x1[..cells],
            "depth {}",
            tree.depth
        );
    }
}

#[test]
fn x2_pairs_agree_with_the_x1_codes() {
    // Every cell of the X2 table starts with the X1 symbol and, for pairs,
    // continues with the X1 symbol of the following bits.
    let mut x1 = HufTable::boxed();
    let mut x2 = HufTable::boxed();
    assert_eq!(x1.read_x1(&DIRECT_WEIGHTS).unwrap(), 4);
    assert_eq!(x2.read_x2(&DIRECT_WEIGHTS).unwrap(), 4);
    assert_eq!(x2.table_log, FAST_TABLE_LOG);
    for index in 0..1usize << FAST_TABLE_LOG {
        let (e1, e2) = (x1.x1[index], x2.x2[index]);
        assert_eq!(e2.sequence as u8, e1.byte());
        assert!(e2.nb_bits >= e1.nb_bits() && e2.length >= 1);
        if e2.length == 2 {
            let second = x1.x1[(index << e1.nb_bits()) & ((1 << FAST_TABLE_LOG) - 1)];
            assert_eq!((e2.sequence >> 8) as u8, second.byte());
            assert_eq!(e2.nb_bits, e1.nb_bits() + second.nb_bits());
        }
    }
}

#[test]
fn round_trips_through_both_tables_with_one_and_four_streams() {
    let mut rng = Rng::with_seed(0x40_0D);
    let (mut fast, mut strict) = (0, 0);
    for _ in 0..miri_scaled(300) {
        let max_depth = rng.choice([2, 5, 9, 11, 12]).unwrap();
        let tree = TestTree::random(&mut rng, max_depth);
        let most = rng.choice([10, 300, 5000]).unwrap();
        let len = 6 + rng.usize(..most);
        let data = tree.random_data(&mut rng, len);
        for double in [false, true] {
            let mut table = HufTable::boxed();
            let size = if double {
                table.read_x2(&tree.header)
            } else {
                table.read_x1(&tree.header)
            };
            assert_eq!(size, Ok(tree.header.len()));
            assert_eq!(
                table.table_log,
                tree.depth.max(FAST_TABLE_LOG),
                "depth {}",
                tree.depth
            );

            let mut out = vec![0; len];
            table
                .decompress_1x(&mut out, &tree.encode_stream(&data))
                .unwrap();
            assert_same(&out, &data, "one stream");
            let mut out = vec![0; len];
            table
                .decompress_4x(&mut out, &tree.encode_four_streams(&data))
                .unwrap();
            assert_same(&out, &data, "four streams");
            if tree.depth <= FAST_TABLE_LOG && len >= 4 * 40 {
                fast += 1;
            } else {
                strict += 1;
            }
        }
    }
    assert!(
        cfg!(miri) || (fast > 100 && strict > 100),
        "{fast} fast, {strict} strict"
    );
}

// Four streams with a junk byte at the start of the last one: bits nobody reads.
fn four_streams_with_unread_bits(tree: &TestTree, data: &[u8]) -> Vec<u8> {
    let mut streams = tree.encode_four_streams(data);
    let le16 = |at: usize| usize::from(u16::from_le_bytes([streams[at], streams[at + 1]]));
    let last_start = JUMP_TABLE_SIZE + le16(0) + le16(2) + le16(4);
    streams.insert(last_start, 0x55);
    streams
}

#[test]
fn fast_path_ignores_unread_bits_of_the_last_stream() {
    let mut rng = Rng::with_seed(0xE0D);
    let deep = loop {
        let tree = TestTree::random(&mut rng, 12);
        if tree.depth == 12 {
            break tree;
        }
    };
    let cases = [
        // (tree, literals, accepted): a table log of 11 and streams of 8 bytes are needed.
        (TestTree::random(&mut rng, 9), 4000, true),
        (TestTree::random(&mut rng, 3), 24, false),
        (deep, 4000, false),
    ];
    for (tree, len, accepted) in cases {
        let data = tree.random_data(&mut rng, len);
        let streams = four_streams_with_unread_bits(&tree, &data);
        for double in [false, true] {
            let mut table = HufTable::boxed();
            if double {
                table.read_x2(&tree.header).unwrap();
            } else {
                table.read_x1(&tree.header).unwrap();
            }
            let mut out = vec![0; len];
            let result = table.decompress_4x(&mut out, &streams);
            assert_eq!(
                result.is_ok(),
                accepted,
                "depth {}, x2 {double}",
                tree.depth
            );
            if accepted {
                assert_same(&out, &data, "fast path");
            } else {
                assert_eq!(result, Err(Error::CorruptionDetected));
            }
        }
    }
}

#[cfg(not(any(target_family = "wasm", miri)))]
mod against_libzstd {
    use core::ffi::{c_int, c_void};

    use zstd_sys::ZSTD_isError;

    use super::*;
    use crate::testing::{c_huf, skewed_bytes};

    const DISABLE_FAST: c_int = 32;
    const C_TABLE_WORDS: usize = 1 + TABLE_SIZE;

    // `HUF_flags_bmi2` where C's context sets it: the BMI2 loop has the lenient path too.
    fn c_flags() -> c_int {
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("bmi2") {
            return 1;
        }
        0
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Kind {
        OneX1,
        OneX2,
        Four,
        FourX1,
        FourX2,
    }

    const KINDS: [Kind; 5] = [
        Kind::OneX1,
        Kind::OneX2,
        Kind::Four,
        Kind::FourX1,
        Kind::FourX2,
    ];

    unsafe extern "C" {
        fn HUF_readStats_wksp(
            weights: *mut u8,
            weights_size: usize,
            rank_stats: *mut u32,
            nb_symbols: *mut u32,
            table_log: *mut u32,
            src: *const c_void,
            src_size: usize,
            wksp: *mut c_void,
            wksp_size: usize,
            flags: c_int,
        ) -> usize;
        fn HUF_readDTableX1_wksp(
            table: *mut u32,
            src: *const c_void,
            src_size: usize,
            wksp: *mut c_void,
            wksp_size: usize,
            flags: c_int,
        ) -> usize;
        fn HUF_readDTableX2_wksp(
            table: *mut u32,
            src: *const c_void,
            src_size: usize,
            wksp: *mut c_void,
            wksp_size: usize,
            flags: c_int,
        ) -> usize;
        fn HUF_decompress1X1_DCtx_wksp(
            table: *mut u32,
            dst: *mut c_void,
            dst_size: usize,
            src: *const c_void,
            src_size: usize,
            wksp: *mut c_void,
            wksp_size: usize,
            flags: c_int,
        ) -> usize;
        fn HUF_decompress1X2_DCtx_wksp(
            table: *mut u32,
            dst: *mut c_void,
            dst_size: usize,
            src: *const c_void,
            src_size: usize,
            wksp: *mut c_void,
            wksp_size: usize,
            flags: c_int,
        ) -> usize;
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
        fn HUF_decompress1X_usingDTable(
            dst: *mut c_void,
            dst_size: usize,
            src: *const c_void,
            src_size: usize,
            table: *const u32,
            flags: c_int,
        ) -> usize;
        fn HUF_decompress4X_usingDTable(
            dst: *mut c_void,
            dst_size: usize,
            src: *const c_void,
            src_size: usize,
            table: *const u32,
            flags: c_int,
        ) -> usize;
        fn HUF_selectDecoder(dst_size: usize, src_size: usize) -> u32;
    }

    fn is_error(code: usize) -> bool {
        // SAFETY: pure function of its argument.
        unsafe { ZSTD_isError(code) != 0 }
    }

    // A DTable as libzstd's decoder initializes it (`ZSTD_HUFFDTABLE_CAPACITY_LOG` 12).
    fn c_table() -> Vec<u32> {
        let mut table = vec![0; C_TABLE_WORDS];
        table[0] = 12 * 0x0100_0001;
        table
    }

    fn c_workspace() -> Vec<u64> {
        vec![0; 2048]
    }

    // `HUF_READ_STATS_WORKSPACE_SIZE`: libzstd decodes weights in a workspace this small.
    fn c_stats_workspace() -> Vec<u32> {
        vec![0; 219]
    }

    struct CStats {
        bytes_read: usize,
        nb_symbols: usize,
        table_log: u32,
        weights: [u8; SYMBOL_COUNT],
        rank_stats: [u32; RANK_COUNT],
    }

    fn c_read_stats(src: &[u8]) -> Option<CStats> {
        let mut stats = CStats {
            bytes_read: 0,
            nb_symbols: 0,
            table_log: 0,
            weights: [0; SYMBOL_COUNT],
            rank_stats: [0; RANK_COUNT],
        };
        let (mut nb_symbols, mut table_log) = (0, 0);
        let mut wksp = c_stats_workspace();
        // SAFETY: 256 weights, 13 ranks, src and the workspace valid for their lengths.
        let size = unsafe {
            HUF_readStats_wksp(
                stats.weights.as_mut_ptr(),
                SYMBOL_COUNT,
                stats.rank_stats.as_mut_ptr(),
                &raw mut nb_symbols,
                &raw mut table_log,
                src.as_ptr().cast(),
                src.len(),
                wksp.as_mut_ptr().cast(),
                wksp.len() * 4,
                0,
            )
        };
        if is_error(size) {
            return None;
        }
        (stats.bytes_read, stats.nb_symbols, stats.table_log) =
            (size, nb_symbols as usize, table_log);
        Some(stats)
    }

    // The table libzstd builds from `src`: (tree size, table log, is X2, cells).
    fn c_read_table(src: &[u8], double: bool) -> Option<(usize, Vec<u32>)> {
        let mut table = c_table();
        let mut wksp = c_workspace();
        let read = if double {
            HUF_readDTableX2_wksp
        } else {
            HUF_readDTableX1_wksp
        };
        // SAFETY: the table has room for 12 bits, src and the 16 KiB workspace are valid.
        let size = unsafe {
            read(
                table.as_mut_ptr(),
                src.as_ptr().cast(),
                src.len(),
                wksp.as_mut_ptr().cast(),
                wksp.len() * 8,
                0,
            )
        };
        (!is_error(size)).then_some((size, table))
    }

    fn c_decode(kind: Kind, src: &[u8], dst_size: usize, flags: c_int) -> Option<Vec<u8>> {
        let mut dst = vec![0xAA; dst_size];
        let mut table = c_table();
        let mut wksp = c_workspace();
        let wksp_size = wksp.len() * 8;
        let (dst_ptr, src_ptr) = (
            dst.as_mut_ptr().cast::<c_void>(),
            src.as_ptr().cast::<c_void>(),
        );
        // SAFETY: dst, src and the workspace are valid for the sizes passed and the table
        // holds a 12 bit tree.
        let size = unsafe {
            match kind {
                Kind::OneX1 => HUF_decompress1X1_DCtx_wksp(
                    table.as_mut_ptr(),
                    dst_ptr,
                    dst_size,
                    src_ptr,
                    src.len(),
                    wksp.as_mut_ptr().cast(),
                    wksp_size,
                    flags,
                ),
                Kind::OneX2 => HUF_decompress1X2_DCtx_wksp(
                    table.as_mut_ptr(),
                    dst_ptr,
                    dst_size,
                    src_ptr,
                    src.len(),
                    wksp.as_mut_ptr().cast(),
                    wksp_size,
                    flags,
                ),
                Kind::Four => HUF_decompress4X_hufOnly_wksp(
                    table.as_mut_ptr(),
                    dst_ptr,
                    dst_size,
                    src_ptr,
                    src.len(),
                    wksp.as_mut_ptr().cast(),
                    wksp_size,
                    flags,
                ),
                Kind::FourX1 | Kind::FourX2 => {
                    let read = if kind == Kind::FourX2 {
                        HUF_readDTableX2_wksp
                    } else {
                        HUF_readDTableX1_wksp
                    };
                    let tree_size = read(
                        table.as_mut_ptr(),
                        src_ptr,
                        src.len(),
                        wksp.as_mut_ptr().cast(),
                        wksp_size,
                        flags,
                    );
                    if is_error(tree_size) {
                        return None;
                    }
                    if tree_size >= src.len() {
                        return None;
                    }
                    HUF_decompress4X_usingDTable(
                        dst_ptr,
                        dst_size,
                        src.as_ptr().add(tree_size).cast(),
                        src.len() - tree_size,
                        table.as_ptr(),
                        flags,
                    )
                }
            }
        };
        (!is_error(size)).then_some(dst)
    }

    fn our_decode(kind: Kind, src: &[u8], dst_size: usize) -> Option<Vec<u8>> {
        let mut dst = vec![0xAA; dst_size];
        let mut table = HufTable::boxed();
        let result = (|| {
            match kind {
                Kind::OneX1 => {
                    let tree_size = table.read_table_1x(src)?;
                    return table.decompress_1x(&mut dst, &src[tree_size..]);
                }
                Kind::Four => {
                    let tree_size = table.read_table_4x(dst.len(), src)?;
                    return table.decompress_4x(&mut dst, &src[tree_size..]);
                }
                _ => {}
            }
            let tree_size = if matches!(kind, Kind::OneX2 | Kind::FourX2) {
                table.read_x2(src)?
            } else {
                table.read_x1(src)?
            };
            if tree_size >= src.len() {
                return Err(Error::SrcSizeWrong);
            }
            if matches!(kind, Kind::OneX2) {
                table.decompress_1x(&mut dst, &src[tree_size..])
            } else {
                table.decompress_4x(&mut dst, &src[tree_size..])
            }
        })();
        result.ok().map(|()| dst)
    }

    fn weights_of(stats: &CStats) -> &[u8] {
        &stats.weights[..stats.nb_symbols]
    }

    // Cells of libzstd's table in our layout.
    fn c_cells_x1(table: &[u32], count: usize) -> Vec<EltX1> {
        (0..count)
            .map(|i| {
                let cell = (table[1 + i / 2] >> (16 * (i % 2))) as u16;
                EltX1(cell)
            })
            .collect()
    }

    fn c_cells_x2(table: &[u32], count: usize) -> Vec<EltX2> {
        table[1..=count]
            .iter()
            .map(|&cell| EltX2 {
                sequence: cell as u16,
                nb_bits: (cell >> 16) as u8,
                length: (cell >> 24) as u8,
            })
            .collect()
    }

    // Trees and streams from libzstd on varied data, with how many streams each.
    struct Sample {
        data: Vec<u8>,
        compressed: Vec<u8>,
        four_streams: bool,
    }

    fn samples(rng: &mut Rng, count: usize) -> Vec<Sample> {
        let mut samples = Vec::new();
        while samples.len() < count {
            let data = skewed_bytes(rng);
            let four_streams = rng.bool();
            let max_log = rng.choice([9, 10, 11, 12, 12]).unwrap();
            if let Some(compressed) = c_huf::compress(&data, four_streams, max_log) {
                samples.push(Sample {
                    data,
                    compressed,
                    four_streams,
                });
            }
        }
        samples
    }

    // Random edits aimed at the places that matter: stream ends, the jump
    // table, the tree, and the end of the input.
    fn mutate(rng: &mut Rng, sample: &Sample) -> Vec<u8> {
        let mut bytes = sample.compressed.clone();
        let tree_size = c_read_stats(&bytes).map_or(1, |s| s.bytes_read);
        for _ in 0..=rng.usize(..3) {
            let len = bytes.len();
            match rng.usize(..6) {
                0 => bytes[rng.usize(..len)] ^= 1 << rng.usize(..8),
                1 => bytes[rng.usize(..len)] = rng.u8(..),
                2 if sample.four_streams && len > tree_size + 6 => {
                    // The last byte of one of the streams.
                    let at = tree_size + 6;
                    let lengths: Vec<usize> = (0..3)
                        .map(|i| {
                            usize::from(u16::from_le_bytes([
                                bytes[tree_size + 2 * i],
                                bytes[tree_size + 2 * i + 1],
                            ]))
                        })
                        .collect();
                    let ends = [
                        at + lengths[0],
                        at + lengths[0] + lengths[1],
                        at + lengths[0] + lengths[1] + lengths[2],
                        len,
                    ];
                    let end = ends[rng.usize(..4)].clamp(1, len);
                    let flip = rng.choice([1u8, 0x80, 0x10, 0xFF, 0]).unwrap();
                    bytes[end - 1] = if flip == 0xFF {
                        rng.u8(..)
                    } else {
                        bytes[end - 1] ^ flip
                    };
                }
                3 if sample.four_streams && len > tree_size + 6 => {
                    let at = tree_size + 2 * rng.usize(..3);
                    let value = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
                    let delta = rng.choice([1u16, 2, 8, 0xFFFF, 0xFFFE, 0xFFF8]).unwrap();
                    bytes[at..at + 2].copy_from_slice(&value.wrapping_add(delta).to_le_bytes());
                }
                4 => bytes.truncate(len - 1 - rng.usize(..3.min(len))),
                _ => bytes[rng.usize(..tree_size.min(len))] ^= 1 << rng.usize(..8),
            }
            if bytes.is_empty() {
                bytes.push(0);
            }
        }
        bytes
    }

    #[test]
    fn read_stats_agrees_with_libzstd() {
        let mut rng = Rng::with_seed(0x57A7_5001);
        let (mut accepted, mut rejected) = (0, 0);
        let mut check = |src: &[u8]| {
            let mut weights = [0; SYMBOL_COUNT];
            let mut rank_stats = [0; RANK_COUNT];
            let ours = read_stats(&mut weights, &mut rank_stats, src);
            match (ours, c_read_stats(src)) {
                (Ok(ours), Some(theirs)) => {
                    assert_eq!(ours.bytes_read, theirs.bytes_read, "{src:?}");
                    assert_eq!(ours.nb_symbols, theirs.nb_symbols, "{src:?}");
                    assert_eq!(ours.table_log, theirs.table_log, "{src:?}");
                    assert_eq!(weights[..ours.nb_symbols], *weights_of(&theirs), "{src:?}");
                    assert_eq!(rank_stats, theirs.rank_stats, "{src:?}");
                    accepted += 1;
                }
                (Err(_), None) => rejected += 1,
                (ours, theirs) => panic!(
                    "{src:?}: ours {:?}, theirs ok {}",
                    ours.map(|s| s.bytes_read),
                    theirs.is_some()
                ),
            }
        };
        for sample in samples(&mut rng, 150) {
            check(&sample.compressed);
            for _ in 0..30 {
                check(&mutate(&mut rng, &sample));
            }
        }
        // Raw 4-bit headers and FSE headers from random bytes.
        for _ in 0..20_000 {
            let mut src: Vec<u8> = (0..=rng.usize(..40)).map(|_| rng.u8(..)).collect();
            if rng.bool() {
                src[0] = 0x80 + rng.usize(..128) as u8;
            }
            check(&src);
        }
        // FSE headers (table log 5, symbols 1 and 17): table log 5 allows symbols up to 91, and
        // only weights above 12 are an error.
        let ncount = [16, 136, 241, 239, 7];
        for _ in 0..2000 {
            let length = 3 + rng.usize(..8);
            let mut src = vec![(ncount.len() + length) as u8];
            src.extend(ncount);
            src.extend((0..length).map(|_| rng.u8(..)));
            check(&src);
        }
        assert!(
            accepted > 2000 && rejected > 2000,
            "{accepted} accepted, {rejected} rejected"
        );
    }

    #[test]
    fn tables_agree_with_libzstd() {
        let mut rng = Rng::with_seed(0x007A_B1E5);
        let mut logs = [0usize; 13];
        let mut check = |src: &[u8]| {
            for double in [false, true] {
                let mut ours = HufTable::boxed();
                let result = if double {
                    ours.read_x2(src)
                } else {
                    ours.read_x1(src)
                };
                match (result, c_read_table(src, double)) {
                    (Ok(size), Some((c_size, c_table))) => {
                        let dtd = c_table[0].to_le_bytes();
                        let cells = 1usize << ours.table_log;
                        assert_eq!(
                            (size, ours.table_log),
                            (c_size, u32::from(dtd[2])),
                            "{src:?}"
                        );
                        assert_eq!(ours.is_x2, dtd[1] == 1);
                        if double {
                            assert_eq!(ours.x2[..cells], c_cells_x2(&c_table, cells), "{src:?}");
                        } else {
                            assert_eq!(ours.x1[..cells], c_cells_x1(&c_table, cells), "{src:?}");
                        }
                        logs[ours.table_log as usize] += 1;
                    }
                    (Err(_), None) => {}
                    (ours, theirs) => {
                        panic!("{src:?}: ours {ours:?}, theirs ok {}", theirs.is_some())
                    }
                }
            }
        };
        for sample in samples(&mut rng, 400) {
            check(&sample.compressed);
            for _ in 0..10 {
                check(&mutate(&mut rng, &sample));
            }
        }
        assert!(logs[11] > 100 && logs[12] > 5, "table logs seen: {logs:?}");
    }

    // Both libzstd with and without its fast path, and ours, on one input.
    // Returns (accepted, accepted only thanks to the fast path in C).
    fn check_decode(kind: Kind, src: &[u8], dst_size: usize, what: &str) -> (bool, bool) {
        let theirs = c_decode(kind, src, dst_size, c_flags());
        let ours = our_decode(kind, src, dst_size);
        assert_eq!(
            ours,
            theirs,
            "{what}: {kind:?} dst {dst_size} src len {}",
            src.len()
        );
        let strict = c_decode(kind, src, dst_size, c_flags() | DISABLE_FAST);
        (theirs.is_some(), theirs.is_some() && strict.is_none())
    }

    #[test]
    fn decoding_agrees_with_libzstd_and_takes_its_lenient_path() {
        let mut rng = Rng::with_seed(0x00DE_C0DE);
        let mut lenient = [0usize; 2];
        let mut accepted = [0usize; 5];
        for sample in samples(&mut rng, 300) {
            let kinds: &[Kind] = if sample.four_streams {
                &[Kind::Four, Kind::FourX1, Kind::FourX2]
            } else {
                &[Kind::OneX1, Kind::OneX2]
            };
            for &kind in kinds {
                let size = sample.data.len();
                assert_eq!(
                    our_decode(kind, &sample.compressed, size),
                    Some(sample.data.clone())
                );
                let mut variants = vec![sample.compressed.clone()];
                variants.extend((0..15).map(|_| mutate(&mut rng, &sample)));
                for variant in variants {
                    let dst_size = match rng.usize(..6) {
                        0 => size + 1 + rng.usize(..8),
                        1 => size.saturating_sub(1 + rng.usize(..8)),
                        _ => size,
                    };
                    let (ok, only_fast) = check_decode(kind, &variant, dst_size, "mutated");
                    lenient[usize::from(matches!(kind, Kind::FourX2))] += usize::from(only_fast);
                    accepted[KINDS.iter().position(|&k| k == kind).unwrap()] += usize::from(ok);
                }
            }
        }
        assert!(
            lenient[0] > 20 && lenient[1] > 20,
            "lenient verdicts seen: {lenient:?}"
        );
        assert!(accepted.iter().all(|&n| n > 100), "accepted: {accepted:?}");
    }

    #[test]
    fn streams_of_other_data_and_random_bytes_agree_with_libzstd() {
        let mut rng = Rng::with_seed(0xF0_0F);
        let samples = samples(&mut rng, 150);
        let mut lenient = 0;
        for (index, sample) in samples.iter().enumerate().filter(|(_, s)| s.four_streams) {
            let tree_size = c_read_stats(&sample.compressed).unwrap().bytes_read;
            let other = &samples[(index * 7 + 1) % samples.len()];
            for round in 0..40 {
                // This tree, streams of another sample or random bytes under a valid jump table.
                let mut src = sample.compressed[..tree_size].to_vec();
                let body: Vec<u8> = if round % 2 == 0 && other.four_streams {
                    let other_tree = c_read_stats(&other.compressed).unwrap().bytes_read;
                    other.compressed[other_tree..].to_vec()
                } else {
                    let mut lengths: [usize; 4] = core::array::from_fn(|_| 8 + rng.usize(..300));
                    if rng.bool() {
                        // A short stream next to long ones: it can run into its neighbour.
                        lengths[rng.usize(..4)] = 8 + rng.usize(..8);
                    }
                    let mut body = Vec::new();
                    for length in &lengths[..3] {
                        body.extend_from_slice(&(*length as u16).to_le_bytes());
                    }
                    for length in lengths {
                        // Random bytes, or all ones and all zeros: shortest and longest codes.
                        let fill = rng.choice([None, Some(0x00), Some(0xFF)]).unwrap();
                        body.extend((0..length).map(|_| fill.unwrap_or_else(|| rng.u8(..))));
                    }
                    body
                };
                src.extend_from_slice(&body);
                for kind in [Kind::Four, Kind::FourX1, Kind::FourX2] {
                    let dst_size = (sample.data.len() / (1 + rng.usize(..4))).max(6);
                    lenient += usize::from(check_decode(kind, &src, dst_size, "foreign streams").1);
                }
            }
        }
        assert!(lenient > 50, "{lenient} lenient verdicts");
    }

    #[test]
    fn select_decoder_agrees_with_libzstd() {
        let mut rng = Rng::with_seed(0x5E1E);
        for _ in 0..50_000 {
            let dst_size = 1 + rng.usize(..128 << 10);
            let src_size = rng.usize(..dst_size + dst_size / 8 + 2);
            // SAFETY: pure function.
            let theirs = unsafe { HUF_selectDecoder(dst_size, src_size) };
            assert_eq!(
                select_decoder(dst_size, src_size),
                theirs == 1,
                "{dst_size} {src_size}"
            );
        }
    }

    // Repeated tables (treeless literals): one tree, streams of other data.
    #[test]
    fn repeated_table_agrees_with_libzstd() {
        let mut rng = Rng::with_seed(0x2EEA7);
        for sample in samples(&mut rng, 100) {
            let tree_size = c_read_stats(&sample.compressed).unwrap().bytes_read;
            let (mut ours_table, mut c_table_words) = (HufTable::boxed(), c_table());
            let mut wksp = c_workspace();
            let double = rng.bool();
            ours_table.read_x1(&sample.compressed).unwrap();
            if double {
                ours_table.read_x2(&sample.compressed).unwrap();
            }
            let read = if double {
                HUF_readDTableX2_wksp
            } else {
                HUF_readDTableX1_wksp
            };
            // SAFETY: valid buffers, as in `c_read_table`.
            unsafe {
                read(
                    c_table_words.as_mut_ptr(),
                    sample.compressed.as_ptr().cast(),
                    sample.compressed.len(),
                    wksp.as_mut_ptr().cast(),
                    wksp.len() * 8,
                    0,
                )
            };
            let streams = &sample.compressed[tree_size..];
            for dst_size in [
                sample.data.len(),
                sample.data.len() + 5,
                sample.data.len() / 2 + 6,
            ] {
                let mut ours = vec![0; dst_size];
                let mut theirs = vec![0; dst_size];
                let ours_result = if sample.four_streams {
                    ours_table.decompress_4x(&mut ours, streams)
                } else {
                    ours_table.decompress_1x(&mut ours, streams)
                };
                // SAFETY: valid buffers, the table was read above.
                let size = unsafe {
                    let decode = if sample.four_streams {
                        HUF_decompress4X_usingDTable
                    } else {
                        HUF_decompress1X_usingDTable
                    };
                    decode(
                        theirs.as_mut_ptr().cast(),
                        dst_size,
                        streams.as_ptr().cast(),
                        streams.len(),
                        c_table_words.as_ptr(),
                        c_flags(),
                    )
                };
                assert_eq!(ours_result.is_ok(), !is_error(size));
                if ours_result.is_ok() {
                    assert_eq!(ours, theirs);
                }
            }
        }
    }
}
