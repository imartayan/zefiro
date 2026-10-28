//! Port of libzstd's `BIT_DStream_t`: a backward bit reader over a `u64`
//! container on every target. Shifts are masked by 63 like C, so reading past
//! the start of the stream gives the same garbage bits.

const CONTAINER_BITS: u32 = u64::BITS;
const CONTAINER_BYTES: usize = size_of::<u64>();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    // Container fully refilled.
    Unfinished,
    // Start of the stream reached, some bits are left.
    EndOfBuffer,
    // Every bit consumed, exactly.
    Completed,
    // More bits requested than present; sticky.
    Overflow,
}

#[derive(Debug, Clone)]
pub(crate) struct BitReader<'a> {
    container: u64,
    bits_consumed: u32,
    // Offset in `src` of the first byte of `container` (for streams under 8 bytes: 0).
    ptr: usize,
    src: &'a [u8],
}

impl<'a> BitReader<'a> {
    // `src` is the exact stream. `None` when empty or when the end mark (the
    // highest set bit of the last byte) is missing.
    pub(crate) fn new(src: &'a [u8]) -> Option<Self> {
        let last_byte = *src.last()?;
        if last_byte == 0 {
            return None;
        }
        let mut bits_consumed = last_byte.leading_zeros() + 1;
        let (ptr, container) = if src.len() >= CONTAINER_BYTES {
            let ptr = src.len() - CONTAINER_BYTES;
            (ptr, read_container(src, ptr))
        } else {
            let mut bytes = [0; CONTAINER_BYTES];
            bytes[..src.len()].copy_from_slice(src);
            bits_consumed += (CONTAINER_BYTES - src.len()) as u32 * 8;
            (0, u64::from_le_bytes(bytes))
        };
        Some(Self {
            container,
            bits_consumed,
            ptr,
            src,
        })
    }

    // Resumes a stream at `ptr` (offset in `src`, 8 bytes readable there) with
    // `bits_consumed` bits used, like `HUF_initRemainingDStream`: `src` starts
    // at the lowest byte reloads may reach, which can be below the stream.
    pub(crate) fn resume(src: &'a [u8], ptr: usize, bits_consumed: u32) -> Self {
        Self {
            container: read_container(src, ptr),
            bits_consumed,
            ptr,
            src,
        }
    }

    // `nb_bits` < 32. Valid when `bits_consumed + nb_bits` <= 64.
    #[inline(always)]
    pub(crate) fn look_bits(&self, nb_bits: u32) -> u64 {
        self.look_bits_after(self.bits_consumed, nb_bits)
    }

    #[inline(always)]
    fn look_bits_after(&self, consumed: u32, nb_bits: u32) -> u64 {
        debug_assert!(nb_bits < 32);
        let start = CONTAINER_BITS.wrapping_sub(consumed).wrapping_sub(nb_bits);
        (self.container >> (start & 63)) & ((1 << nb_bits) - 1)
    }

    // Three `read_bits` in a row, each position one add away from `bits_consumed`.
    #[inline(always)]
    pub(crate) fn read_bits_x3(&mut self, nb_bits: [u32; 3]) -> [u64; 3] {
        let [first, second, third] = nb_bits;
        let consumed = self.bits_consumed;
        let after_second = first + second;
        let values = [
            self.look_bits_after(consumed, first),
            self.look_bits_after(consumed + first, second),
            self.look_bits_after(consumed + after_second, third),
        ];
        self.bits_consumed = consumed + (after_second + third);
        values
    }

    // `nb_bits` in 1..=63 (0 gives garbage).
    #[inline(always)]
    pub(crate) fn look_bits_fast(&self, nb_bits: u32) -> u64 {
        (self.container << (self.bits_consumed & 63)) >> (CONTAINER_BITS.wrapping_sub(nb_bits) & 63)
    }

    #[inline(always)]
    pub(crate) fn unread(&self) -> u64 {
        self.container << (self.bits_consumed & 63)
    }

    #[inline(always)]
    pub(crate) fn skip(&mut self, nb_bits: u32) {
        self.bits_consumed += nb_bits;
    }

    // X2's last-symbol step: skips unless the container is spent, clamped to its size.
    #[inline(always)]
    pub(crate) fn skip_clamped(&mut self, nb_bits: u32) {
        if self.bits_consumed < CONTAINER_BITS {
            self.bits_consumed = (self.bits_consumed + nb_bits).min(CONTAINER_BITS);
        }
    }

    #[inline(always)]
    pub(crate) fn read_bits(&mut self, nb_bits: u32) -> u64 {
        let value = self.look_bits(nb_bits);
        self.skip(nb_bits);
        value
    }

    #[inline(always)]
    pub(crate) fn read_bits_fast(&mut self, nb_bits: u32) -> u64 {
        let value = self.look_bits_fast(nb_bits);
        self.skip(nb_bits);
        value
    }

    // Refills the container. Once `Overflow`, always `Overflow`.
    #[inline(always)]
    pub(crate) fn reload(&mut self) -> Status {
        if self.bits_consumed > CONTAINER_BITS {
            // C also points `ptr` at a zeroed word, which nothing can observe.
            return Status::Overflow;
        }
        if self.ptr >= CONTAINER_BYTES {
            // SAFETY: `bits_consumed <= 64` (checked above) and `ptr >= 8`.
            return unsafe { self.reload_unchecked() };
        }
        if self.ptr == 0 {
            return if self.bits_consumed < CONTAINER_BITS {
                Status::EndOfBuffer
            } else {
                Status::Completed
            };
        }
        let mut nb_bytes = (self.bits_consumed >> 3) as usize;
        let mut status = Status::Unfinished;
        if nb_bytes > self.ptr {
            nb_bytes = self.ptr;
            status = Status::EndOfBuffer;
        }
        self.ptr -= nb_bytes;
        self.bits_consumed -= nb_bytes as u32 * 8;
        self.container = read_container(self.src, self.ptr);
        status
    }

    // `Overflow` means "use `reload`".
    // Safety: `bits_consumed <= 64`.
    #[inline(always)]
    pub(crate) unsafe fn reload_fast(&mut self) -> Status {
        if self.ptr < CONTAINER_BYTES {
            return Status::Overflow;
        }
        // SAFETY: `ptr >= 8` and the caller's.
        unsafe { self.reload_unchecked() }
    }

    // True when every bit, end mark included, was consumed exactly.
    #[inline(always)]
    pub(crate) fn end_of_stream(&self) -> bool {
        self.ptr == 0 && self.bits_consumed == CONTAINER_BITS
    }

    // Needs `bits_consumed <= 64` and `ptr >= 8`.
    #[inline(always)]
    unsafe fn reload_unchecked(&mut self) -> Status {
        debug_assert!(self.bits_consumed <= CONTAINER_BITS && self.ptr >= CONTAINER_BYTES);
        self.ptr -= (self.bits_consumed >> 3) as usize;
        self.bits_consumed &= 7;
        // SAFETY: `ptr + 8 <= src.len()` held at construction (checked read) and `ptr` only decreases.
        self.container = unsafe {
            u64::from_le(
                self.src
                    .as_ptr()
                    .add(self.ptr)
                    .cast::<u64>()
                    .read_unaligned(),
            )
        };
        Status::Unfinished
    }
}

#[inline(always)]
fn read_container(src: &[u8], ptr: usize) -> u64 {
    u64::from_le_bytes(src[ptr..ptr + CONTAINER_BYTES].try_into().unwrap())
}

#[cfg(test)]
mod tests;
