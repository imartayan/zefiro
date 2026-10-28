use core::fmt;

/// The `ZSTD_ErrorCode` values libzstd's decoder can return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Error {
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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::PrefixUnknown => "unknown frame descriptor",
            Self::SrcSizeWrong => "src size is incorrect",
            Self::FrameParameterUnsupported => "unsupported frame parameter",
            Self::WindowTooLarge => "frame requires too much memory for decoding",
            Self::CorruptionDetected => "data corruption detected",
            Self::ChecksumWrong => "restored data doesn't match checksum",
            Self::LiteralsHeaderWrong => "header of literals block doesn't respect format",
            Self::DstSizeTooSmall => "destination buffer is too small",
            Self::DictionaryWrong => "dictionary mismatch",
            Self::DictionaryCorrupted => "dictionary is corrupted",
        })
    }
}

impl core::error::Error for Error {}

#[cfg(feature = "std")]
impl From<Error> for std::io::Error {
    fn from(error: Error) -> Self {
        Self::new(std::io::ErrorKind::InvalidData, error)
    }
}
