//! Wild copies of sequence execution (libzstd's `ZSTD_wildcopy` family).
//!
//! Slack contract: unless stated otherwise a copy of `len` bytes may read and
//! write up to `WILDCOPY_OVERLENGTH` bytes past `len`, at the source and the
//! destination. The caller owns that room, and the bytes written there are
//! garbage the next write overwrites.

// Bytes of slack after the last byte read or written by a wild copy.
pub(crate) const WILDCOPY_OVERLENGTH: usize = 32;
const CHUNK: usize = 16;

// The operations a wider instruction set would change.
pub(crate) trait Kernel: Copy {
    // Copies `len` bytes in chunks. Safety: readable and writable slack, and
    // the ranges are `CHUNK` bytes apart or disjoint.
    unsafe fn wildcopy(self, dst: *mut u8, src: *const u8, len: usize);

    // Copies the `len >= 1` bytes of a match that starts `offset >= 1` bytes
    // before `dst`, repeating the pattern when `offset < len`. Safety: slack
    // after `dst + len`, and `offset` bytes before `dst` are initialized.
    unsafe fn copy_match(self, dst: *mut u8, offset: usize, len: usize);
}

// 16-byte chunks, as vector loads and stores on the targets that have them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Portable;

impl Kernel for Portable {
    #[inline(always)]
    unsafe fn wildcopy(self, mut dst: *mut u8, mut src: *const u8, len: usize) {
        // SAFETY: chunks stay within `len + 31` bytes of the starts, the slack.
        unsafe {
            copy16(dst, src);
            if len <= CHUNK {
                return;
            }
            let end = dst.add(len);
            dst = dst.add(CHUNK);
            src = src.add(CHUNK);
            loop {
                copy16(dst, src);
                copy16(dst.add(CHUNK), src.add(CHUNK));
                dst = dst.add(2 * CHUNK);
                src = src.add(2 * CHUNK);
                if dst >= end {
                    break;
                }
            }
        }
    }

    #[inline(always)]
    unsafe fn copy_match(self, dst: *mut u8, offset: usize, len: usize) {
        // SAFETY: each branch keeps its chunks as far from their stores as its copy needs;
        // slack and history are the caller's.
        unsafe {
            let src = dst.sub(offset).cast_const();
            if offset >= 2 * CHUNK {
                // Chunks do not alias their own stores: two straight copies, then the rest.
                copy16(dst, src);
                copy16(dst.add(CHUNK), src.add(CHUNK));
                if len > 2 * CHUNK {
                    self.wildcopy(dst.add(2 * CHUNK), src.add(2 * CHUNK), len - 2 * CHUNK);
                }
            } else if offset >= CHUNK {
                self.wildcopy(dst, src, len);
            } else {
                let (dst, src) = overlap_copy8(dst, src, offset);
                if len > 8 {
                    wildcopy_overlap8(dst, src, len - 8);
                }
            }
        }
    }
}

// Match copies without a branch on the offset, for blocks whose short offsets
// make the tests of `Portable::copy_match` unpredictable: the first 16 bytes
// take the same straight line at every offset, the rest 8 bytes at a time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ShortOffsets;

impl Kernel for ShortOffsets {
    #[inline(always)]
    unsafe fn wildcopy(self, dst: *mut u8, src: *const u8, len: usize) {
        // SAFETY: the caller's.
        unsafe { Portable.wildcopy(dst, src, len) }
    }

    #[inline(always)]
    unsafe fn copy_match(self, dst: *mut u8, offset: usize, len: usize) {
        // SAFETY: after `overlap_copy8`, `dst - src >= 8`: the second chunk and the loop
        // keep to the contract of `wildcopy_overlap8`, and overshoot by under 8 bytes.
        unsafe {
            let (dst, src) = overlap_copy8(dst, dst.sub(offset).cast_const(), offset);
            core::ptr::copy_nonoverlapping(src, dst, 8);
            if len > 16 {
                wildcopy_overlap8(dst.add(8), src.add(8), len - 16);
            }
        }
    }
}

// Needs 16 bytes readable at `src`, writable at `dst`, disjoint.
#[inline(always)]
pub(crate) unsafe fn copy16(dst: *mut u8, src: *const u8) {
    // SAFETY: the caller's.
    unsafe { core::ptr::copy_nonoverlapping(src, dst, CHUNK) }
}

// `ZSTD_wildcopy` for `8 <= dst - src < 16`: 8-byte chunks, overshoot under 8.
#[inline(always)]
unsafe fn wildcopy_overlap8(mut dst: *mut u8, mut src: *const u8, len: usize) {
    // SAFETY: chunks stay within `len + 7` bytes of the starts; `dst - src >= 8`.
    unsafe {
        let end = dst.add(len);
        loop {
            core::ptr::copy_nonoverlapping(src, dst, 8);
            dst = dst.add(8);
            src = src.add(8);
            if dst >= end {
                break;
            }
        }
    }
}

// `ZSTD_overlapCopy8` for any `offset >= 1`: copies 8 bytes from `src = dst - offset`
// without branching (offsets from 8 on take the row 8), returns both advanced so that
// `dst - src >= 8`.
#[inline(always)]
unsafe fn overlap_copy8(dst: *mut u8, src: *const u8, offset: usize) -> (*mut u8, *const u8) {
    const DEC32: [usize; 9] = [0, 1, 2, 1, 4, 4, 4, 4, 4];
    const DEC64: [usize; 9] = [8, 8, 8, 7, 8, 9, 10, 11, 4];
    let row = offset.min(8);
    // SAFETY: reads stay within `src + 8` and writes within `dst + 8`; the second
    // copy's source starts at least 4 bytes before its destination.
    unsafe {
        for i in 0..4 {
            *dst.add(i) = *src.add(i);
        }
        let second = src.add(DEC32[row]);
        core::ptr::copy_nonoverlapping(second, dst.add(4), 4);
        (dst.add(8), second.add(8).sub(DEC64[row]))
    }
}

#[cfg(test)]
mod tests;
