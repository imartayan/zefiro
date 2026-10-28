use std::ffi::c_void;

use zstd_sys::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralMode {
    Auto,
    Huffman,
    Raw,
}

/// Compression parameters (`ZSTD_c_*`); 0 means libzstd's default where noted.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub level: i32,
    pub checksum: bool,
    pub content_size: bool,
    /// 0: from level.
    pub window_log: i32,
    pub long_distance: bool,
    /// 0: off.
    pub target_cblock_size: i32,
    pub literal_mode: LiteralMode,
    /// 0: from level.
    pub min_match: i32,
}

struct Cctx(*mut ZSTD_CCtx);

impl Default for Params {
    fn default() -> Self {
        Self {
            level: 3,
            checksum: false,
            content_size: true,
            window_log: 0,
            long_distance: false,
            target_cblock_size: 0,
            literal_mode: LiteralMode::Auto,
            min_match: 0,
        }
    }
}

impl Params {
    #[must_use]
    pub fn level(level: i32) -> Self {
        Self {
            level,
            ..Self::default()
        }
    }
}

impl Cctx {
    fn new(params: &Params) -> Self {
        // SAFETY: no preconditions.
        let cctx = Self(unsafe { ZSTD_createCCtx() });
        assert!(!cctx.0.is_null());
        let literal_mode = match params.literal_mode {
            LiteralMode::Auto => ZSTD_literalCompressionMode_e::ZSTD_lcm_auto,
            LiteralMode::Huffman => ZSTD_literalCompressionMode_e::ZSTD_lcm_huffman,
            LiteralMode::Raw => ZSTD_literalCompressionMode_e::ZSTD_lcm_uncompressed,
        };
        cctx.set(ZSTD_cParameter::ZSTD_c_compressionLevel, params.level);
        cctx.set(ZSTD_cParameter::ZSTD_c_minMatch, params.min_match);
        cctx.set(ZSTD_cParameter::ZSTD_c_checksumFlag, params.checksum.into());
        cctx.set(
            ZSTD_cParameter::ZSTD_c_contentSizeFlag,
            params.content_size.into(),
        );
        cctx.set(ZSTD_cParameter::ZSTD_c_windowLog, params.window_log);
        cctx.set(
            ZSTD_cParameter::ZSTD_c_enableLongDistanceMatching,
            params.long_distance.into(),
        );
        cctx.set(
            ZSTD_cParameter::ZSTD_c_targetCBlockSize,
            params.target_cblock_size,
        );
        cctx.set(
            ZSTD_cParameter::ZSTD_c_experimentalParam5,
            literal_mode as i32,
        );
        cctx
    }

    fn compress(&self, data: &[u8]) -> Option<Vec<u8>> {
        // SAFETY: bound is a pure function.
        let mut out = vec![0u8; unsafe { ZSTD_compressBound(data.len()) }];
        // SAFETY: slices are valid for the lengths passed.
        let n = unsafe {
            ZSTD_compress2(
                self.0,
                out.as_mut_ptr().cast::<c_void>(),
                out.len(),
                data.as_ptr().cast::<c_void>(),
                data.len(),
            )
        };
        if is_error(n) {
            return None;
        }
        out.truncate(n);
        Some(out)
    }

    fn set(&self, param: ZSTD_cParameter, value: i32) {
        // SAFETY: cctx is live.
        let result = unsafe { ZSTD_CCtx_setParameter(self.0, param, value) };
        assert!(!is_error(result), "{param:?}={value} rejected");
    }
}

impl Drop for Cctx {
    fn drop(&mut self) {
        // SAFETY: created by ZSTD_createCCtx and freed once.
        unsafe { ZSTD_freeCCtx(self.0) };
    }
}

/// A dictionary trained by `ZDICT_trainFromBuffer` on `samples`, of at most `capacity`
/// bytes (`None` if libzstd cannot train one).
#[must_use]
pub fn train_dictionary(samples: &[&[u8]], capacity: usize) -> Option<Vec<u8>> {
    let flat = samples.concat();
    let sizes: Vec<usize> = samples.iter().map(|sample| sample.len()).collect();
    let mut dictionary = vec![0u8; capacity];
    // SAFETY: the buffers are valid for the lengths passed, `sizes` has one entry per sample.
    let size = unsafe {
        ZDICT_trainFromBuffer(
            dictionary.as_mut_ptr().cast::<c_void>(),
            capacity,
            flat.as_ptr().cast::<c_void>(),
            sizes.as_ptr(),
            sizes.len() as u32,
        )
    };
    // SAFETY: pure function of its argument.
    if unsafe { ZDICT_isError(size) } != 0 {
        return None;
    }
    dictionary.truncate(size);
    Some(dictionary)
}

/// One frame with `dictionary` (`ZSTD_CCtx_loadDictionary`, content type auto), writing
/// its ID in the frame header if it has one and `write_id`. `None` if the encoder refuses
/// the dictionary (or fails on this input).
#[must_use]
pub fn compress_with_dictionary(
    data: &[u8],
    params: &Params,
    dictionary: &[u8],
    write_id: bool,
) -> Option<Vec<u8>> {
    let cctx = Cctx::new(params);
    cctx.set(ZSTD_cParameter::ZSTD_c_dictIDFlag, write_id.into());
    // SAFETY: the slice is valid for its length, cctx is live.
    let loaded = unsafe {
        ZSTD_CCtx_loadDictionary(
            cctx.0,
            dictionary.as_ptr().cast::<c_void>(),
            dictionary.len(),
        )
    };
    if is_error(loaded) {
        return None;
    }
    cctx.compress(data)
}

/// One frame, one call (`ZSTD_compress2`).
#[must_use]
pub fn compress(data: &[u8], params: &Params) -> Vec<u8> {
    Cctx::new(params)
        .compress(data)
        .expect("compression failed")
}

/// One frame of unknown content size, compressed in steps: each step feeds
/// `add` bytes and flushes (ending the current block) if `flush`. Many small
/// blocks come out.
#[must_use]
pub fn compress_stream(data: &[u8], params: &Params, steps: &[(usize, bool)]) -> Vec<u8> {
    let cctx = Cctx::new(params);
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 1 << 17];
    let mut pos = 0;
    let mut run = |end: usize, op: ZSTD_EndDirective, pos: &mut usize| loop {
        let mut in_buf = ZSTD_inBuffer {
            src: data.as_ptr().cast::<c_void>(),
            size: end,
            pos: *pos,
        };
        let mut out_buf = ZSTD_outBuffer {
            dst: chunk.as_mut_ptr().cast::<c_void>(),
            size: chunk.len(),
            pos: 0,
        };
        // SAFETY: buffers describe valid slices and pos <= size.
        let left = unsafe { ZSTD_compressStream2(cctx.0, &raw mut out_buf, &raw mut in_buf, op) };
        assert!(!is_error(left), "stream compression failed");
        out.extend_from_slice(&chunk[..out_buf.pos]);
        *pos = in_buf.pos;
        let done = *pos == end && (op == ZSTD_EndDirective::ZSTD_e_continue || left == 0);
        if done {
            break;
        }
    };
    for &(add, flush) in steps {
        let end = data.len().min(pos + add);
        let op = if flush {
            ZSTD_EndDirective::ZSTD_e_flush
        } else {
            ZSTD_EndDirective::ZSTD_e_continue
        };
        run(end, op, &mut pos);
    }
    run(data.len(), ZSTD_EndDirective::ZSTD_e_end, &mut pos);
    out
}

/// A skippable frame with magic `0x184D2A50 + nibble` (nibble < 16).
#[must_use]
pub fn skippable_frame(payload: &[u8], nibble: u8) -> Vec<u8> {
    let mut frame = (0x184D_2A50 + u32::from(nibble)).to_le_bytes().to_vec();
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn is_error(result: usize) -> bool {
    // SAFETY: pure function of its argument.
    unsafe { ZSTD_isError(result) != 0 }
}
