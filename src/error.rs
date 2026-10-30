use core::fmt;

/// The `ZSTD_ErrorCode` values libzstd's decoder can return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Error {
    /// Not a zstd or skippable frame magic number.
    PrefixUnknown,
    /// The input is a legacy zstd frame (v0.1 to v0.7), which is not supported.
    LegacyFormatUnsupported(u8),
    /// Input ends inside a frame, or has bytes after the last frame.
    SrcSizeWrong,
    /// The frame header uses an unsupported parameter.
    FrameParameterUnsupported,
    /// The window of the frame is above the limit of the decoder.
    WindowTooLarge,
    /// The frame is malformed.
    CorruptionDetected,
    /// The content checksum does not match.
    ChecksumWrong,
    /// Invalid literals section header.
    LiteralsHeaderWrong,
    /// The output buffer or `max_len` is too small.
    DstSizeTooSmall,
    /// The frame needs another dictionary than the one set.
    DictionaryWrong,
    /// The dictionary is invalid.
    DictionaryCorrupted,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LegacyFormatUnsupported(version) => {
                write!(f, "unsupported legacy zstd format (v0.{version})")
            }
            _ => f.write_str(match self {
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
                Self::LegacyFormatUnsupported(_) => unreachable!(),
            }),
        }
    }
}

impl core::error::Error for Error {}

#[cfg(feature = "std")]
impl From<Error> for std::io::Error {
    fn from(error: Error) -> Self {
        Self::new(std::io::ErrorKind::InvalidData, error)
    }
}
