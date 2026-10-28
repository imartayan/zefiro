use std::ffi::c_void;
use std::fmt;

use zstd_sys::*;

use crate::{Step, run_stream};

/// Error codes the decoder can return, plus harness-only ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    PrefixUnknown,
    SrcSizeWrong,
    FrameParameterUnsupported,
    WindowTooLarge,
    CorruptionDetected,
    ChecksumWrong,
    LiteralsHeaderWrong,
    DstSizeTooSmall,
    DictionaryWrong,
    DictionaryCorrupted,
    /// Streaming input ended inside a frame (libzstd reports no error).
    Truncated,
    Other(u32),
}

pub enum Outcome {
    /// `consumed` is the input length for one-shot (all frames are decoded).
    Ok {
        consumed: usize,
        output: Vec<u8>,
    },
    Err(Code),
}

/// A reused C decompression context.
pub struct Libzstd {
    dctx: *mut ZSTD_DCtx,
}

impl Code {
    fn from_result(result: usize) -> Self {
        use ZSTD_ErrorCode::*;
        // SAFETY: plain functions of an error value returned by libzstd.
        match unsafe { ZSTD_getErrorCode(result) } {
            ZSTD_error_prefix_unknown => Self::PrefixUnknown,
            ZSTD_error_srcSize_wrong => Self::SrcSizeWrong,
            ZSTD_error_frameParameter_unsupported => Self::FrameParameterUnsupported,
            ZSTD_error_frameParameter_windowTooLarge => Self::WindowTooLarge,
            ZSTD_error_corruption_detected => Self::CorruptionDetected,
            ZSTD_error_checksum_wrong => Self::ChecksumWrong,
            ZSTD_error_literals_headerWrong => Self::LiteralsHeaderWrong,
            ZSTD_error_dstSize_tooSmall => Self::DstSizeTooSmall,
            ZSTD_error_dictionary_wrong => Self::DictionaryWrong,
            ZSTD_error_dictionary_corrupted => Self::DictionaryCorrupted,
            other => Self::Other(other as u32),
        }
    }
}

impl fmt::Debug for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok { consumed, output } => {
                write!(
                    f,
                    "Ok {{ consumed: {consumed}, output: {} bytes }}",
                    output.len()
                )
            }
            Self::Err(code) => write!(f, "Err({code:?})"),
        }
    }
}

impl Libzstd {
    #[must_use]
    pub fn new() -> Self {
        // SAFETY: no preconditions.
        let dctx = unsafe { ZSTD_createDCtx() };
        assert!(!dctx.is_null());
        Self { dctx }
    }

    /// `ZSTD_decompressDCtx` into a buffer of `capacity` bytes.
    pub fn decompress(&mut self, input: &[u8], capacity: usize) -> Outcome {
        self.reset_session();
        let mut output = vec![0u8; capacity];
        match self.decompress_into(input, &mut output) {
            Ok(written) => {
                output.truncate(written);
                Outcome::Ok {
                    consumed: input.len(),
                    output,
                }
            }
            Err(code) => Outcome::Err(code),
        }
    }

    /// `ZSTD_decompressDCtx` into `output`, returning the number of bytes written.
    pub fn decompress_into(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize, Code> {
        // SAFETY: both slices are valid for the lengths passed, dctx is live.
        let result = unsafe {
            ZSTD_decompressDCtx(
                self.dctx,
                output.as_mut_ptr().cast::<c_void>(),
                output.len(),
                input.as_ptr().cast::<c_void>(),
                input.len(),
            )
        };
        if is_error(result) {
            return Err(Code::from_result(result));
        }
        Ok(result)
    }

    /// `ZSTD_decompressStream`, feeding `input` in pieces of `in_chunk` bytes
    /// into output buffers of `out_chunk` bytes. Input ending inside a frame
    /// is `Err(Code::Truncated)`.
    pub fn decompress_stream(
        &mut self,
        input: &[u8],
        in_chunk: usize,
        out_chunk: usize,
    ) -> Outcome {
        assert!(in_chunk > 0 && out_chunk > 0);
        self.reset_session();
        run_stream(
            |i, o| self.stream_step(i, o),
            input,
            &[in_chunk],
            &[out_chunk],
        )
        .outcome
    }

    /// One `ZSTD_decompressStream` call on the whole of `input` and `output`.
    pub fn stream_step(&mut self, input: &[u8], output: &mut [u8]) -> Result<Step, Code> {
        let mut in_buf = ZSTD_inBuffer {
            src: input.as_ptr().cast::<c_void>(),
            size: input.len(),
            pos: 0,
        };
        let mut out_buf = ZSTD_outBuffer {
            dst: output.as_mut_ptr().cast::<c_void>(),
            size: output.len(),
            pos: 0,
        };
        // SAFETY: buffers describe valid slices and pos <= size.
        let hint = unsafe { ZSTD_decompressStream(self.dctx, &raw mut out_buf, &raw mut in_buf) };
        if is_error(hint) {
            return Err(Code::from_result(hint));
        }
        Ok(Step {
            consumed: in_buf.pos,
            written: out_buf.pos,
            done: hint == 0,
        })
    }

    /// `ZSTD_d_windowLogMax` (0: default).
    pub fn set_window_log_max(&mut self, log: u32) {
        // SAFETY: dctx is live.
        let result = unsafe {
            ZSTD_DCtx_setParameter(
                self.dctx,
                ZSTD_dParameter::ZSTD_d_windowLogMax,
                log as std::ffi::c_int,
            )
        };
        assert!(!is_error(result), "window log {log} out of bounds");
    }

    pub(crate) fn reset_session(&mut self) {
        // SAFETY: dctx is live.
        let result =
            unsafe { ZSTD_DCtx_reset(self.dctx, ZSTD_ResetDirective::ZSTD_reset_session_only) };
        assert!(!is_error(result));
    }
}

impl Default for Libzstd {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Libzstd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Libzstd")
    }
}

impl Drop for Libzstd {
    fn drop(&mut self) {
        // SAFETY: dctx came from ZSTD_createDCtx and is freed once.
        unsafe { ZSTD_freeDCtx(self.dctx) };
    }
}

/// Panics with a readable diff unless both outcomes are the same. Partial
/// output on error is not compared.
pub fn assert_agree(ours: &Outcome, theirs: &Outcome, what: &str) {
    match (ours, theirs) {
        (Outcome::Err(a), Outcome::Err(b)) => assert_eq!(a, b, "{what}: error codes differ"),
        (
            Outcome::Ok {
                consumed: ca,
                output: oa,
            },
            Outcome::Ok {
                consumed: cb,
                output: ob,
            },
        ) => {
            assert_eq!(ca, cb, "{what}: consumed differs");
            if oa != ob {
                let first = oa.iter().zip(ob).position(|(a, b)| a != b);
                panic!(
                    "{what}: outputs differ (ours {} B, theirs {} B, first difference at {first:?})",
                    oa.len(),
                    ob.len()
                );
            }
        }
        _ => panic!("{what}: ours {ours:?}, libzstd {theirs:?}"),
    }
}

/// Checks that the linked C library is the oracle PLAN.md D1 and 4.2-6 expect:
/// libzstd 1.5.7, no legacy support, dictionary ID check enabled (it is
/// compiled out in `FUZZING_BUILD_MODE_UNSAFE_FOR_PRODUCTION` builds).
pub fn check_oracle_build() {
    // SAFETY: no preconditions.
    assert_eq!(unsafe { ZSTD_versionNumber() }, 10507, "not libzstd 1.5.7");
    let mut oracle = Libzstd::new();
    // Single-segment frame, 1-byte dictionary ID 5, content size 0, empty raw last block.
    let with_dictionary_id = [0x28, 0xB5, 0x2F, 0xFD, 0x21, 0x05, 0x00, 0x01, 0x00, 0x00];
    assert!(
        matches!(
            oracle.decompress(&with_dictionary_id, 16),
            Outcome::Err(Code::DictionaryWrong)
        ),
        "dictionary ID check is compiled out"
    );
    let legacy_v07_magic = [0x27, 0xB5, 0x2F, 0xFD, 0, 0, 0, 0, 0, 0];
    assert!(
        matches!(
            oracle.decompress(&legacy_v07_magic, 16),
            Outcome::Err(Code::PrefixUnknown)
        ),
        "legacy formats are enabled"
    );
}

fn is_error(result: usize) -> bool {
    // SAFETY: pure function of its argument.
    unsafe { ZSTD_isError(result) != 0 }
}
