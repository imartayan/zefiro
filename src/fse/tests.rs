#![allow(clippy::unreadable_literal)] // libzstd's tables as published

use alloc::vec::Vec;

use super::*;
use crate::testing::Rng;

// Cells (next_state, nb_additional_bits, nb_bits, base_value) of libzstd's `*_defaultDTable`.
const LL_C_TABLE: [(u16, u8, u8, u32); 64] = [
    (0, 0, 4, 0),
    (16, 0, 4, 0),
    (32, 0, 5, 1),
    (0, 0, 5, 3),
    (0, 0, 5, 4),
    (0, 0, 5, 6),
    (0, 0, 5, 7),
    (0, 0, 5, 9),
    (0, 0, 5, 10),
    (0, 0, 5, 12),
    (0, 0, 6, 14),
    (0, 1, 5, 16),
    (0, 1, 5, 20),
    (0, 1, 5, 22),
    (0, 2, 5, 28),
    (0, 3, 5, 32),
    (0, 4, 5, 48),
    (32, 6, 5, 64),
    (0, 7, 5, 128),
    (0, 8, 6, 256),
    (0, 10, 6, 1024),
    (0, 12, 6, 4096),
    (32, 0, 4, 0),
    (0, 0, 4, 1),
    (0, 0, 5, 2),
    (32, 0, 5, 4),
    (0, 0, 5, 5),
    (32, 0, 5, 7),
    (0, 0, 5, 8),
    (32, 0, 5, 10),
    (0, 0, 5, 11),
    (0, 0, 6, 13),
    (32, 1, 5, 16),
    (0, 1, 5, 18),
    (32, 1, 5, 22),
    (0, 2, 5, 24),
    (32, 3, 5, 32),
    (0, 3, 5, 40),
    (0, 6, 4, 64),
    (16, 6, 4, 64),
    (32, 7, 5, 128),
    (0, 9, 6, 512),
    (0, 11, 6, 2048),
    (48, 0, 4, 0),
    (16, 0, 4, 1),
    (32, 0, 5, 2),
    (32, 0, 5, 3),
    (32, 0, 5, 5),
    (32, 0, 5, 6),
    (32, 0, 5, 8),
    (32, 0, 5, 9),
    (32, 0, 5, 11),
    (32, 0, 5, 12),
    (0, 0, 6, 15),
    (32, 1, 5, 18),
    (32, 1, 5, 20),
    (32, 2, 5, 24),
    (32, 2, 5, 28),
    (32, 3, 5, 40),
    (32, 4, 5, 48),
    (0, 16, 6, 65536),
    (0, 15, 6, 32768),
    (0, 14, 6, 16384),
    (0, 13, 6, 8192),
];
const OF_C_TABLE: [(u16, u8, u8, u32); 32] = [
    (0, 0, 5, 0),
    (0, 6, 4, 61),
    (0, 9, 5, 509),
    (0, 15, 5, 32765),
    (0, 21, 5, 2097149),
    (0, 3, 5, 5),
    (0, 7, 4, 125),
    (0, 12, 5, 4093),
    (0, 18, 5, 262141),
    (0, 23, 5, 8388605),
    (0, 5, 5, 29),
    (0, 8, 4, 253),
    (0, 14, 5, 16381),
    (0, 20, 5, 1048573),
    (0, 2, 5, 1),
    (16, 7, 4, 125),
    (0, 11, 5, 2045),
    (0, 17, 5, 131069),
    (0, 22, 5, 4194301),
    (0, 4, 5, 13),
    (16, 8, 4, 253),
    (0, 13, 5, 8189),
    (0, 19, 5, 524285),
    (0, 1, 5, 1),
    (16, 6, 4, 61),
    (0, 10, 5, 1021),
    (0, 16, 5, 65533),
    (0, 28, 5, 268435453),
    (0, 27, 5, 134217725),
    (0, 26, 5, 67108861),
    (0, 25, 5, 33554429),
    (0, 24, 5, 16777213),
];
const ML_C_TABLE: [(u16, u8, u8, u32); 64] = [
    (0, 0, 6, 3),
    (0, 0, 4, 4),
    (32, 0, 5, 5),
    (0, 0, 5, 6),
    (0, 0, 5, 8),
    (0, 0, 5, 9),
    (0, 0, 5, 11),
    (0, 0, 6, 13),
    (0, 0, 6, 16),
    (0, 0, 6, 19),
    (0, 0, 6, 22),
    (0, 0, 6, 25),
    (0, 0, 6, 28),
    (0, 0, 6, 31),
    (0, 0, 6, 34),
    (0, 1, 6, 37),
    (0, 1, 6, 41),
    (0, 2, 6, 47),
    (0, 3, 6, 59),
    (0, 4, 6, 83),
    (0, 7, 6, 131),
    (0, 9, 6, 515),
    (16, 0, 4, 4),
    (0, 0, 4, 5),
    (32, 0, 5, 6),
    (0, 0, 5, 7),
    (32, 0, 5, 9),
    (0, 0, 5, 10),
    (0, 0, 6, 12),
    (0, 0, 6, 15),
    (0, 0, 6, 18),
    (0, 0, 6, 21),
    (0, 0, 6, 24),
    (0, 0, 6, 27),
    (0, 0, 6, 30),
    (0, 0, 6, 33),
    (0, 1, 6, 35),
    (0, 1, 6, 39),
    (0, 2, 6, 43),
    (0, 3, 6, 51),
    (0, 4, 6, 67),
    (0, 5, 6, 99),
    (0, 8, 6, 259),
    (32, 0, 4, 4),
    (48, 0, 4, 4),
    (16, 0, 4, 5),
    (32, 0, 5, 7),
    (32, 0, 5, 8),
    (32, 0, 5, 10),
    (32, 0, 5, 11),
    (0, 0, 6, 14),
    (0, 0, 6, 17),
    (0, 0, 6, 20),
    (0, 0, 6, 23),
    (0, 0, 6, 26),
    (0, 0, 6, 29),
    (0, 0, 6, 32),
    (0, 16, 6, 65539),
    (0, 15, 6, 32771),
    (0, 14, 6, 16387),
    (0, 13, 6, 8195),
    (0, 12, 6, 4099),
    (0, 11, 6, 2051),
    (0, 10, 6, 1027),
];

// Port of `FSE_writeNCount_generic` (libzstd compress side).
fn write_ncount(norm: &[i16], table_log: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut bit_stream = 0u32;
    let mut bit_count = 4;
    bit_stream += table_log - MIN_TABLE_LOG as u32;
    let mut remaining = (1i32 << table_log) + 1;
    let mut threshold = 1i32 << table_log;
    let mut nb_bits = table_log as i32 + 1;
    let mut symbol = 0;
    let mut previous_is_zero = false;
    let flush = |out: &mut Vec<u8>, bit_stream: &mut u32, bit_count: &mut i32| {
        out.extend_from_slice(&(*bit_stream as u16).to_le_bytes());
        *bit_stream >>= 16;
        *bit_count -= 16;
    };
    while symbol < norm.len() && remaining > 1 {
        if previous_is_zero {
            let mut start = symbol;
            while symbol < norm.len() && norm[symbol] == 0 {
                symbol += 1;
            }
            if symbol == norm.len() {
                break;
            }
            while symbol >= start + 24 {
                start += 24;
                bit_stream += 0xFFFF << bit_count;
                out.extend_from_slice(&(bit_stream as u16).to_le_bytes());
                bit_stream >>= 16;
            }
            while symbol >= start + 3 {
                start += 3;
                bit_stream += 3 << bit_count;
                bit_count += 2;
            }
            bit_stream += ((symbol - start) as u32) << bit_count;
            bit_count += 2;
            if bit_count > 16 {
                flush(&mut out, &mut bit_stream, &mut bit_count);
            }
        }
        let mut count = i32::from(norm[symbol]);
        symbol += 1;
        let max = (2 * threshold - 1) - remaining;
        remaining -= count.abs();
        count += 1;
        if count >= threshold {
            count += max;
        }
        bit_stream += (count as u32) << bit_count;
        bit_count += nb_bits;
        bit_count -= i32::from(count < max);
        previous_is_zero = count == 1;
        assert!(remaining >= 1);
        while remaining < threshold {
            nb_bits -= 1;
            threshold >>= 1;
        }
        if bit_count > 16 {
            flush(&mut out, &mut bit_stream, &mut bit_count);
        }
    }
    assert_eq!(remaining, 1);
    out.extend_from_slice(&(bit_stream as u16).to_le_bytes());
    out.truncate(out.len() - 2 + ((bit_count + 7) / 8) as usize);
    out
}

// A random valid distribution: counts of -1, 0 (in runs) and positive ones,
// summing to `1 << table_log`, ending on a non-zero count.
fn random_norm(rng: &mut Rng) -> (Vec<i16>, u32) {
    let table_log = 5 + rng.usize(..5) as u32;
    let mut left = 1i32 << table_log;
    let mut norm: Vec<i16> = Vec::new();
    while left > 0 && norm.len() < 200 {
        if !norm.is_empty() && norm[norm.len() - 1] != 0 && rng.usize(..5) == 0 {
            let run = [1, 2, 3, 5, 24, 25, 40][rng.usize(..7)];
            norm.extend((0..run).map(|_| 0));
        }
        let wanted = match rng.usize(..3) {
            0 => 1,
            1 => 1 + rng.usize(..4) as i32,
            _ => 1 + rng.i32(0..left),
        };
        let count = wanted.min(left);
        left -= count;
        norm.push(if count == 1 && rng.usize(..2) == 0 {
            -1
        } else {
            count as i16
        });
    }
    if left > 0 {
        // Too many symbols: put the rest on the last one.
        *norm.last_mut().unwrap() = (i32::from(*norm.last().unwrap()).abs() + left) as i16;
    }
    (norm, table_log)
}

#[test]
fn predefined_tables_match_libzstd() {
    fn check<const N: usize, const USED: usize>(
        table: &FseTable<N>,
        table_log: u32,
        expected: &[(u16, u8, u8, u32); USED],
    ) {
        assert_eq!(table.table_log, table_log);
        assert_eq!(1 << table_log, USED);
        for (cell, &(next_state, extra_bits, nb_bits, base_value)) in
            table.entries.iter().zip(expected)
        {
            let expected = SeqSymbol {
                next_state,
                nb_additional_bits: extra_bits,
                nb_bits,
                base_value,
            };
            assert_eq!(*cell, expected);
        }
        assert!(
            table.entries[USED..]
                .iter()
                .all(|cell| *cell == SeqSymbol::default())
        );
    }
    check(&LL_DEFAULT_TABLE, 6, &LL_C_TABLE);
    check(&ML_DEFAULT_TABLE, 6, &ML_C_TABLE);
    check(&OF_DEFAULT_TABLE, 5, &OF_C_TABLE);
}

#[test]
fn default_norms_sum_to_their_table_size() {
    let sum = |norm: &[i16]| norm.iter().map(|&c| i32::from(c).abs()).sum::<i32>();
    assert_eq!(sum(&LL_DEFAULT_NORM), 64);
    assert_eq!(sum(&ML_DEFAULT_NORM), 64);
    assert_eq!(sum(&OF_DEFAULT_NORM), 32);
}

#[test]
fn ncount_round_trips_default_and_random_distributions() {
    let mut rng = Rng::with_seed(0xA5A5_1234_9876_5432);
    let defaults: [(&[i16], u32); 3] = [
        (&LL_DEFAULT_NORM, 6),
        (&ML_DEFAULT_NORM, 6),
        (&OF_DEFAULT_NORM, 5),
    ];
    let mut cases: Vec<(Vec<i16>, u32)> =
        defaults.iter().map(|&(n, log)| (n.to_vec(), log)).collect();
    cases.extend((0..3000).map(|_| random_norm(&mut rng)));
    for (norm, table_log) in cases {
        let mut header = write_ncount(&norm, table_log);
        let written = header.len();
        // Garbage after the header must not be read as part of it.
        header.extend((0..rng.usize(..9)).map(|_| rng.u8(..)));
        let mut parsed = [7i16; 256];
        let ncount = read_ncount(&mut parsed, &header).unwrap_or_else(|e| panic!("{norm:?}: {e}"));
        let last_nonzero = norm.iter().rposition(|&c| c != 0).unwrap();
        assert_eq!(ncount.table_log, table_log, "{norm:?}");
        assert_eq!(ncount.max_symbol as usize, last_nonzero, "{norm:?}");
        assert_eq!(ncount.bytes_read, written, "{norm:?}");
        assert_eq!(&parsed[..=last_nonzero], &norm[..=last_nonzero], "{norm:?}");
        assert!(parsed[last_nonzero + 1..].iter().all(|&c| c == 0));
    }
}

#[test]
fn ncount_rejects_what_libzstd_rejects() {
    let mut norm = [0i16; 256];
    // Table log 20 (nibble 15 + 5) exceeds 15.
    assert!(read_ncount(&mut norm, &[0x0F, 0, 0, 0, 0, 0, 0, 0]).is_err());
    assert!(read_ncount(&mut norm, &[0x0F]).is_err());
    assert!(read_ncount(&mut norm, &[]).is_err());
    // More symbols than the caller allows (LL: 36), distribution not finished.
    let header = write_ncount(&ML_DEFAULT_NORM, 6);
    assert!(read_ncount(&mut norm[..36], &header).is_err());
    assert!(read_ncount(&mut norm[..53], &header).is_ok());
    // Header cut short: the zero padding cannot complete it.
    for cut in 1..header.len() {
        let result = read_ncount(&mut norm, &header[..cut]);
        assert!(
            result.is_err() || result.unwrap().bytes_read <= cut,
            "cut {cut}"
        );
    }
}

#[test]
fn built_tables_are_consistent() {
    let mut rng = Rng::with_seed(0x00DD_BA11);
    for _ in 0..500 {
        let (norm, table_log) = random_norm(&mut rng);
        let identity: Vec<u32> = (0..256).collect();
        let mut table = FseTable::<512>::EMPTY;
        table.build(&norm, table_log, &identity, &[0; 256]);
        let size = 1usize << table_log;
        let mut seen = [0u32; 256];
        for cell in &table.entries[..size] {
            seen[cell.base_value as usize] += 1;
            // Reading nb_bits then adding next_state stays inside the table.
            assert!(u32::from(cell.next_state) + (1 << cell.nb_bits) <= size as u32 * 2);
            assert!((1u32 << cell.nb_bits) + u32::from(cell.next_state) > 0);
        }
        for (symbol, &count) in norm.iter().enumerate() {
            assert_eq!(
                seen[symbol],
                u32::from(count.unsigned_abs()),
                "symbol {symbol}"
            );
        }
    }
}

#[test]
fn rle_table_has_one_cell_without_state_bits() {
    let mut table = FseTable::<512>::EMPTY;
    table.set_rle(ML_BASE[40], ML_EXTRA_BITS[40]);
    assert_eq!(table.table_log, 0);
    assert_eq!(
        table.entries[0],
        SeqSymbol {
            next_state: 0,
            nb_additional_bits: 4,
            nb_bits: 0,
            base_value: 67
        }
    );
}

#[cfg(not(target_family = "wasm"))]
mod against_libzstd {
    use core::ffi::c_void;

    use zstd_sys::ZSTD_isError;

    use super::*;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CSeqSymbol {
        next_state: u16,
        nb_additional_bits: u8,
        nb_bits: u8,
        base_value: u32,
    }

    unsafe extern "C" {
        fn FSE_readNCount(
            norm: *mut i16,
            max_symbol: *mut u32,
            table_log: *mut u32,
            src: *const c_void,
            size: usize,
        ) -> usize;
        fn FSE_buildDTable_wksp(
            dt: *mut u32,
            norm: *const i16,
            max_symbol: u32,
            table_log: u32,
            wksp: *mut c_void,
            wksp_size: usize,
        ) -> usize;
        fn ZSTD_buildFSETable(
            dt: *mut CSeqSymbol,
            norm: *const i16,
            max_symbol: u32,
            base: *const u32,
            extra_bits: *const u8,
            table_log: u32,
            wksp: *mut c_void,
            wksp_size: usize,
            bmi2: i32,
        ) -> ();
    }

    // `Some((max_symbol, table_log, bytes_read))` and fills `norm` on success.
    fn c_read_ncount(norm: &mut [i16], src: &[u8]) -> Option<(u32, u32, usize)> {
        let mut max_symbol = norm.len() as u32 - 1;
        let mut table_log = 0;
        // SAFETY: norm has max_symbol + 1 entries, src is valid for its length.
        let result = unsafe {
            FSE_readNCount(
                norm.as_mut_ptr(),
                &raw mut max_symbol,
                &raw mut table_log,
                src.as_ptr().cast(),
                src.len(),
            )
        };
        // SAFETY: pure function of its argument.
        (unsafe { ZSTD_isError(result) } == 0).then_some((max_symbol, table_log, result))
    }

    fn check_tables(norm: &[i16], max_symbol: u32, table_log: u32) {
        let size = 1usize << table_log;
        let counts = &norm[..=max_symbol as usize];

        let mut dt = [0u32; 1 + 512];
        let mut wksp = [0u32; 2048];
        // SAFETY: dt holds 1 + (1 << table_log) cells, wksp is large and aligned.
        let result = unsafe {
            FSE_buildDTable_wksp(
                dt.as_mut_ptr(),
                counts.as_ptr(),
                max_symbol,
                table_log,
                wksp.as_mut_ptr().cast(),
                8192,
            )
        };
        assert_eq!(result, 0);
        let identity: Vec<u32> = (0..256).collect();
        let mut ours = FseTable::<512>::EMPTY;
        ours.build(counts, table_log, &identity, &[0; 256]);
        assert_eq!(u32::from(dt[0] as u16), table_log);
        for (cell, &c) in ours.entries[..size].iter().zip(&dt[1..]) {
            let theirs = (c as u16, (c >> 16) as u8, (c >> 24) as u8);
            assert_eq!(
                (cell.next_state, cell.base_value as u8, cell.nb_bits),
                theirs,
                "{counts:?}"
            );
            assert_eq!(cell.base_value, u32::from(theirs.1));
        }

        if max_symbol <= ML_MAX_SYMBOL {
            let mut c_table = [CSeqSymbol::default(); 1 + 512];
            // SAFETY: valid distribution, dt has 1 + 512 cells, wksp is aligned and large enough.
            unsafe {
                ZSTD_buildFSETable(
                    c_table.as_mut_ptr(),
                    counts.as_ptr(),
                    max_symbol,
                    ML_BASE.as_ptr(),
                    ML_EXTRA_BITS.as_ptr(),
                    table_log,
                    wksp.as_mut_ptr().cast(),
                    8192,
                    0,
                );
            }
            ours.build(counts, table_log, &ML_BASE, &ML_EXTRA_BITS);
            assert_eq!(c_table[0].base_value, table_log);
            for (cell, theirs) in ours.entries[..size].iter().zip(&c_table[1..]) {
                let expected = SeqSymbol {
                    next_state: theirs.next_state,
                    nb_additional_bits: theirs.nb_additional_bits,
                    nb_bits: theirs.nb_bits,
                    base_value: theirs.base_value,
                };
                assert_eq!(*cell, expected, "{counts:?}");
            }
        }
    }

    #[test]
    fn ncount_matches_libzstd_on_random_and_mutated_headers() {
        let mut rng = Rng::with_seed(0x1357_9BDF_2468_ACE0);
        let (mut accepted, mut rejected) = (0, 0);
        for round in 0..60_000 {
            let header: Vec<u8> = if round % 2 == 0 {
                let len = 1 + rng.usize(..40);
                (0..len)
                    .map(|_| {
                        if rng.usize(..4) == 0 {
                            0xFF
                        } else {
                            rng.u8(..)
                        }
                    })
                    .collect()
            } else {
                let (norm, table_log) = random_norm(&mut rng);
                let mut header = write_ncount(&norm, table_log);
                for _ in 0..rng.usize(..3) {
                    let at = rng.usize(..header.len());
                    header[at] ^= 1 << rng.usize(..8);
                }
                header.truncate(header.len() - rng.usize(..2));
                header
            };
            let limit = [36, 53, 32, 256, 1 + rng.usize(..255)][rng.usize(..5)];
            let (mut theirs, mut ours) = ([5i16; 256], [5i16; 256]);
            let c_result = c_read_ncount(&mut theirs[..limit], &header);
            let rust_result = read_ncount(&mut ours[..limit], &header);
            match (c_result, rust_result) {
                (Some((max_symbol, table_log, bytes_read)), Ok(ncount)) => {
                    accepted += 1;
                    assert_eq!(
                        (ncount.max_symbol, ncount.table_log, ncount.bytes_read),
                        (max_symbol, table_log, bytes_read),
                        "{header:?}"
                    );
                    assert_eq!(theirs[..limit], ours[..limit], "{header:?}");
                    if table_log <= 9 {
                        check_tables(&ours, max_symbol, table_log);
                    }
                }
                (None, Err(Error::CorruptionDetected)) => rejected += 1,
                (c, r) => panic!("limit {limit} header {header:?}: libzstd {c:?}, ours {r:?}"),
            }
        }
        assert!(
            accepted > 5000 && rejected > 5000,
            "{accepted} accepted, {rejected} rejected"
        );
    }

    #[test]
    fn default_tables_match_libzstd_builder() {
        let mut wksp = [0u32; 2048];
        type Case<'a> = (&'a [i16], u32, &'a [u32], &'a [u8], &'a [SeqSymbol]);
        let cases: [Case<'_>; 3] = [
            (
                &LL_DEFAULT_NORM,
                6,
                &LL_BASE,
                &LL_EXTRA_BITS,
                &LL_DEFAULT_TABLE.entries,
            ),
            (
                &ML_DEFAULT_NORM,
                6,
                &ML_BASE,
                &ML_EXTRA_BITS,
                &ML_DEFAULT_TABLE.entries,
            ),
            (
                &OF_DEFAULT_NORM,
                5,
                &OF_BASE,
                &OF_EXTRA_BITS,
                &OF_DEFAULT_TABLE.entries,
            ),
        ];
        for (norm, table_log, base, extra, ours) in cases {
            let mut c_table = [CSeqSymbol::default(); 1 + 512];
            // SAFETY: valid default distribution, big enough table and aligned workspace.
            unsafe {
                ZSTD_buildFSETable(
                    c_table.as_mut_ptr(),
                    norm.as_ptr(),
                    norm.len() as u32 - 1,
                    base.as_ptr(),
                    extra.as_ptr(),
                    table_log,
                    wksp.as_mut_ptr().cast(),
                    8192,
                    0,
                );
            }
            for (cell, theirs) in ours.iter().zip(&c_table[1..]).take(1 << table_log) {
                assert_eq!(
                    (
                        cell.next_state,
                        cell.nb_additional_bits,
                        cell.nb_bits,
                        cell.base_value
                    ),
                    (
                        theirs.next_state,
                        theirs.nb_additional_bits,
                        theirs.nb_bits,
                        theirs.base_value
                    )
                );
            }
        }
    }
}

#[test]
fn weights_workspace_limits_the_described_symbols() {
    let limit = workspace_words(WEIGHTS_TABLE_LOG_MAX, 11);
    // Table log 6: symbols 0..=11 fit, 12 does not. Table log 5 leaves room up to 91.
    assert!(workspace_words(6, 11) <= limit && workspace_words(6, 12) > limit);
    assert!(workspace_words(5, 91) <= limit && workspace_words(5, 92) > limit);
}
