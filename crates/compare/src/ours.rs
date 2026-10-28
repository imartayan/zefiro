use rust_zstd_decoder::{Decoder, Error};

use crate::{Code, Outcome};

impl From<Error> for Code {
    fn from(error: Error) -> Self {
        match error {
            Error::PrefixUnknown => Self::PrefixUnknown,
            Error::SrcSizeWrong => Self::SrcSizeWrong,
            Error::FrameParameterUnsupported => Self::FrameParameterUnsupported,
            Error::WindowTooLarge => Self::WindowTooLarge,
            Error::CorruptionDetected => Self::CorruptionDetected,
            Error::ChecksumWrong => Self::ChecksumWrong,
            Error::LiteralsHeaderWrong => Self::LiteralsHeaderWrong,
            Error::DstSizeTooSmall => Self::DstSizeTooSmall,
            Error::DictionaryWrong => Self::DictionaryWrong,
            Error::DictionaryCorrupted => Self::DictionaryCorrupted,
            _ => Self::Other(u32::MAX),
        }
    }
}

/// Our one-shot decoder into a buffer of `capacity` bytes, as an [`Outcome`].
pub fn decompress_ours(decoder: &mut Decoder, input: &[u8], capacity: usize) -> Outcome {
    let mut output = vec![0u8; capacity];
    match decoder.decompress_into(input, &mut output) {
        Ok(written) => {
            output.truncate(written);
            Outcome::Ok {
                consumed: input.len(),
                output,
            }
        }
        Err(error) => Outcome::Err(error.into()),
    }
}
