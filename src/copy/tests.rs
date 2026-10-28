use alloc::vec::Vec;

use super::*;

const HISTORY: usize = 64;

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 37 + 11) as u8).collect()
}

#[test]
fn copy_match_repeats_the_pattern_and_stays_in_the_slack() {
    check_copy_match(Portable);
    check_copy_match(ShortOffsets);
}

fn check_copy_match(kernel: impl Kernel) {
    for offset in (1..=70).chain([100, 1000]) {
        for len in 1..=150 {
            let history = offset.max(HISTORY);
            let mut buffer = pattern(history + len + WILDCOPY_OVERLENGTH + 8);
            let mut expected = buffer.clone();
            for i in 0..len {
                expected[history + i] = expected[history + i - offset];
            }
            let dst = buffer.as_mut_ptr().wrapping_add(history);
            // SAFETY: `history >= offset` bytes before, `len + 32` after `dst`.
            unsafe { kernel.copy_match(dst, offset, len) };
            let written = history + len;
            assert_eq!(
                buffer[..written],
                expected[..written],
                "offset {offset} len {len}"
            );
            let untouched = written + WILDCOPY_OVERLENGTH;
            assert_eq!(
                buffer[untouched..],
                expected[untouched..],
                "offset {offset} len {len}"
            );
        }
    }
}

#[test]
fn wildcopy_copies_len_bytes_and_stays_in_the_slack() {
    for len in 0..=150 {
        let source = pattern(len + WILDCOPY_OVERLENGTH);
        let mut destination = alloc::vec![0xAA; len + WILDCOPY_OVERLENGTH + 8];
        // SAFETY: both buffers have `len + 32` bytes at the start, and are disjoint.
        unsafe { Portable.wildcopy(destination.as_mut_ptr(), source.as_ptr(), len) };
        assert_eq!(destination[..len], source[..len], "len {len}");
        assert!(
            destination[len + WILDCOPY_OVERLENGTH..]
                .iter()
                .all(|&b| b == 0xAA)
        );
    }
}
