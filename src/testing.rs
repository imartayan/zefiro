//! Helpers shared by unit tests.

use alloc::vec;
use alloc::vec::Vec;

pub(crate) use fastrand::Rng;

// Bytes with a geometric symbol distribution over a random alphabet, from
// nearly flat to very skewed, so that trees of every depth appear.
#[cfg_attr(target_family = "wasm", allow(dead_code))]
pub(crate) fn skewed_bytes(rng: &mut Rng) -> Vec<u8> {
    let alphabet = rng.choice([2, 3, 5, 8, 16, 16, 40, 100, 256]).unwrap();
    let ratio = [0.5, 0.7, 0.8, 0.9, 0.97, 1.0][rng.usize(..6)];
    let mut symbols: Vec<u8> = (0..=255).collect();
    rng.shuffle(&mut symbols);
    let mut cumulative = Vec::new();
    let (mut weight, mut total) = (1_000_000.0f64, 0u64);
    for _ in 0..alphabet {
        total += (weight as u64).max(1);
        cumulative.push(total);
        weight *= ratio;
    }
    let len = (1 << (6 + rng.usize(..11))) + rng.usize(..100);
    (0..len)
        .map(|_| {
            let pick = rng.u64(..) % total;
            symbols[cumulative.partition_point(|&c| c <= pick)]
        })
        .collect()
}

// Header of a raw (`kind` 0) or RLE (1) section: 1, 2 or 3 bytes by size.
fn header(kind: u8, len: usize) -> Vec<u8> {
    match len {
        0..=31 => vec![(len << 3) as u8 | kind],
        32..=4095 => ((len << 4) as u16 | 0x4 | u16::from(kind))
            .to_le_bytes()
            .to_vec(),
        _ => ((len << 4) as u32 | 0xC | u32::from(kind)).to_le_bytes()[..3].to_vec(),
    }
}

pub(crate) fn raw_section(data: &[u8]) -> Vec<u8> {
    [header(0, data.len()), data.to_vec()].concat()
}

pub(crate) fn rle_section(byte: u8, len: usize) -> Vec<u8> {
    [header(1, len), vec![byte]].concat()
}

// Compressed or treeless section; `size_format` 0 is one stream, 1 to 3 four.
pub(crate) fn huffman_section(kind: u8, size_format: u8, len: usize, payload: &[u8]) -> Vec<u8> {
    let (header_size, size_bits) = [(3, 10), (3, 10), (4, 14), (5, 18)][size_format as usize];
    let value = u64::from(kind)
        | u64::from(size_format) << 2
        | (len as u64) << 4
        | (payload.len() as u64) << (4 + size_bits);
    [&value.to_le_bytes()[..header_size], payload].concat()
}

// A random complete prefix code in zstd's tree description (raw 4-bit
// weights), with an encoder for it, so that streams can be made without libzstd.
#[derive(Debug)]
pub(crate) struct TestTree {
    // The tree description as stored before the streams.
    pub(crate) header: Vec<u8>,
    pub(crate) symbols: Vec<u8>,
    // Depth of the deepest leaf, which is the tree's table log.
    pub(crate) depth: u32,
    // Code value (read MSB first) and length by symbol.
    codes: [(u32, u32); 256],
}

impl TestTree {
    pub(crate) fn random(rng: &mut Rng, max_depth: u32) -> Self {
        let most = rng.choice([2, 6, 30, 126]).unwrap().min(1 << max_depth);
        let count = 2 + rng.usize(..most - 1);
        // Split random leaves until there are `count`; depths stay within `max_depth`.
        let mut depths = alloc::vec![1u32, 1];
        while depths.len() < count {
            let leaf = rng.usize(..depths.len());
            if depths[leaf] < max_depth {
                depths[leaf] += 1;
                depths.push(depths[leaf]);
            }
        }
        rng.shuffle(&mut depths);
        let depth = *depths.iter().max().unwrap();
        // Symbols below 129 so that the raw 4-bit description covers them all.
        let mut symbols: Vec<u8> = (0..=128).collect();
        rng.shuffle(&mut symbols);
        symbols.truncate(count);
        symbols.sort_unstable();
        let weights: Vec<u32> = depths.iter().map(|&d| depth - d + 1).collect();

        // Cells are handed out by increasing weight, then symbol, as libzstd's table fill does.
        let mut codes = [(0, 0); 256];
        let mut next_cell = 0u32;
        for weight in 1..=depth {
            for (_, &symbol) in symbols
                .iter()
                .enumerate()
                .filter(|(i, _)| weights[*i] == weight)
            {
                codes[symbol as usize] = (next_cell >> (weight - 1), depth + 1 - weight);
                next_cell += 1 << (weight - 1);
            }
        }
        // One weight per symbol value up to the last one, whose weight is implied.
        let last = *symbols.last().unwrap() as usize;
        let mut listed = alloc::vec![0u32; last];
        for (&symbol, &weight) in symbols.iter().zip(&weights) {
            if (symbol as usize) < last {
                listed[symbol as usize] = weight;
            }
        }
        let mut header = alloc::vec![127 + listed.len() as u8];
        for pair in listed.chunks(2) {
            header.push((pair[0] << 4 | pair.get(1).copied().unwrap_or(0)) as u8);
        }
        Self {
            header,
            symbols,
            depth,
            codes,
        }
    }

    pub(crate) fn random_data(&self, rng: &mut Rng, len: usize) -> Vec<u8> {
        (0..len)
            .map(|_| self.symbols[rng.usize(..self.symbols.len())])
            .collect()
    }

    // One stream: the symbols in reverse, each code LSB first, then the end mark.
    pub(crate) fn encode_stream(&self, data: &[u8]) -> Vec<u8> {
        let mut bits = Vec::new();
        for &symbol in data.iter().rev() {
            let (code, length) = self.codes[symbol as usize];
            assert!(length > 0, "symbol {symbol} has no code");
            bits.extend((0..length).map(|k| code >> k & 1 == 1));
        }
        let mut bytes = alloc::vec![0u8; (bits.len() + 1).div_ceil(8)];
        for (i, _) in bits.iter().enumerate().filter(|(_, bit)| **bit) {
            bytes[i / 8] |= 1 << (i % 8);
        }
        bytes[bits.len() / 8] |= 1 << (bits.len() % 8);
        bytes
    }

    // A jump table and four streams, the output split as the decoder does.
    pub(crate) fn encode_four_streams(&self, data: &[u8]) -> Vec<u8> {
        let segment = data.len().div_ceil(4);
        let streams: Vec<Vec<u8>> = (0..4)
            .map(|i| {
                self.encode_stream(
                    &data[(i * segment).min(data.len())..((i + 1) * segment).min(data.len())],
                )
            })
            .collect();
        let mut out = Vec::new();
        for stream in &streams[..3] {
            out.extend_from_slice(&(stream.len() as u16).to_le_bytes());
        }
        for stream in &streams {
            out.extend_from_slice(stream);
        }
        out
    }
}

// Huffman streams made by C libzstd.
#[cfg(not(target_family = "wasm"))]
pub(crate) mod c_huf {
    use core::ffi::{c_int, c_void};

    use alloc::vec;
    use alloc::vec::Vec;

    use zstd_sys::ZSTD_isError;

    unsafe extern "C" {
        fn HUF_compress1X_repeat(
            dst: *mut c_void,
            dst_size: usize,
            src: *const c_void,
            src_size: usize,
            max_symbol: u32,
            table_log: u32,
            wksp: *mut c_void,
            wksp_size: usize,
            table: *mut usize,
            repeat: *mut c_int,
            flags: c_int,
        ) -> usize;
        fn HUF_compress4X_repeat(
            dst: *mut c_void,
            dst_size: usize,
            src: *const c_void,
            src_size: usize,
            max_symbol: u32,
            table_log: u32,
            wksp: *mut c_void,
            wksp_size: usize,
            table: *mut usize,
            repeat: *mut c_int,
            flags: c_int,
        ) -> usize;
    }

    // Tree description and one stream (or a jump table and four): `None` when
    // libzstd finds the data incompressible or too short.
    pub(crate) fn compress(data: &[u8], four_streams: bool, max_table_log: u32) -> Option<Vec<u8>> {
        let mut dst = vec![0u8; data.len() + 1024];
        let mut table = [0usize; 257];
        let mut wksp = [0u64; 1088];
        let mut repeat = 0;
        let compress = if four_streams {
            HUF_compress4X_repeat
        } else {
            HUF_compress1X_repeat
        };
        // SAFETY: buffers are valid for the sizes given, the workspace is 8-byte
        // aligned and larger than HUF_WORKSPACE_SIZE, the table holds 257 words.
        let size = unsafe {
            compress(
                dst.as_mut_ptr().cast(),
                dst.len(),
                data.as_ptr().cast(),
                data.len(),
                255,
                max_table_log,
                wksp.as_mut_ptr().cast(),
                size_of_val(&wksp),
                table.as_mut_ptr(),
                &raw mut repeat,
                0,
            )
        };
        // SAFETY: pure function of its argument.
        if unsafe { ZSTD_isError(size) } != 0 || size <= 1 {
            return None;
        }
        dst.truncate(size);
        Some(dst)
    }
}

// Compares byte strings, reporting the first difference instead of dumping both.
pub(crate) fn assert_same(actual: &[u8], expected: &[u8], what: &str) {
    assert_eq!(actual.len(), expected.len(), "{what}: lengths differ");
    if let Some(at) = actual.iter().zip(expected).position(|(a, b)| a != b) {
        panic!("{what}: first difference at {at}");
    }
}
